//! GPU-H6 fair work queue: deficit round robin over a few classes of verifier work
//! (compute output checks, transfer-lane checks, ...). One class cannot starve another, an idle
//! class does not waste capacity, and work inside a class stays in order. Pure logic; the
//! adapter wraps it in a lock and a condition variable.
use std::collections::VecDeque;

struct Class<T> {
    items: VecDeque<(u64, T)>,
    weight: u64,
    deficit: u64,
    fresh: bool,
    served: u64,
    served_cost: u64,
}

pub struct FairQueue<T> {
    classes: Vec<Class<T>>,
    quantum: u64,
    cursor: usize,
}

impl<T> FairQueue<T> {
    /// `weights[i]` is class `i`'s share when every class is busy; `quantum` is the cost one
    /// unit of weight buys per round (a few typical items keeps the interleaving fine-grained).
    #[must_use]
    pub fn new(weights: &[u64], quantum: u64) -> Self {
        Self {
            classes: weights
                .iter()
                .map(|w| Class {
                    items: VecDeque::new(),
                    weight: (*w).max(1),
                    deficit: 0,
                    fresh: false,
                    served: 0,
                    served_cost: 0,
                })
                .collect(),
            quantum: quantum.max(1),
            cursor: 0,
        }
    }

    pub fn push(&mut self, class: usize, cost: u64, item: T) {
        self.classes[class].items.push_back((cost.max(1), item));
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.classes.iter().map(|c| c.items.len()).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn queued(&self, class: usize) -> usize {
        self.classes[class].items.len()
    }

    /// Items and total cost served per class so far.
    #[must_use]
    pub fn served(&self, class: usize) -> (u64, u64) {
        (self.classes[class].served, self.classes[class].served_cost)
    }

    /// The next item by deficit round robin, with its class.
    pub fn pop(&mut self) -> Option<(usize, T)> {
        if self.is_empty() {
            return None;
        }
        loop {
            let n = self.classes.len();
            let c = &mut self.classes[self.cursor];
            if c.items.is_empty() {
                // An idle class keeps no credit: it cannot bank service for a later burst.
                c.deficit = 0;
                c.fresh = false;
                self.cursor = (self.cursor + 1) % n;
                continue;
            }
            if !c.fresh {
                c.deficit += c.weight * self.quantum;
                c.fresh = true;
            }
            let cost = c.items.front().map_or(0, |(cost, _)| *cost);
            if cost <= c.deficit
                && let Some((cost, item)) = c.items.pop_front()
            {
                c.deficit -= cost;
                c.served += 1;
                c.served_cost += cost;
                let class = self.cursor;
                return Some((class, item));
            }
            c.fresh = false;
            self.cursor = (self.cursor + 1) % n;
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::*;

    fn drain(q: &mut FairQueue<u64>, count: usize) -> Vec<(usize, u64)> {
        (0..count).map_while(|_| q.pop()).collect()
    }

    #[test]
    fn saturated_classes_share_service_by_weight_and_cost_not_by_item_count() {
        // Class 0 sends small items, class 1 sends items eight times larger; weights 1:1.
        let mut q = FairQueue::new(&[1, 1], 1_000);
        for i in 0..4_000 {
            q.push(0, 1_000, i);
            q.push(1, 8_000, i);
        }
        let popped = drain(&mut q, 2_000);
        let cost = |class| {
            popped.iter().filter(|(c, _)| *c == class).count() as u64
                * if class == 0 { 1_000 } else { 8_000 }
        };
        let (a, b) = (cost(0), cost(1));
        let ratio = a as f64 / b as f64;
        assert!((0.9..=1.1).contains(&ratio), "served cost {a} vs {b}");
        // Weights 3:1 give three times the service.
        let mut q = FairQueue::new(&[3, 1], 1_000);
        for i in 0..4_000 {
            q.push(0, 1_000, i);
            q.push(1, 1_000, i);
        }
        let popped = drain(&mut q, 4_000);
        let a = popped.iter().filter(|(c, _)| *c == 0).count() as f64;
        let b = popped.iter().filter(|(c, _)| *c == 1).count() as f64;
        assert!((2.8..=3.2).contains(&(a / b)), "{a} vs {b}");
    }

    #[test]
    fn an_idle_class_wastes_nothing_and_a_late_class_is_served_at_once() {
        let mut q = FairQueue::new(&[1, 1], 100);
        for i in 0..50 {
            q.push(0, 100, i);
        }
        // Only class 0 has work: it gets everything.
        assert!(drain(&mut q, 10).iter().all(|(c, _)| *c == 0));
        // A class that shows up later is served within one round, not after the backlog.
        q.push(1, 100, 999);
        let next: Vec<usize> = drain(&mut q, 3).into_iter().map(|(c, _)| c).collect();
        assert!(next.contains(&1), "{next:?}");
    }

    #[test]
    fn no_class_starves_even_when_its_items_are_far_larger_than_the_quantum() {
        let mut q = FairQueue::new(&[1, 1], 10);
        for i in 0..1_000 {
            q.push(0, 10, i);
        }
        q.push(1, 1_000_000, 7);
        let popped = drain(&mut q, 1_100);
        let position = popped
            .iter()
            .position(|(c, _)| *c == 1)
            .expect("the big item is served");
        assert!(position < 1_100, "served after {position} small items");
        assert_eq!(q.served(1), (1, 1_000_000));
    }

    #[test]
    fn items_stay_in_order_within_a_class_and_nothing_is_lost() {
        let mut q = FairQueue::new(&[2, 1, 1], 500);
        for i in 0..300_u64 {
            q.push(usize::try_from(i % 3).unwrap(), 100 + i, i);
        }
        let popped = drain(&mut q, 1_000);
        assert_eq!(popped.len(), 300);
        assert!(q.is_empty() && q.pop().is_none());
        for class in 0..3 {
            let items: Vec<u64> = popped
                .iter()
                .filter(|(c, _)| *c == class)
                .map(|(_, i)| *i)
                .collect();
            assert!(
                items.windows(2).all(|w| w[0] < w[1]),
                "class {class} reordered"
            );
            assert_eq!(items.len(), 100);
        }
        assert_eq!(q.queued(0), 0);
    }
}

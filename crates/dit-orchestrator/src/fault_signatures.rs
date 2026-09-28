//! Failure signatures and the tier-2 ranking (`docs/COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md`,
//! step F6, sections 2.4–2.6). A failure the rules could not place on one cell stays in an
//! unresolved group (`PCIe-GPU 1|VRAM 1` …). At the end of a run the failures of each group are
//! read together, and every signature that points at one of the group's cells adds its weight to
//! that cell:
//!
//! - **time**: hardware events on the cell coincide with the failures more often than chance
//!   (binomial test against the share of the run the event windows cover);
//! - **hardware**: the cell counted hardware events (AER, replays, ECC, SMART, EDAC) in the run;
//! - **address**: failures repeat at the same address (a memory cell), or never repeat (a path);
//! - **bit**: one data bit is in almost every syndrome (a data line of a memory chip);
//! - **load**: failures only while every layer runs, none in the phases alone (Poisson test).
//!
//! A group whose best cell has a strong enough score and a clear lead moves to that cell as
//! SUSPECTED; weak or conflicting evidence leaves it unresolved. The ranking never makes anything CONFIRMED. Weights are versioned.
use crate::fault_ledger::{Component, Confidence, Ledger, Occurrence, Ranking};
use std::collections::{BTreeMap, BTreeSet};

/// Version of [`WEIGHTS`] and of the tests below; written into each ranking.
pub const WEIGHTS_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub struct Weights {
    pub time: i64,
    pub hardware: i64,
    pub address_repeat: i64,
    pub address_spread: i64,
    pub fixed_bit: i64,
    pub load_only: i64,
}

pub const WEIGHTS: Weights = Weights {
    time: 4,
    hardware: 1,
    address_repeat: 3,
    address_spread: 1,
    fixed_bit: 2,
    load_only: 3,
};

/// A group moves only on at least one strong signature (time, repeated address) or a medium one
/// with support; hardware counts and spread addresses alone only corroborate.
pub const MIN_SCORE: i64 = 3;
/// Conflicting strong signatures leave the group unresolved.
pub const MIN_MARGIN: i64 = 2;
/// Significance level of the signature tests.
const ALPHA: f64 = 0.01;
/// The time test runs for every link of every group and carries the largest weight: it must
/// clear a stricter level so chance coincidences with background link noise do not outvote the
/// memory signatures.
const TIME_ALPHA: f64 = 0.001;
/// Failures (or failure moments) a signature needs before it counts.
const MIN_FAILURES: usize = 3;
/// The link monitor samples every 5 s: an event seen at `T` happened in `(T-5, T]`. Failures are
/// timed when the session drains them (up to a tick later), hence the window `[T-6, T+2]`.
const WINDOW_BEFORE: f64 = 6.0;
const WINDOW_AFTER: f64 = 2.0;

/// One phase of a session: how long it ran, whether it is the all-layers load, and which layers
/// (slugs: `cpu-ram`, `gpu-vram`, `cpu-nvme`, `chain`) it ran.
#[derive(Clone, Debug)]
pub struct PhaseExposure {
    pub label: String,
    pub seconds: f64,
    pub load: bool,
    pub layers: Vec<&'static str>,
}

/// What the run knows besides the failures themselves.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// Times of the hardware events seen on each cell (seconds since the run started).
    pub hardware_times: BTreeMap<Component, Vec<f64>>,
    pub elapsed_seconds: f64,
    pub phases: Vec<PhaseExposure>,
}

/// Scores of one group and why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scored {
    pub scores: BTreeMap<Component, i64>,
    pub reasons: Vec<String>,
}

impl Scored {
    /// The cell that leads the group: at least [`MIN_SCORE`] and [`MIN_MARGIN`] ahead of the next.
    #[must_use]
    pub fn leader(&self) -> Option<Component> {
        let mut ranked: Vec<(Component, i64)> = self
            .scores
            .iter()
            .map(|(cell, score)| (*cell, *score))
            .collect();
        ranked.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        let (cell, best) = *ranked.first()?;
        let next = ranked.get(1).map_or(0, |(_, score)| *score);
        (best >= MIN_SCORE && best - next >= MIN_MARGIN).then_some(cell)
    }
}

const fn is_memory(cell: Component) -> bool {
    matches!(
        cell,
        Component::Ram | Component::Vram(_) | Component::Ssd(_)
    )
}

const fn is_path(cell: Component) -> bool {
    matches!(
        cell,
        Component::Cpu | Component::PcieGpu(_) | Component::Gpu(_) | Component::SsdPath(_)
    )
}

/// Number following `marker` in `text`: `0x…` hex or decimal.
fn number_after(text: &str, marker: &str) -> Option<u64> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let rest = rest.trim_start_matches(['=', ' ', ':']);
    if let Some(hex) = rest.strip_prefix("0x") {
        let digits: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
        u64::from_str_radix(&digits, 16).ok()
    } else {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }
}

/// Byte offset and XOR syndrome of a failure detail, as the engines write them:
/// `transfer mismatch at byte 5701632: expected=0xc9, actual=0x31`,
/// `first at global offset 4096 (…), expected 0xaa actual 0xab, syndrome 0x01`,
/// `offset 1048576: deep block SHA-256 mismatch`. A matrix index (`matrix mismatch at 2`) is
/// not an address and is left out.
#[must_use]
pub fn locate(detail: &str) -> (Option<u64>, Option<u64>) {
    let address = number_after(detail, "at byte ")
        .or_else(|| number_after(detail, "global offset "))
        .or_else(|| {
            detail
                .strip_prefix("offset ")
                .and_then(|_| number_after(detail, "offset "))
        });
    let syndrome = number_after(detail, "syndrome ").or_else(|| {
        let expected = number_after(detail, "expected")?;
        let actual = number_after(detail, "actual")?;
        Some(expected ^ actual)
    });
    (address, syndrome.filter(|s| *s != 0))
}

/// `P(X ≥ k)` for `X ~ Binomial(n, p)`.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn binomial_tail(n: u64, k: u64, p: f64) -> f64 {
    if k == 0 || p >= 1.0 {
        return 1.0;
    }
    if k > n || p <= 0.0 {
        return 0.0;
    }
    let q = 1.0 - p;
    let ln_choose: f64 = (1..=k).map(|i| ((n - k + i) as f64 / i as f64).ln()).sum();
    let mut term = (ln_choose + k as f64 * p.ln() + (n - k) as f64 * q.ln()).exp();
    let mut sum = 0.0;
    for i in k..=n {
        sum += term;
        term *= (n - i) as f64 / (i + 1) as f64 * p / q;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum.min(1.0)
}

/// Share of `[0, elapsed]` covered by the windows of events at `times`.
fn covered_share(times: &[f64], elapsed: f64) -> f64 {
    if elapsed <= 0.0 {
        return 1.0;
    }
    let mut windows: Vec<(f64, f64)> = times
        .iter()
        .map(|t| {
            (
                (t - WINDOW_BEFORE).max(0.0),
                (t + WINDOW_AFTER).min(elapsed),
            )
        })
        .collect();
    windows.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut covered = 0.0;
    let mut reach = 0.0_f64;
    for (start, end) in windows {
        let start = start.max(reach);
        if end > start {
            covered += end - start;
            reach = end;
        }
    }
    (covered / elapsed).min(1.0)
}

/// Section 2.4: do the failures of the group fall next to the hardware events of `cell`? Storms
/// count once: failures are counted per distinct second.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn time_signature(failures: &[&Occurrence], times: &[f64], elapsed: f64) -> Option<String> {
    if times.is_empty() {
        return None;
    }
    let moments: BTreeSet<u64> = failures
        .iter()
        .filter_map(|o| o.at_seconds)
        .map(|t| t.max(0.0).floor() as u64)
        .collect();
    if moments.len() < MIN_FAILURES {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let near = moments
        .iter()
        .filter(|&&m| {
            let t = m as f64 + 0.5;
            times
                .iter()
                .any(|&e| t >= e - WINDOW_BEFORE - 0.5 && t <= e + WINDOW_AFTER + 0.5)
        })
        .count();
    let share = covered_share(times, elapsed);
    let n = moments.len() as u64;
    let k = near as u64;
    let tail = binomial_tail(n, k, share);
    (tail < TIME_ALPHA).then(|| {
        format!(
            "{k}/{n} failure moments next to its hardware events (chance {:.0}%, p={tail:.1e})",
            share * 100.0
        )
    })
}

/// Section 2.5, address: repeated addresses point at a memory cell, all distinct at a path.
fn address_signature(failures: &[&Occurrence]) -> Option<(bool, String)> {
    let addresses: Vec<u64> = failures.iter().filter_map(|o| o.address).collect();
    if addresses.len() < MIN_FAILURES {
        return None;
    }
    let mut seen: BTreeMap<u64, usize> = BTreeMap::new();
    for address in &addresses {
        *seen.entry(*address).or_default() += 1;
    }
    let repeated: usize = seen.values().filter(|&&n| n > 1).sum();
    let n = addresses.len();
    if repeated >= 2 && repeated * 2 >= n {
        Some((
            true,
            format!("{repeated}/{n} failures at a repeated address"),
        ))
    } else if repeated == 0 && n > MIN_FAILURES {
        Some((false, format!("{n} failures at {n} different addresses")))
    } else {
        None
    }
}

/// Section 2.5, bit position: one bit in at least 80% of the syndromes, far above chance. Only
/// failures at an address count: a fixed bit across memory locations is a data line, a fixed bit
/// in computed results is not.
#[allow(clippy::cast_precision_loss)]
fn bit_signature(failures: &[&Occurrence]) -> Option<String> {
    let syndromes: Vec<u64> = failures
        .iter()
        .filter(|o| o.address.is_some())
        .filter_map(|o| o.syndrome)
        .collect();
    let n = syndromes.len();
    if n < MIN_FAILURES {
        return None;
    }
    let widest = syndromes.iter().copied().max().unwrap_or(0);
    let width: u32 = if widest <= 0xff {
        8
    } else if u32::try_from(widest).is_ok() {
        32
    } else {
        64
    };
    let (bit, hits) = (0..width)
        .map(|bit| {
            (
                bit,
                syndromes.iter().filter(|s| (*s >> bit) & 1 == 1).count(),
            )
        })
        .max_by_key(|(_, hits)| *hits)?;
    let flipped: u32 = syndromes.iter().map(|s| s.count_ones()).sum();
    let chance = (f64::from(flipped) / n as f64 / f64::from(width)).min(1.0);
    let tail = binomial_tail(n as u64, hits as u64, chance);
    (hits * 5 >= n * 4 && tail < ALPHA)
        .then(|| format!("bit {bit} in {hits}/{n} syndromes (p={tail:.1e})"))
}

/// Section 2.5, phase: failures only in the all-layers load while the same layers ran alone long
/// enough that, at the load's rate, some should have failed there too.
#[allow(clippy::cast_precision_loss)]
fn load_signature(failures: &[&Occurrence], phases: &[PhaseExposure]) -> Option<String> {
    let layers: BTreeSet<&str> = failures.iter().map(|o| o.layer).collect();
    let runs_layer = |phase: &PhaseExposure| phase.layers.iter().any(|l| layers.contains(l));
    let alone: Vec<&PhaseExposure> = phases.iter().filter(|p| !p.load && runs_layer(p)).collect();
    let load_seconds: f64 = phases.iter().filter(|p| p.load).map(|p| p.seconds).sum();
    let alone_seconds: f64 = alone.iter().map(|p| p.seconds).sum();
    let in_load = failures
        .iter()
        .filter(|o| phases.iter().any(|p| p.load && p.label == o.phase))
        .count();
    let in_alone = failures
        .iter()
        .filter(|o| alone.iter().any(|p| p.label == o.phase))
        .count();
    if in_alone > 0 || in_load < MIN_FAILURES || load_seconds <= 0.0 || alone_seconds <= 0.0 {
        return None;
    }
    let expected = in_load as f64 / load_seconds * alone_seconds;
    let p = (-expected).exp();
    (p < ALPHA).then(|| {
        format!(
            "{in_load} failures under full load, none in {alone_seconds:.0} s alone (expected {expected:.1}, p={p:.1e})"
        )
    })
}

/// Scores the cells of one unresolved `group` from its failures. `hardware(cell)` is what the
/// run counted on the cell (kind → count).
#[must_use]
pub fn score_group(
    group: &[Component],
    failures: &[&Occurrence],
    hardware: &dyn Fn(Component) -> BTreeMap<String, u64>,
    context: &Context,
) -> Scored {
    let mut scored = Scored {
        scores: group.iter().map(|cell| (*cell, 0)).collect(),
        reasons: Vec::new(),
    };
    let mut credit = |cells: &mut dyn Iterator<Item = Component>, weight: i64, why: &str| {
        for cell in cells {
            *scored.scores.entry(cell).or_default() += weight;
            scored
                .reasons
                .push(format!("{} +{weight}: {why}", cell.label()));
        }
    };
    for &cell in group {
        if let Some(why) = context
            .hardware_times
            .get(&cell)
            .and_then(|times| time_signature(failures, times, context.elapsed_seconds))
        {
            credit(&mut std::iter::once(cell), WEIGHTS.time, &why);
        }
        let counted = hardware(cell);
        let total: u64 = counted.values().sum();
        if total > 0 {
            let kinds: Vec<&str> = counted.keys().map(String::as_str).take(4).collect();
            let why = format!("{total} hardware events ({})", kinds.join(", "));
            credit(&mut std::iter::once(cell), WEIGHTS.hardware, &why);
        }
    }
    if let Some((repeat, why)) = address_signature(failures) {
        let (test, weight): (fn(Component) -> bool, i64) = if repeat {
            (is_memory, WEIGHTS.address_repeat)
        } else {
            (is_path, WEIGHTS.address_spread)
        };
        credit(
            &mut group.iter().copied().filter(|c| test(*c)),
            weight,
            &why,
        );
    }
    if let Some(why) = bit_signature(failures) {
        credit(
            &mut group.iter().copied().filter(|c| is_memory(*c)),
            WEIGHTS.fixed_bit,
            &why,
        );
    }
    if let Some(why) = load_signature(failures, &context.phases) {
        if group.contains(&Component::Platform) {
            credit(
                &mut std::iter::once(Component::Platform),
                WEIGHTS.load_only,
                &why,
            );
        } else {
            scored.reasons.push(format!("note: {why}"));
        }
    }
    scored
}

fn same_group(occurrence: &Occurrence, group: &[Component]) -> bool {
    occurrence.confidence == Confidence::Unresolved && occurrence.candidates == group
}

/// Ranks every unresolved group of `ledger` and moves each group with a leading cell there as
/// SUSPECTED. Returns the groups moved.
pub fn rank(ledger: &mut Ledger, context: &Context) -> usize {
    let mut placed = 0;
    for (group, _) in ledger.unresolved_groups() {
        let mut sorted = group.clone();
        sorted.sort();
        sorted.dedup();
        let failures: Vec<&Occurrence> = ledger
            .occurrences()
            .iter()
            .filter(|o| same_group(o, &sorted))
            .collect();
        let scored = score_group(
            &sorted,
            &failures,
            &|cell| ledger.hardware_of(cell),
            context,
        );
        let Some(leader) = scored.leader() else {
            continue;
        };
        let ranking = Ranking {
            weights_version: WEIGHTS_VERSION,
            group: sorted
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>()
                .join("|"),
            suspected: leader.label(),
            count: 0,
            scores: scored
                .scores
                .iter()
                .map(|(cell, score)| (cell.label(), *score))
                .collect(),
            reasons: scored.reasons,
        };
        ledger.rank_unresolved(&group, leader, ranking);
        placed += 1;
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fault_ledger::{Attribution, FaultEvent, HardwareEvent};

    #[test]
    fn details_give_addresses_and_syndromes() {
        assert_eq!(
            locate("transfer mismatch at byte 5701632: expected=0xc9, actual=0x31"),
            (Some(5_701_632), Some(0xc9 ^ 0x31))
        );
        assert_eq!(
            locate(
                "first at global offset 4096 (arena 1 offset 0), expected 0xaa actual 0xab, syndrome 0x01, writer CPU 2"
            ),
            (Some(4096), Some(1))
        );
        assert_eq!(
            locate("offset 1048576: deep block SHA-256 mismatch"),
            (Some(1_048_576), None)
        );
        assert_eq!(
            locate(
                "matrix mismatch at 2: expected 0xaf0e0b05, actual 0xaf0e0b85 (burst: 1 failed batches)"
            ),
            (None, Some(0x80))
        );
    }

    #[test]
    fn binomial_tail_matches_hand_values() {
        assert!((binomial_tail(3, 3, 0.125) - 0.125_f64.powi(3)).abs() < 1e-12);
        assert!((binomial_tail(10, 0, 0.3) - 1.0).abs() < 1e-12);
        let two_of_four =
            6.0 * 0.25 * 0.25 * 0.75 * 0.75 + 4.0 * 0.25_f64.powi(3) * 0.75 + 0.25_f64.powi(4);
        assert!((binomial_tail(4, 2, 0.5 * 0.5) - two_of_four).abs() < 1e-12);
        assert!(binomial_tail(8000, 4000, 0.01) < 1e-100);
    }

    /// Small deterministic generator for the synthetic sessions.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        #[allow(clippy::cast_precision_loss)]
        fn uniform(&mut self, max: f64) -> f64 {
            (self.next() % 1_000_000) as f64 / 1_000_000.0 * max
        }
    }

    const ELAPSED: f64 = 1800.0;

    fn event(group: &[Component], detail: String, at: f64, phase: &str) -> FaultEvent {
        let layer = if group.iter().any(|c| matches!(c, Component::Ssd(_))) {
            "cpu-nvme"
        } else if group.contains(&Component::Ram) {
            "cpu-ram"
        } else {
            "gpu-vram"
        };
        FaultEvent::new(
            layer,
            "synthetic",
            1,
            Attribution::Unresolved(group.to_vec()),
            "test/synthetic",
            detail,
        )
        .in_phase(phase)
        .at(at)
    }

    /// Builds one synthetic session whose failures come from `truth` and returns the cell the
    /// ranking placed the group on (`None` when it left the group unresolved).
    #[allow(clippy::cast_precision_loss)]
    fn synthetic(truth: Component, group: &[Component], rng: &mut Rng) -> Option<Component> {
        let mut ledger = Ledger::default();
        let mut context = Context {
            elapsed_seconds: ELAPSED,
            ..Context::default()
        };
        // Background noise on the links (the only cells the monitor times): events at random times.
        for cell in group
            .iter()
            .filter(|c| matches!(c, Component::PcieGpu(_) | Component::SsdPath(_)))
        {
            let noise: Vec<f64> = (0..rng.next() % 30).map(|_| rng.uniform(ELAPSED)).collect();
            context.hardware_times.insert(*cell, noise);
        }
        // A third of the link faults happen where the counters cannot be read (no AER, no replay).
        let blind = rng.next().is_multiple_of(3);
        let failures = 4 + rng.next() % 12;
        let fixed_bit = rng.next() % 8;
        let weak_addresses: Vec<u64> = (0..2).map(|_| rng.next() % (1 << 30)).collect();
        for _ in 0..failures {
            let at = rng.uniform(ELAPSED);
            let expected = rng.next() & 0xff;
            let (address, syndrome) = if is_memory(truth) {
                // A weak cell: mostly the same few addresses, mostly the same data line.
                let address = if rng.next().is_multiple_of(5) {
                    rng.next() % (1 << 30)
                } else {
                    weak_addresses[usize::try_from(rng.next() % 2).unwrap()]
                };
                let syndrome = if rng.next().is_multiple_of(10) {
                    1 << (rng.next() % 8)
                } else {
                    1 << fixed_bit
                };
                (address, syndrome)
            } else {
                // A path: anywhere, any bits, next to the link's own error events when readable.
                if !blind {
                    context
                        .hardware_times
                        .entry(truth)
                        .or_default()
                        .push(at + rng.uniform(4.0));
                }
                (rng.next() % (1 << 30), (rng.next() & 0xff).max(1))
            };
            let detail = format!(
                "transfer mismatch at byte {address}: expected={expected:#04x}, actual={:#04x}",
                expected ^ syndrome
            );
            ledger.add(event(group, detail, at, "OVERLAP"));
        }
        // Every timed event is also counted on its cell, noise included, as the monitor does.
        for (cell, times) in &context.hardware_times {
            let total = u64::try_from(times.len()).unwrap();
            ledger.add_hardware(&HardwareEvent::new(
                *cell,
                "aer_corrected:RxErr@port",
                total,
            ));
        }
        rank(&mut ledger, &context);
        let ranking = ledger.report().rankings.first().cloned()?;
        if ranking.suspected != truth.label() {
            eprintln!(
                "wrong: {} placed on {} {:?} {:?}",
                truth.label(),
                ranking.suspected,
                ranking.scores,
                ranking.reasons
            );
        }
        group
            .iter()
            .copied()
            .find(|c| c.label() == ranking.suspected)
    }

    /// Roadmap 7.3, F6: on synthetic data the top-ranked cell is the known cell in ≥ 90% of runs.
    #[test]
    fn synthetic_sessions_rank_the_known_cell() {
        let gpu_link = [Component::PcieGpu(1), Component::Vram(1)];
        let compute = [Component::PcieGpu(1), Component::Gpu(1), Component::Vram(1)];
        let ssd = [Component::Ssd(1), Component::SsdPath(1)];
        let cases: [(Component, &[Component]); 5] = [
            (Component::PcieGpu(1), &gpu_link),
            (Component::Vram(1), &gpu_link),
            (Component::Vram(1), &compute),
            (Component::Ssd(1), &ssd),
            (Component::SsdPath(1), &ssd),
        ];
        let (mut right, mut wrong, mut unplaced) = (0_u32, 0_u32, 0_u32);
        for seed in 0..10_u64 {
            let mut rng = Rng(0x5eed_f6f6_1234_abcd ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            for round in 0..200 {
                let (truth, group) = cases[round % cases.len()];
                match synthetic(truth, group, &mut rng) {
                    Some(cell) if cell == truth => right += 1,
                    Some(_) => wrong += 1,
                    None => unplaced += 1,
                }
            }
        }
        eprintln!("synthetic ranking: right {right}, wrong {wrong}, unplaced {unplaced}");
        // Roadmap 7.3 asks ≥ 90% of placed groups on the known cell; a wrong placement is worse
        // than none, so hold it to 99%. Blind link faults may stay unresolved; most must not.
        assert!(
            right * 100 >= (right + wrong) * 99,
            "right {right}, wrong {wrong}"
        );
        assert!(
            right * 4 >= (right + unplaced) * 3,
            "placed too few: unplaced {unplaced}"
        );
    }

    #[test]
    fn nothing_to_go_on_leaves_the_group_unresolved() {
        let group = [Component::PcieGpu(1), Component::Gpu(1), Component::Vram(1)];
        let mut ledger = Ledger::default();
        for i in 0..5 {
            ledger.add(event(
                &group,
                "matrix mismatch at 2: expected 0x1, actual 0x3".into(),
                f64::from(i) * 100.0,
                "OVERLAP",
            ));
        }
        assert_eq!(rank(&mut ledger, &Context::default()), 0);
        assert_eq!(ledger.unresolved_total(), 5);
    }

    #[test]
    fn load_only_failures_point_at_the_platform() {
        let group = [Component::PcieGpu(1), Component::Platform];
        let mut ledger = Ledger::default();
        for i in 0..6 {
            ledger.add(event(
                &group,
                "Xid 79".into(),
                700.0 + f64::from(i),
                "OVERLAP",
            ));
        }
        let context = Context {
            elapsed_seconds: 1500.0,
            phases: vec![
                PhaseExposure {
                    label: "BASELINE GPU-VRAM".into(),
                    seconds: 600.0,
                    load: false,
                    layers: vec!["gpu-vram"],
                },
                PhaseExposure {
                    label: "OVERLAP".into(),
                    seconds: 300.0,
                    load: true,
                    layers: vec!["gpu-vram", "cpu-ram"],
                },
            ],
            ..Context::default()
        };
        assert_eq!(rank(&mut ledger, &context), 1);
        let report = ledger.report();
        assert_eq!(report.rankings[0].suspected, "PLATFORM");
        assert_eq!(report.rankings[0].count, 6);
        assert_eq!(report.unresolved_total, 0);
        assert_eq!(report.confirmed_total, 0);
    }
}

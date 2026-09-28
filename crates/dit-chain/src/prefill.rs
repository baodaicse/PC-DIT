//! Prefill: every record's expected answers are computed twice, independently, on two disjoint
//! groups of CPU cores (threads pinned with `sched_setaffinity`, separate buffers), and a record is
//! written only when both agree word for word. An answer is reused thousands of times, so one
//! CPU/RAM error at build time would otherwise become a wrong answer with a correct hash, and a
//! healthy GPU would be blamed for it.
use gpu_engine::gemm::{Arith, Corpus};
use nix::sched::{CpuSet, sched_getaffinity, sched_setaffinity};
use nix::unistd::Pid;

/// Logical CPUs this process may run on.
#[must_use]
pub fn allowed_cpus() -> Vec<usize> {
    sched_getaffinity(Pid::from_raw(0)).map_or_else(
        |_| (0..std::thread::available_parallelism().map_or(1, usize::from)).collect(),
        |set| {
            (0..CpuSet::count())
                .filter(|cpu| set.is_set(*cpu).unwrap_or(false))
                .collect()
        },
    )
}

/// Two disjoint halves of `cpus`; with one CPU both "groups" are that CPU (the two builds then
/// share a core, which the report states).
#[must_use]
pub fn split_groups(cpus: &[usize], per_group: usize) -> (Vec<usize>, Vec<usize>) {
    if cpus.len() < 2 {
        return (cpus.to_vec(), cpus.to_vec());
    }
    let half = (cpus.len() / 2).min(per_group.max(1));
    (cpus[..half].to_vec(), cpus[cpus.len() - half..].to_vec())
}

fn pin(cpus: &[usize]) {
    let mut set = CpuSet::new();
    for cpu in cpus {
        let _ = set.set(*cpu);
    }
    // Best effort: an unpinnable thread still computes correctly; the report lists the groups.
    let _ = sched_setaffinity(Pid::from_raw(0), &set);
}

/// Where two builds of the same corpus disagreed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleDisagreement {
    pub seed: u64,
    pub arith: Arith,
    /// `input` or `answers`.
    pub part: &'static str,
    pub word: usize,
    pub first: u32,
    pub second: u32,
}

/// Builds the corpus of `seed` twice at once, each build pinned to its own CPU group, and returns
/// it when both builds agree. `corrupt_second` flips one answer bit of the second build
/// (negative gate only).
/// # Errors
/// The first disagreement, or a build error.
pub fn build_twice(
    arith: Arith,
    seed: u64,
    jobs: usize,
    k: u32,
    groups: &(Vec<usize>, Vec<usize>),
    corrupt_second: bool,
) -> Result<Corpus, Result<OracleDisagreement, &'static str>> {
    let build = |cpus: &[usize]| {
        pin(cpus);
        Corpus::new_arith(arith, seed, jobs, k, cpus.len().max(1))
    };
    let (first, second) = std::thread::scope(|scope| {
        let a = scope.spawn(|| build(&groups.0));
        let b = scope.spawn(|| build(&groups.1));
        (a.join(), b.join())
    });
    let first = first
        .map_err(|_| Err("prefill build panicked"))?
        .map_err(Err)?;
    let mut second = second
        .map_err(|_| Err("prefill build panicked"))?
        .map_err(Err)?;
    if corrupt_second && let Some(word) = second.dot.first_mut() {
        *word ^= 1;
    }
    let differ = |part, a: &[u32], b: &[u32]| {
        a.iter()
            .zip(b)
            .position(|(x, y)| x != y)
            .map(|word| OracleDisagreement {
                seed,
                arith,
                part,
                word,
                first: a[word],
                second: b[word],
            })
    };
    if let Some(found) = differ("input", &first.input, &second.input)
        .or_else(|| differ("answers", &first.dot, &second.dot))
    {
        return Err(Ok(found));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_agreeing_builds_pass_and_a_flipped_answer_is_caught() {
        let groups = split_groups(&allowed_cpus(), 2);
        let corpus = build_twice(Arith::F32, 9, 2, 64, &groups, false).unwrap();
        assert_eq!(corpus.seed, 9);
        let Err(Ok(found)) = build_twice(Arith::U32, 9, 2, 64, &groups, true) else {
            panic!("a flipped answer must be caught");
        };
        assert_eq!((found.part, found.word), ("answers", 0));
        assert_eq!(found.first ^ found.second, 1);
    }

    #[test]
    fn groups_are_disjoint_halves() {
        let (a, b) = split_groups(&[0, 1, 2, 3, 4, 5], 8);
        assert_eq!((a, b), (vec![0, 1, 2], vec![3, 4, 5]));
        let (a, b) = split_groups(&[0, 1, 2, 3, 4, 5], 2);
        assert_eq!((a, b), (vec![0, 1], vec![4, 5]));
        assert_eq!(split_groups(&[7], 4), (vec![7], vec![7]));
    }
}

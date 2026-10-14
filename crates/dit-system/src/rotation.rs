//! Core rotation (v1.2, C6): one core at a time, the rest of the CPU idle, so that core runs at
//! the highest boost its firmware gives a lightly loaded CPU.
//!
//! An all-core test keeps every core around 5.0-5.5 GHz; the highest clocks (6.0 GHz on an
//! i9-14900K, 5.7 GHz on a Ryzen 9 9950X) only come with one or two busy cores. A degraded
//! Raptor Lake core or an over-tuned Curve Optimizer offset passes the all-core test and fails a
//! game or a single-threaded application. The rotation runs checked kernels on one target at a
//! time, its SMT sibling and the other cores idle:
//!
//! - a performance core (or any core of a CPU whose cores are alike) on its first thread;
//! - a cluster of efficient cores (they share an L2 and a clock) on all its cores at once.
//!
//! Targets go in the order the firmware ranks them (`acpi_cppc/highest_perf`, its preferred
//! cores first). Kind A (`a`) runs the light integer kernel, which reaches the highest boost;
//! kind B (`b`) runs it, then the C3 FMA kernel (the largest current), and is meant to run while
//! the GPU test loads the machine (`keep_off_pid`: that process tree is kept off the core under
//! test). Every result is checked; a wrong one is a CPU failure. The clock each step reached is
//! read from cpufreq (MSRs are locked under Secure Boot): a step under 95% of the core's maximum
//! is a note, never a failure.

use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use nix::{
    sched::{CpuSet, sched_setaffinity},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};

use crate::{
    compute_oracle::{self, BRANCH_PREDICTOR_LENGTH},
    cpu::pin_current_thread,
    inventory::{CoreKind, CpuInventory, parse_cpu_list},
    safety::RunControl,
};

/// Name of the rotation recipe, recorded in reports.
pub const ROTATION_ID: &str = "CPU-DIT-CORE-ROTATION-v1";
/// A step under this fraction of the core's maximum clock did not reach boost (a note).
const BOOST_FRACTION: f64 = 0.95;
/// Clock samples start this long into a step (the core ramps up first).
const SETTLE: Duration = Duration::from_secs(1);
const SAMPLE_EVERY: Duration = Duration::from_millis(200);
/// How often the caller hears of the step (progress line, watchdog heartbeat).
const PROGRESS_EVERY: Duration = Duration::from_secs(2);
/// The light kernel's work per check: the branch kernel at a shorter length than the compute
/// oracle's, so the check comes often and the core never waits on anything.
const LIGHT_LENGTH: u32 = BRANCH_PREDICTOR_LENGTH;
/// The FMA kernel's iterations per check in kind B (about 30 ms on a desktop core).
const AVX_ITERATIONS: u32 = 16_000_000;

/// Which rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RotationKind {
    /// The machine otherwise idle: the light kernel at peak boost.
    A,
    /// While the GPU test runs: the light kernel, then the FMA kernel.
    B,
}

impl RotationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }

    /// The shortest step: long enough for the clock and the temperature to settle.
    const fn minimum_step_seconds(self) -> f64 {
        match self {
            Self::A => 10.0,
            Self::B => 20.0,
        }
    }
}

/// One rotation target: the threads that run the kernel, those that must stay idle with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) cpus: Vec<usize>,
    /// The SMT siblings of `cpus`, idle during the step.
    pub(crate) quiet: Vec<usize>,
    pub(crate) core_kind: CoreKind,
    /// The firmware's ranking (`acpi_cppc/highest_perf`, 0 when not reported).
    pub(crate) rank: u64,
    /// `cpuinfo_max_freq`, in kHz.
    pub(crate) max_khz: Option<u64>,
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The rotation targets of the allowed threads, in the order they are tested: firmware ranking
/// first (highest first), performance and uniform cores before efficient clusters, then CPU
/// number. `sys_cpu` is `/sys/devices/system/cpu`.
pub(crate) fn targets(inventory: &CpuInventory, sys_cpu: &Path) -> Vec<Target> {
    let allowed = |cpu: usize| inventory.allowed_cpu_ids.contains(&cpu);
    let mut targets: Vec<Target> = Vec::new();
    // Performance and uniform cores: one target per physical core.
    let mut cores: std::collections::BTreeMap<(i32, i32), Vec<&crate::CpuThread>> =
        std::collections::BTreeMap::new();
    for thread in &inventory.threads {
        if thread.core_kind != CoreKind::Efficient && allowed(thread.cpu_id) {
            cores
                .entry((thread.package_id, thread.core_id))
                .or_default()
                .push(thread);
        }
    }
    for threads in cores.values() {
        let first = threads
            .iter()
            .min_by_key(|thread| (thread.smt_rank, thread.cpu_id))
            .map_or(0, |thread| thread.cpu_id);
        let kind = threads
            .first()
            .map_or(CoreKind::Uniform, |thread| thread.core_kind);
        targets.push(Target {
            cpus: vec![first],
            quiet: threads
                .iter()
                .map(|thread| thread.cpu_id)
                .filter(|&cpu| cpu != first)
                .collect(),
            core_kind: kind,
            rank: 0,
            max_khz: None,
        });
    }
    // Efficient cores: one target per cluster sharing an L2.
    let mut clusters: std::collections::BTreeMap<Vec<usize>, Vec<usize>> =
        std::collections::BTreeMap::new();
    for thread in &inventory.threads {
        if thread.core_kind == CoreKind::Efficient && allowed(thread.cpu_id) {
            let l2 = std::fs::read_to_string(
                sys_cpu.join(format!("cpu{}/cache/index2/shared_cpu_list", thread.cpu_id)),
            )
            .map_or_else(|_| vec![thread.cpu_id], |text| parse_cpu_list(&text));
            clusters.entry(l2).or_default().push(thread.cpu_id);
        }
    }
    for cpus in clusters.into_values() {
        targets.push(Target {
            cpus,
            quiet: Vec::new(),
            core_kind: CoreKind::Efficient,
            rank: 0,
            max_khz: None,
        });
    }
    for target in &mut targets {
        target.rank = target
            .cpus
            .iter()
            .filter_map(|cpu| read_u64(&sys_cpu.join(format!("cpu{cpu}/acpi_cppc/highest_perf"))))
            .max()
            .unwrap_or(0);
        target.max_khz = target
            .cpus
            .first()
            .and_then(|cpu| read_u64(&sys_cpu.join(format!("cpu{cpu}/cpufreq/cpuinfo_max_freq"))));
    }
    targets.sort_by_key(|target| {
        (
            std::cmp::Reverse(target.rank),
            target.core_kind == CoreKind::Efficient,
            target.cpus.first().copied().unwrap_or(usize::MAX),
        )
    });
    targets
}

/// The steps a budget allows: every target when each gets at least the kind's shortest step,
/// otherwise the best-ranked ones; the budget is shared out evenly.
pub(crate) fn plan(targets: &[Target], kind: RotationKind, budget_seconds: u64) -> (usize, f64) {
    if targets.is_empty() || budget_seconds == 0 {
        return (0, 0.0);
    }
    #[allow(clippy::cast_precision_loss)] // Seconds and core counts.
    let budget = budget_seconds as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // At least 1, small.
    let fit = ((budget / kind.minimum_step_seconds()).floor() as usize).max(1);
    let steps = targets.len().min(fit);
    #[allow(clippy::cast_precision_loss)]
    let step_seconds = budget / steps as f64;
    (steps, step_seconds)
}

/// The clock a step reached, from its samples: average and peak in kHz, and whether the average
/// is within [`BOOST_FRACTION`] of the core's maximum (unknown without cpufreq).
pub(crate) fn boost(
    samples: &[u64],
    max_khz: Option<u64>,
) -> (Option<u64>, Option<u64>, Option<bool>) {
    if samples.is_empty() {
        return (None, None, None);
    }
    let average = samples.iter().sum::<u64>() / samples.len() as u64;
    let peak = samples.iter().copied().max();
    #[allow(clippy::cast_precision_loss)]
    let reached = max_khz
        .filter(|&max| max > 0)
        .map(|max| average as f64 >= max as f64 * BOOST_FRACTION);
    (Some(average), peak, reached)
}

/// One step of the rotation in the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RotationStepEvidence {
    /// The threads that ran the kernels (one, or an efficient cluster).
    pub cpus: Vec<usize>,
    pub core_kind: CoreKind,
    /// The firmware's ranking of the core (`acpi_cppc/highest_perf`; 0 when not reported).
    pub rank: u64,
    pub seconds: f64,
    /// `cpuinfo_max_freq`, in kHz.
    pub max_khz: Option<u64>,
    /// The clock the step held (`scaling_cur_freq`, sampled after the first second), in kHz.
    pub average_khz: Option<u64>,
    pub peak_khz: Option<u64>,
    /// Average within 95% of the maximum; unknown without cpufreq.
    pub boost_reached: Option<bool>,
    pub light_checks: u64,
    pub fma_checks: u64,
    pub mismatches: u64,
}

/// The rotation in the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoreRotationEvidence {
    pub rotation_id: String,
    pub kind: RotationKind,
    pub budget_seconds: u64,
    pub step_seconds: f64,
    pub steps: Vec<RotationStepEvidence>,
    /// Targets the budget left out (lowest ranked), by their threads.
    pub not_rotated: Vec<Vec<usize>>,
    pub mismatches: u64,
    pub passed: bool,
    /// Steps whose clock stayed under 95% of the maximum (a note, not a failure).
    pub steps_below_boost: usize,
    /// Threads of the kept-off process tree (kind B) moved off each step's core.
    pub kept_off_threads: u64,
    /// The first wrong result: step, threads and kernel.
    pub first_mismatch: Option<String>,
}

/// A step under way, for the caller's progress line.
pub(crate) struct StepProgress<'a> {
    pub(crate) step: usize,
    pub(crate) steps: usize,
    pub(crate) cpus: &'a [usize],
    pub(crate) current_khz: Option<u64>,
    pub(crate) max_khz: Option<u64>,
    pub(crate) mismatches: u64,
}

/// The rotation of a session.
pub(crate) struct Rotation<'a> {
    pub(crate) kind: RotationKind,
    pub(crate) budget_seconds: u64,
    pub(crate) seed: u64,
    /// Kind B: the GPU test's process tree, kept off the core under test.
    pub(crate) keep_off_pid: Option<u32>,
    pub(crate) inventory: &'a CpuInventory,
    pub(crate) control: &'a RunControl,
}

const LIGHT: u8 = 0;
const FMA: u8 = 1;

impl Rotation<'_> {
    /// Runs every planned step; `progress` hears of each step every couple of seconds.
    pub(crate) fn run(&self, progress: &mut dyn FnMut(&StepProgress<'_>)) -> CoreRotationEvidence {
        let sys_cpu = Path::new("/sys/devices/system/cpu");
        let all = targets(self.inventory, sys_cpu);
        let (count, step_seconds) = plan(&all, self.kind, self.budget_seconds);
        let light_reference = compute_oracle::branch_predictor_pass(self.seed, LIGHT_LENGTH);
        let fma_reference = (self.kind == RotationKind::B)
            .then(|| compute_oracle::fma_chain_reference(self.seed, AVX_ITERATIONS));
        let mut evidence = CoreRotationEvidence {
            rotation_id: ROTATION_ID.to_owned(),
            kind: self.kind,
            budget_seconds: self.budget_seconds,
            step_seconds,
            steps: Vec::new(),
            not_rotated: all[count..]
                .iter()
                .map(|target| target.cpus.clone())
                .collect(),
            mismatches: 0,
            passed: true,
            steps_below_boost: 0,
            kept_off_threads: 0,
            first_mismatch: None,
        };
        for (index, target) in all[..count].iter().enumerate() {
            if self.control.should_stop() {
                break;
            }
            if let Some(pid) = self.keep_off_pid {
                let avoid: Vec<usize> = target.cpus.iter().chain(&target.quiet).copied().collect();
                evidence.kept_off_threads += keep_off(pid, &avoid, &self.inventory.allowed_cpu_ids);
            }
            let step = self.step(
                target,
                Duration::from_secs_f64(step_seconds),
                light_reference,
                fma_reference.as_ref(),
                &mut |current_khz, mismatches| {
                    progress(&StepProgress {
                        step: index + 1,
                        steps: count,
                        cpus: &target.cpus,
                        current_khz,
                        max_khz: target.max_khz,
                        mismatches,
                    });
                },
            );
            if step.mismatches > 0 && evidence.first_mismatch.is_none() {
                evidence.first_mismatch = Some(format!(
                    "step {} (CPU {:?}, {:?}): {} wrong results",
                    index + 1,
                    target.cpus,
                    target.core_kind,
                    step.mismatches
                ));
            }
            evidence.mismatches += step.mismatches;
            evidence.steps_below_boost += usize::from(step.boost_reached == Some(false));
            evidence.steps.push(step);
        }
        if let Some(pid) = self.keep_off_pid {
            keep_off(pid, &[], &self.inventory.allowed_cpu_ids);
        }
        evidence.passed = evidence.mismatches == 0;
        evidence
    }

    fn step(
        &self,
        target: &Target,
        length: Duration,
        light_reference: u64,
        fma_reference: Option<&[[u64; 4]; compute_oracle::FMA_CHAINS]>,
        progress: &mut dyn FnMut(Option<u64>, u64),
    ) -> RotationStepEvidence {
        let stop = AtomicBool::new(false);
        let kernel = AtomicU8::new(LIGHT);
        let light_checks = AtomicU64::new(0);
        let fma_checks = AtomicU64::new(0);
        let mismatches = AtomicU64::new(0);
        let started = Instant::now();
        let mut samples = Vec::new();
        thread::scope(|scope| {
            for &cpu in &target.cpus {
                let (stop, kernel) = (&stop, &kernel);
                let (light_checks, fma_checks, mismatches) =
                    (&light_checks, &fma_checks, &mismatches);
                let seed = self.seed;
                let control = self.control;
                scope.spawn(move || {
                    let _ = pin_current_thread(cpu);
                    while !stop.load(Ordering::Relaxed) && !control.should_stop() {
                        let wrong = if kernel.load(Ordering::Relaxed) == FMA
                            && let Some(reference) = fma_reference
                        {
                            fma_checks.fetch_add(1, Ordering::Relaxed);
                            compute_oracle::burn_fma(std::hint::black_box(seed), AVX_ITERATIONS)
                                != *reference
                        } else {
                            light_checks.fetch_add(1, Ordering::Relaxed);
                            compute_oracle::branch_predictor_pass(
                                std::hint::black_box(seed),
                                LIGHT_LENGTH,
                            ) != light_reference
                        };
                        mismatches.fetch_add(u64::from(wrong), Ordering::Relaxed);
                    }
                });
            }
            let mut last_progress = Instant::now();
            while started.elapsed() < length && !self.control.should_stop() {
                thread::sleep(SAMPLE_EVERY);
                if fma_reference.is_some() && started.elapsed() >= length / 2 {
                    kernel.store(FMA, Ordering::Relaxed);
                }
                let current = average_khz(&target.cpus);
                if started.elapsed() >= SETTLE
                    && let Some(khz) = current
                {
                    samples.push(khz);
                }
                if last_progress.elapsed() >= PROGRESS_EVERY {
                    last_progress = Instant::now();
                    progress(current, mismatches.load(Ordering::Relaxed));
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
        let (average_khz, peak_khz, boost_reached) = boost(&samples, target.max_khz);
        RotationStepEvidence {
            cpus: target.cpus.clone(),
            core_kind: target.core_kind,
            rank: target.rank,
            seconds: started.elapsed().as_secs_f64(),
            max_khz: target.max_khz,
            average_khz,
            peak_khz,
            boost_reached,
            light_checks: light_checks.load(Ordering::Relaxed),
            fma_checks: fma_checks.load(Ordering::Relaxed),
            mismatches: mismatches.load(Ordering::Relaxed),
        }
    }
}

/// The clock of `cpus` now (`scaling_cur_freq`, averaged over a cluster), in kHz.
fn average_khz(cpus: &[usize]) -> Option<u64> {
    let readings: Vec<u64> = cpus
        .iter()
        .filter_map(|cpu| {
            read_u64(Path::new(&format!(
                "/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq"
            )))
        })
        .collect();
    (!readings.is_empty()).then(|| readings.iter().sum::<u64>() / readings.len() as u64)
}

/// Sets every thread of `pid`'s process tree to the allowed CPUs but `avoid`; returns how many
/// threads it moved. Best effort: a thread that ended or refuses is left as it is.
fn keep_off(pid: u32, avoid: &[usize], allowed: &[usize]) -> u64 {
    let mut set = CpuSet::new();
    let mut any = false;
    for &cpu in allowed.iter().filter(|cpu| !avoid.contains(cpu)) {
        any |= set.set(cpu).is_ok();
    }
    if !any {
        return 0;
    }
    let processes = crate::procs::processes(Path::new("/proc"));
    let mut moved = 0;
    for process in crate::procs::tree(&processes, pid) {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{}/task", process.pid)) else {
            continue;
        };
        for task in tasks.flatten() {
            let Some(tid) = task
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            if sched_setaffinity(Pid::from_raw(tid), &set).is_ok() {
                moved += 1;
            }
        }
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::{RotationKind, Target, boost, plan, targets};
    use crate::{CpuInventory, CpuThread, inventory::CoreKind};

    fn write(root: &std::path::Path, path: &str, value: &str) {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, value).unwrap();
    }

    /// i9-14900K-like: P-cores 0-3 with siblings next to them (CPU 0-7), two E clusters of four
    /// (CPU 8-15).
    fn hybrid(root: &std::path::Path) -> CpuInventory {
        let mut threads = Vec::new();
        for cpu in 0..16_usize {
            let performance = cpu < 8;
            let core_id = if performance { cpu / 2 } else { cpu };
            threads.push(CpuThread {
                cpu_id: cpu,
                package_id: 0,
                core_id: i32::try_from(core_id).unwrap(),
                numa_node: Some(0),
                allowed: true,
                core_kind: if performance {
                    CoreKind::Performance
                } else {
                    CoreKind::Efficient
                },
                smt_rank: if performance { cpu % 2 } else { 0 },
                l3_id: Some(0),
            });
            let (rank, max, l2) = if performance {
                // P-core 2 (CPU 4) is the firmware's favourite.
                (
                    if cpu / 2 == 2 { 77 } else { 70 },
                    6_000_000,
                    format!("{},{}", cpu & !1, cpu | 1),
                )
            } else if cpu < 12 {
                (40, 4_400_000, "8-11".to_owned())
            } else {
                (40, 4_400_000, "12-15".to_owned())
            };
            write(
                root,
                &format!("cpu{cpu}/acpi_cppc/highest_perf"),
                &rank.to_string(),
            );
            write(
                root,
                &format!("cpu{cpu}/cpufreq/cpuinfo_max_freq"),
                &max.to_string(),
            );
            write(root, &format!("cpu{cpu}/cache/index2/shared_cpu_list"), &l2);
        }
        CpuInventory {
            architecture: "x86_64".into(),
            vendor_id: String::new(),
            model_name: String::new(),
            logical_cpu_count: 16,
            allowed_cpu_ids: (0..16).collect(),
            physical_core_count: 12,
            avx2: true,
            aes: true,
            fma: true,
            sse4_2: true,
            threads,
        }
    }

    #[test]
    fn performance_cores_one_by_one_and_efficient_cores_by_cluster() {
        let root = std::env::temp_dir().join(format!("dit-rotation-{}", std::process::id()));
        let inventory = hybrid(&root);
        let found = targets(&inventory, &root);
        let cpus: Vec<Vec<usize>> = found.iter().map(|target| target.cpus.clone()).collect();
        // The favourite P-core first, then the other P-cores, then the E clusters.
        assert_eq!(
            cpus,
            [
                vec![4],
                vec![0],
                vec![2],
                vec![6],
                vec![8, 9, 10, 11],
                vec![12, 13, 14, 15]
            ]
        );
        assert_eq!(found[0].quiet, [5]);
        assert_eq!(found[0].max_khz, Some(6_000_000));
        assert_eq!(found[4].core_kind, CoreKind::Efficient);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_budget_is_shared_out_and_cut_to_the_best_ranked() {
        let target = Target {
            cpus: vec![0],
            quiet: Vec::new(),
            core_kind: CoreKind::Uniform,
            rank: 0,
            max_khz: None,
        };
        let twelve = vec![target; 12];
        // Menu [1] Full on a 14900K: 360 s over 12 steps.
        assert_eq!(plan(&twelve, RotationKind::A, 360), (12, 30.0));
        // Combined Deep: B gets 480 s, 40 s a step.
        assert_eq!(plan(&twelve, RotationKind::B, 480), (12, 40.0));
        // Too short for every core: the best-ranked six, 20 s each (B's shortest).
        assert_eq!(plan(&twelve, RotationKind::B, 120), (6, 20.0));
        assert_eq!(plan(&twelve, RotationKind::A, 5), (1, 5.0));
        assert_eq!(plan(&[], RotationKind::A, 100), (0, 0.0));
    }

    #[test]
    fn a_step_under_95_percent_of_the_maximum_did_not_reach_boost() {
        assert_eq!(boost(&[], Some(6_000_000)), (None, None, None));
        assert_eq!(
            boost(&[5_800_000, 6_000_000], Some(6_000_000)),
            (Some(5_900_000), Some(6_000_000), Some(true))
        );
        assert_eq!(
            boost(&[5_500_000, 5_600_000], Some(6_000_000)).2,
            Some(false)
        );
        assert_eq!(boost(&[4_000_000], None).2, None);
    }
}

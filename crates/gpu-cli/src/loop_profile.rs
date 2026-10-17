//! Where the generation loop's time goes (v1.2.023).
//!
//! On one field machine (TUF Z790, i9-14900K, RTX 5080, v1.2.022) the loop spent half of the
//! overlap and of rotation B outside the compute pipeline, 100–130 ms per burst against 2 ms in
//! the GPU baseline and 12 ms in v1.1 on the same machine, and the GPU idled meanwhile. Nothing
//! said in what. Every step of a generation is timed here:
//!
//! - the report keeps the totals (`loop_profile`);
//! - the bottleneck log, each second's share (`loop_window`);
//! - the progress file, the running totals, which the black box copies into its journal.
//!
//! Inside the pipeline call the async report splits the time further (`pipeline` here, from
//! `async_compute.timing`): preparing the corpus, the burst loop, what follows it.
use gpu_engine::accounting::Timing;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// A timed step of one generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The compute burst (or transfer slice): the pipeline call itself.
    Pipeline,
    /// Transfer and retention pairs between bursts (no lane).
    Transfers,
    /// The lane's failures and counters after the burst.
    LaneCollect,
    /// `PCIe` link speed, width and AER counters (sysfs).
    LinkSample,
    /// Hardware events in the kernel log (`journalctl -k`, every 10 s).
    KernelLog,
    /// GPU telemetry and throttle tracking (every second).
    Telemetry,
    /// The bottleneck sample and its log line (every second).
    BottleneckSample,
    /// Host CPU and memory usage for the screen and progress.
    HostSample,
    /// Building the screen state (sweep and arithmetic notes).
    UiState,
    /// Writing the progress file (the watchdog's heartbeat).
    ProgressWrite,
    /// Drawing the screen.
    Render,
    /// Appending failure records to the fault events.
    FaultEvents,
}

impl Step {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pipeline => "pipeline",
            Self::Transfers => "transfers",
            Self::LaneCollect => "lane_collect",
            Self::LinkSample => "link_sample",
            Self::KernelLog => "kernel_log",
            Self::Telemetry => "telemetry",
            Self::BottleneckSample => "bottleneck_sample",
            Self::HostSample => "host_sample",
            Self::UiState => "ui_state",
            Self::ProgressWrite => "progress_write",
            Self::Render => "render",
            Self::FaultEvents => "fault_events",
        }
    }
}

/// Time one step took, over the run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StepTime {
    pub seconds: f64,
    pub calls: u64,
    /// The longest single call.
    pub max_seconds: f64,
}

/// The pipeline call split up, from the async report's cumulative timing (CUDA only).
#[allow(clippy::struct_field_names)] // The names of the async timing fields they copy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineParts {
    /// From the call to the burst loop; the next four are parts of it.
    pub prepare_seconds: f64,
    /// Waiting for a prefetched corpus not built yet.
    pub corpus_wait_seconds: f64,
    pub corpus_install_seconds: f64,
    pub origin_sync_seconds: f64,
    pub setup_seconds: f64,
    pub loop_seconds: f64,
    /// Sweep drain, last hash, boundary and corpus integrity checks.
    pub after_loop_seconds: f64,
    /// Part of `after_loop_seconds`.
    pub integrity_check_seconds: f64,
}

impl PipelineParts {
    #[must_use]
    pub fn from_timing(timing: &Timing) -> Self {
        Self {
            prepare_seconds: timing.prepare_seconds,
            corpus_wait_seconds: timing.corpus_wait_seconds,
            corpus_install_seconds: timing.corpus_install_seconds,
            origin_sync_seconds: timing.origin_sync_seconds,
            setup_seconds: timing.setup_seconds,
            loop_seconds: timing.loop_seconds,
            after_loop_seconds: timing.after_loop_seconds,
            integrity_check_seconds: timing.integrity_check_seconds,
        }
    }
}

/// Running totals of the generation loop.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopProfile {
    pub generations: u64,
    /// Wall time of the finished generations.
    pub wall_seconds: f64,
    /// Every timed step, by [`Step::name`].
    pub steps: BTreeMap<String, StepTime>,
    /// Wall time no timed step covers (finished generations).
    pub other_seconds: f64,
    /// The pipeline call split up, as of the last sample (absent without the async pipeline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<PipelineParts>,
    /// Steps timed in the generation not finished yet (they count towards `other_seconds` of
    /// nothing until it ends).
    #[serde(skip)]
    open_seconds: f64,
}

impl LoopProfile {
    pub fn record(&mut self, step: Step, took: Duration) {
        let seconds = took.as_secs_f64();
        let entry = self.steps.entry(step.name().to_owned()).or_default();
        entry.seconds += seconds;
        entry.calls += 1;
        entry.max_seconds = entry.max_seconds.max(seconds);
        self.open_seconds += seconds;
    }

    /// Closes a generation that took `wall` from its start.
    pub fn end_generation(&mut self, wall: Duration) {
        let wall = wall.as_secs_f64();
        self.generations += 1;
        self.wall_seconds += wall;
        self.other_seconds += (wall - self.open_seconds).max(0.0);
        self.open_seconds = 0.0;
    }

    #[must_use]
    pub fn seconds(&self, step: Step) -> f64 {
        self.steps.get(step.name()).map_or(0.0, |s| s.seconds)
    }

    /// What changed since `earlier` (the previous sample of this run).
    #[must_use]
    pub fn window_since(&self, earlier: &Self) -> LoopWindow {
        let steps = self
            .steps
            .iter()
            .filter(|(name, _)| name.as_str() != Step::Pipeline.name())
            .filter_map(|(name, now)| {
                let before = earlier.steps.get(name).map_or(0.0, |s| s.seconds);
                let delta = now.seconds - before;
                // Under a millisecond is noise in a once-a-second line.
                (delta >= 0.001).then(|| (name.clone(), round_ms(delta)))
            })
            .collect();
        LoopWindow {
            generations: self.generations - earlier.generations,
            seconds: round_ms(self.wall_seconds - earlier.wall_seconds),
            pipeline_seconds: round_ms(
                self.seconds(Step::Pipeline) - earlier.seconds(Step::Pipeline),
            ),
            other_seconds: round_ms(self.other_seconds - earlier.other_seconds),
            steps,
        }
    }
}

/// The loop between two bottleneck samples: how much of it was the pipeline call, and where the
/// rest went.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LoopWindow {
    pub generations: u64,
    pub seconds: f64,
    pub pipeline_seconds: f64,
    /// Steps other than the pipeline call, from a millisecond up.
    pub steps: BTreeMap<String, f64>,
    pub other_seconds: f64,
}

fn round_ms(seconds: f64) -> f64 {
    (seconds * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_generation_splits_into_its_steps_and_the_rest() {
        let mut profile = LoopProfile::default();
        profile.record(Step::Pipeline, ms(200));
        profile.record(Step::LinkSample, ms(5));
        profile.record(Step::ProgressWrite, ms(20));
        profile.end_generation(ms(250));
        profile.record(Step::Pipeline, ms(100));
        profile.record(Step::LinkSample, ms(15));
        profile.end_generation(ms(120));
        assert_eq!(profile.generations, 2);
        assert!((profile.wall_seconds - 0.370).abs() < 1e-9);
        assert!((profile.seconds(Step::Pipeline) - 0.300).abs() < 1e-9);
        let link = profile.steps["link_sample"];
        assert_eq!(link.calls, 2);
        assert!((link.seconds - 0.020).abs() < 1e-9);
        assert!((link.max_seconds - 0.015).abs() < 1e-9);
        // 25 ms of the first generation and 5 ms of the second no step covers.
        assert!((profile.other_seconds - 0.030).abs() < 1e-9);
    }

    #[test]
    fn a_step_timed_longer_than_its_generation_counts_no_negative_rest() {
        let mut profile = LoopProfile::default();
        profile.record(Step::Pipeline, ms(300));
        profile.end_generation(ms(299));
        assert!(profile.other_seconds.abs() < 1e-12);
    }

    #[test]
    fn the_window_holds_what_changed_since_the_last_sample() {
        let mut profile = LoopProfile::default();
        profile.record(Step::Pipeline, ms(400));
        profile.record(Step::KernelLog, ms(300));
        profile.end_generation(ms(710));
        let earlier = profile.clone();
        profile.record(Step::Pipeline, ms(500));
        profile.record(Step::HostSample, ms(120));
        // Under a millisecond: left out of the line.
        profile.record(Step::LinkSample, Duration::from_micros(300));
        profile.end_generation(ms(650));
        let window = profile.window_since(&earlier);
        assert_eq!(window.generations, 1);
        assert!((window.seconds - 0.650).abs() < 1e-9);
        assert!((window.pipeline_seconds - 0.500).abs() < 1e-9);
        assert_eq!(window.steps.len(), 1, "{:?}", window.steps);
        assert!((window.steps["host_sample"] - 0.120).abs() < 1e-9);
        assert!((window.other_seconds - 0.030).abs() < 1e-9);
    }

    #[test]
    fn the_progress_file_carries_the_totals_without_the_open_generation() {
        let mut profile = LoopProfile::default();
        profile.record(Step::Pipeline, ms(10));
        profile.end_generation(ms(12));
        profile.record(Step::Pipeline, ms(10));
        profile.pipeline = Some(PipelineParts {
            prepare_seconds: 0.5,
            corpus_wait_seconds: 0.25,
            ..PipelineParts::default()
        });
        let text = serde_json::to_string(&profile).unwrap();
        assert!(!text.contains("open_seconds"), "{text}");
        let back: LoopProfile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.generations, 1);
        assert_eq!(back.steps["pipeline"].calls, 2);
        assert!((back.pipeline.unwrap().corpus_wait_seconds - 0.25).abs() < 1e-12);
        // A file written before the pipeline split existed still reads.
        let older: LoopProfile = serde_json::from_str(
            r#"{"generations":3,"wall_seconds":1.0,"steps":{},"other_seconds":0.1}"#,
        )
        .unwrap();
        assert_eq!(older.pipeline, None);
    }

    #[test]
    fn the_pipeline_parts_come_from_the_async_timing() {
        let timing = Timing {
            prepare_seconds: 3.0,
            corpus_wait_seconds: 2.0,
            corpus_install_seconds: 0.5,
            origin_sync_seconds: 0.25,
            setup_seconds: 0.125,
            loop_seconds: 10.0,
            after_loop_seconds: 1.0,
            integrity_check_seconds: 0.75,
            ..Timing::default()
        };
        assert_eq!(
            PipelineParts::from_timing(&timing),
            PipelineParts {
                prepare_seconds: 3.0,
                corpus_wait_seconds: 2.0,
                corpus_install_seconds: 0.5,
                origin_sync_seconds: 0.25,
                setup_seconds: 0.125,
                loop_seconds: 10.0,
                after_loop_seconds: 1.0,
                integrity_check_seconds: 0.75,
            }
        );
    }
}

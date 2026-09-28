use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::telemetry::{
    CounterReading, TelemetryCollector, TelemetryEvidence, TelemetrySnapshot, TemperatureReading,
    build_evidence, is_safety_temperature,
};

pub const SAFETY_POLICY_VERSION: &str = "CPU-DIT-01-SAFETY-v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdacPolicy {
    Observe,
    FailUncorrected,
    FailAny,
}

#[allow(clippy::struct_excessive_bools)] // Independent safety requirements stay explicit in JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyPolicy {
    pub version: String,
    pub deadline_ms: u64,
    pub monitor_interval_ms: u64,
    pub temperature_limit_enabled: bool,
    pub max_temperature_millicelsius: i64,
    pub require_temperature_sensor: bool,
    pub require_edac_counters: bool,
    pub edac_policy: EdacPolicy,
    pub fail_on_throttle_delta: bool,
    pub fail_on_cgroup_oom: bool,
    pub fail_on_kernel_hardware_events: bool,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self {
            version: SAFETY_POLICY_VERSION.to_owned(),
            deadline_ms: 300_000,
            monitor_interval_ms: 250,
            temperature_limit_enabled: false,
            max_temperature_millicelsius: 95_000,
            require_temperature_sensor: false,
            require_edac_counters: false,
            edac_policy: EdacPolicy::FailUncorrected,
            fail_on_throttle_delta: false,
            fail_on_cgroup_oom: true,
            fail_on_kernel_hardware_events: true,
        }
    }
}

impl SafetyPolicy {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.version != SAFETY_POLICY_VERSION {
            return Err(format!(
                "unsupported safety policy version {}",
                self.version
            ));
        }
        if self.deadline_ms == 0 {
            return Err("deadline_ms must be positive".to_owned());
        }
        if self.monitor_interval_ms == 0 || self.monitor_interval_ms > self.deadline_ms {
            return Err("monitor_interval_ms must be positive and <= deadline_ms".to_owned());
        }
        if !(1_000..=150_000).contains(&self.max_temperature_millicelsius) {
            return Err("max temperature must be between 1 C and 150 C".to_owned());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SafetyFaultInjection {
    pub timeout: bool,
    pub temperature_millicelsius: Option<i64>,
    pub edac_corrected_delta: u64,
    pub edac_uncorrected_delta: u64,
    pub cgroup_oom_delta: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortKind {
    Deadline,
    Temperature,
    Edac,
    CgroupOom,
    RequiredTelemetryUnavailable,
    WorkerCrash,
    WorkerError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbortEvent {
    pub kind: AbortKind,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyViolation {
    pub kind: AbortKind,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyEvidence {
    pub policy: SafetyPolicy,
    pub stop_requested: bool,
    pub stop_event: Option<AbortEvent>,
    pub violations: Vec<SafetyViolation>,
    pub deadline_elapsed_ms: u128,
    pub fault_injection: SafetyFaultInjection,
}

#[derive(Debug)]
pub(crate) struct RunControl {
    cancelled: AtomicBool,
    event: Mutex<Option<AbortEvent>>,
}

impl RunControl {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            event: Mutex::new(None),
        }
    }

    pub fn cancel(&self, kind: AbortKind, reason: impl Into<String>) {
        let event = AbortEvent {
            kind,
            reason: reason.into(),
        };
        if let Ok(mut slot) = self.event.lock()
            && slot.is_none()
        {
            *slot = Some(event);
        }
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn should_stop(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn event(&self) -> Option<AbortEvent> {
        self.event.lock().ok().and_then(|event| event.clone())
    }
}

struct MonitorResult {
    end: TelemetrySnapshot,
    samples: u64,
    peak_temperature_millicelsius: Option<i64>,
}

pub(crate) struct SafetyMonitor {
    pub control: Arc<RunControl>,
    policy: SafetyPolicy,
    faults: SafetyFaultInjection,
    collector: TelemetryCollector,
    start: TelemetrySnapshot,
    started: Instant,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<MonitorResult>>,
}

impl Drop for SafetyMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl SafetyMonitor {
    #[must_use]
    pub fn start(policy: SafetyPolicy, faults: SafetyFaultInjection) -> Self {
        let collector = TelemetryCollector::default();
        let start = collector.collect();
        let availability = TelemetryCollector::availability(&start);
        let control = Arc::new(RunControl::new());
        if policy.require_temperature_sensor && !availability.temperature_sensors {
            control.cancel(
                AbortKind::RequiredTelemetryUnavailable,
                "temperature sensor is required by policy but unavailable",
            );
        }
        if policy.require_edac_counters && !availability.edac_counters {
            control.cancel(
                AbortKind::RequiredTelemetryUnavailable,
                "EDAC counters are required by policy but unavailable",
            );
        }
        if faults.timeout {
            control.cancel(AbortKind::Deadline, "synthetic deadline fault injection");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_control = Arc::clone(&control);
        let thread_collector = collector.clone();
        let thread_policy = policy.clone();
        let thread_faults = faults.clone();
        let thread_start = start.clone();
        let started = Instant::now();
        let thread_started = started;
        let handle = thread::spawn(move || {
            monitor_loop(
                &thread_collector,
                &thread_policy,
                &thread_faults,
                &thread_start,
                &thread_control,
                &thread_stop,
                thread_started,
            )
        });
        Self {
            control,
            policy,
            faults,
            collector,
            start,
            started,
            stop,
            handle: Some(handle),
        }
    }

    #[must_use]
    pub fn finish(mut self) -> (TelemetryEvidence, SafetyEvidence) {
        self.stop.store(true, Ordering::Release);
        let result = self
            .handle
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_else(|| MonitorResult {
                end: apply_faults(self.collector.collect(), &self.faults),
                samples: 1,
                peak_temperature_millicelsius: self.start.max_temperature_millicelsius(),
            });
        let kernel_events =
            TelemetryCollector::collect_kernel_events_since(self.start.captured_unix_ms);
        let telemetry = build_evidence(
            self.start.clone(),
            result.end,
            result.samples,
            result.peak_temperature_millicelsius,
            kernel_events,
        );
        let violations = evaluate_telemetry(&self.policy, &telemetry);
        if let Some(violation) = violations.first() {
            self.control
                .cancel(violation.kind, violation.reason.clone());
        }
        let safety = SafetyEvidence {
            policy: self.policy.clone(),
            stop_requested: self.control.should_stop(),
            stop_event: self.control.event(),
            violations,
            deadline_elapsed_ms: self.started.elapsed().as_millis(),
            fault_injection: self.faults.clone(),
        };
        (telemetry, safety)
    }
}

fn monitor_loop(
    collector: &TelemetryCollector,
    policy: &SafetyPolicy,
    faults: &SafetyFaultInjection,
    baseline: &TelemetrySnapshot,
    control: &RunControl,
    stop: &AtomicBool,
    started: Instant,
) -> MonitorResult {
    let mut samples = 0_u64;
    let mut peak = baseline.max_temperature_millicelsius();
    let mut end;
    loop {
        end = apply_faults(collector.collect(), faults);
        samples = samples.saturating_add(1);
        peak = max_option(peak, end.max_temperature_millicelsius());
        evaluate_live(policy, baseline, &end, control, started.elapsed());
        if stop.load(Ordering::Acquire) {
            break;
        }
        thread::sleep(Duration::from_millis(policy.monitor_interval_ms));
    }
    MonitorResult {
        end,
        samples,
        peak_temperature_millicelsius: peak,
    }
}

fn evaluate_live(
    policy: &SafetyPolicy,
    baseline: &TelemetrySnapshot,
    current: &TelemetrySnapshot,
    control: &RunControl,
    elapsed: Duration,
) {
    if elapsed.as_millis() >= u128::from(policy.deadline_ms) {
        control.cancel(
            AbortKind::Deadline,
            format!("run exceeded deadline of {} ms", policy.deadline_ms),
        );
        return;
    }
    if policy.temperature_limit_enabled
        && current
            .max_temperature_millicelsius()
            .is_some_and(|value| value >= policy.max_temperature_millicelsius)
    {
        control.cancel(
            AbortKind::Temperature,
            format!(
                "temperature reached policy limit of {} millidegrees C",
                policy.max_temperature_millicelsius
            ),
        );
        return;
    }
    if let Some(reading) = current.temperatures.iter().find(|reading| {
        policy.temperature_limit_enabled
            && is_safety_temperature(reading)
            && reading
                .critical_millidegrees_c
                .is_some_and(|critical| reading.millidegrees_c >= critical)
    }) {
        control.cancel(
            AbortKind::Temperature,
            format!(
                "sensor {} reached its critical limit at {} millidegrees C",
                reading.label, reading.millidegrees_c
            ),
        );
        return;
    }
    let corrected_delta = current
        .edac_corrected_total()
        .saturating_sub(baseline.edac_corrected_total());
    let uncorrected_delta = current
        .edac_uncorrected_total()
        .saturating_sub(baseline.edac_uncorrected_total());
    if uncorrected_delta > 0 || (policy.edac_policy == EdacPolicy::FailAny && corrected_delta > 0) {
        control.cancel(
            AbortKind::Edac,
            format!("EDAC delta corrected={corrected_delta}, uncorrected={uncorrected_delta}"),
        );
        return;
    }
    if policy.fail_on_cgroup_oom
        && (current.cgroup_event("oom") > baseline.cgroup_event("oom")
            || current.cgroup_event("oom_kill") > baseline.cgroup_event("oom_kill"))
    {
        control.cancel(
            AbortKind::CgroupOom,
            "cgroup OOM event increased during run",
        );
    }
}

fn evaluate_telemetry(
    policy: &SafetyPolicy,
    telemetry: &TelemetryEvidence,
) -> Vec<SafetyViolation> {
    let mut violations = Vec::new();
    if policy.temperature_limit_enabled
        && telemetry
            .peak_temperature_millicelsius
            .is_some_and(|value| value >= policy.max_temperature_millicelsius)
    {
        violations.push(SafetyViolation {
            kind: AbortKind::Temperature,
            reason: format!(
                "peak temperature reached or exceeded {} millidegrees C",
                policy.max_temperature_millicelsius
            ),
        });
    }
    for reading in telemetry.end.temperatures.iter().filter(|reading| {
        policy.temperature_limit_enabled
            && is_safety_temperature(reading)
            && reading
                .critical_millidegrees_c
                .is_some_and(|critical| reading.millidegrees_c >= critical)
    }) {
        violations.push(SafetyViolation {
            kind: AbortKind::Temperature,
            reason: format!(
                "sensor {} reached its critical limit at {} millidegrees C",
                reading.label, reading.millidegrees_c
            ),
        });
    }
    if telemetry.edac_uncorrected_delta > 0
        || (policy.edac_policy == EdacPolicy::FailAny && telemetry.edac_corrected_delta > 0)
    {
        violations.push(SafetyViolation {
            kind: AbortKind::Edac,
            reason: format!(
                "EDAC delta corrected={}, uncorrected={}",
                telemetry.edac_corrected_delta, telemetry.edac_uncorrected_delta
            ),
        });
    }
    if policy.fail_on_throttle_delta
        && (telemetry.throttle_core_delta > 0 || telemetry.throttle_package_delta > 0)
    {
        violations.push(SafetyViolation {
            kind: AbortKind::Temperature,
            reason: format!(
                "thermal throttle counters increased: core={}, package={}",
                telemetry.throttle_core_delta, telemetry.throttle_package_delta
            ),
        });
    }
    if policy.fail_on_cgroup_oom
        && (telemetry.cgroup_oom_delta > 0 || telemetry.cgroup_oom_kill_delta > 0)
    {
        violations.push(SafetyViolation {
            kind: AbortKind::CgroupOom,
            reason: format!(
                "cgroup OOM delta oom={}, oom_kill={}",
                telemetry.cgroup_oom_delta, telemetry.cgroup_oom_kill_delta
            ),
        });
    }
    if policy.fail_on_kernel_hardware_events
        && (telemetry.kernel_events.machine_check_events > 0
            || telemetry.kernel_events.edac_ecc_events > 0)
    {
        violations.push(SafetyViolation {
            kind: AbortKind::Edac,
            reason: format!(
                "kernel hardware events during run: machine_check={}, edac_ecc={}",
                telemetry.kernel_events.machine_check_events,
                telemetry.kernel_events.edac_ecc_events
            ),
        });
    }
    if policy.fail_on_throttle_delta && telemetry.kernel_events.thermal_throttle_events > 0 {
        violations.push(SafetyViolation {
            kind: AbortKind::Temperature,
            reason: format!(
                "kernel thermal/throttle events during run: {}",
                telemetry.kernel_events.thermal_throttle_events
            ),
        });
    }
    violations
}

fn apply_faults(
    mut snapshot: TelemetrySnapshot,
    faults: &SafetyFaultInjection,
) -> TelemetrySnapshot {
    if let Some(value) = faults.temperature_millicelsius {
        snapshot.temperatures.push(TemperatureReading {
            source: "synthetic:fault_injection".to_owned(),
            label: "injected_temperature".to_owned(),
            millidegrees_c: value,
            critical_millidegrees_c: None,
        });
    }
    if faults.edac_corrected_delta > 0 {
        snapshot.edac_corrected.push(CounterReading {
            source: "synthetic:fault_injection".to_owned(),
            value: faults.edac_corrected_delta,
        });
    }
    if faults.edac_uncorrected_delta > 0 {
        snapshot.edac_uncorrected.push(CounterReading {
            source: "synthetic:fault_injection".to_owned(),
            value: faults.edac_uncorrected_delta,
        });
    }
    if faults.cgroup_oom_delta > 0 {
        let value = snapshot
            .cgroup_event("oom")
            .saturating_add(faults.cgroup_oom_delta);
        snapshot
            .cgroup_memory_events
            .insert("oom".to_owned(), value);
    }
    snapshot
}

const fn max_option(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(if left > right { left } else { right }),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::telemetry::{
        KernelEventEvidence, TelemetryAvailability, TelemetryEvidence, TelemetrySnapshot,
        TemperatureReading,
    };

    use super::{AbortKind, EdacPolicy, SafetyPolicy, evaluate_telemetry};

    fn evidence() -> TelemetryEvidence {
        TelemetryEvidence {
            availability: TelemetryAvailability::default(),
            start: TelemetrySnapshot::default(),
            end: TelemetrySnapshot::default(),
            samples_collected: 2,
            peak_temperature_millicelsius: None,
            edac_corrected_delta: 0,
            edac_uncorrected_delta: 0,
            throttle_core_delta: 0,
            throttle_package_delta: 0,
            cgroup_oom_delta: 0,
            cgroup_oom_kill_delta: 0,
            kernel_events: KernelEventEvidence::default(),
        }
    }

    #[test]
    fn uncorrected_edac_is_a_violation() {
        let mut evidence = evidence();
        evidence.edac_uncorrected_delta = 1;
        let violations = evaluate_telemetry(&SafetyPolicy::default(), &evidence);
        assert_eq!(violations[0].kind, AbortKind::Edac);
    }

    #[test]
    fn corrected_edac_observe_policy_does_not_fail() {
        let mut evidence = evidence();
        evidence.edac_corrected_delta = 1;
        let policy = SafetyPolicy {
            edac_policy: EdacPolicy::Observe,
            ..SafetyPolicy::default()
        };
        assert!(evaluate_telemetry(&policy, &evidence).is_empty());
    }

    #[test]
    fn high_temperature_is_a_violation() {
        let mut evidence = evidence();
        evidence.peak_temperature_millicelsius = Some(96_000);
        let policy = SafetyPolicy {
            temperature_limit_enabled: true,
            ..SafetyPolicy::default()
        };
        let violations = evaluate_telemetry(&policy, &evidence);
        assert_eq!(violations[0].kind, AbortKind::Temperature);
    }

    #[test]
    fn high_temperature_is_telemetry_only_when_limit_is_disabled() {
        let mut evidence = evidence();
        evidence.peak_temperature_millicelsius = Some(127_000);
        assert!(evaluate_telemetry(&SafetyPolicy::default(), &evidence).is_empty());
    }

    #[test]
    fn per_sensor_critical_limit_is_a_violation() {
        let mut evidence = evidence();
        evidence.end.temperatures.push(TemperatureReading {
            source: "test".to_owned(),
            label: "coretemp:Package id 0".to_owned(),
            millidegrees_c: 80_000,
            critical_millidegrees_c: Some(79_000),
        });
        let policy = SafetyPolicy {
            temperature_limit_enabled: true,
            ..SafetyPolicy::default()
        };
        let violations = evaluate_telemetry(&policy, &evidence);
        assert_eq!(violations[0].kind, AbortKind::Temperature);
    }
}

//! GPU-H5 host memory pressure sensing. Reads only; it never changes swap or any system
//! setting. Pressure means "other programs are taking the RAM this pipeline budgeted", and the
//! controller answers by draining, not by asking for more.
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

/// `MemAvailable` from `/proc/meminfo`, in bytes.
#[must_use]
pub fn parse_available_bytes(meminfo: &str) -> Option<u64> {
    let kib = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?;
    kib.checked_mul(1024)
}

/// The `some avg10` percentage from `/proc/pressure/memory`.
#[must_use]
pub fn parse_psi_some_avg10(psi: &str) -> Option<f64> {
    let line = psi.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

#[derive(Clone, Debug)]
pub struct PressureConfig {
    /// Pressure when `MemAvailable` drops below this (0 disables the check).
    pub min_available_bytes: u64,
    /// Pressure when PSI memory "some avg10" reaches this percentage (0 disables the check).
    pub psi_some_avg10_limit: f64,
    /// Rate limit for reading the files.
    pub interval: Duration,
    /// Test hook: when set, pressure is whatever this file says (`1` or `0`).
    pub override_file: Option<PathBuf>,
}

impl Default for PressureConfig {
    fn default() -> Self {
        Self {
            min_available_bytes: 256 * 1024 * 1024,
            psi_some_avg10_limit: 25.0,
            interval: Duration::from_millis(250),
            override_file: None,
        }
    }
}

/// Decides whether the host is under memory pressure right now.
#[must_use]
pub fn under_pressure(
    config: &PressureConfig,
    meminfo: Option<&str>,
    psi: Option<&str>,
    override_text: Option<&str>,
) -> bool {
    if let Some(text) = override_text {
        return text.trim() == "1";
    }
    let low_memory = config.min_available_bytes > 0
        && meminfo
            .and_then(parse_available_bytes)
            .is_some_and(|available| available < config.min_available_bytes);
    let stalled = config.psi_some_avg10_limit > 0.0
        && psi
            .and_then(parse_psi_some_avg10)
            .is_some_and(|avg| avg >= config.psi_some_avg10_limit);
    low_memory || stalled
}

/// Rate-limited reader. An unreadable file counts as "no pressure signal", never as pressure.
#[derive(Debug)]
pub struct PressureSampler {
    config: PressureConfig,
    last: Option<Instant>,
    current: bool,
    pub samples: u64,
    pub pressured_samples: u64,
}

impl PressureSampler {
    #[must_use]
    pub fn new(config: PressureConfig) -> Self {
        Self {
            config,
            last: None,
            current: false,
            samples: 0,
            pressured_samples: 0,
        }
    }

    /// Current pressure, re-reading at most once per interval.
    pub fn check(&mut self) -> bool {
        let due = self
            .last
            .is_none_or(|t| t.elapsed() >= self.config.interval);
        if !due {
            return self.current;
        }
        self.last = Some(Instant::now());
        let read = |path: &str| fs::read_to_string(path).ok();
        let override_text = self
            .config
            .override_file
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok());
        let meminfo = read("/proc/meminfo");
        let psi = read("/proc/pressure/memory");
        self.current = under_pressure(
            &self.config,
            meminfo.as_deref(),
            psi.as_deref(),
            override_text.as_deref(),
        );
        self.samples += 1;
        self.pressured_samples += u64::from(self.current);
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str =
        "MemTotal:       16384000 kB\nMemFree:         1000000 kB\nMemAvailable:    2097152 kB\n";
    const PSI: &str = "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";

    #[test]
    fn parsers_read_real_formats_and_reject_garbage() {
        assert_eq!(parse_available_bytes(MEMINFO), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_available_bytes("MemTotal: 1 kB\n"), None);
        assert_eq!(parse_available_bytes("MemAvailable: x kB\n"), None);
        assert_eq!(parse_psi_some_avg10(PSI), Some(0.0));
        assert_eq!(
            parse_psi_some_avg10("some avg10=31.25 avg60=1.00 avg300=0.10 total=9\n"),
            Some(31.25)
        );
        assert_eq!(parse_psi_some_avg10("full avg10=9.00\n"), None);
        assert_eq!(parse_psi_some_avg10(""), None);
    }

    #[test]
    fn pressure_needs_low_available_memory_or_a_stalling_psi_and_missing_data_is_not_pressure() {
        let c = PressureConfig::default();
        assert!(!under_pressure(&c, Some(MEMINFO), Some(PSI), None));
        let low = "MemAvailable:     100000 kB\n";
        assert!(under_pressure(&c, Some(low), Some(PSI), None));
        let stall = "some avg10=40.00 avg60=1 avg300=1 total=1\n";
        assert!(under_pressure(&c, Some(MEMINFO), Some(stall), None));
        // Unreadable sources give no signal at all.
        assert!(!under_pressure(&c, None, None, None));
        // Checks can be disabled independently.
        let off = PressureConfig {
            min_available_bytes: 0,
            psi_some_avg10_limit: 0.0,
            ..PressureConfig::default()
        };
        assert!(!under_pressure(&off, Some(low), Some(stall), None));
        // The test hook overrides everything else.
        assert!(under_pressure(&off, Some(MEMINFO), Some(PSI), Some("1\n")));
        assert!(!under_pressure(&c, Some(low), Some(stall), Some("0")));
    }

    #[test]
    fn the_sampler_rate_limits_and_the_override_file_toggles_pressure() {
        let path = std::env::temp_dir().join(format!("dit-pressure-{}", std::process::id()));
        fs::write(&path, "0").unwrap();
        let mut sampler = PressureSampler::new(PressureConfig {
            interval: Duration::from_millis(20),
            override_file: Some(path.clone()),
            ..PressureConfig::default()
        });
        assert!(!sampler.check());
        fs::write(&path, "1").unwrap();
        assert!(!sampler.check(), "still inside the rate limit: no re-read");
        std::thread::sleep(Duration::from_millis(30));
        assert!(sampler.check());
        fs::write(&path, "0").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(!sampler.check());
        assert_eq!((sampler.samples, sampler.pressured_samples), (3, 1));
        fs::remove_file(path).unwrap();
    }
}

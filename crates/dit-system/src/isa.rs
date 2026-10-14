//! Instruction-set levels of the CPU kernels (v1.2, C4).
//!
//! `dit-cli` is built three times: for the x86-64 baseline (v1, SSE2), for x86-64-v3 (AVX2,
//! FMA...) and for x86-64-v4 (AVX-512). The baseline binary, the only entry point, works out
//! here which one this CPU runs and `exec`s it (`dit-cli-v3`, `dit-cli-v4` next to it). The
//! workspace forbids `unsafe`, so separate binaries are how the kernels get the wider
//! instructions. Results do not depend on the level: every kernel is checked against a reference
//! computed in the same binary.

use std::{fmt, sync::OnceLock};

use serde::{Deserialize, Serialize};

/// Environment variable a technician sets to force a level (`v1`, `v3`, `v4` or `auto`).
pub const ISA_ENV: &str = "DIT_ISA";
/// Kernel command line parameter with the same meaning (`dit_isa=v3`); the variable wins.
pub const ISA_CMDLINE: &str = "dit_isa";
/// Set by the baseline binary on the binary it `exec`s: `<level>:<auto|manual>`. A binary that
/// finds it does not choose again.
pub const ISA_CHOSEN_ENV: &str = "DIT_ISA_CHOSEN";

/// An x86-64 microarchitecture level the kernels are built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsaLevel {
    /// The x86-64 baseline: SSE2.
    V1,
    /// x86-64-v3: AVX, AVX2, FMA, BMI1/2, F16C, LZCNT, MOVBE.
    V3,
    /// x86-64-v4: x86-64-v3 and AVX-512 F, BW, CD, DQ, VL.
    V4,
}

impl IsaLevel {
    pub const ALL: [Self; 3] = [Self::V1, Self::V3, Self::V4];

    /// `v1`, `v3`, `v4`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V3 => "v3",
            Self::V4 => "v4",
        }
    }

    /// The instructions the operator recognises.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::V1 => "SSE2",
            Self::V3 => "AVX2+FMA",
            Self::V4 => "AVX-512",
        }
    }

    /// What a CPU needs for this level, for a refusal.
    const fn needs(self) -> &'static str {
        match self {
            Self::V1 => "x86-64",
            Self::V3 => "AVX, AVX2, FMA, BMI1, BMI2, F16C, LZCNT and MOVBE",
            Self::V4 => "AVX-512 F, BW, CD, DQ and VL",
        }
    }

    /// The suffix of this level's binary next to the baseline one (`dit-cli-v3`); none for v1.
    #[must_use]
    pub const fn binary_suffix(self) -> Option<&'static str> {
        match self {
            Self::V1 => None,
            Self::V3 => Some("-v3"),
            Self::V4 => Some("-v4"),
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|level| text.eq_ignore_ascii_case(level.as_str()))
    }
}

impl fmt::Display for IsaLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The level this binary was built for.
#[must_use]
pub const fn compiled_level() -> IsaLevel {
    if cfg!(all(
        target_feature = "avx512f",
        target_feature = "avx512bw",
        target_feature = "avx512dq",
        target_feature = "avx512vl"
    )) {
        IsaLevel::V4
    } else if cfg!(all(target_feature = "avx2", target_feature = "fma")) {
        IsaLevel::V3
    } else {
        IsaLevel::V1
    }
}

/// The highest level this CPU runs. `is_x86_feature_detected!` also checks that the kernel has
/// enabled the AVX and AVX-512 register state, which the `/proc/cpuinfo` flags alone do not say.
#[must_use]
pub fn cpu_level() -> IsaLevel {
    #[cfg(target_arch = "x86_64")]
    {
        let v3 = std::is_x86_feature_detected!("avx")
            && std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("fma")
            && std::is_x86_feature_detected!("bmi1")
            && std::is_x86_feature_detected!("bmi2")
            && std::is_x86_feature_detected!("f16c")
            && std::is_x86_feature_detected!("lzcnt")
            && std::is_x86_feature_detected!("movbe")
            && std::is_x86_feature_detected!("xsave")
            && std::is_x86_feature_detected!("sse4.2")
            && std::is_x86_feature_detected!("popcnt")
            && std::is_x86_feature_detected!("cmpxchg16b");
        let v4 = v3
            && std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && std::is_x86_feature_detected!("avx512cd")
            && std::is_x86_feature_detected!("avx512dq")
            && std::is_x86_feature_detected!("avx512vl");
        if v4 {
            IsaLevel::V4
        } else if v3 {
            IsaLevel::V3
        } else {
            IsaLevel::V1
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        IsaLevel::V1
    }
}

/// How the level was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsaSelection {
    /// The highest level this CPU runs and the image has.
    Auto,
    /// Forced with `DIT_ISA` or `dit_isa=`.
    Manual,
    /// This binary was started on its own, without the choice (development, host tools).
    Direct,
}

impl IsaSelection {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Direct => "direct",
        }
    }
}

/// What was asked for: the highest that works, or one level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsaRequest {
    Auto,
    Manual(IsaLevel),
}

/// Reads the request: `DIT_ISA` if set, else `dit_isa=` on the kernel command line, else auto.
///
/// # Errors
///
/// A value that is not `v1`, `v3`, `v4` or `auto`.
pub fn request(env_value: Option<&str>, cmdline: &str) -> Result<IsaRequest, String> {
    let (source, value) = match env_value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => (ISA_ENV, value.to_owned()),
        None => match cmdline
            .split_whitespace()
            .find_map(|word| word.strip_prefix(ISA_CMDLINE)?.strip_prefix('='))
        {
            Some(value) => (ISA_CMDLINE, value.to_owned()),
            None => return Ok(IsaRequest::Auto),
        },
    };
    if value.eq_ignore_ascii_case("auto") {
        return Ok(IsaRequest::Auto);
    }
    IsaLevel::parse(&value)
        .map(IsaRequest::Manual)
        .ok_or_else(|| format!("{source}={value} is not one of v1, v3, v4, auto"))
}

/// The level to run: for auto the highest the CPU runs among the installed ones (v1 always is);
/// a forced level must be installed and run on this CPU.
///
/// # Errors
///
/// A forced level this CPU cannot run (it would die of SIGILL) or that is not installed.
pub fn choose(
    request: IsaRequest,
    cpu: IsaLevel,
    installed: &[IsaLevel],
) -> Result<(IsaLevel, IsaSelection), String> {
    match request {
        IsaRequest::Auto => Ok((
            installed
                .iter()
                .copied()
                .filter(|&level| level <= cpu)
                .chain([IsaLevel::V1])
                .max()
                .unwrap_or(IsaLevel::V1),
            IsaSelection::Auto,
        )),
        IsaRequest::Manual(level) if level > cpu => Err(format!(
            "dit_isa={level} needs {}, which this CPU does not have (it runs up to {cpu})",
            level.needs()
        )),
        IsaRequest::Manual(level) if level != IsaLevel::V1 && !installed.contains(&level) => Err(
            format!("dit_isa={level}: the {level} binary is not installed next to this one"),
        ),
        IsaRequest::Manual(level) => Ok((level, IsaSelection::Manual)),
    }
}

/// The value of [`ISA_CHOSEN_ENV`] for a choice.
#[must_use]
pub fn chosen_value(level: IsaLevel, selection: IsaSelection) -> String {
    format!("{level}:{}", selection.as_str())
}

/// The selection a [`ISA_CHOSEN_ENV`] value records, when it names this binary's level.
#[must_use]
pub fn parse_chosen(value: &str) -> Option<IsaSelection> {
    let (level, selection) = value.split_once(':')?;
    if IsaLevel::parse(level)? != compiled_level() {
        return None;
    }
    match selection {
        "auto" => Some(IsaSelection::Auto),
        "manual" => Some(IsaSelection::Manual),
        _ => None,
    }
}

static SELECTION: OnceLock<IsaSelection> = OnceLock::new();

/// Records how this process's level was decided (once, at start; later calls are ignored).
pub fn record_selection(selection: IsaSelection) {
    let _ = SELECTION.set(selection);
}

/// The kernels' level in a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsaEvidence {
    /// The level the kernels ran at (this binary's).
    pub level: IsaLevel,
    /// How it was decided.
    pub selection: IsaSelection,
    /// The highest level this CPU runs.
    pub cpu_level: IsaLevel,
}

impl IsaEvidence {
    /// This process's.
    #[must_use]
    pub fn current() -> Self {
        Self {
            level: compiled_level(),
            selection: SELECTION.get().copied().unwrap_or(IsaSelection::Direct),
            cpu_level: cpu_level(),
        }
    }

    /// `AVX-512 (v4, auto)`.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} ({}, {})",
            self.level.label(),
            self.level,
            self.selection.as_str()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{IsaLevel, IsaRequest, IsaSelection, choose, request};
    use IsaLevel::{V1, V3, V4};

    #[test]
    fn the_request_comes_from_the_variable_then_the_command_line() {
        let cmdline = "BOOT_IMAGE=/live/vmlinuz boot=live dit_isa=v3 quiet";
        assert_eq!(request(None, cmdline), Ok(IsaRequest::Manual(V3)));
        assert_eq!(request(Some("v4"), cmdline), Ok(IsaRequest::Manual(V4)));
        assert_eq!(request(Some(" "), cmdline), Ok(IsaRequest::Manual(V3)));
        assert_eq!(request(Some("auto"), cmdline), Ok(IsaRequest::Auto));
        assert_eq!(request(None, "boot=live dit_isa_x=1"), Ok(IsaRequest::Auto));
        assert_eq!(request(None, "boot=live"), Ok(IsaRequest::Auto));
        assert!(request(Some("v2"), "").is_err());
        assert!(
            request(None, "dit_isa=avx512")
                .unwrap_err()
                .contains("dit_isa=avx512")
        );
    }

    #[test]
    fn auto_takes_the_highest_level_the_cpu_runs_and_the_image_has() {
        let all = [V1, V3, V4];
        assert_eq!(
            choose(IsaRequest::Auto, V4, &all),
            Ok((V4, IsaSelection::Auto))
        );
        // Intel 12th-14th gen desktop: no AVX-512.
        assert_eq!(
            choose(IsaRequest::Auto, V3, &all),
            Ok((V3, IsaSelection::Auto))
        );
        assert_eq!(
            choose(IsaRequest::Auto, V1, &all),
            Ok((V1, IsaSelection::Auto))
        );
        // Only the baseline installed (a development checkout).
        assert_eq!(
            choose(IsaRequest::Auto, V4, &[V1]),
            Ok((V1, IsaSelection::Auto))
        );
        assert_eq!(
            choose(IsaRequest::Auto, V4, &[V1, V3]),
            Ok((V3, IsaSelection::Auto))
        );
    }

    #[test]
    fn a_forced_level_the_cpu_cannot_run_is_refused() {
        let all = [V1, V3, V4];
        assert_eq!(
            choose(IsaRequest::Manual(V3), V4, &all),
            Ok((V3, IsaSelection::Manual))
        );
        assert_eq!(
            choose(IsaRequest::Manual(V1), V4, &[V1]),
            Ok((V1, IsaSelection::Manual))
        );
        let refused = choose(IsaRequest::Manual(V4), V3, &all).unwrap_err();
        assert!(refused.contains("AVX-512"), "{refused}");
        assert!(
            choose(IsaRequest::Manual(V4), V4, &[V1, V3])
                .unwrap_err()
                .contains("not installed")
        );
    }
}

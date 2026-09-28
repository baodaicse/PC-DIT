//! Additive, versioned compute profiling. Device intervals overlap host wait;
//! they must never be added to host end-to-end time.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeviceIntervals {
    /// Includes stage boundary/barrier overhead, not isolated `PCIe` wire time.
    pub h2d_seconds: f64,
    pub kernel_seconds: f64,
    pub d2h_seconds: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ComputeProfile {
    pub version: u32,
    pub seed: u64,
    pub generation: u64,
    pub workload: String,
    #[serde(default)]
    pub corpus_sha256: String,
    #[serde(default)]
    pub corpus_reused: bool,
    #[serde(default)]
    pub input_resident: bool,
    #[serde(default)]
    pub matrix_dimension: usize,
    #[serde(default)]
    pub integer_operations: u64,
    pub input_generation_seconds: f64,
    pub cpu_oracle_seconds: f64,
    pub buffer_setup_seconds: f64,
    pub buffers_reused: bool,
    pub pipeline_reused: bool,
    pub persistent_device_buffer_bytes: u64,
    pub persistent_host_buffer_bytes: u64,
    pub pipeline_setup_seconds: f64,
    pub host_upload_seconds: f64,
    pub command_record_seconds: f64,
    pub submit_wait_seconds: f64,
    pub host_readback_seconds: f64,
    pub cpu_compare_seconds: f64,
    pub resource_cleanup_seconds: f64,
    pub result_hash_seconds: f64,
    pub end_to_end_seconds: f64,
    pub gpu: Option<DeviceIntervals>,
    pub gpu_unavailable_reason: Option<String>,
    /// G1 is sequential. This is not an asynchronous scheduler counter.
    pub dispatches_submitted: u64,
    pub dispatches_completed: u64,
    pub dispatches_verified: u64,
}

/// Handles wrapping timestamp counters using the queue family's valid bit count.
/// Returns no measurement if the host wait spans a possible complete wrap.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn timestamp_intervals(
    stamps: [u64; 4],
    valid_bits: u32,
    period_ns: f32,
    host_wait_seconds: f64,
) -> Option<DeviceIntervals> {
    if !(1..=64).contains(&valid_bits) || !period_ns.is_finite() || period_ns <= 0.0 {
        return None;
    }
    let scale = f64::from(period_ns) * 1e-9;
    let modulus = 2_f64.powi(i32::try_from(valid_bits).ok()?);
    if !host_wait_seconds.is_finite()
        || host_wait_seconds < 0.0
        || host_wait_seconds >= modulus * scale
    {
        return None;
    }
    let mask = u64::MAX >> (64 - valid_bits);
    let delta = |a: u64, b: u64| (b.wrapping_sub(a) & mask) as f64 * scale;
    Some(DeviceIntervals {
        h2d_seconds: delta(stamps[0], stamps[1]),
        kernel_seconds: delta(stamps[1], stamps[2]),
        d2h_seconds: delta(stamps[2], stamps[3]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_intervals_handle_wrap_and_mask_unused_bits() {
        let result = timestamp_intervals([250, 259, 270, 280], 8, 1.0, 1e-7).unwrap();
        assert!((result.h2d_seconds - 9e-9).abs() < 1e-15);
        assert!((result.kernel_seconds - 11e-9).abs() < 1e-15);
        assert!((result.d2h_seconds - 10e-9).abs() < 1e-15);
    }

    #[test]
    fn unavailable_or_ambiguous_timestamp_is_not_zero_time() {
        for bits in [0, 65] {
            assert!(timestamp_intervals([0; 4], bits, 1.0, 0.0).is_none());
        }
        assert!(timestamp_intervals([0; 4], 8, 1.0, 1.0).is_none());
        assert!(timestamp_intervals([0; 4], 64, f32::NAN, 0.0).is_none());
        assert!(timestamp_intervals([0; 4], 64, 1.0, f64::NAN).is_none());
        assert!(timestamp_intervals([10, 20, 30, 40], 64, 1.0, 1.0).is_some());
    }
}

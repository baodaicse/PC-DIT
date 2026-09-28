//! Deterministic reference core for CPU-DIT-01.

mod error;
mod gf256;
mod manifest;
mod pattern;
mod pipeline;
mod report;

pub use error::DitError;
pub use manifest::{
    GoldenVector, SourceManifest, VerifiedSource, load_golden_vector, load_verified_source,
};
pub use pattern::{expanded_byte, expanded_word};
pub use pipeline::{FaultInjection, RunConfig, run_reference};
pub use report::{CoverageEvidence, MismatchEvidence, RunReport, RunStatus, StageCheckpoint};

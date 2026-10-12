//! The black box and the rescue console (`docs/FAULT_CATCH_ROADMAP.md`, `FC3a`/`FC3b` and FC1):
//! two small processes beside the app. The black box only watches and records; the rescue
//! console acts only on the operator's key.
pub mod host;
pub mod journal;
pub mod kernel_events;
pub mod outcome;
pub mod postmortem;
pub mod watch;

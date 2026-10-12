//! Kernel log lines that say a device or the machine is in trouble (shared with the watchdog in
//! `dit-system`).
pub use dit_system::kernel_events::{KernelEvent, classify};

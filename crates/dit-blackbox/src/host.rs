//! What the machine says about itself, read straight from `/proc` and `/dev/kmsg` (shared with
//! the watchdog in `dit-system`): no program is started, so this keeps working while the machine
//! is short of memory.
pub use dit_system::pressure::{Memory, Pressure, memory, pressure};
pub use dit_system::procs::{
    KernelLine, KernelLog, Process, alive, parse_kmsg, process, processes, tree,
};

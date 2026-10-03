//! Linux system backend for adaptive RAM and multi-core CPU-DIT routes.

pub mod availability;
mod compute_oracle;
mod cpu;
mod engine;
mod error;
mod gpu;
mod inventory;
mod memory;
pub mod pcie;
pub mod power_platform;
mod report;
mod safety;
mod telemetry;
mod window;

pub use compute_oracle::{
    ComputeOracleConfig, ComputeOracleMismatch, ComputeOracleReport, ComputeOracleStatus,
    ComputeOracleWorkerEvidence, run_compute_oracle,
};
pub use engine::{SeedSource, SystemRunConfig, run_system};
pub use error::SystemDitError;
pub use gpu::{
    GpuDevice, GpuTelemetry, collect_gpu_inventory, collect_gpu_telemetry, format_gpu_summary,
};
pub use inventory::{CpuInventory, CpuThread, MemoryInventory, NumaNode, SystemInventory};
pub use memory::{LockPolicy, MemoryTarget};
pub use report::{
    ArenaEvidence, CpuEvidence, DiagnosticEvidence, ExecutionEvidence, MemoryPlanEvidence,
    SystemComputeOracleEvidence, SystemConfigEvidence, SystemMismatch, SystemProgressReport,
    SystemRunReport, SystemSourceEvidence, WindowCoverageEvidence, WorkerCrashEvidence,
    WorkerEvidence,
};
pub use safety::{
    AbortEvent, AbortKind, EdacPolicy, SafetyEvidence, SafetyFaultInjection, SafetyPolicy,
};
pub use telemetry::{
    KernelEventEvidence, LiveSensorSampler, PowerReading, SensorReport, TelemetryAvailability,
    TelemetryEvidence, TelemetrySnapshot, TemperatureReading, ThrottleDelta, ThrottleGroup,
    collect_sensor_report, throttle_delta,
};

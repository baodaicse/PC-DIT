#[derive(Debug, thiserror::Error)]
pub enum SystemDitError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid system-run configuration: {0}")]
    InvalidConfig(String),

    #[error("anonymous mmap failed for {bytes} bytes: {source}")]
    Mmap {
        bytes: usize,
        #[source]
        source: std::io::Error,
    },

    #[error("worker {worker_id} panicked")]
    WorkerPanic { worker_id: usize },

    #[error("worker {worker_id} failed: {reason}")]
    Worker { worker_id: usize, reason: String },
}

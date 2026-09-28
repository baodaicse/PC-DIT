use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum DitError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot parse JSON {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("source verification failed: {0}")]
    InvalidSource(String),

    #[error("memory allocation failed for {bytes} bytes")]
    Allocation { bytes: usize },

    #[error("matrix is not invertible")]
    MatrixNotInvertible,
}

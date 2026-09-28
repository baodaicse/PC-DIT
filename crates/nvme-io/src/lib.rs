//! Backend-neutral bounded I/O contract.

use std::io;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("backend range [{offset}, {end}) exceeds capacity {capacity}")]
    OutOfRange {
        offset: u64,
        end: u64,
        capacity: u64,
    },
    #[error("I/O offset {offset} or length {length} is not aligned to {alignment} bytes")]
    Unaligned {
        offset: u64,
        length: usize,
        alignment: u32,
    },
    #[error("host-write budget exceeded: attempted {attempted}, remaining {remaining}")]
    WriteBudgetExceeded { attempted: u64, remaining: u64 },
    #[error("test path already exists and will not be overwritten: {0}")]
    Exists(PathBuf),
    #[error("target must be the authorized file or block device type: {0}")]
    UnsafeTarget(PathBuf),
    #[error("target identity changed before open: {0}")]
    IdentityChanged(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

pub trait StorageBackend {
    fn id(&self) -> &str;
    fn capacity_bytes(&self) -> u64;
    /// Writes the entire buffer at an offset relative to the authorized region.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] for range, alignment, budget or underlying I/O failures.
    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<(), BackendError>;
    /// Reads the entire buffer at an offset relative to the authorized region.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] for range, alignment, short read or underlying I/O failures.
    fn read_exact_at(&mut self, offset: u64, data: &mut [u8]) -> Result<(), BackendError>;
    /// Flushes submitted data according to the backend durability contract.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] when the underlying flush fails.
    fn flush(&mut self) -> Result<(), BackendError>;
    fn host_bytes_written(&self) -> u64;
}

/// Checks an offset/length pair against an exclusive capacity.
///
/// # Errors
///
/// Returns [`BackendError::OutOfRange`] on overflow or when the end exceeds capacity.
pub fn check_range(offset: u64, len: usize, capacity: u64) -> Result<(), BackendError> {
    let len = u64::try_from(len).unwrap_or(u64::MAX);
    let end = offset.checked_add(len).ok_or(BackendError::OutOfRange {
        offset,
        end: u64::MAX,
        capacity,
    })?;
    if end > capacity {
        Err(BackendError::OutOfRange {
            offset,
            end,
            capacity,
        })
    } else {
        Ok(())
    }
}

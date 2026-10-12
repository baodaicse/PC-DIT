//! Non-device backends for TASK-A. This crate deliberately rejects block devices.

use memmap2::MmapMut;
use nvme_io::check_range;
pub use nvme_io::{BackendError, StorageBackend};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct FileBackend {
    file: File,
    path: PathBuf,
    id: String,
    capacity_bytes: u64,
    host_bytes_written: u64,
}

impl FileBackend {
    /// Creates a regular-file simulator with an exact logical capacity.
    ///
    /// Existing paths, symlinks and devices are always rejected. Test code must never truncate
    /// user data.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] when the target is unsafe, already exists without permission,
    /// or cannot be created/resized.
    pub fn create(path: impl AsRef<Path>, capacity_bytes: u64) -> Result<Self, BackendError> {
        let path = path.as_ref().to_path_buf();
        if path.symlink_metadata().is_ok() {
            return Err(BackendError::Exists(path));
        }

        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        let file = options.open(&path)?;
        let initialization = (|| -> Result<(), BackendError> {
            if !file.metadata()?.is_file() {
                return Err(BackendError::UnsafeTarget(path.clone()));
            }
            file.set_len(capacity_bytes)?;
            Ok(())
        })();
        if let Err(error) = initialization {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let id = format!("file:{}", path.display());
        Ok(Self {
            file,
            path,
            id,
            capacity_bytes,
            host_bytes_written: 0,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether the filesystem holding `directory` fills with zeros everything between a file's
/// written data and a write beyond it (`ntfs3` and exFAT keep a "valid data length"; FAT extends
/// with zeros): a far first write then waits for gigabytes of zeros. The field's eight
/// "PLATFORM" SSD stalls were this, on NTFS (`docs/FAULT_CATCH_ROADMAP.md`, FC6).
#[must_use]
pub fn zero_fills_ahead(directory: &Path) -> bool {
    /// `ntfs3` ("ntfs"), the old `ntfs` driver, exFAT, FAT (`msdos` / `vfat`).
    const MAGICS: [i64; 4] = [0x7366_746e, 0x5346_544e, 0x2011_bab0, 0x4d44];
    nix::sys::statfs::statfs(directory).is_ok_and(|fs| MAGICS.contains(&fs.filesystem_type().0))
}

/// A data-preserving regular-file backend using direct I/O.
///
/// It can only create a new file and therefore cannot overwrite an existing path. The caller is
/// responsible for placing the unique file on the filesystem belonging to the selected device
/// and removing it after the report has been committed.
#[derive(Debug)]
pub struct DirectFileBackend {
    file: File,
    path: PathBuf,
    id: String,
    capacity_bytes: u64,
    max_host_write_bytes: u64,
    host_bytes_written: u64,
}

impl DirectFileBackend {
    /// Creates a new 4 KiB-aligned direct-I/O test file without replacing any existing path.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] if the path already exists, the capacity/budget is invalid, or
    /// the filesystem cannot create a direct-I/O regular file.
    pub fn create_new(
        path: impl AsRef<Path>,
        capacity_bytes: u64,
        max_host_write_bytes: u64,
    ) -> Result<Self, BackendError> {
        Self::create(path.as_ref(), capacity_bytes, max_host_write_bytes, true)
    }

    fn create(
        path: &Path,
        capacity_bytes: u64,
        max_host_write_bytes: u64,
        extend: bool,
    ) -> Result<Self, BackendError> {
        let path = path.to_path_buf();
        if path.symlink_metadata().is_ok() {
            return Err(BackendError::Exists(path));
        }
        if capacity_bytes < 1024 * 1024
            || !capacity_bytes.is_multiple_of(4096)
            || max_host_write_bytes < 4096
            || !max_host_write_bytes.is_multiple_of(4096)
        {
            return Err(BackendError::Unaligned {
                offset: 0,
                length: usize::try_from(capacity_bytes).unwrap_or(usize::MAX),
                alignment: 4096,
            });
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_DIRECT | libc::O_DSYNC | libc::O_NOFOLLOW)
            .open(&path)?;
        if !file.metadata()?.is_file() {
            return Err(BackendError::UnsafeTarget(path));
        }
        if extend {
            file.set_len(capacity_bytes)?;
        }
        // The new file's record reaches the disk before the load starts: a crash mid-test then
        // leaves a complete file entry behind, not a half-created one.
        file.sync_all()?;
        Ok(Self {
            file,
            id: format!("preserving-file:{}", path.display()),
            path,
            capacity_bytes,
            max_host_write_bytes,
            host_bytes_written: 0,
        })
    }

    /// [`Self::create_new`] for a filesystem that fills with zeros everything before a write
    /// beyond the data already written ([`zero_fills_ahead`]): the file is not extended up front
    /// and grows as the test writes it, which it must do in order from its start
    /// (`docs/FAULT_CATCH_ROADMAP.md`, FC6).
    ///
    /// # Errors
    ///
    /// As [`Self::create_new`].
    pub fn create_new_in_order(
        path: impl AsRef<Path>,
        capacity_bytes: u64,
        max_host_write_bytes: u64,
    ) -> Result<Self, BackendError> {
        Self::create(path.as_ref(), capacity_bytes, max_host_write_bytes, false)
    }

    /// Opens a direct-I/O test file this suite wrote earlier (e.g. `dit-chain prefill`) for
    /// reading only: the descriptor is read-only and the write budget is zero.
    ///
    /// # Errors
    ///
    /// Returns [`BackendError`] if the path is missing, a symlink or not a regular file, or its
    /// length is not a multiple of 4 KiB.
    pub fn open_existing_read_only(path: impl AsRef<Path>) -> Result<Self, BackendError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT | libc::O_NOFOLLOW)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(BackendError::UnsafeTarget(path));
        }
        let capacity_bytes = metadata.len();
        if !capacity_bytes.is_multiple_of(4096) {
            return Err(BackendError::Unaligned {
                offset: 0,
                length: usize::try_from(capacity_bytes).unwrap_or(usize::MAX),
                alignment: 4096,
            });
        }
        Ok(Self {
            file,
            id: format!("preserving-file:{}", path.display()),
            path,
            capacity_bytes,
            max_host_write_bytes: 0,
            host_bytes_written: 0,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn check_alignment(offset: u64, length: usize) -> Result<(), BackendError> {
        if !offset.is_multiple_of(4096) || !length.is_multiple_of(4096) {
            return Err(BackendError::Unaligned {
                offset,
                length,
                alignment: 4096,
            });
        }
        Ok(())
    }
}

impl StorageBackend for DirectFileBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<(), BackendError> {
        check_range(offset, data.len(), self.capacity_bytes)?;
        Self::check_alignment(offset, data.len())?;
        let attempted = u64::try_from(data.len()).unwrap_or(u64::MAX);
        let remaining = self
            .max_host_write_bytes
            .saturating_sub(self.host_bytes_written);
        if attempted > remaining {
            return Err(BackendError::WriteBudgetExceeded {
                attempted,
                remaining,
            });
        }
        let mut aligned = MmapMut::map_anon(data.len())?;
        aligned.copy_from_slice(data);
        let written = self.file.write_at(&aligned, offset)?;
        if written != data.len() {
            return Err(BackendError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("short direct file write: {written}/{}", data.len()),
            )));
        }
        self.host_bytes_written += attempted;
        Ok(())
    }

    fn read_exact_at(&mut self, offset: u64, data: &mut [u8]) -> Result<(), BackendError> {
        check_range(offset, data.len(), self.capacity_bytes)?;
        Self::check_alignment(offset, data.len())?;
        let mut aligned = MmapMut::map_anon(data.len())?;
        let read = self.file.read_at(&mut aligned, offset)?;
        if read != data.len() {
            return Err(BackendError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("short direct file read: {read}/{}", data.len()),
            )));
        }
        data.copy_from_slice(&aligned);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.file.sync_data()?;
        Ok(())
    }

    fn host_bytes_written(&self) -> u64 {
        self.host_bytes_written
    }
}

impl StorageBackend for FileBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<(), BackendError> {
        check_range(offset, data.len(), self.capacity_bytes)?;
        self.file.write_all_at(data, offset)?;
        self.host_bytes_written = self
            .host_bytes_written
            .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
        Ok(())
    }

    fn read_exact_at(&mut self, offset: u64, data: &mut [u8]) -> Result<(), BackendError> {
        check_range(offset, data.len(), self.capacity_bytes)?;
        self.file.read_exact_at(data, offset)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.file.sync_data()?;
        Ok(())
    }

    fn host_bytes_written(&self) -> u64 {
        self.host_bytes_written
    }
}

#[derive(Clone, Debug)]
pub struct MemoryBackend {
    id: String,
    bytes: Vec<u8>,
    host_bytes_written: u64,
}

impl MemoryBackend {
    #[must_use]
    pub fn new(id: impl Into<String>, capacity_bytes: usize) -> Self {
        Self {
            id: id.into(),
            bytes: vec![0; capacity_bytes],
            host_bytes_written: 0,
        }
    }
}

impl StorageBackend for MemoryBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<(), BackendError> {
        check_range(offset, data.len(), self.capacity_bytes())?;
        let start = usize::try_from(offset).expect("range checked against usize vector");
        self.bytes[start..start + data.len()].copy_from_slice(data);
        self.host_bytes_written = self
            .host_bytes_written
            .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
        Ok(())
    }

    fn read_exact_at(&mut self, offset: u64, data: &mut [u8]) -> Result<(), BackendError> {
        check_range(offset, data.len(), self.capacity_bytes())?;
        let start = usize::try_from(offset).expect("range checked against usize vector");
        data.copy_from_slice(&self.bytes[start..start + data.len()]);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        Ok(())
    }

    fn host_bytes_written(&self) -> u64 {
        self.host_bytes_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_round_trip_and_range_guard() {
        let mut backend = MemoryBackend::new("memory:test", 8192);
        let input = vec![0x5a; 4096];
        backend.write_all_at(4096, &input).unwrap();
        let mut output = vec![0; 4096];
        backend.read_exact_at(4096, &mut output).unwrap();
        assert_eq!(output, input);
        assert!(matches!(
            backend.write_all_at(4097, &input),
            Err(BackendError::OutOfRange { .. })
        ));
    }

    #[test]
    fn file_backend_never_overwrites_an_existing_path() {
        let path = std::env::temp_dir().join(format!(
            "dit-nvme-backend-test-{}-{}.dit-sim",
            std::process::id(),
            1
        ));
        let _ = std::fs::remove_file(&path);
        {
            let mut backend = FileBackend::create(&path, 8192).unwrap();
            backend.write_all_at(0, &[7; 16]).unwrap();
        }
        assert!(matches!(
            FileBackend::create(&path, 8192),
            Err(BackendError::Exists(_))
        ));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn direct_file_budget_is_enforced_before_submission() {
        let path = std::env::temp_dir().join(format!(
            "dit-nvme-direct-budget-test-{}-{}.dit-test",
            std::process::id(),
            1
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = DirectFileBackend::create_new(&path, 1024 * 1024, 4096).unwrap();
        backend.write_all_at(0, &[7; 4096]).unwrap();
        assert!(matches!(
            backend.write_all_at(4096, &[8; 4096]),
            Err(BackendError::WriteBudgetExceeded { .. })
        ));
        drop(backend);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_file_written_in_order_grows_as_it_is_written() {
        let path = std::env::temp_dir().join(format!(
            "dit-nvme-direct-in-order-test-{}.dit-test",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend =
            DirectFileBackend::create_new_in_order(&path, 1024 * 1024, 1024 * 1024).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "not extended up front"
        );
        assert_eq!(backend.capacity_bytes(), 1024 * 1024);
        backend.write_all_at(0, &[5; 8192]).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8192);
        let mut data = [0_u8; 4096];
        backend.read_exact_at(4096, &mut data).unwrap();
        assert_eq!(data, [5; 4096]);
        drop(backend);
        std::fs::remove_file(path).unwrap();
        // A directory on ext4 or tmpfs does not zero-fill ahead of a write.
        assert!(!zero_fills_ahead(&std::env::temp_dir()));
    }

    #[test]
    fn an_existing_direct_file_opens_read_only() {
        let path = std::env::temp_dir().join(format!(
            "dit-nvme-direct-reopen-test-{}.dit-test",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut backend = DirectFileBackend::create_new(&path, 1024 * 1024, 8192).unwrap();
        backend.write_all_at(4096, &[9; 4096]).unwrap();
        drop(backend);
        let mut reopened = DirectFileBackend::open_existing_read_only(&path).unwrap();
        assert_eq!(reopened.capacity_bytes(), 1024 * 1024);
        let mut data = [0_u8; 4096];
        reopened.read_exact_at(4096, &mut data).unwrap();
        assert_eq!(data, [9; 4096]);
        assert!(matches!(
            reopened.write_all_at(0, &[1; 4096]),
            Err(BackendError::WriteBudgetExceeded { .. })
        ));
        drop(reopened);
        std::fs::remove_file(&path).unwrap();
        assert!(DirectFileBackend::open_existing_read_only(&path).is_err());
    }
}

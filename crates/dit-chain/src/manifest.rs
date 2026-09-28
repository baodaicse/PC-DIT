//! Manifest of a prefilled record file (`dit-chain-<session>.json` next to the `.bin`), written by
//! `dit-chain prefill` and read by `dit-chain run --existing`. It keeps the session, the shape and
//! every record's sealed header, so a later run knows what each record must contain without
//! trusting the file, plus a SHA-256 over those headers that refuses a damaged manifest.
use crate::MixArg;
use crate::record::RecordHeader;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const FORMAT: &str = "DITCHAN1";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    pub schema_version: u32,
    pub format: String,
    pub session: String,
    pub base_seed: u64,
    pub jobs: usize,
    pub inner: u32,
    pub arith: MixArg,
    pub records: u64,
    pub stride_bytes: u64,
    /// Hex of each record's sealed header, in record order.
    pub headers: Vec<String>,
    pub headers_sha256: String,
    pub prefill_bytes_written: u64,
    pub prefill_seconds: f64,
    pub oracle_groups: (Vec<usize>, Vec<usize>),
    pub oracle_disagreements: Vec<String>,
}

/// Where the manifest of record file `file` lives.
#[must_use]
pub fn path_for(file: &Path) -> PathBuf {
    file.with_extension("json")
}

fn digest(headers: &[String]) -> String {
    let mut sha = Sha256::new();
    for header in headers {
        sha.update(header.as_bytes());
    }
    hex::encode(sha.finalize())
}

impl Manifest {
    #[must_use]
    pub fn seal_headers(headers: &[RecordHeader]) -> (Vec<String>, String) {
        let sealed: Vec<String> = headers.iter().map(|h| hex::encode(h.sealed())).collect();
        let sha = digest(&sealed);
        (sealed, sha)
    }

    /// The session number and every header, after checking the format, the digest and that each
    /// header names this session and its own index.
    /// # Errors
    /// A description of the first problem found.
    pub fn open(&self) -> Result<(u64, Vec<RecordHeader>), String> {
        if self.format != FORMAT || self.schema_version != 1 {
            return Err(format!(
                "unsupported manifest {} v{}",
                self.format, self.schema_version
            ));
        }
        if digest(&self.headers) != self.headers_sha256 {
            return Err("manifest header digest mismatch".into());
        }
        if self.headers.len() as u64 != self.records || self.records == 0 {
            return Err("manifest record count mismatch".into());
        }
        let session = u64::from_str_radix(&self.session, 16)
            .map_err(|_| format!("bad session {}", self.session))?;
        let mut headers = Vec::with_capacity(self.headers.len());
        for (index, text) in self.headers.iter().enumerate() {
            let bytes = hex::decode(text).map_err(|_| format!("header {index}: not hex"))?;
            let header = RecordHeader::from_sealed(&bytes)
                .map_err(|error| format!("header {index}: {error}"))?;
            if header.session != session || header.index != index as u64 {
                return Err(format!("header {index}: wrong session or index"));
            }
            headers.push(header);
        }
        Ok((session, headers))
    }

    /// # Errors
    /// Reading or parsing failures.
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes =
            std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
        serde_json::from_slice(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
    }

    /// Writes the manifest, refusing to replace an existing file.
    /// # Errors
    /// Creation or writing failures.
    pub fn store(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| format!("create {}: {error}", path.display()))?;
        std::io::Write::write_all(&mut file, &json)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("write {}: {error}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpu_engine::gemm::{Arith, Corpus};

    fn manifest(headers: &[RecordHeader]) -> Manifest {
        let (sealed, sha) = Manifest::seal_headers(headers);
        Manifest {
            schema_version: 1,
            format: FORMAT.into(),
            session: format!("{:016x}", 0x77_u64),
            base_seed: 3,
            jobs: 1,
            inner: 16,
            arith: MixArg::Alternate,
            records: headers.len() as u64,
            stride_bytes: 8192,
            headers: sealed,
            headers_sha256: sha,
            prefill_bytes_written: 0,
            prefill_seconds: 0.0,
            oracle_groups: (vec![0], vec![1]),
            oracle_disagreements: Vec::new(),
        }
    }

    #[test]
    fn a_manifest_round_trips_and_refuses_damage() {
        let headers: Vec<RecordHeader> = (0..3)
            .map(|index| {
                let corpus = Corpus::new_arith(Arith::U32, index, 1, 16, 1).unwrap();
                RecordHeader::for_corpus(&corpus, 0x77, index)
            })
            .collect();
        let good = manifest(&headers);
        let dir = std::env::temp_dir().join(format!("dit-chain-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.json");
        let _ = std::fs::remove_file(&path);
        good.store(&path).unwrap();
        assert!(good.store(&path).is_err(), "never replaces a manifest");
        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.open().unwrap(), (0x77, headers.clone()));
        std::fs::remove_dir_all(dir).unwrap();

        let mut edited = good.clone();
        edited.headers[1] = edited.headers[2].clone();
        assert!(edited.open().unwrap_err().contains("digest"));
        let mut resealed = edited.clone();
        resealed.headers_sha256 = digest(&resealed.headers);
        assert!(
            resealed
                .open()
                .unwrap_err()
                .contains("wrong session or index")
        );
        let mut other = good;
        other.session = format!("{:016x}", 0x78_u64);
        assert!(other.open().is_err());
    }
}

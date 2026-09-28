use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{DitError, RunConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub schema_version: u32,
    pub source_id: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct VerifiedSource {
    manifest: SourceManifest,
    bytes: Vec<u8>,
}

impl VerifiedSource {
    #[must_use]
    pub fn manifest(&self) -> &SourceManifest {
        &self.manifest
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldenVector {
    pub schema_version: u32,
    pub test_id: String,
    pub descriptor_version: u32,
    pub source_sha256: String,
    pub seed_hex: String,
    pub target_bytes: u64,
    pub block_sizes: Vec<u64>,
    pub expected_generated_sha256: String,
    pub expected_encoded_sha256: String,
    pub expected_final_sha256: String,
}

impl GoldenVector {
    /// Convert this golden vector into a validated runtime configuration.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error for an unsupported schema,
    /// malformed seed, or invalid runtime limit.
    pub fn to_config(&self) -> Result<RunConfig, DitError> {
        if self.schema_version != 1 {
            return Err(DitError::InvalidConfig(format!(
                "unsupported golden schema version {}",
                self.schema_version
            )));
        }

        let seed_text = self
            .seed_hex
            .strip_prefix("0x")
            .unwrap_or(self.seed_hex.as_str());
        let seed = u64::from_str_radix(seed_text, 16).map_err(|error| {
            DitError::InvalidConfig(format!("invalid seed_hex {}: {error}", self.seed_hex))
        })?;

        RunConfig::new(
            self.test_id.clone(),
            self.descriptor_version,
            self.target_bytes,
            seed,
            self.block_sizes.clone(),
        )
    }

    pub(crate) fn validate_hashes(&self) -> Result<(), DitError> {
        for (name, value) in [
            ("source_sha256", &self.source_sha256),
            ("expected_generated_sha256", &self.expected_generated_sha256),
            ("expected_encoded_sha256", &self.expected_encoded_sha256),
            ("expected_final_sha256", &self.expected_final_sha256),
        ] {
            if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(DitError::InvalidConfig(format!(
                    "{name} must be a 64-character hexadecimal SHA-256"
                )));
            }
        }
        Ok(())
    }
}

/// Load a source and verify its size and SHA-256 against its manifest.
///
/// # Errors
///
/// Returns an I/O/JSON error when an artifact cannot be read or parsed, or an
/// invalid-source error when source evidence does not match the manifest.
pub fn load_verified_source(
    source_path: impl AsRef<Path>,
    manifest_path: impl AsRef<Path>,
) -> Result<VerifiedSource, DitError> {
    let source_path = source_path.as_ref();
    let manifest_path = manifest_path.as_ref();
    let manifest_bytes = fs::read(manifest_path).map_err(|source| DitError::Read {
        path: manifest_path.to_path_buf(),
        source,
    })?;
    let manifest: SourceManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|source| DitError::Json {
            path: manifest_path.to_path_buf(),
            source,
        })?;

    if manifest.schema_version != 1 {
        return Err(DitError::InvalidSource(format!(
            "unsupported source manifest schema version {}",
            manifest.schema_version
        )));
    }

    let bytes = fs::read(source_path).map_err(|source| DitError::Read {
        path: source_path.to_path_buf(),
        source,
    })?;
    if bytes.is_empty() {
        return Err(DitError::InvalidSource("source is empty".to_owned()));
    }

    let actual_size = u64::try_from(bytes.len())
        .map_err(|_| DitError::InvalidSource("source size does not fit u64".to_owned()))?;
    if actual_size != manifest.size_bytes {
        return Err(DitError::InvalidSource(format!(
            "size mismatch: expected {}, actual {actual_size}",
            manifest.size_bytes
        )));
    }

    let actual_sha256 = hex::encode(Sha256::digest(&bytes));
    if !actual_sha256.eq_ignore_ascii_case(&manifest.sha256) {
        return Err(DitError::InvalidSource(format!(
            "SHA-256 mismatch: expected {}, actual {actual_sha256}",
            manifest.sha256
        )));
    }

    Ok(VerifiedSource { manifest, bytes })
}

/// Load and structurally validate an independent golden vector.
///
/// # Errors
///
/// Returns an I/O/JSON error or an invalid-configuration error for malformed
/// hash fields.
pub fn load_golden_vector(path: impl AsRef<Path>) -> Result<GoldenVector, DitError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| DitError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let golden: GoldenVector = serde_json::from_slice(&bytes).map_err(|source| DitError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    golden.validate_hashes()?;
    Ok(golden)
}

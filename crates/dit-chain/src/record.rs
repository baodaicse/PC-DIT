//! `DITCHAN1` storage record: one corpus with its expected answers, as the chain writes it to the
//! SSD once and reads it back for the rest of the session.
//!
//! Layout (little endian): a 4 KiB header, then the input words, then the expected dot-product
//! words, zero-padded to the record stride (a multiple of 4 KiB, the `O_DIRECT` unit). The header
//! names the session, the record index and the corpus identity, carries the SHA-256 of the input
//! and of the answers as the corpus computed them, and ends with a SHA-256 of itself, so a record
//! read from the wrong place, from another session or with a damaged header is refused before
//! any of its data is used.
use gpu_engine::gemm::{Arith, Corpus, MAX_N};
use sha2::{Digest, Sha256};

pub const MAGIC: [u8; 8] = *b"DITCHAN1";
pub const HEADER_BYTES: usize = 4096;
const ALIGN: usize = 4096;
/// Bytes of the header covered by its own hash (everything before the hash).
const HASHED_BYTES: usize = 128;
/// The header's meaningful prefix: the hashed fields and their hash (the rest is zero padding).
pub const SEALED_BYTES: usize = HASHED_BYTES + 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordHeader {
    pub session: u64,
    pub index: u64,
    pub seed: u64,
    pub arith: Arith,
    pub jobs: u32,
    pub n: u32,
    pub k: u32,
    pub input_words: u64,
    pub dot_words: u64,
    /// SHA-256 of the input as `Corpus` computes it (arithmetic version tag included).
    pub input_sha: [u8; 32],
    /// SHA-256 of the expected dot products.
    pub dot_sha: [u8; 32],
}

#[derive(Debug, PartialEq, Eq)]
pub enum RecordError {
    Magic,
    HeaderHash,
    Identity(&'static str),
    Length,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Magic => write!(f, "not a DITCHAN1 record"),
            Self::HeaderHash => write!(f, "record header hash mismatch"),
            Self::Identity(what) => write!(f, "record identity mismatch: {what}"),
            Self::Length => write!(f, "record length does not match its header"),
        }
    }
}

fn arith_code(arith: Arith) -> u32 {
    match arith {
        Arith::U32 => 0,
        Arith::F32 => 1,
        Arith::I8 => 2,
    }
}

fn arith_from(code: u32) -> Option<Arith> {
    match code {
        0 => Some(Arith::U32),
        1 => Some(Arith::F32),
        2 => Some(Arith::I8),
        _ => None,
    }
}

fn hex32(text: &str) -> [u8; 32] {
    let mut out = [0_u8; 32];
    if let Ok(bytes) = hex::decode(text) {
        let len = bytes.len().min(32);
        out[..len].copy_from_slice(&bytes[..len]);
    }
    out
}

fn align(bytes: usize) -> usize {
    bytes.div_ceil(ALIGN) * ALIGN
}

/// Bytes every record occupies: the largest corpus of this shape (n = 65 for u32/FP32) plus the
/// header, so record `i` always starts at `i * stride`.
#[must_use]
pub fn stride(jobs: usize, k: usize) -> usize {
    let n = MAX_N;
    HEADER_BYTES + align((jobs * Corpus::input_words_per_job(Arith::U32, n, k) + jobs * n * n) * 4)
}

impl RecordHeader {
    #[must_use]
    pub fn for_corpus(corpus: &Corpus, session: u64, index: u64) -> Self {
        #[allow(clippy::cast_possible_truncation)]
        Self {
            session,
            index,
            seed: corpus.seed,
            arith: corpus.arith,
            jobs: corpus.jobs as u32,
            n: corpus.n as u32,
            k: corpus.k as u32,
            input_words: corpus.input.len() as u64,
            dot_words: corpus.dot.len() as u64,
            input_sha: hex32(&corpus.sha256),
            dot_sha: hex32(&corpus.dot_sha256),
        }
    }

    /// Whether `corpus` (e.g. rebuilt from the bytes read back) is the one this header names.
    #[must_use]
    pub fn matches(&self, corpus: &Corpus) -> bool {
        hex32(&corpus.sha256) == self.input_sha && hex32(&corpus.dot_sha256) == self.dot_sha
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0_u8; HEADER_BYTES];
        let mut at = 0;
        let mut put = |bytes: &[u8]| {
            out[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        put(&MAGIC);
        put(&self.session.to_le_bytes());
        put(&self.index.to_le_bytes());
        put(&self.seed.to_le_bytes());
        put(&arith_code(self.arith).to_le_bytes());
        put(&self.jobs.to_le_bytes());
        put(&self.n.to_le_bytes());
        put(&self.k.to_le_bytes());
        put(&self.input_words.to_le_bytes());
        put(&self.dot_words.to_le_bytes());
        put(&self.input_sha);
        put(&self.dot_sha);
        let digest = Sha256::digest(&out[..HASHED_BYTES]);
        out[HASHED_BYTES..HASHED_BYTES + 32].copy_from_slice(&digest);
        out
    }

    /// The sealed prefix ([`SEALED_BYTES`]) as a manifest keeps it.
    #[must_use]
    pub fn sealed(&self) -> Vec<u8> {
        self.encode()[..SEALED_BYTES].to_vec()
    }

    /// Header from a sealed prefix, with the same checks as [`Self::decode`].
    /// # Errors
    /// Refuses a prefix of the wrong length or one that fails [`Self::decode`].
    pub fn from_sealed(prefix: &[u8]) -> Result<Self, RecordError> {
        if prefix.len() != SEALED_BYTES {
            return Err(RecordError::Length);
        }
        let mut padded = vec![0_u8; HEADER_BYTES];
        padded[..SEALED_BYTES].copy_from_slice(prefix);
        Self::decode(&padded)
    }

    /// # Errors
    /// Refuses a wrong magic, a header whose own hash does not match, or an unknown arithmetic.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        if bytes.len() < HEADER_BYTES || bytes[..8] != MAGIC {
            return Err(RecordError::Magic);
        }
        if Sha256::digest(&bytes[..HASHED_BYTES])[..] != bytes[HASHED_BYTES..HASHED_BYTES + 32] {
            return Err(RecordError::HeaderHash);
        }
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or([0; 8]));
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or([0; 4]));
        let mut input_sha = [0_u8; 32];
        input_sha.copy_from_slice(&bytes[64..96]);
        let mut dot_sha = [0_u8; 32];
        dot_sha.copy_from_slice(&bytes[96..128]);
        Ok(Self {
            session: u64_at(8),
            index: u64_at(16),
            seed: u64_at(24),
            arith: arith_from(u32_at(32)).ok_or(RecordError::Identity("arithmetic"))?,
            jobs: u32_at(36),
            n: u32_at(40),
            k: u32_at(44),
            input_words: u64_at(48),
            dot_words: u64_at(56),
            input_sha,
            dot_sha,
        })
    }
}

/// The whole record for `corpus`, `stride` bytes long.
/// # Errors
/// Refuses a corpus that does not fit the stride.
pub fn encode(
    corpus: &Corpus,
    session: u64,
    index: u64,
    stride: usize,
) -> Result<Vec<u8>, RecordError> {
    let payload = (corpus.input.len() + corpus.dot.len()) * 4;
    if HEADER_BYTES + payload > stride {
        return Err(RecordError::Length);
    }
    let mut out = RecordHeader::for_corpus(corpus, session, index).encode();
    out.reserve(stride - HEADER_BYTES);
    for word in corpus.input.iter().chain(&corpus.dot) {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.resize(stride, 0);
    Ok(out)
}

/// Header, input words and answer words of a record read back, after checking that it is the
/// record expected here. The data hashes are checked by rebuilding the corpus (`Corpus::from_parts`)
/// and [`RecordHeader::matches`].
/// # Errors
/// Refuses a record of another session or index, or one whose lengths disagree with its header.
pub fn decode(
    bytes: &[u8],
    session: u64,
    index: u64,
) -> Result<(RecordHeader, Vec<u32>, Vec<u32>), RecordError> {
    let header = RecordHeader::decode(bytes)?;
    if header.session != session {
        return Err(RecordError::Identity("session"));
    }
    if header.index != index {
        return Err(RecordError::Identity("index"));
    }
    let input_words = usize::try_from(header.input_words).map_err(|_| RecordError::Length)?;
    let dot_words = usize::try_from(header.dot_words).map_err(|_| RecordError::Length)?;
    let end = HEADER_BYTES + (input_words + dot_words) * 4;
    if end > bytes.len() {
        return Err(RecordError::Length);
    }
    let words = |from: usize, to: usize| -> Vec<u32> {
        bytes[from..to]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect()
    };
    let split = HEADER_BYTES + input_words * 4;
    Ok((header, words(HEADER_BYTES, split), words(split, end)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_refuse_damage_or_the_wrong_place() {
        for arith in [Arith::U32, Arith::F32, Arith::I8] {
            let corpus = Corpus::new_arith(arith, 5, 2, 64, 1).unwrap();
            let stride = stride(2, 64);
            let bytes = encode(&corpus, 77, 3, stride).unwrap();
            assert_eq!(bytes.len(), stride);
            assert!(stride.is_multiple_of(4096));
            let (header, input, dot) = decode(&bytes, 77, 3).unwrap();
            let rebuilt = Corpus::from_parts(arith, header.seed, 2, 64, input, dot).unwrap();
            assert!(header.matches(&rebuilt));
            assert_eq!(header.arith, arith);
            assert_eq!(
                decode(&bytes, 78, 3).unwrap_err(),
                RecordError::Identity("session")
            );
            assert_eq!(
                decode(&bytes, 77, 4).unwrap_err(),
                RecordError::Identity("index")
            );
            let mut damaged_header = bytes.clone();
            damaged_header[20] ^= 1;
            assert_eq!(
                decode(&damaged_header, 77, 3).unwrap_err(),
                RecordError::HeaderHash
            );
            // Damaged data passes the header but not the corpus hashes.
            let mut damaged_data = bytes.clone();
            damaged_data[HEADER_BYTES + 5] ^= 0x10;
            let (header, input, dot) = decode(&damaged_data, 77, 3).unwrap();
            let rebuilt = Corpus::from_parts(arith, header.seed, 2, 64, input, dot).unwrap();
            assert!(!header.matches(&rebuilt));
            let sealed = header.sealed();
            assert_eq!(RecordHeader::from_sealed(&sealed).unwrap(), header);
            let mut damaged_seal = sealed.clone();
            damaged_seal[70] ^= 2;
            assert_eq!(
                RecordHeader::from_sealed(&damaged_seal).unwrap_err(),
                RecordError::HeaderHash
            );
        }
    }
}

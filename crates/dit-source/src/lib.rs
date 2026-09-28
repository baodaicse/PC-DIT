//! Deterministic 4 KiB integrity blocks used by the `NVMe` simulator and engine.

use sha2::{Digest, Sha256};
use thiserror::Error;

pub const BLOCK_SIZE: usize = 4096;
pub const HEADER_SIZE: usize = 128;
pub const PAYLOAD_SIZE: usize = BLOCK_SIZE - HEADER_SIZE;
pub const STRIPE_BLOCKS: u32 = 16;
pub const STRIPE_SIZE: usize = BLOCK_SIZE * STRIPE_BLOCKS as usize;

const MAGIC: &[u8; 8] = b"DITNVM01";
const FORMAT_VERSION: u16 = 1;
const DOMAIN_PAYLOAD: &[u8] = b"DIT-CPU-NVME/payload/v1";
const DOMAIN_TAG: &[u8] = b"DIT-CPU-NVME/tag/v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockContext {
    pub session_seed: u64,
    pub generation: u64,
    pub device_id: String,
    /// 4 KiB logical block address in the DIT data space.
    pub lba: u64,
    pub stripe_id: u64,
    pub stripe_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedHeader {
    pub session_seed: u64,
    pub generation: u64,
    pub device_hash: u64,
    pub lba: u64,
    pub stripe_id: u64,
    pub stripe_index: u32,
    pub payload_len: u32,
    pub payload_hash: [u8; 32],
    pub tag: [u8; 32],
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum VerifyError {
    #[error("block length is {actual}, expected {BLOCK_SIZE}")]
    Length { actual: usize },
    #[error("invalid DIT block magic")]
    Magic,
    #[error("unsupported block format version {0}")]
    Version(u16),
    #[error("invalid header or payload length")]
    Layout,
    #[error("context mismatch in {field}: expected {expected}, actual {actual}")]
    Context {
        field: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error("payload digest mismatch")]
    PayloadDigest,
    #[error("block authentication tag mismatch")]
    Tag,
    #[error(
        "exact byte mismatch at offset {offset}: expected {expected:#04x}, actual {actual:#04x}"
    )]
    ExactMismatch {
        offset: usize,
        expected: u8,
        actual: u8,
    },
}

#[must_use]
pub fn device_hash(device_id: &str) -> u64 {
    let digest = Sha256::digest(device_id.as_bytes());
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(bytes)
}

#[must_use]
pub fn build_block(context: &BlockContext) -> [u8; BLOCK_SIZE] {
    let mut block = [0_u8; BLOCK_SIZE];
    block[0..8].copy_from_slice(MAGIC);
    put_u16(&mut block, 8, FORMAT_VERSION);
    put_u16(&mut block, 10, 128);
    put_u32(&mut block, 12, 0);
    put_u64(&mut block, 16, context.session_seed);
    put_u64(&mut block, 24, context.generation);
    put_u64(&mut block, 32, device_hash(&context.device_id));
    put_u64(&mut block, 40, context.lba);
    put_u64(&mut block, 48, context.stripe_id);
    put_u32(&mut block, 56, context.stripe_index);
    put_u32(&mut block, 60, 3968);

    fill_payload(&mut block[HEADER_SIZE..], context);
    let payload_hash = Sha256::digest(&block[HEADER_SIZE..]);
    block[64..96].copy_from_slice(&payload_hash);
    let tag = compute_tag(&block);
    block[96..128].copy_from_slice(&tag);
    block
}

#[must_use]
pub fn build_stripe(base: &BlockContext) -> Vec<u8> {
    let mut stripe = Vec::with_capacity(STRIPE_SIZE);
    for stripe_index in 0..STRIPE_BLOCKS {
        let mut context = base.clone();
        context.lba = base.lba + u64::from(stripe_index);
        context.stripe_index = stripe_index;
        stripe.extend_from_slice(&build_block(&context));
    }
    stripe
}

/// Decodes and validates the structural header of one DIT block.
///
/// # Errors
///
/// Returns [`VerifyError`] when the block length, magic, version or layout is invalid.
pub fn decode_header(block: &[u8]) -> Result<DecodedHeader, VerifyError> {
    if block.len() != BLOCK_SIZE {
        return Err(VerifyError::Length {
            actual: block.len(),
        });
    }
    if &block[0..8] != MAGIC {
        return Err(VerifyError::Magic);
    }
    let version = get_u16(block, 8);
    if version != FORMAT_VERSION {
        return Err(VerifyError::Version(version));
    }
    if usize::from(get_u16(block, 10)) != HEADER_SIZE || get_u32(block, 60) as usize != PAYLOAD_SIZE
    {
        return Err(VerifyError::Layout);
    }
    Ok(DecodedHeader {
        session_seed: get_u64(block, 16),
        generation: get_u64(block, 24),
        device_hash: get_u64(block, 32),
        lba: get_u64(block, 40),
        stripe_id: get_u64(block, 48),
        stripe_index: get_u32(block, 56),
        payload_len: get_u32(block, 60),
        payload_hash: {
            let mut hash = [0_u8; 32];
            hash.copy_from_slice(&block[64..96]);
            hash
        },
        tag: {
            let mut tag = [0_u8; 32];
            tag.copy_from_slice(&block[96..128]);
            tag
        },
    })
}

/// Verifies identity, location, digest, tag and every byte against an independently rebuilt block.
///
/// # Errors
///
/// Returns [`VerifyError`] for any structural, contextual, digest, tag or exact-byte mismatch.
pub fn verify_block(block: &[u8], expected_context: &BlockContext) -> Result<(), VerifyError> {
    let header = decode_header(block)?;
    check_context(
        "session_seed",
        expected_context.session_seed,
        header.session_seed,
    )?;
    check_context("generation", expected_context.generation, header.generation)?;
    check_context(
        "device_hash",
        device_hash(&expected_context.device_id),
        header.device_hash,
    )?;
    check_context("lba", expected_context.lba, header.lba)?;
    check_context("stripe_id", expected_context.stripe_id, header.stripe_id)?;
    check_context(
        "stripe_index",
        u64::from(expected_context.stripe_index),
        u64::from(header.stripe_index),
    )?;

    let payload_hash = Sha256::digest(&block[HEADER_SIZE..]);
    if payload_hash.as_slice() != header.payload_hash {
        return Err(VerifyError::PayloadDigest);
    }
    if compute_tag(block) != header.tag {
        return Err(VerifyError::Tag);
    }

    let expected = build_block(expected_context);
    if let Some((offset, (&actual, &wanted))) = block
        .iter()
        .zip(expected.iter())
        .enumerate()
        .find(|(_, (actual, wanted))| actual != wanted)
    {
        return Err(VerifyError::ExactMismatch {
            offset,
            expected: wanted,
            actual,
        });
    }
    Ok(())
}

fn check_context(field: &'static str, expected: u64, actual: u64) -> Result<(), VerifyError> {
    if expected == actual {
        Ok(())
    } else {
        Err(VerifyError::Context {
            field,
            expected,
            actual,
        })
    }
}

fn fill_payload(payload: &mut [u8], context: &BlockContext) {
    for (counter, chunk) in payload.chunks_mut(32).enumerate() {
        let mut hasher = Sha256::new();
        hasher.update(DOMAIN_PAYLOAD);
        hasher.update(context.session_seed.to_le_bytes());
        hasher.update(context.generation.to_le_bytes());
        hasher.update(device_hash(&context.device_id).to_le_bytes());
        hasher.update(context.lba.to_le_bytes());
        hasher.update(context.stripe_id.to_le_bytes());
        hasher.update(context.stripe_index.to_le_bytes());
        hasher.update(
            u64::try_from(counter)
                .expect("4 KiB payload chunk count")
                .to_le_bytes(),
        );
        let digest = hasher.finalize();
        chunk.copy_from_slice(&digest[..chunk.len()]);
    }
}

fn compute_tag(block: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_TAG);
    hasher.update(&block[..96]);
    hasher.update([0_u8; 32]);
    hasher.update(&block[HEADER_SIZE..]);
    hasher.finalize().into()
}

fn put_u16(buffer: &mut [u8], offset: usize, value: u16) {
    buffer[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(buffer: &mut [u8], offset: usize, value: u64) {
    buffer[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn get_u16(buffer: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        buffer[offset..offset + 2]
            .try_into()
            .expect("checked block"),
    )
}

fn get_u32(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        buffer[offset..offset + 4]
            .try_into()
            .expect("checked block"),
    )
}

fn get_u64(buffer: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        buffer[offset..offset + 8]
            .try_into()
            .expect("checked block"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> BlockContext {
        BlockContext {
            session_seed: 0x0123_4567_89ab_cdef,
            generation: 7,
            device_id: "file:golden-device".into(),
            lba: 42,
            stripe_id: 2,
            stripe_index: 10,
        }
    }

    #[test]
    fn block_round_trip() {
        let context = context();
        let block = build_block(&context);
        verify_block(&block, &context).expect("valid block");
        assert_ne!(&block[HEADER_SIZE..], &[0_u8; PAYLOAD_SIZE]);
    }

    #[test]
    fn bit_flip_is_detected() {
        let context = context();
        let mut block = build_block(&context);
        block[777] ^= 0x40;
        assert_eq!(
            verify_block(&block, &context),
            Err(VerifyError::PayloadDigest)
        );
    }

    #[test]
    fn stale_generation_is_detected() {
        let expected = context();
        let mut stale = expected.clone();
        stale.generation -= 1;
        let block = build_block(&stale);
        assert!(matches!(
            verify_block(&block, &expected),
            Err(VerifyError::Context {
                field: "generation",
                ..
            })
        ));
    }

    #[test]
    fn stripe_binds_each_lba_and_index() {
        let mut base = context();
        base.stripe_index = 0;
        let stripe = build_stripe(&base);
        assert_eq!(stripe.len(), STRIPE_SIZE);
        for index in 0..STRIPE_BLOCKS {
            let mut expected = base.clone();
            expected.lba += u64::from(index);
            expected.stripe_index = index;
            let start = index as usize * BLOCK_SIZE;
            verify_block(&stripe[start..start + BLOCK_SIZE], &expected).unwrap();
        }
    }

    #[test]
    fn wrong_lba_is_detected() {
        let actual = context();
        let mut expected = actual.clone();
        expected.lba += 1;
        assert!(matches!(
            verify_block(&build_block(&actual), &expected),
            Err(VerifyError::Context { field: "lba", .. })
        ));
    }

    #[test]
    fn canonical_block_matches_golden_digest() {
        let digest = Sha256::digest(build_block(&context()));
        assert_eq!(
            digest.as_slice(),
            &[
                0x23, 0x9f, 0x61, 0x23, 0x27, 0x09, 0x48, 0x52, 0x4c, 0x30, 0x3d, 0x83, 0x81, 0x3b,
                0xe4, 0x58, 0x44, 0x4d, 0x44, 0x86, 0x64, 0xf2, 0xba, 0x4f, 0xa7, 0x51, 0xea, 0x51,
                0x9d, 0x74, 0xd8, 0x89,
            ]
        );
    }
}

const EXPANSION_MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

/// Expand a verified source corpus into one deterministic byte.
///
/// # Panics
///
/// Panics when `source` is empty. Source verification rejects empty corpora
/// before a DIT route starts.
#[must_use]
pub fn expanded_byte(source: &[u8], seed: u64, global_offset: u64) -> u8 {
    assert!(!source.is_empty(), "source must be verified and non-empty");
    let word_index = global_offset / 8;
    let lane = u32::try_from((global_offset % 8) * 8).expect("lane shift fits u32");
    let mixed = splitmix64(seed.wrapping_add(word_index.wrapping_mul(EXPANSION_MULTIPLIER)));
    let source_index = usize::try_from(global_offset % u64::try_from(source.len()).unwrap_or(1))
        .expect("source index fits usize");
    source[source_index] ^ u8::try_from((mixed >> lane) & 0xff).expect("masked byte fits u8")
}

/// Expand eight bytes starting at an arbitrary global byte offset.
///
/// # Panics
///
/// Panics when `source` is empty.
#[must_use]
pub fn expanded_word(source: &[u8], seed: u64, global_offset: u64) -> u64 {
    let bytes = std::array::from_fn(|index| {
        expanded_byte(
            source,
            seed,
            global_offset + u64::try_from(index).expect("word lane fits u64"),
        )
    });
    u64::from_le_bytes(bytes)
}

/// A verified source laid out for [`expand_into`]: the corpus, then its first seven bytes again
/// (wrapping around a corpus shorter than that), so the eight source bytes of any word starting
/// inside the corpus are contiguous.
#[derive(Debug, Clone)]
pub struct ExpansionSource {
    bytes: Vec<u8>,
    len: usize,
}

impl ExpansionSource {
    /// # Panics
    ///
    /// Panics when `source` is empty, like [`expanded_byte`].
    #[must_use]
    pub fn new(source: &[u8]) -> Self {
        assert!(!source.is_empty(), "source must be verified and non-empty");
        let len = source.len();
        Self {
            bytes: (0..len + 7).map(|index| source[index % len]).collect(),
            len,
        }
    }

    /// The corpus itself.
    #[must_use]
    pub fn source(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Writes the bytes [`expanded_byte`] gives for `global_offset..global_offset + out.len()` into
/// `out`, the same bytes, eight at a time: one `splitmix64` and one source read per 8-byte word
/// instead of a `splitmix64`, a division and a source lookup per byte. Offsets that do not start
/// or end on a word boundary are produced byte by byte.
///
/// # Panics
///
/// Panics when the range runs past `u64::MAX`, which no memory range can.
pub fn expand_into(source: &ExpansionSource, seed: u64, global_offset: u64, out: &mut [u8]) {
    let head = usize::try_from(global_offset.wrapping_neg() % 8)
        .expect("head fits usize")
        .min(out.len());
    let (head_bytes, body) = out.split_at_mut(head);
    for (index, byte) in head_bytes.iter_mut().enumerate() {
        *byte = expanded_byte(
            source.source(),
            seed,
            global_offset + u64::try_from(index).expect("head index fits u64"),
        );
    }
    let start = global_offset + u64::try_from(head).expect("head fits u64");
    let len = u64::try_from(source.len).expect("source length fits u64");
    let mut source_index = usize::try_from(start % len).expect("source index fits usize");
    let mut word_index = start / 8;
    let (words, tail) = body.as_chunks_mut::<8>();
    let tail_start = start + 8 * u64::try_from(words.len()).expect("word count fits u64");
    for word in words {
        let mixed = splitmix64(seed.wrapping_add(word_index.wrapping_mul(EXPANSION_MULTIPLIER)));
        let corpus = u64::from_le_bytes(
            source.bytes[source_index..source_index + 8]
                .try_into()
                .expect("eight source bytes"),
        );
        *word = (corpus ^ mixed).to_le_bytes();
        word_index = word_index.wrapping_add(1);
        source_index += 8;
        while source_index >= source.len {
            source_index -= source.len;
        }
    }
    for (index, byte) in tail.iter_mut().enumerate() {
        *byte = expanded_byte(
            source.source(),
            seed,
            tail_start + u64::try_from(index).expect("tail index fits u64"),
        );
    }
}

#[must_use]
pub(crate) fn splitmix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

pub(crate) const fn expansion_multiplier() -> u64 {
    EXPANSION_MULTIPLIER
}

#[cfg(test)]
mod tests {
    use super::{ExpansionSource, expand_into, expanded_byte, expanded_word, splitmix64};

    #[test]
    fn mixing_is_deterministic() {
        assert_eq!(splitmix64(123), splitmix64(123));
        assert_ne!(splitmix64(123), splitmix64(124));
    }

    #[test]
    fn word_matches_eight_expanded_bytes() {
        let source = b"source";
        let expected = std::array::from_fn(|index| {
            expanded_byte(source, 7, 13 + u64::try_from(index).expect("index"))
        });
        assert_eq!(expanded_word(source, 7, 13).to_le_bytes(), expected);
    }

    #[test]
    fn expand_into_matches_expanded_byte_at_every_alignment() {
        // Corpus lengths below, at and above one word, odd and even; ranges starting on every
        // lane, ending on every lane, shorter than a word, and crossing many corpus wraps.
        for corpus_len in [1_usize, 3, 7, 8, 9, 64, 451] {
            let corpus: Vec<u8> = (0..corpus_len)
                .map(|index| u8::try_from((index * 37 + 11) % 251).expect("byte"))
                .collect();
            let source = ExpansionSource::new(&corpus);
            assert_eq!(source.source(), corpus.as_slice());
            for start in [0_u64, 1, 5, 7, 8, 13, 4093, (1 << 40) + 3, u64::MAX - 600] {
                for length in [0_usize, 1, 7, 8, 9, 15, 16, 63, 517] {
                    let mut out = vec![0_u8; length];
                    expand_into(&source, 0x5eed ^ start, start, &mut out);
                    let reference: Vec<u8> = (0..length)
                        .map(|index| {
                            expanded_byte(
                                &corpus,
                                0x5eed ^ start,
                                start + u64::try_from(index).expect("index"),
                            )
                        })
                        .collect();
                    assert_eq!(
                        out, reference,
                        "corpus {corpus_len}, start {start}, len {length}"
                    );
                }
            }
        }
    }
}

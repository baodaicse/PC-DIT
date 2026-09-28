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
    use super::{expanded_byte, expanded_word, splitmix64};

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
}

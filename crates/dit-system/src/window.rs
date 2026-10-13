use std::collections::{BTreeMap, BTreeSet};

use dit_core::{ExpansionSource, expand_into, expanded_byte};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::memory::MemoryArena;

pub(crate) const WINDOW_RECIPE_ID: &str = "CPU-DIT-WINDOW-v1";
pub(crate) const PATTERN_RECIPE_ID: &str = "CPU-DIT-PATTERN-v1";
const RELEASE_RECIPE_MINIMUM_BYTES: u64 = 64 * 1024 * 1024;
const MIX_A: u64 = 0xbf58_476d_1ce4_e5b9;
const MIX_B: u64 = 0x94d0_49bb_1331_11eb;
const WINDOW_DOMAIN: u64 = 0x5749_4e44_4f57_2d31;

#[derive(Debug, Clone, Copy)]
struct SizeClass {
    id: &'static str,
    bytes: u64,
}

const SIZE_CLASSES: [SizeClass; 12] = [
    SizeClass {
        id: "W-4K-M3",
        bytes: 4_093,
    },
    SizeClass {
        id: "W-4K",
        bytes: 4_096,
    },
    SizeClass {
        id: "W-64K-M1",
        bytes: 65_535,
    },
    SizeClass {
        id: "W-64K",
        bytes: 65_536,
    },
    SizeClass {
        id: "W-64K-P1",
        bytes: 65_537,
    },
    SizeClass {
        id: "W-128K-M1",
        bytes: 131_071,
    },
    SizeClass {
        id: "W-128K",
        bytes: 131_072,
    },
    SizeClass {
        id: "W-128K-P1",
        bytes: 131_073,
    },
    SizeClass {
        id: "W-1M",
        bytes: 1_048_576,
    },
    SizeClass {
        id: "W-4M",
        bytes: 4_194_304,
    },
    SizeClass {
        id: "W-12M",
        bytes: 12_582_912,
    },
    SizeClass {
        id: "W-32M",
        bytes: 33_554_432,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PatternFamily {
    SourceExpanded,
    AllZero,
    AllOne,
    Checkerboard,
    CheckerboardInverse,
    WalkingOne,
    WalkingZero,
    AddressDerived,
}

impl PatternFamily {
    pub(crate) const ALL: [Self; 8] = [
        Self::SourceExpanded,
        Self::AllZero,
        Self::AllOne,
        Self::Checkerboard,
        Self::CheckerboardInverse,
        Self::WalkingOne,
        Self::WalkingZero,
        Self::AddressDerived,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::SourceExpanded => "source_expanded",
            Self::AllZero => "all_zero",
            Self::AllOne => "all_one",
            Self::Checkerboard => "checkerboard",
            Self::CheckerboardInverse => "checkerboard_inverse",
            Self::WalkingOne => "walking_one",
            Self::WalkingZero => "walking_zero",
            Self::AddressDerived => "address_derived",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WindowSpec {
    pub id: u64,
    pub arena_id: usize,
    pub arena_offset: usize,
    pub global_offset: u64,
    pub length: usize,
    pub class_id: &'static str,
    pub pattern: PatternFamily,
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // Planner invariants are checked and reported independently.
pub(crate) struct WindowPlan {
    pub by_arena: Vec<Vec<WindowSpec>>,
    pub sha256: String,
    pub size_class_counts: BTreeMap<String, u64>,
    pub size_class_bytes: BTreeMap<String, u64>,
    pub pattern_counts: BTreeMap<String, u64>,
    pub mandatory_recipe_applicable: bool,
    pub mandatory_recipe_met: bool,
    pub no_gaps: bool,
    pub no_overlaps: bool,
    pub tail_present: bool,
    pub total_windows: u64,
}

pub(crate) fn mandatory_size_classes() -> Vec<String> {
    SIZE_CLASSES
        .iter()
        .map(|class| class.id.to_owned())
        .collect()
}

pub(crate) fn mandatory_pattern_families() -> Vec<String> {
    PatternFamily::ALL
        .iter()
        .map(|family| family.as_str().to_owned())
        .collect()
}

pub(crate) fn window_sizes_bytes() -> Vec<u64> {
    SIZE_CLASSES.iter().map(|class| class.bytes).collect()
}

pub(crate) fn build_window_plan(arenas: &[MemoryArena], round_seed: u64) -> WindowPlan {
    let total_bytes: u64 = arenas
        .iter()
        .map(|arena| u64::try_from(arena.len()).unwrap_or(u64::MAX))
        .sum();
    let largest_arena = arenas.iter().map(MemoryArena::len).max().unwrap_or(0);
    let mandatory_recipe_applicable = total_bytes >= RELEASE_RECIPE_MINIMUM_BYTES
        && largest_arena
            >= usize::try_from(SIZE_CLASSES.last().expect("size recipe").bytes)
                .unwrap_or(usize::MAX);
    let mut by_arena: Vec<Vec<WindowSpec>> = (0..arenas.len()).map(|_| Vec::new()).collect();
    let mut used = vec![0_usize; arenas.len()];

    if mandatory_recipe_applicable {
        let mut descending: Vec<SizeClass> = SIZE_CLASSES.into_iter().collect();
        descending.sort_by_key(|class| std::cmp::Reverse(class.bytes));
        for (class_index, class) in descending.into_iter().enumerate() {
            let start = usize::try_from(mix64(
                round_seed ^ u64::try_from(class_index).unwrap_or(u64::MAX),
            ))
            .unwrap_or(0)
                % arenas.len();
            let selected = (0..arenas.len())
                .map(|delta| (start + delta) % arenas.len())
                .find(|&arena_id| {
                    arenas[arena_id].len().saturating_sub(used[arena_id])
                        >= usize::try_from(class.bytes).unwrap_or(usize::MAX)
                });
            if let Some(arena_id) = selected {
                push_window(
                    &mut by_arena,
                    &mut used,
                    arenas,
                    arena_id,
                    usize::try_from(class.bytes).expect("recipe size fits usize"),
                    class.id,
                );
            }
        }
    }

    for arena_id in 0..arenas.len() {
        let mut step = 0_u64;
        while used[arena_id] < arenas[arena_id].len() {
            let remaining = arenas[arena_id].len() - used[arena_id];
            let start = usize::try_from(mix64(
                round_seed ^ u64::try_from(arena_id).unwrap_or(u64::MAX).rotate_left(17) ^ step,
            ))
            .unwrap_or(0)
                % SIZE_CLASSES.len();
            let class = (0..SIZE_CLASSES.len())
                .map(|delta| SIZE_CLASSES[(start + delta) % SIZE_CLASSES.len()])
                .find(|class| usize::try_from(class.bytes).is_ok_and(|bytes| bytes <= remaining));
            if let Some(class) = class {
                push_window(
                    &mut by_arena,
                    &mut used,
                    arenas,
                    arena_id,
                    usize::try_from(class.bytes).expect("recipe size fits usize"),
                    class.id,
                );
            } else {
                push_window(
                    &mut by_arena,
                    &mut used,
                    arenas,
                    arena_id,
                    remaining,
                    "W-TAIL",
                );
            }
            step = step.wrapping_add(1);
        }
    }

    let mut next_id = 0_u64;
    let mut pattern_order = PatternFamily::ALL;
    deterministic_shuffle(&mut pattern_order, round_seed ^ WINDOW_DOMAIN);
    for windows in &mut by_arena {
        windows.sort_by_key(|window| window.arena_offset);
        for window in windows {
            window.id = next_id;
            let pattern_index =
                if usize::try_from(next_id).unwrap_or(usize::MAX) < pattern_order.len() {
                    usize::try_from(next_id).unwrap_or(0)
                } else {
                    usize::try_from(mix64(round_seed ^ next_id.wrapping_mul(WINDOW_DOMAIN)))
                        .unwrap_or(0)
                        % pattern_order.len()
                };
            window.pattern = pattern_order[pattern_index];
            next_id = next_id.wrapping_add(1);
        }
    }

    summarize_plan(arenas, by_arena, mandatory_recipe_applicable)
}

fn push_window(
    by_arena: &mut [Vec<WindowSpec>],
    used: &mut [usize],
    arenas: &[MemoryArena],
    arena_id: usize,
    length: usize,
    class_id: &'static str,
) {
    let arena_offset = used[arena_id];
    by_arena[arena_id].push(WindowSpec {
        id: 0,
        arena_id,
        arena_offset,
        global_offset: arenas[arena_id].global_offset
            + u64::try_from(arena_offset).unwrap_or(u64::MAX),
        length,
        class_id,
        pattern: PatternFamily::SourceExpanded,
    });
    used[arena_id] += length;
}

fn summarize_plan(
    arenas: &[MemoryArena],
    by_arena: Vec<Vec<WindowSpec>>,
    mandatory_recipe_applicable: bool,
) -> WindowPlan {
    let mut size_class_counts = BTreeMap::new();
    let mut size_class_bytes = BTreeMap::new();
    let mut pattern_counts = BTreeMap::new();
    let mut observed_sizes = BTreeSet::new();
    let mut observed_patterns = BTreeSet::new();
    let mut no_gaps = true;
    let mut no_overlaps = true;
    let mut tail_present = false;
    let mut hasher = Sha256::new();
    hasher.update(WINDOW_RECIPE_ID.as_bytes());
    let mut total_windows = 0_u64;

    for (arena_id, windows) in by_arena.iter().enumerate() {
        let mut expected_offset = 0_usize;
        for window in windows {
            no_gaps &= window.arena_offset <= expected_offset;
            no_overlaps &= window.arena_offset >= expected_offset;
            expected_offset = window.arena_offset.saturating_add(window.length);
            observed_sizes.insert(window.class_id);
            observed_patterns.insert(window.pattern.as_str());
            tail_present |= window.class_id == "W-TAIL";
            *size_class_counts
                .entry(window.class_id.to_owned())
                .or_insert(0) += 1;
            *size_class_bytes
                .entry(window.class_id.to_owned())
                .or_insert(0) += u64::try_from(window.length).unwrap_or(u64::MAX);
            *pattern_counts
                .entry(window.pattern.as_str().to_owned())
                .or_insert(0) += 1;
            hasher.update(window.id.to_le_bytes());
            hasher.update(
                u64::try_from(window.arena_id)
                    .unwrap_or(u64::MAX)
                    .to_le_bytes(),
            );
            hasher.update(
                u64::try_from(window.arena_offset)
                    .unwrap_or(u64::MAX)
                    .to_le_bytes(),
            );
            hasher.update(
                u64::try_from(window.length)
                    .unwrap_or(u64::MAX)
                    .to_le_bytes(),
            );
            hasher.update(window.class_id.as_bytes());
            hasher.update([0]);
            hasher.update(window.pattern.as_str().as_bytes());
            total_windows += 1;
        }
        no_gaps &= expected_offset == arenas[arena_id].len();
        no_overlaps &= expected_offset <= arenas[arena_id].len();
    }
    let mandatory_sizes_met = SIZE_CLASSES
        .iter()
        .all(|class| observed_sizes.contains(class.id));
    let mandatory_patterns_met = PatternFamily::ALL
        .iter()
        .all(|family| observed_patterns.contains(family.as_str()));
    let mandatory_recipe_met = !mandatory_recipe_applicable
        || (mandatory_sizes_met && mandatory_patterns_met && no_gaps && no_overlaps);

    WindowPlan {
        by_arena,
        sha256: hex::encode(hasher.finalize()),
        size_class_counts,
        size_class_bytes,
        pattern_counts,
        mandatory_recipe_applicable,
        mandatory_recipe_met,
        no_gaps,
        no_overlaps,
        tail_present,
        total_windows,
    }
}

/// The seed every byte of `window` derives from in `round_seed`'s round.
fn window_seed(round_seed: u64, window: &WindowSpec) -> u64 {
    mix64(
        round_seed
            ^ window.id.wrapping_mul(WINDOW_DOMAIN)
            ^ u64::try_from(window.pattern as usize).unwrap_or(0),
    )
}

/// The expected byte at `global_offset` inside `window`: the definition of `CPU-DIT-PATTERN-v1`.
/// [`WindowExpected`] produces the same bytes a range at a time; this one stays the reference the
/// tests hold it to.
#[cfg(test)]
pub(crate) fn expected_byte(
    source: &[u8],
    round_seed: u64,
    window: &WindowSpec,
    global_offset: u64,
) -> u8 {
    pattern_byte(
        source,
        window.pattern,
        window_seed(round_seed, window),
        global_offset,
    )
}

fn pattern_byte(source: &[u8], pattern: PatternFamily, window_seed: u64, global_offset: u64) -> u8 {
    match pattern {
        PatternFamily::SourceExpanded => expanded_byte(source, window_seed, global_offset),
        PatternFamily::AllZero => 0,
        PatternFamily::AllOne => u8::MAX,
        PatternFamily::Checkerboard => {
            if (global_offset ^ window_seed) & 1 == 0 {
                0xaa
            } else {
                0x55
            }
        }
        PatternFamily::CheckerboardInverse => {
            if (global_offset ^ window_seed) & 1 == 0 {
                0x55
            } else {
                0xaa
            }
        }
        PatternFamily::WalkingOne => {
            1_u8 << u32::try_from((global_offset ^ window_seed) & 7).unwrap_or(0)
        }
        PatternFamily::WalkingZero => {
            !(1_u8 << u32::try_from((global_offset ^ window_seed) & 7).unwrap_or(0))
        }
        PatternFamily::AddressDerived => address_derived_byte(window_seed, global_offset),
    }
}

fn address_derived_byte(window_seed: u64, global_offset: u64) -> u8 {
    u8::try_from(mix64(window_seed ^ global_offset) & 0xff).unwrap_or(0)
}

const LANE_OFFSETS: [u64; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// One window's expected bytes, a range at a time ([`Self::fill`]), byte for byte the same as
/// [`expected_byte`]. The window's seed is worked out once, not once per byte:
/// - `source_expanded`: one `splitmix64` per 8-byte word ([`expand_into`]);
/// - all-zero, all-one, checkerboard and walking patterns depend only on the offset modulo 8:
///   their eight bytes are worked out once and repeated;
/// - `address_derived` mixes every offset on its own, so it stays one `mix64` per byte.
pub(crate) struct WindowExpected<'a> {
    source: &'a ExpansionSource,
    pattern: PatternFamily,
    seed: u64,
    global_offset: u64,
    /// Periodic families: the byte at global offset `g` is `period[g % 8]`.
    period: [u8; 8],
}

impl<'a> WindowExpected<'a> {
    pub(crate) fn new(source: &'a ExpansionSource, round_seed: u64, window: &WindowSpec) -> Self {
        let seed = window_seed(round_seed, window);
        let period = std::array::from_fn(|lane| {
            pattern_byte(
                source.source(),
                window.pattern,
                seed,
                u64::try_from(lane).expect("lane fits u64"),
            )
        });
        Self {
            source,
            pattern: window.pattern,
            seed,
            global_offset: window.global_offset,
            period,
        }
    }

    /// Writes the expected bytes from `window_offset` bytes into the window on into `out`.
    pub(crate) fn fill(&self, window_offset: usize, out: &mut [u8]) {
        let start = self.global_offset + u64::try_from(window_offset).unwrap_or(u64::MAX);
        match self.pattern {
            PatternFamily::SourceExpanded => expand_into(self.source, self.seed, start, out),
            PatternFamily::AddressDerived => {
                // Eight independent mixes per step, so the core overlaps their multiplies.
                let (words, tail) = out.as_chunks_mut::<8>();
                let mut offset = start;
                for word in words {
                    *word = std::array::from_fn(|lane| {
                        address_derived_byte(self.seed, offset + LANE_OFFSETS[lane])
                    });
                    offset = offset.wrapping_add(8);
                }
                for (lane, byte) in tail.iter_mut().enumerate() {
                    *byte = address_derived_byte(self.seed, offset + LANE_OFFSETS[lane]);
                }
            }
            PatternFamily::AllZero
            | PatternFamily::AllOne
            | PatternFamily::Checkerboard
            | PatternFamily::CheckerboardInverse
            | PatternFamily::WalkingOne
            | PatternFamily::WalkingZero => {
                let phase = usize::try_from(start % 8).unwrap_or(0);
                let word: [u8; 8] = std::array::from_fn(|lane| self.period[(phase + lane) % 8]);
                let (words, tail) = out.as_chunks_mut::<8>();
                words.fill(word);
                tail.copy_from_slice(&word[..tail.len()]);
            }
        }
    }
}

fn deterministic_shuffle<T>(values: &mut [T], seed: u64) {
    let mut state = seed;
    for index in (1..values.len()).rev() {
        state = mix64(state.wrapping_add(u64::try_from(index).unwrap_or(u64::MAX)));
        let selected = usize::try_from(state).unwrap_or(0) % (index + 1);
        values.swap(index, selected);
    }
}

const fn mix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(MIX_A);
    value = (value ^ (value >> 27)).wrapping_mul(MIX_B);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use memmap2::MmapOptions;

    use dit_core::ExpansionSource;

    use super::{
        PatternFamily, WindowExpected, build_window_plan, expected_byte, mandatory_size_classes,
        summarize_plan,
    };
    use crate::memory::MemoryArena;

    fn arenas(lengths: &[usize]) -> Vec<MemoryArena> {
        let mut global_offset = 0_u64;
        lengths
            .iter()
            .enumerate()
            .map(|(id, &length)| {
                let arena = MemoryArena {
                    id,
                    global_offset,
                    target_numa_node: None,
                    map: MmapOptions::new()
                        .len(length)
                        .map_anon()
                        .expect("test mmap"),
                    first_touch_cpu: None,
                    last_writer_cpu: None,
                    verify_cpu: None,
                    lock_succeeded: false,
                    lock_error: None,
                    rounds_written: 0,
                    rounds_verified: 0,
                };
                global_offset += u64::try_from(length).expect("test length");
                arena
            })
            .collect()
    }

    #[test]
    fn release_plan_covers_every_byte_and_mandatory_recipe() {
        let arenas = arenas(&[32 * 1024 * 1024, 32 * 1024 * 1024]);
        let plan = build_window_plan(&arenas, 0x0123_4567_89ab_cdef);
        assert!(plan.mandatory_recipe_applicable);
        assert!(plan.mandatory_recipe_met);
        assert!(plan.no_gaps);
        assert!(plan.no_overlaps);
        assert!(
            mandatory_size_classes()
                .iter()
                .all(|class| plan.size_class_counts.contains_key(class))
        );
        assert_eq!(plan.pattern_counts.len(), PatternFamily::ALL.len());
        assert_eq!(
            plan.size_class_bytes.values().sum::<u64>(),
            64 * 1024 * 1024
        );
    }

    #[test]
    fn same_seed_replays_plan_and_new_seed_changes_it() {
        let arenas = arenas(&[32 * 1024 * 1024, 32 * 1024 * 1024]);
        let first = build_window_plan(&arenas, 7);
        let replay = build_window_plan(&arenas, 7);
        let rotated = build_window_plan(&arenas, 8);
        assert_eq!(first.sha256, replay.sha256);
        assert_ne!(first.sha256, rotated.sha256);
    }

    #[test]
    fn planner_validation_detects_gap_and_overlap() {
        let arenas = arenas(&[32 * 1024 * 1024, 32 * 1024 * 1024]);
        let plan = build_window_plan(&arenas, 9);

        let mut gap_windows = plan.by_arena.clone();
        gap_windows[0][0].arena_offset += 1;
        let gap = summarize_plan(&arenas, gap_windows, true);
        assert!(!gap.no_gaps);

        let mut overlap_windows = plan.by_arena;
        overlap_windows[0][1].arena_offset = 0;
        let overlap = summarize_plan(&arenas, overlap_windows, true);
        assert!(!overlap.no_overlaps);
    }

    #[test]
    fn small_development_target_is_covered_but_recipe_is_not_applicable() {
        let arenas = arenas(&[4 * 1024 * 1024 + 17]);
        let plan = build_window_plan(&arenas, 11);
        assert!(!plan.mandatory_recipe_applicable);
        assert!(plan.mandatory_recipe_met);
        assert!(plan.no_gaps);
        assert!(plan.no_overlaps);
        assert_eq!(
            plan.size_class_bytes.values().sum::<u64>(),
            4 * 1024 * 1024 + 17
        );
    }

    #[test]
    fn pattern_expected_bytes_are_deterministic_and_family_specific() {
        let arenas = arenas(&[64 * 1024]);
        let plan = build_window_plan(&arenas, 13);
        let source = b"verified-source";
        let values: Vec<u8> = plan.by_arena[0]
            .iter()
            .take(PatternFamily::ALL.len())
            .map(|window| expected_byte(source, 13, window, window.global_offset))
            .collect();
        let replay: Vec<u8> = plan.by_arena[0]
            .iter()
            .take(PatternFamily::ALL.len())
            .map(|window| expected_byte(source, 13, window, window.global_offset))
            .collect();
        assert_eq!(values, replay);
        assert!(values.windows(2).any(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn window_expected_matches_expected_byte_for_every_family_and_alignment() {
        // The second arena starts at an odd global offset, and 4093-byte windows shift the ones
        // after them: windows start on every lane of a word.
        let arenas = arenas(&[2 * 1024 * 1024 + 4093, 1024 * 1024 + 13]);
        let plan = build_window_plan(&arenas, 21);
        let corpus: Vec<u8> = (0..451_usize)
            .map(|index| u8::try_from((index * 37 + 11) % 251).expect("byte"))
            .collect();
        let source = ExpansionSource::new(&corpus);
        let windows: Vec<_> = plan.by_arena.iter().flatten().collect();
        let lanes: std::collections::BTreeSet<u64> = windows
            .iter()
            .map(|window| window.global_offset % 8)
            .collect();
        assert!(lanes.len() > 1, "windows start on several lanes: {lanes:?}");
        let mut checked = 0_usize;
        for window in windows.iter().filter(|window| window.length <= 131_073) {
            for family in PatternFamily::ALL {
                let mut window = (*window).clone();
                window.pattern = family;
                let expected = WindowExpected::new(&source, 21, &window);
                let length = window.length;
                // The whole window, a range starting and ending inside a word, the last bytes.
                for (offset, count) in [
                    (0, length),
                    (5.min(length), 517.min(length - 5.min(length))),
                    (length.saturating_sub(13), 13.min(length)),
                ] {
                    let mut out = vec![0_u8; count];
                    expected.fill(offset, &mut out);
                    let reference: Vec<u8> = (offset..offset + count)
                        .map(|index| {
                            expected_byte(
                                &corpus,
                                21,
                                &window,
                                window.global_offset + u64::try_from(index).expect("index"),
                            )
                        })
                        .collect();
                    assert_eq!(
                        out,
                        reference,
                        "{} window {} at {} (+{offset}, {count} bytes)",
                        family.as_str(),
                        window.id,
                        window.global_offset
                    );
                    checked += count;
                }
            }
        }
        assert!(checked > 1_000_000, "checked {checked} bytes");
    }

    /// Generation speed of each family (`cargo test --release -p dit-system --lib
    /// window_expected_speed -- --ignored --nocapture`).
    #[test]
    #[ignore = "measurement, run by hand in release"]
    fn window_expected_speed() {
        let arenas = arenas(&[64 * 1024 * 1024]);
        let plan = build_window_plan(&arenas, 5);
        let corpus: Vec<u8> = (0..451_usize)
            .map(|index| u8::try_from(index % 251).expect("byte"))
            .collect();
        let source = ExpansionSource::new(&corpus);
        let mut window = plan.by_arena[0][0].clone();
        window.length = 64 * 1024 * 1024;
        window.global_offset = 3;
        let mut out = vec![0_u8; 4096];
        for family in PatternFamily::ALL {
            window.pattern = family;
            let expected = WindowExpected::new(&source, 5, &window);
            let started = std::time::Instant::now();
            for offset in (0..window.length).step_by(out.len()) {
                expected.fill(offset, &mut out);
                std::hint::black_box(&out);
            }
            let seconds = started.elapsed().as_secs_f64();
            println!(
                "{:22} {:6.2} GB/s",
                family.as_str(),
                f64::from(u32::try_from(window.length).expect("length")) / seconds / 1e9
            );
        }
    }
}

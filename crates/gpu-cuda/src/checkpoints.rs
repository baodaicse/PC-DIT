//! GPU boundary checks of the async GEMM pipeline (`docs/COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md`,
//! step F3). The path of a batch is
//! `host staging --H2D--> VRAM input --GPU--> VRAM output --D2H--> host readback --CPU check`;
//! the CPU check alone cannot say where a wrong word came from. With the checks on:
//!
//! - **after every corpus upload** the GPU hashes the input and the answers it received and the
//!   CPU compares them with the hashes of what it sent (c2). A difference is hashed again (a GPU
//!   that hashes wrong is not a link fault), then uploaded again: clean the second time means the
//!   copy went wrong on the link (`h2d`), wrong again means link or VRAM (`h2d-persistent`);
//! - **after every GEMM** the GPU checks its own output against the answers and hashes it before
//!   the copy back (c3), and seals the result in a tail behind the output;
//! - **on a failed batch** the CPU checks the same bytes a second time and hashes them: wrong on
//!   the GPU already is `device`; right on the GPU but changed on the way is `d2h` (or `host-ram`
//!   when the host buffer cannot hold the right word); identical to what the GPU checked means the
//!   CPU's own expectation is wrong (`host`); passing the second time is `host` too;
//! - **after a burst with failures** the input and the answers are hashed again on the GPU: if
//!   they changed since the upload check, the VRAM holding them lost data (`vram-input`,
//!   `vram-answers`) and the corpus is uploaded again.
//!
//! The counts go into the burst's failure detail as `[boundary: kind=count …]`, which the suite's
//! fault ledger turns into component cells. With the checks off nothing here runs.
use crate::{
    CudaBackend, Result,
    driver::check,
    memory::{Buffer, Pinned},
};
use gpu_engine::gemm::{Corpus, MAX_N, boundary_hash_bytes, boundary_hash_threads};
use serde::Serialize;
use std::{ffi::c_void, fmt::Write as _, ptr};

/// Tail the GPU writes behind a batch output: `[bad words, seal, hash lo, hash hi]`.
pub(crate) const TAIL_WORDS: usize = 4;
/// Extra device and host bytes per output slot: alignment of the tail plus the tail.
pub(crate) const TAIL_RESERVE: usize = 8 + TAIL_WORDS * 4;
const SEAL: u32 = 0xB0DA_7A11;

/// Most 16x16 output tiles of one job (`MAX_N` = 65: 5 x 5).
const MAX_TILES: usize = MAX_N.div_ceil(16) * MAX_N.div_ceil(16);

/// Device-only bytes at the end of each output slot: the check's partial
/// `{bad, 0, hash lo, hash hi}` per block (one per tile and job) and the completion counter.
pub(crate) const fn partials_bytes(jobs: usize) -> usize {
    jobs * MAX_TILES * TAIL_WORDS * 4 + 16
}

/// Partials of the slot whose buffer is `len` bytes at `address` (a fixed place, whatever the
/// batch's output size).
pub(crate) const fn partials_address(address: u64, len: usize, jobs: usize) -> u64 {
    address + (len - partials_bytes(jobs)) as u64
}

/// The fused check's completion counter: the last 16 bytes of the slot.
pub(crate) const fn counter_address(address: u64, len: usize) -> u64 {
    address + (len - 16) as u64
}

/// Byte offset of the tail behind `bytes` of output (8-byte aligned for the 64-bit hash).
pub(crate) const fn tail_offset(bytes: usize) -> usize {
    bytes.next_multiple_of(8)
}

/// Bytes a batch copies back with the checks on.
pub(crate) const fn readback_len(bytes: usize) -> usize {
    tail_offset(bytes) + TAIL_WORDS * 4
}

/// What the GPU said about its own output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tail {
    pub bad: u32,
    pub hash: u64,
}

/// The tail of a readback when its seal holds for this generation (`None`: stale or damaged).
pub(crate) fn read_tail(readback: &[u8], bytes: usize, generation: u64) -> Option<Tail> {
    let tail = readback.get(tail_offset(bytes)..readback_len(bytes))?;
    let word = |i: usize| {
        u32::from_le_bytes([
            tail[i * 4],
            tail[i * 4 + 1],
            tail[i * 4 + 2],
            tail[i * 4 + 3],
        ])
    };
    #[allow(clippy::cast_possible_truncation)]
    let lo = generation as u32;
    let (bad, seal, hash_lo, hash_hi) = (word(0), word(1), word(2), word(3));
    (seal == SEAL ^ lo ^ bad ^ hash_lo ^ hash_hi).then_some(Tail {
        bad,
        hash: u64::from(hash_lo) | (u64::from(hash_hi) << 32),
    })
}

/// What a verifier worker gets about a batch from the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GpuCheck {
    /// Boundary checks off.
    Off,
    /// Checks on: the GPU's verdict, `None` when the tail's seal does not hold.
    On(Option<Tail>),
}

/// Where one failed batch went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Already wrong on the GPU (computed or written wrong, or its input changed at rest).
    Device,
    /// Right on the GPU, different on the host, and the host buffer holds the right word.
    D2h,
    /// Right on the GPU, different on the host, and the host buffer cannot hold the right word.
    HostRam,
    /// The host has exactly what the GPU checked, or the CPU's second check passed: the CPU side.
    Host,
    /// No valid tail (stale or damaged): nothing to go on.
    Unsealed,
}

/// Classifies a failed batch on the worker that found it (`bytes`: the output, `tail`: the
/// GPU's verdict, `still_wrong`: the CPU's second check of the same bytes failed too).
pub(crate) fn classify(bytes: &[u8], tail: Option<Tail>, still_wrong: bool) -> Kind {
    if !still_wrong {
        return Kind::Host;
    }
    match tail {
        None => Kind::Unsealed,
        Some(tail) if tail.bad > 0 => Kind::Device,
        Some(tail) if tail.hash == boundary_hash_bytes(bytes) => Kind::Host,
        Some(_) => Kind::D2h,
    }
}

/// Counts of one burst (or, summed, of the run).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BoundaryCounts {
    pub h2d: u64,
    pub h2d_persistent: u64,
    pub d2h: u64,
    pub device: u64,
    pub vram_input: u64,
    pub vram_answers: u64,
    pub host: u64,
    pub host_ram: u64,
    pub unsealed: u64,
}

impl BoundaryCounts {
    pub(crate) fn add_kind(&mut self, kind: Kind) {
        match kind {
            Kind::Device => self.device += 1,
            Kind::D2h => self.d2h += 1,
            Kind::HostRam => self.host_ram += 1,
            Kind::Host => self.host += 1,
            Kind::Unsealed => self.unsealed += 1,
        }
    }

    pub(crate) fn add(&mut self, other: &Self) {
        self.h2d += other.h2d;
        self.h2d_persistent += other.h2d_persistent;
        self.d2h += other.d2h;
        self.device += other.device;
        self.vram_input += other.vram_input;
        self.vram_answers += other.vram_answers;
        self.host += other.host;
        self.host_ram += other.host_ram;
        self.unsealed += other.unsealed;
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `[boundary: d2h=1 device=2]`: the non-zero counts, in the suite's tag format.
    #[must_use]
    pub fn tag(&self) -> String {
        let mut body = String::new();
        for (name, count) in [
            ("h2d", self.h2d),
            ("h2d-persistent", self.h2d_persistent),
            ("d2h", self.d2h),
            ("device", self.device),
            ("vram-input", self.vram_input),
            ("vram-answers", self.vram_answers),
            ("host", self.host),
            ("host-ram", self.host_ram),
        ] {
            if count > 0 {
                let _ = write!(
                    body,
                    "{}{name}={count}",
                    if body.is_empty() { "" } else { " " }
                );
            }
        }
        if body.is_empty() {
            String::new()
        } else {
            format!("[boundary: {body}]")
        }
    }
}

/// Run totals of the checks, in the async report (absent when the checks are off).
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct BoundaryReport {
    pub version: u32,
    /// Batches the GPU checked itself.
    pub batches_checked: u64,
    /// Batches the GPU found wrong while the CPU found them right (answers damaged, or a GPU that
    /// checks wrong); each triggers a check of the answers at rest.
    pub device_disagreements: u64,
    pub upload_checks: u64,
    pub at_rest_checks: u64,
    #[serde(flatten)]
    pub counts: BoundaryCounts,
}

/// Test-only injection (`DIT_GPU_BOUNDARY_FAULT`), each fired once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryFault {
    /// One input word wrong right after the first upload, as a bad copy would leave it.
    H2d,
    /// The same, after the second upload too.
    H2dPersistent,
    /// One input word changed after the upload check (VRAM at rest).
    VramInput,
    /// One answer word changed after the upload check.
    VramAnswers,
    /// One output word changed after the GPU's check, before the copy back.
    AfterCheck,
    /// One readback byte changed on the host after the copy landed.
    HostBuffer,
}

impl BoundaryFault {
    #[must_use]
    pub fn from_env() -> Option<Self> {
        match std::env::var("DIT_GPU_BOUNDARY_FAULT").ok()?.as_str() {
            "h2d" => Some(Self::H2d),
            "h2d-persistent" => Some(Self::H2dPersistent),
            "vram-input" => Some(Self::VramInput),
            "vram-answers" => Some(Self::VramAnswers),
            "after-check" => Some(Self::AfterCheck),
            "host-buffer" => Some(Self::HostBuffer),
            _ => None,
        }
    }
}

/// Result of checking an upload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Upload {
    Clean,
    /// Wrong on the device, right after a second upload.
    H2d,
    /// Wrong after both uploads.
    H2dPersistent,
    /// The GPU hashed the same data two different ways.
    Device,
}

/// Device buffers of the checks and the hashes of the corpus in use.
pub(crate) struct Checkpoints {
    answers: Buffer,
    answers_staging: Pinned,
    probe: Buffer,
    probe_host: Pinned,
    input_hash: u64,
    answers_hash: u64,
    input_words: usize,
    answers_words: usize,
    pub fault: Option<BoundaryFault>,
    pub report: BoundaryReport,
}

impl Checkpoints {
    /// Four counted allocation steps: answers, their staging, the probe and its readback.
    pub(crate) const STEPS: usize = 4;

    /// Allocates the check buffers and zeroes the completion counter of every output slot.
    pub(crate) fn new(
        backend: &CudaBackend,
        jobs: usize,
        slots: &[crate::async_compute::DeviceSlot],
    ) -> Result<Self> {
        crate::async_compute::clear_check_counters(backend, slots)?;
        let answers_bytes = jobs * MAX_N * MAX_N * 4;
        backend.alloc_step()?;
        let answers = Buffer::new(&backend.context, answers_bytes)?;
        backend.alloc_step()?;
        let answers_staging = Pinned::new(&backend.context, answers_bytes)?;
        backend.alloc_step()?;
        let probe = Buffer::new(&backend.context, 16)?;
        backend.alloc_step()?;
        let probe_host = Pinned::new(&backend.context, 16)?;
        Ok(Self {
            answers,
            answers_staging,
            probe,
            probe_host,
            input_hash: 0,
            answers_hash: 0,
            input_words: 0,
            answers_words: 0,
            fault: BoundaryFault::from_env(),
            report: BoundaryReport {
                version: 1,
                ..BoundaryReport::default()
            },
        })
    }

    pub(crate) const fn answers_address(&self) -> u64 {
        self.answers.address
    }

    pub(crate) fn device_bytes(&self) -> u64 {
        (self.answers.len + self.probe.len) as u64
    }

    pub(crate) fn pinned_bytes(&self) -> u64 {
        (self.answers_staging.len + self.probe_host.len) as u64
    }

    /// Takes the injected fault if it is `fault` (each fires once).
    pub(crate) fn fire(&mut self, fault: BoundaryFault) -> bool {
        let fired = self.fault == Some(fault);
        if fired {
            self.fault = None;
        }
        fired
    }

    /// Hash of `words` words at `address`, computed on the GPU (owner stream, waited).
    fn device_hash(&self, backend: &CudaBackend, address: u64, words: usize) -> Result<u64> {
        let driver = &backend.context.driver;
        check("boundary probe clear", unsafe {
            (driver.fill)(self.probe.address, 0, 2, backend.stream)
        })?;
        let mut words_arg = address;
        let mut count = words as u64;
        let mut out = self.probe.address;
        let mut params = [
            (&raw mut words_arg).cast::<c_void>(),
            (&raw mut count).cast(),
            (&raw mut out).cast(),
        ];
        let blocks = u32::try_from(words.div_ceil(256))
            .unwrap_or(u32::MAX)
            .clamp(1, backend.sm_count * 16);
        check("boundary hash", unsafe {
            (driver.launch)(
                backend.hash_function,
                blocks,
                1,
                1,
                256,
                1,
                1,
                0,
                backend.stream,
                params.as_mut_ptr(),
                ptr::null_mut(),
            )
        })?;
        check("boundary probe readback", unsafe {
            (driver.download)(
                self.probe_host.address,
                self.probe.address,
                8,
                backend.stream,
            )
        })?;
        crate::async_compute::wait_stream(backend, "boundary hash")?;
        // SAFETY: the stream is idle, so the 8-byte copy has landed; nothing else uses the probe.
        let bytes = unsafe { self.probe_host.completed_bytes(8) };
        Ok(u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])))
    }

    /// Overwrites one word on the device (injection only).
    fn corrupt(backend: &CudaBackend, address: u64, value: u32) -> Result<()> {
        check("boundary injection", unsafe {
            (backend.context.driver.fill)(address, value, 1, backend.stream)
        })?;
        crate::async_compute::wait_stream(backend, "boundary injection")
    }

    /// Hashes the corpus on the host (both parts) and stages the answers for upload.
    pub(crate) fn prepare(&mut self, corpus: &Corpus, threads: usize) {
        self.input_hash = boundary_hash_threads(&corpus.input, threads);
        self.answers_hash = boundary_hash_threads(&corpus.dot, threads);
        self.input_words = corpus.input.len();
        self.answers_words = corpus.dot.len();
        self.answers_staging.write_u32(&corpus.dot);
    }

    fn upload_answers(&self, backend: &CudaBackend) -> Result<()> {
        check("answers upload", unsafe {
            (backend.context.driver.upload)(
                self.answers.address,
                self.answers_staging.address,
                self.answers_words * 4,
                backend.stream,
            )
        })?;
        crate::async_compute::wait_stream(backend, "answers upload")
    }

    /// One part of the c2 protocol: `upload` copies it again from its staging.
    fn check_part(
        &mut self,
        backend: &CudaBackend,
        address: u64,
        words: usize,
        expected: u64,
        upload: &dyn Fn() -> Result<()>,
        persistent_fault: bool,
    ) -> Result<Upload> {
        let first = self.device_hash(backend, address, words)?;
        if first == expected {
            return Ok(Upload::Clean);
        }
        let second = self.device_hash(backend, address, words)?;
        if second != first {
            return Ok(Upload::Device);
        }
        upload()?;
        if persistent_fault {
            Self::corrupt(backend, address, 0xDEAD_BEEF)?;
        }
        let third = self.device_hash(backend, address, words)?;
        Ok(if third == expected {
            Upload::H2d
        } else {
            Upload::H2dPersistent
        })
    }

    /// c2 after the corpus input (already copied to `input` from `staging`) and the answers
    /// (copied here) reached the device. Leaves both right on the device when it can.
    pub(crate) fn check_upload(
        &mut self,
        backend: &CudaBackend,
        input: &Buffer,
        staging: &Pinned,
    ) -> Result<Vec<Upload>> {
        self.upload_answers(backend)?;
        self.report.upload_checks += 1;
        let persistent = self.fire(BoundaryFault::H2dPersistent);
        if persistent || self.fire(BoundaryFault::H2d) {
            Self::corrupt(backend, input.address, 0xDEAD_BEEF)?;
        }
        let input_words = self.input_words;
        let reupload_input = || {
            check("corpus re-upload", unsafe {
                (backend.context.driver.upload)(
                    input.address,
                    staging.address,
                    input_words * 4,
                    backend.stream,
                )
            })?;
            crate::async_compute::wait_stream(backend, "corpus re-upload")
        };
        let input_outcome = self.check_part(
            backend,
            input.address,
            self.input_words,
            self.input_hash,
            &reupload_input,
            persistent,
        )?;
        let answers = self.answers.address;
        let answers_outcome = {
            let (words, expected) = (self.answers_words, self.answers_hash);
            let staging_answers = self.answers_staging.address;
            let reupload_answers = || {
                check("answers re-upload", unsafe {
                    (backend.context.driver.upload)(
                        answers,
                        staging_answers,
                        words * 4,
                        backend.stream,
                    )
                })?;
                crate::async_compute::wait_stream(backend, "answers re-upload")
            };
            self.check_part(backend, answers, words, expected, &reupload_answers, false)?
        };
        if self.fire(BoundaryFault::VramInput) {
            Self::corrupt(backend, input.address, 0xDEAD_BEEF)?;
        }
        if self.fire(BoundaryFault::VramAnswers) {
            Self::corrupt(backend, answers, 0xDEAD_BEEF)?;
        }
        Ok(vec![input_outcome, answers_outcome])
    }

    /// Hashes the input and the answers again on the GPU, twice each; a part counts as changed
    /// only when both hashes agree with each other and not with the host's.
    pub(crate) fn changed_at_rest(
        &mut self,
        backend: &CudaBackend,
        input: &Buffer,
    ) -> Result<(bool, bool)> {
        self.report.at_rest_checks += 1;
        let mut changed = [false; 2];
        for (slot, (address, words, expected)) in [
            (input.address, self.input_words, self.input_hash),
            (self.answers.address, self.answers_words, self.answers_hash),
        ]
        .into_iter()
        .enumerate()
        {
            let first = self.device_hash(backend, address, words)?;
            let second = self.device_hash(backend, address, words)?;
            changed[slot] = first == second && first != expected;
        }
        Ok((changed[0], changed[1]))
    }
}

/// Counts of the upload outcomes, and whether the corpus is unusable as it stands.
pub(crate) fn upload_counts(outcomes: &[Upload]) -> (BoundaryCounts, bool) {
    let mut counts = BoundaryCounts::default();
    let mut broken = false;
    for outcome in outcomes {
        match outcome {
            Upload::Clean => {}
            Upload::H2d => counts.h2d += 1,
            Upload::H2dPersistent => {
                counts.h2d_persistent += 1;
                broken = true;
            }
            Upload::Device => {
                counts.device += 1;
                broken = true;
            }
        }
    }
    (counts, broken)
}

/// Whether the word at `index` of a host readback can hold `expected`: written and read back
/// (the batch is finished and its slot idle). A host RAM cell that cannot hold it is a RAM fault,
/// not a link fault.
pub(crate) fn host_holds(readback: &Pinned, index: usize, expected: u32) -> bool {
    if (index + 1) * 4 > readback.len {
        return true;
    }
    // SAFETY: no copy targets the slot until its verdict is consumed, and no worker reads it.
    unsafe {
        let word = readback.address.cast::<u32>().add(index);
        let before = ptr::read_volatile(word);
        ptr::write_volatile(word, expected);
        let held = ptr::read_volatile(word) == expected;
        ptr::write_volatile(word, before);
        held
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpu_engine::gemm::boundary_hash_bytes;

    #[allow(clippy::cast_possible_truncation)]
    fn readback(output: &[u8], bad: u32, hash: u64, generation: u64) -> Vec<u8> {
        let mut bytes = output.to_vec();
        bytes.resize(tail_offset(output.len()), 0);
        let (lo, hi) = (hash as u32, (hash >> 32) as u32);
        let seal = SEAL ^ (generation as u32) ^ bad ^ lo ^ hi;
        for word in [bad, seal, lo, hi] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn tails_are_read_only_when_sealed_for_the_generation() {
        let output = vec![7_u8; 12];
        let hash = boundary_hash_bytes(&output);
        let bytes = readback(&output, 0, hash, 5);
        assert_eq!(read_tail(&bytes, 12, 5), Some(Tail { bad: 0, hash }));
        assert_eq!(
            read_tail(&bytes, 12, 6),
            None,
            "stale tail of another batch"
        );
        let mut damaged = bytes.clone();
        damaged[tail_offset(12)] ^= 1;
        assert_eq!(read_tail(&damaged, 12, 5), None);
        assert_eq!(readback_len(12), 16 + 16);
    }

    #[test]
    fn failed_batches_are_placed_by_the_gpu_verdict_and_the_hashes() {
        let output = vec![3_u8; 64];
        let right = boundary_hash_bytes(&output);
        let good = Some(Tail {
            bad: 0,
            hash: right,
        });
        assert_eq!(
            classify(&output, Some(Tail { bad: 4, hash: 1 }), true),
            Kind::Device
        );
        assert_eq!(
            classify(&output, good, true),
            Kind::Host,
            "host has what the GPU checked"
        );
        assert_eq!(
            classify(
                &output,
                Some(Tail {
                    bad: 0,
                    hash: right ^ 1
                }),
                true
            ),
            Kind::D2h
        );
        assert_eq!(classify(&output, None, true), Kind::Unsealed);
        assert_eq!(
            classify(&output, good, false),
            Kind::Host,
            "second CPU check passed"
        );
    }

    #[test]
    fn counts_become_the_suite_tag() {
        let mut counts = BoundaryCounts::default();
        assert_eq!(counts.tag(), "");
        counts.add_kind(Kind::D2h);
        counts.add_kind(Kind::Device);
        counts.add_kind(Kind::Device);
        counts.add_kind(Kind::Unsealed);
        counts.vram_input = 1;
        assert_eq!(counts.tag(), "[boundary: d2h=1 device=2 vram-input=1]");
        let (upload, broken) = upload_counts(&[Upload::Clean, Upload::H2d]);
        assert_eq!((upload.h2d, broken), (1, false));
        assert!(upload_counts(&[Upload::H2dPersistent]).1);
    }
}

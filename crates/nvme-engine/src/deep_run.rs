use super::{
    BLOCK_SIZE, BTreeSet, CompletionReason, DeviceReport, EngineError, Event, EventEnvelope,
    FailureKind, FailureRecord, FaultKind, Instant, OperationSpec, PerformanceSummary, Phase,
    RunConfig, SCHEMA_VERSION, StorageBackend, Verdict, apply_deep_data_fault,
    backend_failure_kind, bytes_per_second,
    deep_pool::{BatchResult, Pool, Ticket},
    nanos, operation_contexts, record_failure, wall_time_ms,
};
use dit_contracts::DeepEvidence;

#[derive(Default)]
struct Counters {
    deep: DeepEvidence,
    completed: u64,
    written: u64,
    read: u64,
    verified: u64,
    failures: Vec<FailureRecord>,
    failure_count: u64,
    generations: BTreeSet<u64>,
    peak_read: f64,
    peak_write: f64,
    current_read: f64,
    current_write: f64,
}

impl Counters {
    fn fail(
        &mut self,
        config: &RunConfig,
        kind: FailureKind,
        operation: u64,
        offset: u64,
        detail: &str,
        emit: &mut impl FnMut(Event),
    ) {
        self.fail_confirmed(config, kind, operation, offset, detail, None, emit);
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_confirmed(
        &mut self,
        config: &RunConfig,
        kind: FailureKind,
        operation: u64,
        offset: u64,
        detail: &str,
        confirm: Option<&str>,
        emit: &mut impl FnMut(Event),
    ) {
        record_failure(
            &mut self.failures,
            &mut self.failure_count,
            config.max_failure_records,
            kind,
            operation,
            offset,
            detail,
            confirm,
            emit,
        );
    }

    fn compute(&mut self, result: &BatchResult, prepare: bool) {
        self.deep.compute_jobs_completed += result.jobs;
        self.deep.matrix_checks_completed += result.matrix_checks;
        if prepare {
            self.deep.prepare_cpu_ns = self.deep.prepare_cpu_ns.saturating_add(result.cpu_ns);
        } else {
            self.deep.verify_cpu_ns = self.deep.verify_cpu_ns.saturating_add(result.cpu_ns);
        }
    }

    fn batch_errors(
        &mut self,
        config: &RunConfig,
        operation: &OperationSpec,
        result: &BatchResult,
        emit: &mut impl FnMut(Event),
    ) {
        self.batch_errors_confirmed(config, operation, result, &[], emit);
    }

    /// `confirms[i]` is the confirmation of `result.errors[i]` (empty: none ran).
    fn batch_errors_confirmed(
        &mut self,
        config: &RunConfig,
        operation: &OperationSpec,
        result: &BatchResult,
        confirms: &[Option<&'static str>],
        emit: &mut impl FnMut(Event),
    ) {
        for (position, (block, error)) in result.errors.iter().enumerate() {
            self.fail_confirmed(
                config,
                if error.contains("panic") {
                    FailureKind::Internal
                } else {
                    FailureKind::DataMismatch
                },
                operation.index,
                operation.lba * BLOCK_SIZE as u64 + (*block * BLOCK_SIZE) as u64,
                error,
                confirms.get(position).copied().flatten(),
                emit,
            );
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn progress(
        &self,
        config: &RunConfig,
        started: Instant,
        stage: &str,
        emit: &mut impl FnMut(Event),
    ) {
        emit(Event::Progress {
            completed_operations: self.completed,
            target_operations: config.plan.target_operations,
            bytes_written: self.written,
            bytes_verified: self.verified,
            failures: self.failure_count,
        });
        let elapsed = started.elapsed().as_secs_f64().max(f64::EPSILON);
        emit(Event::Throughput {
            current_read_bytes_per_sec: self.current_read,
            current_write_bytes_per_sec: self.current_write,
            average_read_bytes_per_sec: self.read as f64 / elapsed,
            average_write_bytes_per_sec: self.written as f64 / elapsed,
            peak_read_bytes_per_sec: self.peak_read,
            peak_write_bytes_per_sec: self.peak_write,
        });
        emit(Event::DeepProgress {
            stage: stage.into(),
            evidence: self.deep.clone(),
        });
    }
}

/// Confirmations of every failed block of a batch (empty when confirmation is off).
fn confirmations<B: StorageBackend>(
    backend: &mut B,
    first: Option<&[u8]>,
    result: &BatchResult,
    operation: &OperationSpec,
    config: &RunConfig,
    offset: u64,
) -> Vec<Option<&'static str>> {
    let Some(first) = first.filter(|_| !result.errors.is_empty()) else {
        return Vec::new();
    };
    let contexts = operation_contexts(operation, &config.device_id);
    result
        .errors
        .iter()
        .map(|(block, error)| {
            (!error.contains("panic"))
                .then(|| {
                    confirm_block(
                        backend,
                        offset,
                        *block,
                        first,
                        &contexts,
                        config.matrix_rounds,
                    )
                })
                .flatten()
        })
        .collect()
}

/// Confirmation of a block that failed its check (fault ledger F4): the same bytes checked again
/// (right now: the first check computed wrong, `recheck_clean`), then the block read again from
/// the device (right: the first read went wrong on its way, `reread_clean`; the same wrong bytes:
/// the stored data is wrong, `reread_same`; other wrong bytes: an unstable read path,
/// `reread_differs`).
fn confirm_block<B: StorageBackend>(
    backend: &mut B,
    offset: u64,
    block: usize,
    first: &[u8],
    contexts: &[dit_source::BlockContext],
    rounds: u32,
) -> Option<&'static str> {
    let range = block * BLOCK_SIZE..(block + 1) * BLOCK_SIZE;
    let first_block = first.get(range)?;
    let context = contexts.get(block)?;
    if super::deep_block::verify(first_block, context, rounds, false, false, false).is_ok() {
        return Some("recheck_clean");
    }
    let mut again = vec![0; BLOCK_SIZE];
    backend
        .read_exact_at(offset + (block * BLOCK_SIZE) as u64, &mut again)
        .ok()?;
    Some(
        if super::deep_block::verify(&again, context, rounds, false, false, false).is_ok() {
            "reread_clean"
        } else if again == first_block {
            "reread_same"
        } else {
            "reread_differs"
        },
    )
}

fn expired(config: &RunConfig, started: Instant) -> bool {
    config
        .wall_clock_limit
        .is_some_and(|limit| started.elapsed() >= limit)
}

fn wait(
    pool: &mut Pool,
    ticket: &Ticket,
    counters: &mut Counters,
    stage: &str,
    emit: &mut impl FnMut(Event),
) -> Result<BatchResult, EngineError> {
    let started = Instant::now();
    let result = pool
        .wait(ticket, || {
            emit(Event::DeepProgress {
                stage: stage.into(),
                evidence: counters.deep.clone(),
            });
        })
        .map_err(EngineError::Configuration)?;
    counters.deep.compute_wait_ns = counters
        .deep
        .compute_wait_ns
        .saturating_add(nanos(started.elapsed()));
    Ok(result)
}

fn prepare(
    pool: &mut Pool,
    operation: &OperationSpec,
    config: &RunConfig,
) -> Result<Ticket, EngineError> {
    let fault = config
        .fault
        .filter(|fault| fault.at_operation == operation.index)
        .map(|fault| fault.kind);
    pool.prepare(
        operation_contexts(operation, &config.device_id),
        config.matrix_rounds,
        fault,
    )
    .map_err(EngineError::Configuration)
}

fn read_and_verify<B: StorageBackend>(
    backend: &mut B,
    pool: &mut Pool,
    config: &RunConfig,
    operation: &OperationSpec,
    counters: &mut Counters,
    soak: bool,
    emit: &mut impl FnMut(Event),
) -> Result<bool, EngineError> {
    let offset = operation.lba * BLOCK_SIZE as u64;
    emit(Event::Phase {
        phase: Phase::Reading,
        operation: counters.completed,
    });
    let mut buffer = vec![0; operation.transfer_size];
    let started = Instant::now();
    let read = backend.read_exact_at(offset, &mut buffer);
    let elapsed = started.elapsed();
    counters.deep.read_io_ns = counters.deep.read_io_ns.saturating_add(nanos(elapsed));
    if let Err(error) = read {
        counters.fail(
            config,
            backend_failure_kind(&error),
            counters.completed,
            offset,
            &format!("backend read failed: {error}"),
            emit,
        );
        return Ok(false);
    }
    counters.read += buffer.len() as u64;
    counters.current_read = bytes_per_second(buffer.len() as u64, elapsed);
    counters.peak_read = counters.peak_read.max(counters.current_read);
    let fault = config
        .fault
        .filter(|fault| {
            if soak {
                fault.kind == FaultKind::ReadSoakBitFlip
                    && fault.at_operation == counters.deep.read_soak_operations
            } else {
                fault.at_operation == operation.index
            }
        })
        .map(|fault| fault.kind);
    if fault == Some(FaultKind::ReadSoakBitFlip) {
        buffer[dit_source::HEADER_SIZE + 17] ^= 1;
    }
    if fault == Some(FaultKind::ShortRead) {
        counters.fail(
            config,
            FailureKind::ShortRead,
            counters.completed,
            offset,
            "injected short read",
            emit,
        );
        return Ok(false);
    }
    emit(Event::Phase {
        phase: Phase::Verifying,
        operation: counters.completed,
    });
    // Kept only for the confirmation of a block that fails (fault ledger F4).
    let first = config.confirm_on_fail.then(|| buffer.clone());
    let ticket = pool
        .verify(
            operation_contexts(operation, &config.device_id),
            buffer,
            config.matrix_rounds,
            fault,
        )
        .map_err(EngineError::Configuration)?;
    let result = wait(
        pool,
        &ticket,
        counters,
        if soak {
            "READ_SOAK_COMPUTE"
        } else {
            "READBACK_COMPUTE"
        },
        emit,
    )?;
    counters.compute(&result, false);
    let confirms = confirmations(
        backend,
        first.as_deref(),
        &result,
        operation,
        config,
        offset,
    );
    counters.batch_errors_confirmed(config, operation, &result, &confirms, emit);
    counters.verified += result.verified_blocks * BLOCK_SIZE as u64;
    counters.completed += 1;
    if soak {
        counters.deep.read_soak_operations += 1;
    } else {
        counters.deep.write_operations_completed += 1;
    }
    // A computational mismatch is recorded, but does not masquerade as an I/O stop.
    Ok(true)
}

/// The operation at `index`: the plan's, except that a file that must be written in order (FC6)
/// is first covered from its start, one largest batch after the other.
pub(super) fn operation_at(config: &RunConfig, index: u64, capacity: u64) -> OperationSpec {
    let mut operation = config.plan.operation(index, capacity);
    if config.sequential_first {
        let largest = config
            .plan
            .transfer_sizes
            .iter()
            .copied()
            .max()
            .unwrap_or(BLOCK_SIZE);
        let blocks = (largest / BLOCK_SIZE) as u64;
        let ordered = capacity / largest as u64;
        if index < ordered {
            operation.lba = index * blocks;
            operation.transfer_size = largest;
        }
    }
    operation
}

fn overlaps(a: &OperationSpec, b: &OperationSpec) -> bool {
    let a_start = a.lba * BLOCK_SIZE as u64;
    let b_start = b.lba * BLOCK_SIZE as u64;
    a_start < b_start + b.transfer_size as u64 && b_start < a_start + a.transfer_size as u64
}

fn validate(config: &RunConfig, capacity: u64) -> Result<(), EngineError> {
    if !(1..=64).contains(&config.compute_workers)
        || !(1..=32).contains(&config.matrix_rounds)
        || config.plan.seed_slots == 0
        || config.plan.target_operations == 0
        || config.plan.transfer_sizes.is_empty()
        || config
            .plan
            .transfer_sizes
            .iter()
            .any(|size| *size == 0 || *size > 1024 * 1024 || size % BLOCK_SIZE != 0)
        || (config.sustain_until_deadline
            && config.wall_clock_limit.is_none_or(|limit| limit.is_zero()))
    {
        return Err(EngineError::Configuration("workers 1..64, rounds 1..32, aligned batches <=1 MiB and positive sustained duration required".into()));
    }
    let required = *config.plan.transfer_sizes.iter().max().expect("nonempty");
    if capacity < required as u64 {
        return Err(EngineError::CapacityTooSmall {
            actual: capacity,
            required,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub(super) fn run<B: StorageBackend>(
    backend: &mut B,
    config: &RunConfig,
    mut event_sink: impl FnMut(EventEnvelope),
) -> Result<DeviceReport, EngineError> {
    validate(config, backend.capacity_bytes())?;
    let mut pool = Pool::new(config.compute_workers)
        .map_err(|error| EngineError::Configuration(error.to_string()))?;
    let started = Instant::now();
    let mut sequence = 0;
    let mut emit = |event| {
        event_sink(EventEnvelope::new(
            config.run_id.clone(),
            Some(config.device_id.clone()),
            sequence,
            nanos(started.elapsed()),
            wall_time_ms(),
            event,
        ));
        sequence += 1;
    };
    let mut counters = Counters {
        deep: DeepEvidence {
            pipeline_version: "deep-v1".into(),
            block_format: "DITDEEP2".into(),
            compute_workers: config.compute_workers,
            matrix_rounds: config.matrix_rounds,
            max_inflight_batches: 2,
            duration_target_ms: if config.sustain_until_deadline {
                config.wall_clock_limit.map_or(0, |limit| {
                    u64::try_from(limit.as_millis()).unwrap_or(u64::MAX)
                })
            } else {
                0
            },
            ..DeepEvidence::default()
        },
        ..Counters::default()
    };
    emit(Event::Hello {
        engine_version: env!("CARGO_PKG_VERSION").into(),
    });
    emit(Event::Inventory {
        model: "DIT Deep data path".into(),
        capacity_bytes: backend.capacity_bytes(),
        backend: backend.id().into(),
    });
    emit(Event::Selection {
        selected: vec![config.device_id.clone()],
    });
    counters.progress(config, started, "PREPARE", &mut emit);

    let first = operation_at(config, 0, backend.capacity_bytes());
    let first_ticket = prepare(&mut pool, &first, config)?;
    let mut pending = Some((first, first_ticket));
    let mut stored = Vec::<OperationSpec>::new();
    let mut io_stopped = false;
    while let Some((operation, ticket)) = pending.take() {
        let result = wait(&mut pool, &ticket, &mut counters, "PREPARE", &mut emit)?;
        counters.compute(&result, true);
        counters.batch_errors(config, &operation, &result, &mut emit);
        if !result.errors.is_empty() {
            io_stopped = true;
            break;
        }
        // Legacy finite zero-deadline diagnostics still execute one complete batch.
        if (operation.index > 0 || config.sustain_until_deadline) && expired(config, started) {
            break;
        }
        counters.generations.insert(operation.generation);
        let mut buffer = result.buffer;
        let fault = config
            .fault
            .filter(|fault| fault.at_operation == operation.index)
            .map(|fault| fault.kind);
        if let Some(kind) = fault {
            apply_deep_data_fault(
                kind,
                &operation_contexts(&operation, &config.device_id),
                &mut buffer,
                config.matrix_rounds,
            );
        }
        let offset = operation.lba * BLOCK_SIZE as u64;
        if let Some(kind @ (FaultKind::ShortWrite | FaultKind::IoError)) = fault {
            counters.fail(
                config,
                if kind == FaultKind::ShortWrite {
                    FailureKind::ShortWrite
                } else {
                    FailureKind::IoError
                },
                operation.index,
                offset,
                "injected write I/O failure",
                &mut emit,
            );
            io_stopped = true;
            break;
        }
        // Only CPU preparation is pipelined; disk regions remain owned by one I/O
        // coordinator until the current readback is verified. No overlapping writes.
        if operation.index + 1 < config.plan.target_operations && !expired(config, started) {
            let next = operation_at(config, operation.index + 1, backend.capacity_bytes());
            let next_ticket = prepare(&mut pool, &next, config)?;
            pending = Some((next, next_ticket));
        }
        stored.retain(|previous| !overlaps(previous, &operation));
        emit(Event::Phase {
            phase: Phase::Writing,
            operation: operation.index,
        });
        let io_started = Instant::now();
        let write = backend
            .write_all_at(offset, &buffer)
            .and_then(|()| backend.flush());
        let elapsed = io_started.elapsed();
        counters.deep.write_io_ns = counters.deep.write_io_ns.saturating_add(nanos(elapsed));
        if let Err(error) = write {
            counters.fail(
                config,
                backend_failure_kind(&error),
                operation.index,
                offset,
                &format!("backend write/flush failed: {error}"),
                &mut emit,
            );
            io_stopped = true;
            break;
        }
        counters.written += buffer.len() as u64;
        counters.current_write = bytes_per_second(buffer.len() as u64, elapsed);
        counters.peak_write = counters.peak_write.max(counters.current_write);
        drop(buffer);
        if let Some(kind @ (FaultKind::Timeout | FaultKind::DeviceDisappeared)) = fault {
            counters.fail(
                config,
                if kind == FaultKind::Timeout {
                    FailureKind::Timeout
                } else {
                    FailureKind::DeviceDisappeared
                },
                operation.index,
                offset,
                "injected read I/O failure",
                &mut emit,
            );
            io_stopped = true;
            break;
        }
        let before = counters.failure_count;
        if !read_and_verify(
            backend,
            &mut pool,
            config,
            &operation,
            &mut counters,
            false,
            &mut emit,
        )? {
            io_stopped = true;
            break;
        }
        if counters.failure_count == before {
            if stored.len() == 128 {
                stored.remove(0);
            }
            stored.push(operation);
        }
        counters.progress(config, started, "WRITE_READ_COMPUTE", &mut emit);
        if expired(config, started) {
            break;
        }
    }
    // Drain prefetched work on all normal/error exits. It never authorizes another write.
    if let Some((operation, ticket)) = pending {
        let result = wait(&mut pool, &ticket, &mut counters, "DRAIN", &mut emit)?;
        counters.compute(&result, true);
        counters.batch_errors(config, &operation, &result, &mut emit);
    }
    if config.sustain_until_deadline && !io_stopped && !stored.is_empty() {
        counters.current_write = 0.0;
        counters.progress(config, started, "READ_SOAK_COMPUTE", &mut emit);
        let mut cursor = 0;
        while !expired(config, started) {
            let operation = &stored[cursor];
            if !read_and_verify(
                backend,
                &mut pool,
                config,
                operation,
                &mut counters,
                true,
                &mut emit,
            )? {
                break;
            }
            counters.progress(config, started, "READ_SOAK_COMPUTE", &mut emit);
            cursor = (cursor + 1) % stored.len();
        }
    }
    drop(pool);
    let duration_reached = expired(config, started);
    counters.deep.duration_target_met = config.sustain_until_deadline && duration_reached;
    let completed_target =
        counters.deep.write_operations_completed == config.plan.target_operations;
    let verdict = if counters.failure_count == 0
        && counters.verified > 0
        && if config.sustain_until_deadline {
            duration_reached
        } else {
            completed_target || duration_reached
        } {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    emit(Event::Phase {
        phase: Phase::Finalizing,
        operation: counters.completed,
    });
    counters.current_read = 0.0;
    counters.current_write = 0.0;
    counters.progress(config, started, "COMPLETE", &mut emit);
    emit(Event::Completed {
        verdict,
        completed_operations: counters.completed,
        failures: counters.failure_count,
    });
    let elapsed_seconds = started.elapsed().as_secs_f64().max(f64::EPSILON);
    Ok(DeviceReport {
        schema_version: SCHEMA_VERSION,
        dit_version: dit_contracts::DIT_VERSION.to_owned(),
        run_id: config.run_id.clone(),
        module_id: dit_contracts::NVME_MODULE_ID.into(),
        device_id: config.device_id.clone(),
        verdict,
        target_operations: config.plan.target_operations,
        completed_operations: counters.completed,
        bytes_written: backend.host_bytes_written().max(counters.written),
        bytes_read: counters.read,
        bytes_verified: counters.verified,
        seed_slots: config.plan.seed_slots,
        generations: counters.generations.len() as u64,
        failure_count: counters.failure_count,
        failures: counters.failures,
        completion_reason: if verdict != Verdict::Pass {
            CompletionReason::Failure
        } else if duration_reached {
            CompletionReason::DurationReached
        } else {
            CompletionReason::OperationTarget
        },
        performance: PerformanceSummary {
            elapsed_seconds,
            average_read_bytes_per_sec: counters.read as f64 / elapsed_seconds,
            average_write_bytes_per_sec: counters.written as f64 / elapsed_seconds,
            peak_read_bytes_per_sec: counters.peak_read,
            peak_write_bytes_per_sec: counters.peak_write,
        },
        deep: Some(counters.deep),
    })
}

use std::{fs, path::PathBuf};

use dit_core::{
    FaultInjection, RunStatus, load_golden_vector, load_verified_source, run_reference,
};

fn vectors() -> (PathBuf, PathBuf, PathBuf) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    (
        root.join("vectors/source-v1.dat"),
        root.join("vectors/source-v1.manifest.json"),
        root.join("vectors/golden-v1.json"),
    )
}

#[test]
fn independent_golden_pipeline_passes() {
    let (source_path, manifest_path, golden_path) = vectors();
    let source = load_verified_source(source_path, manifest_path).expect("verified source");
    let golden = load_golden_vector(golden_path).expect("golden vector");
    let config = golden.to_config().expect("config");
    let report = run_reference(&source, &config, &golden, None).expect("run");
    assert_eq!(report.status, RunStatus::Pass);
    assert!(report.coverage.coverage_target_met);
    assert_eq!(
        report.coverage.exact_verified_unique_bytes,
        report.coverage.allocated_unique_bytes
    );
    assert_eq!(
        report.coverage.recovered_verified_bytes,
        report.coverage.requested_payload_bytes
    );
}

#[test]
fn readback_fault_cannot_pass() {
    let (source_path, manifest_path, golden_path) = vectors();
    let source = load_verified_source(source_path, manifest_path).expect("verified source");
    let golden = load_golden_vector(golden_path).expect("golden vector");
    let config = golden.to_config().expect("config");
    let report = run_reference(
        &source,
        &config,
        &golden,
        Some(FaultInjection::ReadbackByte {
            logical_block: 0,
            byte_offset: 17,
            xor_mask: 0x04,
        }),
    )
    .expect("run");
    assert_eq!(report.status, RunStatus::Fail);
    let mismatch = report.mismatch.expect("mismatch evidence");
    assert_eq!(mismatch.stage, "READBACK");
    assert_eq!(mismatch.xor_syndrome, 0x04);
}

#[test]
fn cpu_encode_fault_cannot_pass() {
    let (source_path, manifest_path, golden_path) = vectors();
    let source = load_verified_source(source_path, manifest_path).expect("verified source");
    let golden = load_golden_vector(golden_path).expect("golden vector");
    let config = golden.to_config().expect("config");
    let report = run_reference(
        &source,
        &config,
        &golden,
        Some(FaultInjection::EncodeByte {
            logical_block: 3,
            byte_offset: 31,
            xor_mask: 0x20,
        }),
    )
    .expect("run");
    assert_eq!(report.status, RunStatus::Fail);
    let mismatch = report.mismatch.expect("mismatch evidence");
    assert_eq!(mismatch.stage, "MATRIX_ENCODE");
    assert_eq!(mismatch.xor_syndrome, 0x20);
}

#[test]
fn recovered_join_fault_cannot_pass() {
    let (source_path, manifest_path, golden_path) = vectors();
    let source = load_verified_source(source_path, manifest_path).expect("verified source");
    let golden = load_golden_vector(golden_path).expect("golden vector");
    let config = golden.to_config().expect("config");
    let report = run_reference(
        &source,
        &config,
        &golden,
        Some(FaultInjection::RecoveredByte {
            global_offset: 999_999,
            xor_mask: 0x80,
        }),
    )
    .expect("run");
    assert_eq!(report.status, RunStatus::Fail);
    let mismatch = report.mismatch.expect("mismatch evidence");
    assert_eq!(mismatch.stage, "RECOVER_JOIN");
    assert_eq!(mismatch.global_payload_offset, Some(999_999));
    assert_eq!(mismatch.xor_syndrome, 0x80);
}

#[test]
fn corrupted_source_is_invalid() {
    let (source_path, manifest_path, _) = vectors();
    let mut corrupted = fs::read(source_path).expect("source fixture");
    corrupted[0] ^= 1;
    let temp_path = std::env::temp_dir().join(format!(
        "cpu-dit-01-corrupt-source-{}.dat",
        std::process::id()
    ));
    fs::write(&temp_path, corrupted).expect("write temporary source");
    let result = load_verified_source(&temp_path, manifest_path);
    fs::remove_file(temp_path).expect("remove temporary source");
    assert!(result.is_err());
}

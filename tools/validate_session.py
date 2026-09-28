#!/usr/bin/env python3
"""Validate one positive CPU-DIT live-USB session without modifying evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Any


ROUND_SEED_DOMAIN = b"CPU-DIT-ROUND-SEED-v1"
ROUND_SEED_DERIVATION = "sha256:CPU-DIT-ROUND-SEED-v1"
SEED_COMMITMENT_DOMAIN = b"CPU-DIT-SEED-COMMITMENT-v1"
WINDOW_RECIPE_ID = "CPU-DIT-WINDOW-v1"
PATTERN_RECIPE_ID = "CPU-DIT-PATTERN-v1"
WINDOW_SIZES = [
    4_093,
    4_096,
    65_535,
    65_536,
    65_537,
    131_071,
    131_072,
    131_073,
    1_048_576,
    4_194_304,
    12_582_912,
    33_554_432,
]
WINDOW_CLASSES = [
    "W-4K-M3",
    "W-4K",
    "W-64K-M1",
    "W-64K",
    "W-64K-P1",
    "W-128K-M1",
    "W-128K",
    "W-128K-P1",
    "W-1M",
    "W-4M",
    "W-12M",
    "W-32M",
]
PATTERN_FAMILIES = [
    "source_expanded",
    "all_zero",
    "all_one",
    "checkerboard",
    "checkerboard_inverse",
    "walking_one",
    "walking_zero",
    "address_derived",
]


def load_json(path: Path, errors: list[str]) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        errors.append(f"cannot read valid JSON {path.name}: {exc}")
        return {}
    if not isinstance(value, dict):
        errors.append(f"{path.name}: JSON root is not an object")
        return {}
    return value


def parse_key_values(path: Path, errors: list[str]) -> dict[str, str]:
    result: dict[str, str] = {}
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError) as exc:
        errors.append(f"cannot read {path.name}: {exc}")
        return result
    for line in lines:
        if "=" in line:
            key, value = line.split("=", 1)
            result[key.strip()] = value.strip()
    return result


def require(condition: bool, message: str, errors: list[str]) -> None:
    if not condition:
        errors.append(message)


def integer(value: Any) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) else None


def parse_seed_hex(value: Any) -> int | None:
    if not isinstance(value, str) or re.fullmatch(r"0x[0-9a-f]{16}", value) is None:
        return None
    return int(value[2:], 16)


def derive_round_seed(session_seed: int, test_id: str, round_index: int) -> int:
    digest = hashlib.sha256()
    digest.update(ROUND_SEED_DOMAIN)
    digest.update(b"\0")
    digest.update(test_id.encode("utf-8"))
    digest.update(b"\0")
    digest.update(session_seed.to_bytes(8, "little"))
    digest.update(round_index.to_bytes(4, "little"))
    return int.from_bytes(digest.digest()[:8], "little")


def seed_commitment(test_id: str, source_sha256: str, session_seed: int) -> str:
    digest = hashlib.sha256()
    digest.update(SEED_COMMITMENT_DOMAIN)
    digest.update(b"\0")
    digest.update(test_id.encode("utf-8"))
    digest.update(b"\0")
    digest.update(source_sha256.lower().encode("ascii"))
    digest.update(session_seed.to_bytes(8, "little"))
    return digest.hexdigest()


def validate_reference(report: dict[str, Any], errors: list[str]) -> None:
    require(report.get("status") == "PASS", "reference status is not PASS", errors)
    coverage = report.get("coverage", {})
    require(isinstance(coverage, dict), "reference coverage is missing", errors)
    if isinstance(coverage, dict):
        require(
            coverage.get("coverage_target_met") is True,
            "reference coverage target was not met",
            errors,
        )
        require(
            coverage.get("exact_verified_unique_bytes")
            == coverage.get("allocated_unique_bytes"),
            "reference exact verification does not cover allocation",
            errors,
        )
    checkpoints = report.get("checkpoints")
    require(
        isinstance(checkpoints, list) and bool(checkpoints),
        "reference checkpoints are missing",
        errors,
    )
    if isinstance(checkpoints, list):
        require(
            all(
                isinstance(item, dict) and item.get("matched") is True
                for item in checkpoints
            ),
            "one or more reference checkpoints mismatched",
            errors,
        )


def validate_system(report: dict[str, Any], profile: str | None, errors: list[str]) -> None:
    require(
        report.get("schema_version") in (9, 10),
        "system report is not schema version 9 or 10",
        errors,
    )
    require(report.get("status") == "PASS", "system status is not PASS", errors)
    if report.get("schema_version") == 10:
        require(report.get("availability", {}).get("status") == "COMPLETED",
                "availability is not COMPLETED", errors)
        oracle = report.get("compute_oracle", {})
        if oracle.get("enabled"):
            require(oracle.get("coverage_met") is True,
                    "compute oracle minimum work not met", errors)
    require(
        report.get("failure_reason") is None, "system failure_reason is set", errors
    )
    require(report.get("mismatch") is None, "system mismatch evidence is present", errors)
    require(
        integer(report.get("calculations_completed")) is not None
        and integer(report.get("calculations_completed")) > 0,
        "calculation completion counter is missing or empty",
        errors,
    )
    require(
        report.get("failures_total") == 0,
        "calculation failure counter is not zero",
        errors,
    )

    config = report.get("config", {})
    source = report.get("source", {})
    require(isinstance(config, dict), "system config evidence is missing", errors)
    require(isinstance(source, dict), "system source evidence is missing", errors)
    session_seed = None
    if isinstance(config, dict):
        session_seed = parse_seed_hex(config.get("seed_hex"))
        require(session_seed is not None, "session seed is not canonical 64-bit hex", errors)
        require(
            config.get("seed_source") == "linux_random",
            "validation session did not use Linux random seed source",
            errors,
        )
        require(
            config.get("seed_derivation") == ROUND_SEED_DERIVATION,
            "round seed derivation recipe is unexpected",
            errors,
        )
        require(
            config.get("window_recipe_id") == WINDOW_RECIPE_ID,
            "window recipe ID is unexpected",
            errors,
        )
        require(
            config.get("window_sizes_bytes") == WINDOW_SIZES,
            "window size recipe is unexpected",
            errors,
        )
        require(
            config.get("pattern_recipe_id") == PATTERN_RECIPE_ID,
            "pattern recipe ID is unexpected",
            errors,
        )
        require(
            config.get("pattern_families") == PATTERN_FAMILIES,
            "pattern family recipe is unexpected",
            errors,
        )
        commitment = config.get("seed_commitment_sha256")
        require(
            isinstance(commitment, str)
            and re.fullmatch(r"[0-9a-f]{64}", commitment) is not None,
            "seed commitment is not canonical SHA-256",
            errors,
        )
        if (
            session_seed is not None
            and isinstance(source, dict)
            and isinstance(source.get("sha256"), str)
            and isinstance(report.get("test_id"), str)
        ):
            require(
                commitment
                == seed_commitment(
                    report["test_id"], source["sha256"], session_seed
                ),
                "seed commitment does not match test/source/session seed",
                errors,
            )

    memory = report.get("memory", {})
    require(isinstance(memory, dict), "system memory evidence is missing", errors)
    if isinstance(memory, dict):
        names = [
            "requested_bytes",
            "allocated_unique_bytes",
            "prefaulted_unique_bytes",
            "written_unique_bytes",
            "read_unique_bytes",
            "exact_verified_unique_bytes",
        ]
        values = {name: integer(memory.get(name)) for name in names}
        require(
            all(value is not None for value in values.values()),
            "one or more memory coverage counters are invalid",
            errors,
        )
        if all(value is not None for value in values.values()):
            requested = values["requested_bytes"]
            sequence = [values[name] for name in names[1:]]
            require(
                all(value == requested for value in sequence),
                "unique memory coverage does not exactly equal requested bytes",
                errors,
            )
        require(
            memory.get("allocation_target_met") is True,
            "allocation target was not met",
            errors,
        )
        require(
            memory.get("coverage_target_met") is True,
            "coverage target was not met",
            errors,
        )

        if profile == "safe":
            require(
                memory.get("target_kind") == "explicit_bytes",
                "Safe profile did not use an explicit target",
                errors,
            )
            require(
                memory.get("requested_bytes") == 256 * 1024 * 1024,
                "Safe profile did not request exactly 256 MiB",
                errors,
            )
            require(
                report.get("duration_target_ms", 0) >= 120_000,
                "Safe profile minimum duration is below 2 minutes",
                errors,
            )
        elif profile == "full":
            require(
                memory.get("target_kind") == "percent_safe:95",
                "Full profile did not request 95% of the safe allocatable limit",
                errors,
            )
            require(
                report.get("duration_target_ms", 0) >= 1_800_000,
                "Full profile minimum duration is below 30 minutes",
                errors,
            )
            safe_limit = integer(memory.get("safe_limit_bytes"))
            requested = integer(memory.get("requested_bytes"))
            require(
                safe_limit is not None
                and requested == safe_limit * 95 // 100,
                "Full target is not exactly 95% of the safe allocatable limit",
                errors,
            )

        require(
            memory.get("safety_reserve_bytes") == 512 * 1024 * 1024,
            "profile safety reserve is not 512 MiB",
            errors,
        )

    expected_rounds = {"safe": 2, "full": 4}.get(profile)
    require(
        report.get("rounds_requested") == expected_rounds,
        f"{profile or 'unknown'} profile has an unexpected round count",
        errors,
    )

    rounds_completed = integer(report.get("rounds_completed"))
    rounds_requested = integer(report.get("rounds_requested"))
    require(
        rounds_completed is not None
        and rounds_requested is not None
        and rounds_completed >= rounds_requested,
        "not all minimum requested rounds completed",
        errors,
    )
    require(
        report.get("duration_target_met") is True,
        "minimum duration target was not met",
        errors,
    )
    require(
        report.get("round_seeds_unique") is True,
        "round seeds are not marked unique",
        errors,
    )
    round_digests = report.get("round_digests")
    require(isinstance(round_digests, list), "round seed/digest evidence is missing", errors)
    if isinstance(round_digests, list) and rounds_completed is not None:
        require(
            len(round_digests) == rounds_completed,
            "round seed/digest count does not equal completed rounds",
            errors,
        )
        observed_seeds: list[int] = []
        for expected_round, item in enumerate(round_digests):
            if not isinstance(item, dict):
                errors.append("round digest evidence contains a non-object")
                continue
            require(
                item.get("round") == expected_round,
                f"round digest index {expected_round} has an unexpected round ID",
                errors,
            )
            round_seed = parse_seed_hex(item.get("round_seed_hex"))
            require(
                round_seed is not None,
                f"round {expected_round} seed is not canonical 64-bit hex",
                errors,
            )
            if round_seed is not None:
                observed_seeds.append(round_seed)
                if session_seed is not None and isinstance(report.get("test_id"), str):
                    require(
                        round_seed
                        == derive_round_seed(
                            session_seed, report["test_id"], expected_round
                        ),
                        f"round {expected_round} seed derivation mismatch",
                        errors,
                    )
            plan_sha256 = item.get("window_plan_sha256")
            require(
                isinstance(plan_sha256, str)
                and re.fullmatch(r"[0-9a-f]{64}", plan_sha256) is not None,
                f"round {expected_round} window plan digest is invalid",
                errors,
            )
            for field in (
                "mandatory_recipe_applicable",
                "mandatory_recipe_met",
                "no_gaps",
                "no_overlaps",
                "tail_verified",
            ):
                require(
                    item.get(field) is True,
                    f"round {expected_round} window gate {field} is not true",
                    errors,
                )
            require(
                integer(item.get("total_windows")) is not None
                and item["total_windows"] > 0,
                f"round {expected_round} total window count is invalid",
                errors,
            )
            size_counts = item.get("size_class_counts")
            verified_sizes = item.get("size_class_verified_bytes")
            pattern_counts = item.get("pattern_counts")
            require(isinstance(size_counts, dict), f"round {expected_round} size counts missing", errors)
            require(isinstance(verified_sizes, dict), f"round {expected_round} verified size bytes missing", errors)
            require(isinstance(pattern_counts, dict), f"round {expected_round} pattern counts missing", errors)
            if isinstance(size_counts, dict):
                require(
                    all(integer(size_counts.get(name)) is not None and size_counts[name] > 0 for name in WINDOW_CLASSES),
                    f"round {expected_round} misses a mandatory window class",
                    errors,
                )
            if isinstance(pattern_counts, dict):
                require(
                    all(integer(pattern_counts.get(name)) is not None and pattern_counts[name] > 0 for name in PATTERN_FAMILIES),
                    f"round {expected_round} misses a mandatory pattern family",
                    errors,
                )
            if isinstance(verified_sizes, dict) and isinstance(memory, dict):
                require(
                    all(integer(value) is not None for value in verified_sizes.values())
                    and sum(verified_sizes.values()) == memory.get("requested_bytes"),
                    f"round {expected_round} per-size verified bytes do not equal target",
                    errors,
                )
        require(
            len(observed_seeds) == len(set(observed_seeds)),
            "duplicate round seeds detected",
            errors,
        )

    window_coverage = report.get("window_coverage", {})
    require(isinstance(window_coverage, dict), "window coverage evidence is missing", errors)
    if isinstance(window_coverage, dict):
        require(window_coverage.get("window_recipe_id") == WINDOW_RECIPE_ID, "window coverage recipe ID mismatch", errors)
        require(window_coverage.get("pattern_recipe_id") == PATTERN_RECIPE_ID, "pattern coverage recipe ID mismatch", errors)
        require(window_coverage.get("mandatory_size_classes") == WINDOW_CLASSES, "mandatory window class list mismatch", errors)
        require(window_coverage.get("mandatory_pattern_families") == PATTERN_FAMILIES, "mandatory pattern family list mismatch", errors)
        for field in (
            "mandatory_recipe_applicable",
            "all_completed_rounds_met",
            "plans_no_gaps",
            "plans_no_overlaps",
            "tails_verified",
        ):
            require(window_coverage.get(field) is True, f"window coverage gate {field} is not true", errors)

    workers = report.get("workers")
    require(
        isinstance(workers, list) and bool(workers), "worker evidence is missing", errors
    )
    if isinstance(workers, list):
        for worker in workers:
            if not isinstance(worker, dict):
                errors.append("worker evidence contains a non-object")
                continue
            worker_id = worker.get("worker_id", "?")
            require(
                worker.get("affinity_failures") == 0,
                f"worker {worker_id} has affinity failures",
                errors,
            )
            require(
                worker.get("scalar_simd_mismatches") == 0,
                f"worker {worker_id} has scalar/SIMD mismatches",
                errors,
            )

    safety = report.get("safety", {})
    require(isinstance(safety, dict), "safety evidence is missing", errors)
    if isinstance(safety, dict):
        policy = safety.get("policy", {})
        require(isinstance(policy, dict), "safety policy evidence is missing", errors)
        if isinstance(policy, dict):
            require(
                isinstance(policy.get("temperature_limit_enabled"), bool),
                "temperature limit enable state is not explicit",
                errors,
            )
        require(
            safety.get("stop_requested") is False, "safety stop was requested", errors
        )
        require(safety.get("violations") == [], "safety violations are present", errors)

    execution = report.get("execution", {})
    require(isinstance(execution, dict), "execution evidence is missing", errors)
    if isinstance(execution, dict):
        require(execution.get("aborted") is False, "execution was aborted", errors)
        require(
            execution.get("worker_crashes") == [], "worker crashes are present", errors
        )
        require(
            execution.get("worker_errors") == [], "worker errors are present", errors
        )

    telemetry = report.get("telemetry", {})
    require(isinstance(telemetry, dict), "telemetry evidence is missing", errors)
    if isinstance(telemetry, dict):
        require(
            telemetry.get("edac_uncorrected_delta") == 0,
            "uncorrected EDAC delta is non-zero",
            errors,
        )
        require(
            telemetry.get("edac_corrected_delta") == 0,
            "corrected EDAC delta is non-zero for validation",
            errors,
        )
        require(
            telemetry.get("cgroup_oom_delta") == 0,
            "cgroup OOM delta is non-zero",
            errors,
        )
        require(
            telemetry.get("cgroup_oom_kill_delta") == 0,
            "cgroup OOM-kill delta is non-zero",
            errors,
        )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Validate one positive CPU-DIT live-USB session directory"
    )
    parser.add_argument("session_dir", type=Path)
    parser.add_argument("--manifest", type=Path)
    args = parser.parse_args()

    session_dir: Path = args.session_dir
    errors: list[str] = []
    required_files = [
        "session.txt",
        "summary.txt",
        "preflight.txt",
        "reference-report.json",
        "system-report.json",
        "sensor-report.json",
        "driver-probe.txt",
    ]
    for name in required_files:
        require((session_dir / name).is_file(), f"missing required file: {name}", errors)

    summary = parse_key_values(session_dir / "summary.txt", errors)
    require(
        summary.get("profile") in {"safe", "full"},
        "summary profile is not safe/full",
        errors,
    )
    for name in ("reference_exit", "system_exit", "overall_exit"):
        require(summary.get(name) == "0", f"{name} is not 0", errors)

    reference = load_json(session_dir / "reference-report.json", errors)
    system = load_json(session_dir / "system-report.json", errors)
    load_json(session_dir / "sensor-report.json", errors)
    validate_reference(reference, errors)
    validate_system(system, summary.get("profile"), errors)

    if args.manifest:
        manifest = load_json(args.manifest, errors)
        expected_image_id = manifest.get("image_id")
        require(
            isinstance(expected_image_id, str) and bool(expected_image_id),
            "manifest image_id is missing",
            errors,
        )
        require(
            summary.get("image_id") == expected_image_id,
            "session Image ID does not match manifest",
            errors,
        )

    if errors:
        print("SESSION_NOT_VALIDATED")
        for error in errors:
            print(f"ERROR: {error}")
        return 2

    memory = system["memory"]
    print("SESSION_PASS")
    print(f"image_id={summary.get('image_id', 'unknown')}")
    print(f"profile={summary['profile']}")
    print(f"exact_verified_unique_bytes={memory['exact_verified_unique_bytes']}")
    print(f"rounds={system['rounds_completed']}/{system['rounds_requested']}")
    print("scope=single_session_only")
    return 0


if __name__ == "__main__":
    sys.exit(main())

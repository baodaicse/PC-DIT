#!/usr/bin/env python3
"""Summarise GPU-H1 baseline runs into summary.json and SUMMARY.md.

Reads <dir>/<mode>-K<k>-<phase>/report.json. mode is seq|async, phase is
cold|steady. Missing optional fields are written as null, never as zero.
"""
import json
import pathlib
import sys


def get(d, *path):
    for key in path:
        if not isinstance(d, dict) or key not in d:
            return None
        d = d[key]
    return d


def row(name, report):
    mode, k, phase = name.split("-")
    det = report.get("detection") or {}
    asy = report.get("async_compute") or {}
    rates = asy.get("rates") or {}
    timing = asy.get("timing") or {}
    stress = det.get("stress") or {}
    elapsed = report.get("active_elapsed_seconds")
    ops = get(det, "compute", "verified_integer_ops")
    return {
        "run": name,
        "mode": mode,
        "K": int(k[1:]),
        "phase": phase,
        "verdict": report.get("verdict"),
        "elapsed_seconds": elapsed,
        "matrix_dispatches_verified": report.get("matrix_dispatches_completed"),
        "verified_integer_ops": ops,
        "verified_gops_per_second": (ops / elapsed / 1e9) if ops is not None and elapsed else None,
        "vram_pcie_verified_mib_per_second": get(det, "coverage", "vram_pcie_verified_mib_per_second"),
        "transfer_operations": get(det, "coverage", "transfer_operations"),
        "allocation_sweeps": get(det, "coverage", "allocation_sweeps"),
        "failed_batches": get(det, "compute", "failed_batches"),
        "mismatch_words": get(det, "compute", "mismatch_words"),
        "power_mean_w": get(stress, "power_watts", "mean"),
        "power_max_w": get(stress, "power_watts", "max"),
        "power_max_percent_of_limit": stress.get("power_max_percent_of_limit"),
        "temperature_max_c": get(stress, "temperature_celsius", "max"),
        "gpu_util_mean_percent": get(stress, "utilization_percent", "mean"),
        "throttle_limited_samples": stress.get("throttle_limited_samples"),
        "link_min_speed_gts": get(det, "link", "min_speed_seen_gts"),
        "aer_correctable_delta": get(det, "link", "aer_delta", "correctable"),
        "kernel_log_family_hits": get(det, "kernel_log", "family_hits_total"),
        "output_production_mib_s": rates.get("output_production_mib_s"),
        "verifier_mib_s_per_core": rates.get("verifier_mib_s_per_core"),
        "output_over_verifier_per_core": rates.get("output_over_verifier_per_core"),
        "device_busy_fraction": rates.get("device_busy_fraction"),
        "verify_compare_core_seconds": timing.get("verify_compare_core_seconds"),
        "verify_hash_core_seconds": timing.get("verify_hash_core_seconds"),
        "oracle_seconds": timing.get("oracle_seconds"),
        "conservation": asy.get("conservation"),
        "bursts": asy.get("bursts"),
        "empty_bursts": asy.get("empty_bursts"),
    }


def fmt(v, nd=1):
    if v is None:
        return "n/a"
    if isinstance(v, float):
        return f"{v:.{nd}f}"
    return str(v)


def main():
    root = pathlib.Path(sys.argv[1])
    rows = []
    for report in sorted(root.glob("*/report.json")):
        name = report.parent.name
        if name.count("-") != 2 or name.startswith("negative"):
            continue
        rows.append(row(name, json.loads(report.read_text())))
    rows.sort(key=lambda r: (r["phase"], r["K"], r["mode"]))
    (root / "summary.json").write_text(json.dumps({"version": 1, "runs": rows}, indent=2) + "\n")
    cols = [
        ("run", "run", 0),
        ("Gop/s", "verified_gops_per_second", 1),
        ("VRAM/PCIe MiB/s", "vram_pcie_verified_mib_per_second", 1),
        ("transfers", "transfer_operations", 0),
        ("GPU util %", "gpu_util_mean_percent", 0),
        ("P mean W", "power_mean_w", 1),
        ("P max W", "power_max_w", 1),
        ("T max C", "temperature_max_c", 0),
        ("verify MiB/s/core", "verifier_mib_s_per_core", 0),
        ("out/verify", "output_over_verifier_per_core", 2),
        ("busy frac", "device_busy_fraction", 2),
        ("failed", "failed_batches", 0),
    ]
    lines = [
        "# GPU-H1 baseline summary",
        "",
        "One run per cell on one machine; treat differences under ~10% as noise.",
        "Cold = `--generations 3` (a new corpus every generation). Steady = duration run",
        "(corpus reused). `n/a` means not measured, never zero. `busy frac`, `verify` and",
        "`out/verify` exist only for async runs. `out/verify` above 1 means one core cannot",
        "keep up with the output; it is a regime hint, not a pass/fail input.",
        "",
        "| " + " | ".join(c[0] for c in cols) + " |",
        "|" + "|".join("---" for _ in cols) + "|",
    ]
    for r in rows:
        lines.append("| " + " | ".join(fmt(r[c[1]], c[2]) for c in cols) + " |")
    (root / "SUMMARY.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()

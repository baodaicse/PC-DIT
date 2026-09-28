#!/usr/bin/env python3
"""Summarise GPU-H3 slot-pool runs: <dir>/<name>/report.json -> SUMMARY.md and summary.json.
`n/a` means not measured, never zero."""
import json
import pathlib
import sys


def get(d, *path):
    for key in path:
        if not isinstance(d, dict) or key not in d:
            return None
        d = d[key]
    return d


def fmt(v, nd=1):
    if v is None:
        return "n/a"
    return f"{v:.{nd}f}" if isinstance(v, float) else str(v)


def row(name, r):
    a = r.get("async_compute") or {}
    cfg = a.get("config") or {}
    b = r.get("workspace_budget") or {}
    ops = get(r, "detection", "compute", "verified_integer_ops_per_second")
    return {
        "run": name,
        "verdict": r.get("verdict"),
        "device_slots": cfg.get("device_slots"),
        "host_slots": cfg.get("host_slots"),
        "workers": cfg.get("workers"),
        "jobs": b.get("matrix_jobs_effective"),
        "backoff_steps": b.get("backoff_steps"),
        "pinned_queue_mib": (b.get("pinned_queue_bytes") or 0) / 2**20 if b else None,
        "host_ram_mib": (b.get("matrix_host_bytes") or 0) / 2**20 if b else None,
        "gops": ops / 1e9 if ops is not None else None,
        "verified": get(a, "totals", "verified"),
        "device_in_flight_peak": a.get("device_in_flight_peak"),
        "host_slots_used_peak": a.get("host_slots_used_peak"),
        "throttle_events": get(a, "controller", "throttle_events"),
        "queue_wait_max_ms": (get(a, "timing", "queue_wait_max_seconds") or 0) * 1000 if a else None,
        "quarantined": get(a, "totals", "quarantined"),
        "released": get(a, "totals", "quarantine_released"),
        "cpu_workers": get(r, "cpu_share", "verifier_workers"),
        "conservation": a.get("conservation"),
    }


def main():
    root = pathlib.Path(sys.argv[1])
    rows = []
    for report in sorted(root.glob("*/report.json")):
        rows.append(row(report.parent.name, json.loads(report.read_text())))
    (root / "summary.json").write_text(json.dumps({"version": 1, "runs": rows}, indent=2) + "\n")
    cols = [
        ("run", "run", 0), ("verdict", "verdict", 0), ("dev", "device_slots", 0),
        ("host", "host_slots", 0), ("workers", "workers", 0), ("jobs", "jobs", 0),
        ("backoff", "backoff_steps", 0), ("pinned queue MiB", "pinned_queue_mib", 0),
        ("Gop/s", "gops", 0), ("dev peak", "device_in_flight_peak", 0),
        ("host peak", "host_slots_used_peak", 0), ("throttle", "throttle_events", 0),
        ("qwait max ms", "queue_wait_max_ms", 1),
    ]
    lines = [
        "# GPU-H3 slot-pool summary", "",
        "One run per cell on one machine; treat differences under ~10% as noise. `dev`/`host` are the",
        "effective device and host slots after any backoff; `peak` columns are high-water marks.",
        "`throttle` is the load controller's throttle-event count.", "",
        "| " + " | ".join(c[0] for c in cols) + " |", "|" + "|".join("---" for _ in cols) + "|",
    ]
    for r in rows:
        lines.append("| " + " | ".join(fmt(r[k], nd) for _, k, nd in cols) + " |")
    (root / "SUMMARY.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()

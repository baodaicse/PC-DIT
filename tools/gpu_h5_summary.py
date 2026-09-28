#!/usr/bin/env python3
"""Summarise GPU-H5 controller and coverage runs: <dir>/<name>/report.json -> SUMMARY.md.
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
    c = a.get("controller") or {}
    w = a.get("workers") or {}
    cs = r.get("coverage_schedule") or {}
    ops = get(r, "detection", "compute", "verified_integer_ops_per_second")
    elapsed = r.get("active_elapsed_seconds") or 0
    changes = c.get("state_changes")
    reg = a.get("regimes") or {}
    dominant = max(reg, key=reg.get) if reg and any(reg.values()) else None
    return {
        "run": name,
        "verdict": r.get("verdict"),
        "gops": ops / 1e9 if ops is not None else None,
        "coverage_mib_s": get(r, "detection", "coverage", "vram_pcie_verified_mib_per_second"),
        "floor": cs.get("floor"),
        "share": cs.get("share"),
        "state": c.get("state"),
        "throttle": c.get("throttle_events"),
        "recovery": c.get("recovery_events"),
        "changes": changes,
        "changes_per_min": changes / (elapsed / 60) if changes is not None and elapsed else None,
        "pacing_ms": c.get("pacing_ms"),
        "regime": dominant,
        "backlog_peak": a.get("backlog_fill_peak"),
        "workers_peak": w.get("peak"),
        "worker_increases": w.get("increases"),
        "pressure_hits": a.get("memory_pressure_hits"),
        "conservation": a.get("conservation"),
    }


def main():
    root = pathlib.Path(sys.argv[1])
    rows = [row(p.parent.name, json.loads(p.read_text())) for p in sorted(root.glob("*/report.json"))]
    (root / "summary.json").write_text(json.dumps({"version": 1, "runs": rows}, indent=2) + "\n")
    cols = [
        ("run", "run", 0), ("verdict", "verdict", 0), ("Gop/s", "gops", 0),
        ("cov MiB/s", "coverage_mib_s", 0), ("floor", "floor", 2), ("share", "share", 3),
        ("state", "state", 0), ("throttle", "throttle", 0), ("changes/min", "changes_per_min", 1),
        ("pacing ms", "pacing_ms", 0), ("regime", "regime", 0), ("backlog peak", "backlog_peak", 2),
        ("workers peak", "workers_peak", 0), ("pressure hits", "pressure_hits", 0),
    ]
    lines = [
        "# GPU-H5 controller and coverage summary", "",
        "One run per cell on one machine; treat differences under ~10% as noise. `share` is the achieved",
        "fraction of wall time spent in transfer/retention; `floor` is the configured minimum. `regime` is the",
        "burst label that occurred most often. `changes/min` counts controller state changes.", "",
        "| " + " | ".join(c[0] for c in cols) + " |", "|" + "|".join("---" for _ in cols) + "|",
    ]
    for r in rows:
        lines.append("| " + " | ".join(fmt(r[k], nd) for _, k, nd in cols) + " |")
    (root / "SUMMARY.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()

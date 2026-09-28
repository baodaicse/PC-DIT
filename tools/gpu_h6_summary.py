#!/usr/bin/env python3
"""Summarise and check GPU-H6 runs: <dir>/<name>/report.json -> SUMMARY.md, summary.json, exit 1 on a
failed check. `n/a` means not measured, never zero. One run per cell on one machine: the thresholds
leave margin over what the GTX 960 measured and only claim relative behaviour (overlap against
time-slicing and against the same workload alone), never absolute throughput."""
import json
import pathlib
import sys

MIB = 1024 * 1024


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
    lane = get(r, "async_compute", "lane") or {}
    transfer = lane.get("transfer") or {}
    vram = lane.get("vram") or {}
    elapsed = r.get("active_elapsed_seconds") or 0
    ops = get(r, "detection", "compute", "verified_integer_ops_per_second")
    return {
        "run": name,
        "verdict": r.get("verdict"),
        "profile": get(r, "workload", "profile"),
        "transfer_mode": get(r, "workload", "transfer_mode"),
        "vram_local": get(r, "workload", "vram_local"),
        "seconds": elapsed,
        "gops": ops / 1e9 if ops else 0.0,
        "coverage_mib_s": r.get("bytes_verified", 0) / MIB / elapsed if elapsed else None,
        "transfer_ops": r.get("transfer_operations_completed"),
        "retention_checks": r.get("retention_checks_completed"),
        "sweeps": r.get("allocation_sweeps_completed"),
        "matrix_dispatches": r.get("matrix_dispatches_completed"),
        "failures": r.get("failure_count"),
        "failure_phases": sorted({f.get("phase") for f in r.get("failures") or []}),
        "hardware_tested": r.get("hardware_tested"),
        "h2d_mib": (transfer.get("h2d_bytes") or 0) / MIB if lane else None,
        "d2h_mib": (transfer.get("d2h_bytes") or 0) / MIB if lane else None,
        "h2d_ops": transfer.get("h2d_ops"),
        "d2h_ops": transfer.get("d2h_ops"),
        "write_ops": transfer.get("write_ops"),
        "reread_ops": transfer.get("reread_ops"),
        "forced_rereads": transfer.get("forced_rereads"),
        "verified_mib": (transfer.get("verified_bytes") or 0) / MIB if lane else None,
        "pending_rereads": transfer.get("pending_rereads"),
        "lane_conservation": lane.get("conservation"),
        "lane_busy_s": lane.get("busy_wall_seconds"),
        "lane_overlap_s": lane.get("overlap_with_compute_seconds"),
        "lane_failures": lane.get("failures_total"),
        "vram_touched_mib": (vram.get("touched_bytes") or 0) / MIB if lane else None,
        "vram_verified_mib": (vram.get("cpu_verified_bytes") or 0) / MIB if lane else None,
        "vram_bad_words": vram.get("self_check_bad_words"),
        "vram_write_ops": vram.get("write_ops"),
        "regimes": get(r, "async_compute", "regimes") or {},
        "regime_bursts": sum((get(r, "async_compute", "regimes") or {}).values()),
        "compute_items": get(r, "async_compute", "fairness", "compute_items"),
        "lane_items": get(r, "async_compute", "fairness", "lane_items"),
        "conservation": get(r, "async_compute", "conservation"),
        "lane_traffic_mib_s": ((transfer.get("h2d_bytes") or 0) + (transfer.get("d2h_bytes") or 0))
        / MIB
        / elapsed
        if lane and elapsed
        else None,
    }


class Checks:
    def __init__(self):
        self.results = []

    def add(self, name, ok, detail):
        self.results.append({"check": name, "ok": bool(ok), "detail": detail})

    def ratio(self, name, num, den, minimum, what):
        if num is None or not den:
            self.add(name, False, f"{what}: not measured")
        else:
            self.add(name, num / den >= minimum, f"{what}: {num:.1f} / {den:.1f} = {num / den:.2f} (need >= {minimum})")


def run_checks(rows):
    c = Checks()
    g = rows.get
    for name, r in rows.items():
        if name.startswith("neg_"):
            continue
        c.add(f"{name}: verdict", r["verdict"] == "PASS" and r["failures"] == 0, f"{r['verdict']} failures={r['failures']}")
        if r["conservation"] is not None:
            c.add(f"{name}: async ledger conserved", r["conservation"] == "ok", str(r["conservation"]))
        if r["lane_conservation"] is not None:
            c.add(f"{name}: lane ledger conserved", r["lane_conservation"] == "ok", str(r["lane_conservation"]))
        if r["transfer_mode"] == "overlapped" and (r["write_ops"] or 0) > 0:
            # Per-direction counters agree with what the operations did, and nothing is left unread.
            c.add(
                f"{name}: per-direction counters",
                r["h2d_ops"] == r["write_ops"]
                and r["d2h_ops"] == r["write_ops"] + r["reread_ops"]
                and r["h2d_mib"] > 0
                and r["d2h_mib"] >= r["h2d_mib"]
                and abs(r["verified_mib"] - r["d2h_mib"]) < 1e-6
                and r["pending_rereads"] == 0,
                f"h2d {r['h2d_ops']}/{fmt(r['h2d_mib'], 0)} MiB, d2h {r['d2h_ops']}/{fmt(r['d2h_mib'], 0)} MiB, "
                f"verified {fmt(r['verified_mib'], 0)} MiB, pending {r['pending_rereads']}",
            )

    seq_lean, seq_default = g("seq_lean"), g("seq_default")
    overlap, overlap_vram = g("overlap"), g("overlap_vram")
    compute_only, transfer_only = g("compute_only"), g("transfer_only_overlap")
    default_on = g("default_on")
    if default_on:
        # H6: overlap is now the default for --async-compute; a bare run with no lane flags at
        # all must already schedule the lane on its own streams, not the old time-slice floor.
        c.add(
            "bare --async-compute already overlaps",
            default_on["transfer_mode"] == "overlapped" and (default_on["transfer_ops"] or 0) > 0,
            f"transfer_mode={default_on['transfer_mode']} transfer_ops={default_on['transfer_ops']}",
        )
    if seq_default:
        c.add(
            "--no-transfer-overlap restores the old time-sliced default",
            seq_default["transfer_mode"] == "sequential",
            f"transfer_mode={seq_default['transfer_mode']}",
        )
    if overlap and compute_only:
        c.ratio("overlap costs compute little", overlap["gops"], compute_only["gops"], 0.90, "combined vs compute-only Gop/s")
    if overlap and transfer_only:
        c.ratio("overlap keeps most of the lane", overlap["coverage_mib_s"], transfer_only["coverage_mib_s"], 0.40, "combined vs transfer-only MiB/s")
    if overlap and seq_default:
        c.ratio("overlap beats the time-slice on coverage", overlap["coverage_mib_s"], seq_default["coverage_mib_s"], 2.0, "overlap vs floor 0.25 MiB/s")
        c.ratio("overlap beats the time-slice on compute", overlap["gops"], seq_default["gops"], 1.2, "overlap vs floor 0.25 Gop/s")
    if overlap and seq_lean:
        c.ratio("overlap costs no compute against no coverage", overlap["gops"], seq_lean["gops"], 0.95, "overlap vs floor 0 Gop/s")
        c.ratio("overlap gives far more coverage than floor 0", overlap["coverage_mib_s"], seq_lean["coverage_mib_s"], 5.0, "overlap vs floor 0 MiB/s")
    if overlap:
        busy, ov = overlap["lane_busy_s"] or 0, overlap["lane_overlap_s"] or 0
        c.add("lane device time overlaps compute", busy > 0 and ov >= 0.5 * busy, f"{ov:.2f} s of {busy:.2f} s")
        c.add("both verifier classes served", (overlap["compute_items"] or 0) > 0 and (overlap["lane_items"] or 0) > 0, f"compute {overlap['compute_items']} lane {overlap['lane_items']}")
        c.add("all pattern classes covered", (get_pattern(overlap) or 0) >= 7, f"{get_pattern(overlap)} of 7 classes")
    if overlap_vram:
        c.add(
            "gpu-local workload: touched is not coverage",
            (overlap_vram["vram_write_ops"] or 0) > 0
            and overlap_vram["vram_touched_mib"] > overlap_vram["vram_verified_mib"] > 0
            and overlap_vram["vram_bad_words"] == 0,
            f"touched {fmt(overlap_vram['vram_touched_mib'], 0)} MiB, cpu-verified {fmt(overlap_vram['vram_verified_mib'], 0)} MiB, "
            f"bad words {overlap_vram['vram_bad_words']}",
        )
        if compute_only:
            c.ratio("gpu-local workload costs compute little", overlap_vram["gops"], compute_only["gops"], 0.85, "with vram-local vs compute-only Gop/s")

    # Profiles isolate one path.
    if compute_only:
        c.add(
            "compute-only runs no transfers",
            compute_only["profile"] == "compute-only"
            and compute_only["transfer_mode"] == "off"
            and not compute_only["transfer_ops"]
            and compute_only["gops"] > 0,
            f"transfer_ops={compute_only['transfer_ops']} gops={compute_only['gops']:.0f}",
        )
    for name in ("transfer_only_overlap", "transfer_only_seq"):
        r = g(name)
        if r:
            c.add(
                f"{name}: runs no compute",
                r["profile"] == "transfer-only" and r["gops"] == 0 and not r["matrix_dispatches"] and (r["transfer_ops"] or 0) > 0 and r["hardware_tested"] is True,
                f"gops={r['gops']:.0f} dispatches={r['matrix_dispatches']} transfer_ops={r['transfer_ops']}",
            )

    # Fairness when the verifier is the limit: the lane keeps working and compute is not hurt.
    vb_seq, vb_overlap = g("vb_seq"), g("vb_overlap")
    if vb_seq and vb_overlap:
        c.add(
            "verifier-bound: coverage is not lost",
            (vb_overlap["transfer_ops"] or 0) >= 3 * max(vb_seq["transfer_ops"] or 0, 1),
            f"transfer ops {vb_overlap['transfer_ops']} vs sequential {vb_seq['transfer_ops']}",
        )
        c.ratio("verifier-bound: compute is not lost", vb_overlap["gops"], vb_seq["gops"], 0.85, "overlap vs sequential Gop/s")
        c.add("verifier-bound: both classes served", (vb_overlap["compute_items"] or 0) > 0 and (vb_overlap["lane_items"] or 0) > 0, f"compute {vb_overlap['compute_items']} lane {vb_overlap['lane_items']}")

    transfer_bound = g("transfer_bound")
    if transfer_bound:
        regimes = transfer_bound["regimes"]
        total = transfer_bound["regime_bursts"]
        c.add(
            "transfer-bound regime is reached on real hardware",
            total > 0 and regimes.get("transfer_bound", 0) >= 0.75 * total and regimes.get("gpu_bound", 0) == 0,
            f"{regimes} of {total} bursts",
        )

    cap = g("rate_cap")
    if cap:
        limit = 300.0
        traffic = cap["lane_traffic_mib_s"] or 0
        c.add("rate cap holds", 0.6 * limit <= traffic <= 1.3 * limit, f"{traffic:.0f} MiB/s against {limit:.0f}")

    neg = g("neg_transfer")
    if neg:
        c.add("negative: lane transfer fault detected once", neg["verdict"] == "FAIL" and neg["failures"] == 1 and neg["failure_phases"] == ["transfer"] and neg["lane_failures"] == 1, f"{neg['verdict']} failures={neg['failures']} phases={neg['failure_phases']}")
    neg = g("neg_matrix")
    if neg:
        c.add("negative: matrix fault detected, lane clean", neg["verdict"] == "FAIL" and "compute" in neg["failure_phases"] and not neg["lane_failures"], f"{neg['verdict']} phases={neg['failure_phases']} lane_failures={neg['lane_failures']}")
    return c.results


def get_pattern(r):
    return r.get("pattern_classes")


def main():
    root = pathlib.Path(sys.argv[1])
    rows = {}
    for report in sorted(root.glob("*/report.json")):
        data = json.loads(report.read_text())
        entry = row(report.parent.name, data)
        counts = get(data, "async_compute", "lane", "transfer", "pattern_counts")
        entry["pattern_classes"] = sum(1 for n in counts if n > 0) if counts else None
        rows[entry["run"]] = entry
    checks = run_checks(rows)
    (root / "summary.json").write_text(json.dumps({"version": 1, "runs": list(rows.values()), "checks": checks}, indent=2) + "\n")
    cols = [
        ("run", "run", 0), ("mode", "transfer_mode", 0), ("verdict", "verdict", 0), ("Gop/s", "gops", 0),
        ("cover MiB/s", "coverage_mib_s", 0), ("xfer ops", "transfer_ops", 0), ("h2d MiB", "h2d_mib", 0),
        ("d2h MiB", "d2h_mib", 0), ("lane overlap s", "lane_overlap_s", 2), ("vram touched MiB", "vram_touched_mib", 0),
        ("vram verified MiB", "vram_verified_mib", 0), ("fair c/l", None, 0),
    ]
    lines = [
        "# GPU-H6 summary", "",
        "One run per cell on one machine; treat differences under ~10% as noise. `cover MiB/s` is bytes a CPU",
        "compared with the expected pattern per second (coverage). `vram touched` is device-side activity and is",
        "never counted as coverage. `fair c/l` is verifier items served for compute / lane.", "",
        "| " + " | ".join(c[0] for c in cols) + " |", "|" + "|".join("---" for _ in cols) + "|",
    ]
    for r in rows.values():
        cells = [f"{r['compute_items']}/{r['lane_items']}" if k is None and r["compute_items"] is not None else ("n/a" if k is None else fmt(r[k], nd)) for _, k, nd in cols]
        lines.append("| " + " | ".join(cells) + " |")
    lines += ["", "## Checks", ""]
    failed = 0
    for check in checks:
        lines.append(f"- {'PASS' if check['ok'] else 'FAIL'} — {check['check']}: {check['detail']}")
        failed += not check["ok"]
    (root / "SUMMARY.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))
    if failed:
        print(f"\n{failed} check(s) FAILED")
        sys.exit(1)
    print(f"\nall {len(checks)} checks PASS")


if __name__ == "__main__":
    main()

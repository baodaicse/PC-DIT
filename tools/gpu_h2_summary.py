#!/usr/bin/env python3
"""Summarise GPU-H2 A/B runs and compare them with the retained H1 baseline.

Usage: gpu_h2_summary.py <h2-evidence-dir> [<h1-evidence-dir>]
Runs live in <dir>/<mode>-K<k>-<phase>/report.json with mode in
seq_legacy | seq_lean | async | async_hash_every and phase cold | steady.
`n/a` means not measured, never zero.
"""
import json
import pathlib
import sys

GIB = 1024**3


def get(d, *path):
    for key in path:
        if not isinstance(d, dict) or key not in d:
            return None
        d = d[key]
    return d


def per_op_ms(report, phase):
    ph = get(report, "campaign_profile", phase)
    if not ph or not ph.get("attempts"):
        return None
    return ph["host_seconds"] / ph["attempts"] * 1000


def summarise(name, report):
    mode, k, phase = name.rsplit("-", 2)
    elapsed = report.get("active_elapsed_seconds")
    det = report.get("detection") or {}
    asy = report.get("async_compute") or {}
    timing = asy.get("timing") or {}
    totals = asy.get("totals") or {}
    core = (timing.get("verify_compare_core_seconds") or 0) + (timing.get("verify_hash_core_seconds") or 0)
    verified_bytes = totals.get("verified_bytes")
    ops = get(det, "compute", "verified_integer_ops")
    return {
        "run": name,
        "mode": mode,
        "K": int(k[1:]),
        "phase": phase,
        "verdict": report.get("verdict"),
        "hash_policy": report.get("hash_policy"),
        "elapsed_seconds": elapsed,
        "verified_gops_per_second": ops / elapsed / 1e9 if ops is not None and elapsed else None,
        "vram_pcie_mib_per_second": get(det, "coverage", "vram_pcie_verified_mib_per_second"),
        "transfer_operations": get(det, "coverage", "transfer_operations"),
        "transfer_host_ms_per_op": per_op_ms(report, "transfer"),
        "retention_host_ms_per_op": per_op_ms(report, "retention"),
        "verify_core_seconds_per_gib": core / (verified_bytes / GIB) if verified_bytes else None,
        "verifier_mib_s_per_core": get(asy, "rates", "verifier_mib_s_per_core"),
        "hash_core_seconds": timing.get("verify_hash_core_seconds"),
        "compare_core_seconds": timing.get("verify_compare_core_seconds"),
        "oracle_seconds": timing.get("oracle_seconds"),
        "corpus_wait_seconds": timing.get("corpus_wait_seconds"),
        "corpus_rotations": asy.get("corpus_rotations"),
        "corpus_prefetch_hits": asy.get("corpus_prefetch_hits"),
        "corpus_cold_builds": asy.get("corpus_cold_builds"),
        "hashes_computed": asy.get("hashes_computed"),
        "verified_batches": totals.get("verified"),
        "conservation": asy.get("conservation"),
        "power_mean_w": get(det, "stress", "power_watts", "mean"),
        "power_max_w": get(det, "stress", "power_watts", "max"),
        "failed_batches": get(det, "compute", "failed_batches"),
    }


def load(root, name_filter=None):
    rows = {}
    for report in sorted(pathlib.Path(root).glob("*/report.json")):
        name = report.parent.name
        if name.startswith("negative") or name.count("-") < 2:
            continue
        data = json.loads(report.read_text())
        rows[name] = (summarise(name, data), data)
    return rows


def fmt(v, nd=1):
    if v is None:
        return "n/a"
    return f"{v:.{nd}f}" if isinstance(v, float) else str(v)


def main():
    root = pathlib.Path(sys.argv[1])
    h1_root = pathlib.Path(sys.argv[2]) if len(sys.argv) > 2 else None
    rows = load(root)
    order = {"seq_legacy": 0, "seq_lean": 1, "async_hash_every": 2, "async": 3}
    table = sorted((r for r, _ in rows.values()), key=lambda r: (r["phase"], r["K"], order.get(r["mode"], 9)))
    (root / "summary.json").write_text(json.dumps({"version": 1, "runs": table}, indent=2) + "\n")
    cols = [
        ("run", "run", 0),
        ("Gop/s", "verified_gops_per_second", 1),
        ("VRAM/PCIe MiB/s", "vram_pcie_mib_per_second", 1),
        ("transfer ms/op", "transfer_host_ms_per_op", 0),
        ("retention ms/op", "retention_host_ms_per_op", 0),
        ("verify core-s/GiB", "verify_core_seconds_per_gib", 3),
        ("verify MiB/s/core", "verifier_mib_s_per_core", 0),
        ("hashes", "hashes_computed", 0),
        ("rot/hit/cold", None, 0),
    ]
    lines = [
        "# GPU-H2 A/B summary",
        "",
        "One run per cell on one machine; treat differences under ~10% as noise. Cold = `--generations 3`",
        "(new corpus every generation); steady = duration run. `seq_legacy` hashes every output (the",
        "pre-H2 policy on the new copy-free code), `seq_lean` uses checkpoint hashing, `async_hash_every`",
        "isolates the hash policy inside the async pipeline, `async` is the H2 default (checkpoint hash +",
        "corpus rotation). `verify core-s/GiB` is CPU time per GiB of verified output (async only).",
        "",
        "| " + " | ".join(c[0] for c in cols) + " |",
        "|" + "|".join("---" for _ in cols) + "|",
    ]
    for r in table:
        cells = []
        for title, key, nd in cols:
            if key is None:
                cells.append(
                    "n/a" if r["corpus_rotations"] is None
                    else f"{r['corpus_rotations']}/{r['corpus_prefetch_hits']}/{r['corpus_cold_builds']}"
                )
            else:
                cells.append(fmt(r[key], nd))
        lines.append("| " + " | ".join(cells) + " |")
    if h1_root and h1_root.exists():
        h1 = load(h1_root)
        lines += ["", "## Against the retained H1 baseline (same machine, steady runs)", "",
                  "| K | metric | H1 | H2 | change |", "|---|---|---|---|---|"]
        for k in (256, 1024, 4096):
            pairs = [
                ("async verifier MiB/s/core", h1.get(f"async-K{k}-steady"), rows.get(f"async-K{k}-steady"), "verifier_mib_s_per_core", 0),
                ("async verify core-s/GiB", h1.get(f"async-K{k}-steady"), rows.get(f"async-K{k}-steady"), "verify_core_seconds_per_gib", 3),
                ("async Gop/s", h1.get(f"async-K{k}-steady"), rows.get(f"async-K{k}-steady"), "verified_gops_per_second", 1),
                ("seq VRAM/PCIe MiB/s", h1.get(f"seq-K{k}-steady"), rows.get(f"seq_lean-K{k}-steady"), "vram_pcie_mib_per_second", 1),
                ("async VRAM/PCIe MiB/s", h1.get(f"async-K{k}-steady"), rows.get(f"async-K{k}-steady"), "vram_pcie_mib_per_second", 1),
                ("seq transfer ms/op", h1.get(f"seq-K{k}-steady"), rows.get(f"seq_lean-K{k}-steady"), "transfer_host_ms_per_op", 0),
            ]
            for label, old, new, key, nd in pairs:
                a = old[0].get(key) if old else None
                if a is None and old and key == "verify_core_seconds_per_gib":
                    t = (old[1].get("async_compute") or {}).get("timing") or {}
                    b_ = (old[1].get("async_compute") or {}).get("totals", {}).get("verified_bytes")
                    core = (t.get("verify_compare_core_seconds") or 0) + (t.get("verify_hash_core_seconds") or 0)
                    a = core / (b_ / GIB) if b_ else None
                if a is None and old and key == "transfer_host_ms_per_op":
                    a = per_op_ms(old[1], "transfer")
                b = new[0].get(key) if new else None
                change = f"{b / a:.2f}x" if a and b else "n/a"
                lines.append(f"| {k} | {label} | {fmt(a, nd)} | {fmt(b, nd)} | {change} |")
    (root / "SUMMARY.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))


if __name__ == "__main__":
    main()

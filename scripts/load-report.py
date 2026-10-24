#!/usr/bin/env python3
"""Validate oha results, enforce tower budgets, and compare previous nightly data."""
import json
import math
import os
import pathlib
import sys


def report(root, previous, enforce_tower_budgets=True):
    # Seconds, first-byte percentiles. Mixed adopts the slowest constituent budget.
    budgets = {"album": (0.020, 0.050), "photo": (0.015, 0.040),
               "derivative": (None, 0.005), "search": (None, 0.050), "mixed": (None, 0.050)}
    old = json.loads(previous.read_text()) if enforce_tower_budgets and previous.exists() else None
    current = {"enforce_tower_budgets": enforce_tower_budgets, "scenarios": {}, "failures": []}
    rows = ["| Scenario | p50 TTFB (ms) | p99 TTFB (ms) | Previous p99 (ms) | Peak RSS (bytes) | Gate |",
            "|---|---:|---:|---:|---:|---|"]
    for name, (p50_budget, p99_budget) in budgets.items():
        data = json.loads((root / f"{name}.json").read_text())
        if (data["errorDistribution"] or not data["statusCodeDistribution"].get("200")
                or set(data["statusCodeDistribution"]) != {"200"}):
            raise ValueError(f"{name}: errors/non-200 responses invalidate the measurement")
        p50, p99 = (data["firstBytePercentiles"][key] for key in ("p50", "p99"))
        if not all(isinstance(n, (int, float)) and math.isfinite(n) and n > 0 for n in (p50, p99)):
            raise ValueError(f"{name}: missing or invalid TTFB percentiles")
        before = old["scenarios"][name]["p99"] if old else None
        if before is not None and (type(before) not in (int, float) or not math.isfinite(before) or before <= 0):
            raise ValueError(f"{name}: invalid previous nightly percentile")
        failure = p99 >= p99_budget or (p50_budget is not None and p50 >= p50_budget)
        # 5% nightly relative regression also blocks, even while inside the budget.
        failure |= before is not None and p99 > before * 1.05
        cores = json.loads((root / f"{name}.cpu.json").read_text())["cores"]
        if not isinstance(cores, (int, float)) or not math.isfinite(cores) or cores < 0:
            raise ValueError(f"{name}: missing CPU measurement")
        failure |= name == "album" and cores >= 1
        failure &= enforce_tower_budgets
        rss = [int(line) for line in (root / f"{name}.rss-kib.txt").read_text().splitlines()]
        if not rss or min(rss) <= 0:
            raise ValueError(f"{name}: no valid server RSS samples")
        peak_rss = max(rss) * 1024
        current["scenarios"][name] = {"p50": p50, "p99": p99, "cpu_cores": cores, "rss_bytes": peak_rss}
        if failure:
            current["failures"].append(name)
        prior = f"{before * 1000:.3f}" if before is not None else "first run"
        gate = ("FAIL" if failure else "pass") if enforce_tower_budgets else "reported (hosted)"
        rows.append(f"| {name} | {p50 * 1000:.3f} | {p99 * 1000:.3f} | {prior} | {peak_rss} | {gate} |")
    current["rss_bytes"] = max(s["rss_bytes"] for s in current["scenarios"].values())
    album_rss = current["scenarios"]["album"]["rss_bytes"]
    if album_rss >= 200_000_000:
        current["failures"].append("album RSS")
    rows.append(f"\nAlbum CPU: {current['scenarios']['album']['cpu_cores']:.3f} cores (Tower budget < 1). Album peak RSS: {album_rss} bytes (budget < 200000000). Overall peak RSS: {current['rss_bytes']} bytes (reported).")
    if not enforce_tower_budgets:
        rows.append("\nHosted run: latency and CPU are measurements; Tower budgets and previous Tower baselines apply only on Tower. HTTP success and RSS remain enforced.")
    if current["failures"]:
        rows.append("\nRegressions: " + ", ".join(current["failures"]))
    (root / "load.json").write_text(json.dumps(current, indent=2) + "\n")
    (root / "load.md").write_text("\n".join(rows) + "\n")
    print("\n".join(rows))
    return bool(current["failures"])


if __name__ == "__main__":
    mode = os.environ.get("YHOTOS_ENFORCE_TOWER_BUDGETS", "1")
    if mode not in ("0", "1"):
        raise ValueError("YHOTOS_ENFORCE_TOWER_BUDGETS must be 0 or 1")
    sys.exit(report(pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), mode == "1"))

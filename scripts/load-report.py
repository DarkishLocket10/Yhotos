#!/usr/bin/env python3
"""Validate oha results, enforce tower budgets, and compare previous nightly data."""
import json
import math
import pathlib
import sys


def report(root, previous):
    # Seconds, first-byte percentiles. Mixed adopts the slowest constituent budget.
    budgets = {"album": (0.020, 0.050), "photo": (0.015, 0.040),
               "derivative": (None, 0.005), "search": (None, 0.050), "mixed": (None, 0.050)}
    old = json.loads(previous.read_text()) if previous.exists() else None
    current = {"scenarios": {}, "failures": []}
    rows = ["| Scenario | p50 TTFB (ms) | p99 TTFB (ms) | Previous p99 (ms) | Gate |",
            "|---|---:|---:|---:|---|"]
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
        current["scenarios"][name] = {"p50": p50, "p99": p99, "cpu_cores": cores}
        if failure:
            current["failures"].append(name)
        prior = f"{before * 1000:.3f}" if before is not None else "first run"
        rows.append(f"| {name} | {p50 * 1000:.3f} | {p99 * 1000:.3f} | {prior} | {'FAIL' if failure else 'pass'} |")
    rss = [int(line) for line in (root / "rss-kib.txt").read_text().splitlines()]
    if not rss or min(rss) <= 0:
        raise ValueError("No server RSS samples")
    current["rss_bytes"] = max(rss) * 1024
    if current["rss_bytes"] >= 200_000_000:
        current["failures"].append("RSS")
    rows.append(f"\nAlbum CPU: {current['scenarios']['album']['cpu_cores']:.3f} cores (budget < 1). Peak server RSS: {current['rss_bytes']} bytes (budget < 200000000).")
    if current["failures"]:
        rows.append("\nRegressions: " + ", ".join(current["failures"]))
    (root / "load.json").write_text(json.dumps(current, indent=2) + "\n")
    (root / "load.md").write_text("\n".join(rows) + "\n")
    print("\n".join(rows))
    return bool(current["failures"])


if __name__ == "__main__":
    sys.exit(report(pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])))

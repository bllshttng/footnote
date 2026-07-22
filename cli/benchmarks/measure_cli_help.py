#!/usr/bin/env python3
"""Benchmark harness for ``fno --help`` startup latency.

Runs ``fno --help`` N times via subprocess and reports the median wall time.
The goal pinned by ``2026-05-14-cli-lazy-imports.md`` is a >=30% drop in
median latency from the 225ms baseline measured before the lazy-imports
refactor (target: <=158ms median).

Methodology
-----------
1. Spawn fresh ``fno --help`` subprocesses (no warm-up runs).
2. Time each invocation with ``time.perf_counter``.
3. Report p25/p50/p75/p95 from the sample.
4. Write JSON evidence to ``cli/benchmarks/cli_help_results.json``.

Usage
-----
    python cli/benchmarks/measure_cli_help.py
    python cli/benchmarks/measure_cli_help.py --runs 30
    python cli/benchmarks/measure_cli_help.py --binary /path/to/fno

Re-bench guidance
-----------------
- Run after ``rm -rf cli/.venv && uv sync`` to avoid ``__pycache__`` confounds.
- Compare against the 225ms baseline pinned in the plan, NOT against
  whatever the current branch's HEAD reports.  The point of pinning the
  baseline is that incremental drift between branches is invisible if you
  compare every measurement against its own run.
"""
from __future__ import annotations

import argparse
import json
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

SCRIPT_DIR = Path(__file__).parent.resolve()

# Pinned baseline from 2026-05-14 (pre-refactor measurement).  See plan
# ``2026-05-14-cli-lazy-imports.md`` for the methodology used to capture it.
BASELINE_MS = 225.0
TARGET_MS = 158.0  # 30% drop from the baseline


def _resolve_fno(explicit: str | None) -> str:
    if explicit:
        return explicit
    binary = shutil.which("fno")
    if not binary:
        print("error: 'fno' binary not on PATH", file=sys.stderr)
        print("  install with: uv tool install <fno-repo>/cli", file=sys.stderr)
        sys.exit(2)
    return binary


def _measure(binary: str, runs: int) -> list[float]:
    samples: list[float] = []
    for _ in range(runs):
        start = time.perf_counter()
        subprocess.run([binary, "--help"], capture_output=True, check=False)
        samples.append((time.perf_counter() - start) * 1000)
    return samples


def _summarize(samples: list[float]) -> dict[str, float]:
    sorted_samples = sorted(samples)
    n = len(sorted_samples)
    return {
        "n": n,
        "min": min(sorted_samples),
        "p25": sorted_samples[max(0, int(n * 0.25))],
        "p50": statistics.median(sorted_samples),
        "p75": sorted_samples[min(n - 1, int(n * 0.75))],
        "p95": sorted_samples[min(n - 1, int(n * 0.95))],
        "max": max(sorted_samples),
        "mean": statistics.mean(sorted_samples),
        "stdev": statistics.stdev(sorted_samples) if n > 1 else 0.0,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--runs", type=int, default=20, help="sample size (default 20)")
    parser.add_argument(
        "--binary",
        default=None,
        help="path to fno binary (default: shutil.which('fno'))",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=SCRIPT_DIR / "cli_help_results.json",
        help="path to write JSON evidence",
    )
    args = parser.parse_args()

    binary = _resolve_fno(args.binary)
    print(f"benchmark: {binary} --help  (n={args.runs})")
    samples = _measure(binary, args.runs)
    stats = _summarize(samples)

    print()
    print(f"  n:       {stats['n']}")
    print(f"  min:     {stats['min']:.1f}ms")
    print(f"  p25:     {stats['p25']:.1f}ms")
    print(f"  p50:     {stats['p50']:.1f}ms  (median)")
    print(f"  p75:     {stats['p75']:.1f}ms")
    print(f"  p95:     {stats['p95']:.1f}ms")
    print(f"  max:     {stats['max']:.1f}ms")
    print(f"  mean:    {stats['mean']:.1f}ms")
    print(f"  stdev:   {stats['stdev']:.1f}ms")
    print()
    print(f"  baseline: {BASELINE_MS:.1f}ms (pinned 2026-05-14, pre-refactor)")
    print(f"  target:   <={TARGET_MS:.1f}ms (>=30% drop)")
    median = stats["p50"]
    pct_drop = (BASELINE_MS - median) / BASELINE_MS * 100
    passed = median <= TARGET_MS
    print(
        f"  result:   {median:.1f}ms "
        f"({pct_drop:+.1f}% vs baseline) -- {'PASS' if passed else 'FAIL'}"
    )

    evidence: dict[str, Any] = {
        "binary": binary,
        "baseline_ms": BASELINE_MS,
        "target_ms": TARGET_MS,
        "stats": stats,
        "samples": samples,
        "pct_drop": pct_drop,
        "passed_ac1_hp": passed,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    print(f"  evidence: {args.output}")

    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())

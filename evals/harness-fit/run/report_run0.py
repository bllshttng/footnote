#!/usr/bin/env python3
"""Run 0 report numbers: per arm, paired by task, and the cost bases decision.md quotes.

  report_run0.py final   the record as it stands: every job directory in the run workspace
  report_run0.py first   the record before Amendment 12: each re-run trial swapped back
                         for the original attempt setaside_run0.py moved to aside-amendment-12/

Prints JSON. The per-arm summary is score_run0's. Pairs are bootstrapped over tasks,
4,000 resamples, seed 272. Run through `uv run --with pyyaml` from this directory.
"""
import json
import random
import sys
from pathlib import Path

import score_run0 as s
from paths import LOGS, RUNS, WS

ARMS = ["pi", "claude-code", "opencode", "terminus-2"]
PAIRS = [("claude-code", "terminus-2"), ("opencode", "terminus-2"), ("pi", "terminus-2"),
         ("opencode", "claude-code"), ("pi", "claude-code")]
SETASIDE = LOGS / "amendment12-setaside.jsonl"
ASIDE = WS / "aside-amendment-12"


def setaside() -> list:
    return [json.loads(line) for line in SETASIDE.read_text().splitlines()] if SETASIDE.is_file() else []


def trial_dirs(arm: str, mode: str) -> list:
    """One directory per task. `first` swaps each Amendment 12 re-run for its set-aside original."""
    dirs = {}
    for res in sorted((RUNS / f"harness-fit-run0-{arm}").glob("*/result.json")):
        dirs[res.parent.name.split("__")[0]] = res.parent
    if mode == "first":
        for rec in setaside():
            if rec["arm"] == arm:
                dirs[rec["task"]] = ASIDE / f"run0-{arm}" / rec["trial"]
    return sorted(dirs.values())


def bootstrap(a: dict, b: dict, n: int = 4000, seed: int = 272) -> dict:
    tasks = sorted(set(a) & set(b))
    if not tasks:
        return {"tasks": 0}
    diff = [(a[t]["passed"] - b[t]["passed"]) for t in tasks]
    rng = random.Random(seed)
    means = sorted(sum(rng.choices(diff, k=len(diff))) / len(diff) for _ in range(n))
    return {"tasks": len(tasks),
            "first_only": sum(a[t]["passed"] and not b[t]["passed"] for t in tasks),
            "second_only": sum(b[t]["passed"] and not a[t]["passed"] for t in tasks),
            "difference_points": round(100 * sum(diff) / len(diff), 1),
            "ci95_points": [round(100 * means[int(0.025 * n)], 1), round(100 * means[int(0.975 * n) - 1], 1)]}


def per_pass(rows: list, extra_usd: float = 0.0) -> float | None:
    """Spend over every row given, plus any extra, divided by passes among the graded rows."""
    usd = sum(r["usd"] for r in rows if r["usd"] is not None) + extra_usd
    k = sum(r["passed"] for r in rows if not r["excluded"])
    return round(usd / k, 4) if k else None


def main(mode: str) -> int:
    rows = {arm: [s.trial_row(arm, d) for d in trial_dirs(arm, mode)] for arm in ARMS}
    graded = {arm: {r["task"]: r for r in rs if not r["excluded"]} for arm, rs in rows.items()}
    out = {"mode": mode, "arms": {arm: s.summary(rs) for arm, rs in rows.items()}, "paired": {}, "cost": {}}
    for a, b in PAIRS:
        out["paired"][f"{a} vs {b}"] = bootstrap(graded[a], graded[b])
    shared = sorted(set(graded["opencode"]) & set(graded["claude-code"]))
    for arm in ("opencode", "claude-code"):
        out["cost"][arm] = {
            "all_spend_per_pass": per_pass(rows[arm]),
            "shared_tasks_per_pass": per_pass([graded[arm][t] for t in shared]),
            "graded_only_per_pass": per_pass(list(graded[arm].values())),
        }
    if mode == "final":
        recs = setaside()
        out["set_aside"] = {arm: {"trials": sum(r["arm"] == arm for r in recs),
                                  "usd": round(sum(r["usd"] or 0 for r in recs if r["arm"] == arm), 4)}
                            for arm in ARMS}
        for arm in ("opencode", "claude-code"):
            out["cost"][arm]["all_spend_with_set_aside_per_pass"] = per_pass(rows[arm], out["set_aside"][arm]["usd"])
        retried = {(r["arm"], r["task"]) for r in recs}
        out["retries"] = {arm: {"retried": sum(1 for r in rows[arm] if (arm, r["task"]) in retried),
                                "now_graded": sum(1 for r in rows[arm] if (arm, r["task"]) in retried and not r["excluded"]),
                                "1302_again": sum(1 for r in rows[arm] if (arm, r["task"]) in retried
                                                  and r["excluded"] == "infra-1302"),
                                "max_load1": max((r["load_max"] or 0 for r in rows[arm] if (arm, r["task"]) in retried),
                                                 default=None)}
                          for arm in ARMS}
    json.dump(out, sys.stdout, indent=1)
    print()
    return 0


if __name__ == "__main__":
    if sys.argv[1:] not in (["final"], ["first"]):
        print(__doc__)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1]))

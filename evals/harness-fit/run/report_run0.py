#!/usr/bin/env python3
"""Run 0 report numbers: per arm, paired by task, and the cost bases decision.md quotes.

  report_run0.py final     the record that decides (Amendment 13): each original attempt
                           stands unless it really hit the rate limit; then its retry stands
  report_run0.py first     one pass: every Amendment 12 retry swapped back for the original
                           attempt setaside_run0.py moved to aside-amendment-12/
  report_run0.py retried   every Amendment 12 retry standing, as the job directories hold them

With HARNESS_FIT_LEGACY_1302=1, `first` and `retried` reproduce the tables published
before Amendment 13, which read any "1302" substring as a rate limit.

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


def original(rec: dict) -> Path:
    return ASIDE / f"run0-{rec['arm']}" / rec["trial"]


def really_rate_limited(rec: dict) -> bool:
    return s.trial_row(rec["arm"], original(rec))["excluded"] == "infra-1302"


def trial_dirs(arm: str, mode: str) -> list:
    """One directory per task, chosen by mode (see the module docstring)."""
    dirs = {}
    for res in sorted((RUNS / f"harness-fit-run0-{arm}").glob("*/result.json")):
        dirs[res.parent.name.split("__")[0]] = res.parent
    for rec in setaside():
        if rec["arm"] != arm or mode == "retried":
            continue
        if mode == "first" or not really_rate_limited(rec):
            dirs[rec["task"]] = original(rec)
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
        genuine = [r for r in recs if really_rate_limited(r)]
        genuine_keys = {(r["arm"], r["task"]) for r in genuine}
        retry_rows = {(arm, r["task"]): r for arm in ARMS
                      for r in (s.trial_row(arm, res.parent)
                                for res in (RUNS / f"harness-fit-run0-{arm}").glob("*/result.json"))}
        # Two kinds of attempt sit outside the record: a really rate-limited original
        # (its retry stands), and a retry of a mis-flagged original (the original stands).
        out["outside_record"] = {
            arm: {"rate_limited_originals": sum(r["arm"] == arm for r in genuine),
                  "rate_limited_originals_usd": round(sum(r["usd"] or 0 for r in genuine if r["arm"] == arm), 4),
                  "discarded_retries": sum(r["arm"] == arm and (arm, r["task"]) not in genuine_keys for r in recs),
                  "discarded_retries_usd": round(sum(retry_rows[(arm, r["task"])]["usd"] or 0 for r in recs
                                                     if r["arm"] == arm and (arm, r["task"]) not in genuine_keys), 4)}
            for arm in ARMS}
        for arm in ("opencode", "claude-code"):
            o = out["outside_record"][arm]
            out["cost"][arm]["all_spend_with_outside_record_per_pass"] = per_pass(
                rows[arm], o["rate_limited_originals_usd"] + o["discarded_retries_usd"])
        out["retries"] = {arm: {"retried": sum(1 for k in genuine_keys if k[0] == arm),
                                "graded_on_retry": sum(1 for k in genuine_keys
                                                       if k[0] == arm and not retry_rows[k]["excluded"]),
                                "1302_again": sum(1 for k in genuine_keys
                                                  if k[0] == arm and retry_rows[k]["excluded"] == "infra-1302")}
                          for arm in ARMS}
    json.dump(out, sys.stdout, indent=1)
    print()
    return 0


if __name__ == "__main__":
    if sys.argv[1:] not in (["final"], ["first"], ["retried"]):
        print(__doc__)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1]))

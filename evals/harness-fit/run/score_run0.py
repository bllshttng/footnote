#!/usr/bin/env python3
"""Score Run 0: one row per trial, then per-arm and paired tables.

  score_run0.py            -> JSON on stdout (per-arm summary + paired counts + trials)

Rules (README, amendments 1-2 and 13): a timeout with a 1302 error in the agent
log is an infrastructure exclusion, also reported scored. An environment
exception is an infrastructure exclusion. Reasoning tokens join output. A trial
with no usage reads unmeasured, never zero.

HARNESS_FIT_LEGACY_1302=1 restores the substring test the first results used,
so those tables stay reproducible (Amendment 13).
"""
import json
import math
import os
import re
import sys
from pathlib import Path

import loadlog
from paths import RUNS

LOAD = loadlog.samples()
ARMS = ["claude-code", "opencode", "pi", "terminus-2", "zcode"]
REF = "terminus-2"
P = {"input_per_m": 0.15, "output_per_m": 0.5, "cache_read_per_m": 0.03}
# z.ai's 1302 error carries this message in every harness's log format. A bare
# "1302" also matches token counters, uuids and line numbers (Amendment 13).
RATE_LIMIT = re.compile(r"Rate limit reached for requests")
LEGACY_1302 = os.environ.get("HARNESS_FIT_LEGACY_1302") == "1"


def has_1302(trial: Path) -> bool:
    for f in (trial / "agent").rglob("*"):
        if f.is_file() and f.stat().st_size < 200_000_000:
            try:
                text = f.read_text(errors="ignore")
            except OSError:
                continue
            if ("1302" in text) if LEGACY_1302 else bool(RATE_LIMIT.search(text)):
                return True
    return False


def reasoning(trial: Path) -> int:
    total = 0
    for f in (trial / "agent").rglob("opencode.txt"):
        for line in f.read_text(errors="ignore").splitlines():
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if row.get("type") == "step_finish":
                total += (row.get("part", {}).get("tokens", {}) or {}).get("reasoning") or 0
    return total


def trial_row(arm: str, trial: Path) -> dict:
    d = json.loads((trial / "result.json").read_text())
    info = d.get("exception_info") or {}
    exc = info.get("exception_type")
    # Docker down (the environment never started) is infrastructure, not the arm.
    docker_down = "Docker compose command failed" in (info.get("exception_message") or "")
    reward = ((d.get("verifier_result") or {}).get("rewards") or {}).get("reward")
    a = d.get("agent_result") or {}
    measured = bool(a.get("n_input_tokens") or a.get("n_output_tokens"))
    tin, cache, out = a.get("n_input_tokens") or 0, a.get("n_cache_tokens") or 0, a.get("n_output_tokens") or 0
    out += reasoning(trial)
    timeout = bool(exc and "Timeout" in exc and "Environment" not in exc)
    r1302 = timeout and has_1302(trial)
    infra = r1302 or docker_down or bool(exc and ("Environment" in exc or "Docker" in exc))
    usd = (max(tin - cache, 0) * P["input_per_m"] + cache * P["cache_read_per_m"] + out * P["output_per_m"]) / 1e6
    load = None
    if d.get("started_at") and d.get("finished_at"):
        load = loadlog.max_load(loadlog.ts(d["started_at"]), loadlog.ts(d["finished_at"]), LOAD)
    return {"arm": arm, "task": d["task_name"].split("/")[-1], "reward": reward, "passed": reward == 1.0,
            "load_max": load,
            "exception": exc, "timeout_1302": r1302, "excluded": "infra-1302" if r1302 else ("infra-env" if infra else None),
            "measured": measured, "tokens": {"input": tin, "cache_read": cache, "output": out},
            "usd": round(usd, 5) if measured else None}


def wilson(k: int, n: int) -> list:
    if n == 0:
        return [0.0, 0.0]
    z, p = 1.96, k / n
    c = (p + z * z / (2 * n)) / (1 + z * z / n)
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / (1 + z * z / n)
    return [round(max(c - h, 0), 3), round(min(c + h, 1), 3)]


def summary(rows: list) -> dict:
    graded = [r for r in rows if not r["excluded"]]
    k = sum(r["passed"] for r in graded)
    with1302 = [r for r in rows if r["excluded"] != "infra-env"]
    k2 = sum(r["passed"] for r in with1302)
    meas = [r for r in rows if r["measured"]]
    usd = sum(r["usd"] for r in meas)
    by = {}
    for r in rows:
        if r["excluded"]:
            by[r["excluded"]] = by.get(r["excluded"], 0) + 1
    exc = {}
    for r in graded:
        if r["exception"]:
            exc[r["exception"]] = exc.get(r["exception"], 0) + 1
    # Post hoc (Amendment 5): the same rate without graded trials whose window saw load above 50.
    # A trial with no load sample (before 16:40Z on 09-30) stays in: its load is unknown.
    calm = [r for r in graded if r["load_max"] is None or r["load_max"] <= loadlog.HIGH]
    kc = sum(r["passed"] for r in calm)
    return {"trials": len(rows), "graded": len(graded), "passed": k,
            "rate": round(k / len(graded), 3) if graded else None, "ci95": wilson(k, len(graded)),
            "load_known": sum(r["load_max"] is not None for r in rows),
            "graded_high_load": len(graded) - len(calm),
            "rate_without_high_load": round(kc / len(calm), 3) if calm else None,
            "rate_1302_scored": round(k2 / len(with1302), 3) if with1302 else None,
            "excluded": by, "graded_exceptions": exc, "unmeasured": len(rows) - len(meas),
            "tokens": {t: sum(r["tokens"][t] for r in meas) for t in ("input", "cache_read", "output")},
            "usd": round(usd, 2), "usd_per_pass": round(usd / k, 3) if k else None}


def main() -> int:
    rows = []
    for arm in ARMS:
        job = RUNS / f"harness-fit-run0-{arm}"
        for res in sorted(job.glob("*/result.json")):
            rows.append(trial_row(arm, res.parent))
    arms = {arm: summary([r for r in rows if r["arm"] == arm]) for arm in ARMS}
    ref = {r["task"]: r for r in rows if r["arm"] == REF and not r["excluded"]}
    paired = {}
    for arm in ARMS:
        if arm == REF:
            continue
        mine = {r["task"]: r for r in rows if r["arm"] == arm and not r["excluded"]}
        both = sorted(set(mine) & set(ref))
        paired[arm] = {"tasks": len(both),
                       "arm_only": sum(mine[t]["passed"] and not ref[t]["passed"] for t in both),
                       "ref_only": sum(ref[t]["passed"] and not mine[t]["passed"] for t in both),
                       "both": sum(mine[t]["passed"] and ref[t]["passed"] for t in both)}
    json.dump({"arms": arms, "paired_vs_" + REF: paired, "trials": rows}, sys.stdout, indent=1)
    return 0


if __name__ == "__main__":
    sys.exit(main())

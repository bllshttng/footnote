#!/usr/bin/env python3
"""Score Run 1 from the eval history under the preregistered rules.

  score_run1.py  -> JSON on stdout: per lane and per task.

Rules (README amendments 2 and 4): a stall (worker ran out its budget) is a
scored attempt with no accepted change. opencode rows are re-read from the
opencode store by session id: model = providerID/modelID, output += reasoning.
Unavailable, substituted and contaminated rows are counted by reason, never
scored. A row with lane_status unverified counts as an attempt, never a rule.
"""
import json
import math
import sqlite3
import sys
from pathlib import Path

import loadlog
from paths import HISTORY

LOAD = loadlog.samples()
OPENCODE_DB = Path.home() / ".local/share/opencode/opencode.db"
LANES = ["claude", "opencode", "pi", "zcode"]
P = {"input_per_m": 0.15, "output_per_m": 0.5, "cache_read_per_m": 0.03, "cache_write_per_m": 0.15}


def reread_opencode(session_id: str) -> tuple:
    con = sqlite3.connect(f"file:{OPENCODE_DB}?mode=ro", uri=True)
    model, u = None, {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0}
    for (data,) in con.execute("SELECT data FROM message WHERE session_id=? ORDER BY time_created", (session_id,)):
        m = json.loads(data)
        if m.get("role") != "assistant":
            continue
        if m.get("modelID"):
            model = f"{m['providerID']}/{m['modelID']}" if m.get("providerID") else m["modelID"]
        t = m.get("tokens") or {}
        if t.get("input") is None or t.get("output") is None:
            continue
        cache = t.get("cache") or {}
        u["input"] += t["input"]
        u["output"] += t["output"] + (t.get("reasoning") or 0)
        u["cache_read"] += cache.get("read") or 0
        u["cache_write"] += cache.get("write") or 0
    return model, u


def usd(u: dict) -> float:
    return (u["input"] * P["input_per_m"] + u["output"] * P["output_per_m"]
            + u["cache_read"] * P["cache_read_per_m"] + u["cache_write"] * P["cache_write_per_m"]) / 1e6


def wilson(k: int, n: int) -> list:
    if n == 0:
        return [0.0, 0.0]
    z, p = 1.96, k / n
    c = (p + z * z / (2 * n)) / (1 + z * z / n)
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / (1 + z * z / n)
    return [round(max(c - h, 0), 3), round(min(c + h, 1), 3)]


def family(model: str) -> str:
    """One trailing bracket suffix dropped, the registry's model-family rule."""
    model = model.strip().lower()
    return model[:model.rfind("[")] if model.endswith("]") and model.rfind("[") > 0 else model


def classify(r: dict) -> dict:
    lane = r["experiment_id"].removeprefix("harness-fit-")
    stalled = "timed out after" in (r.get("reason") or "")
    lane_status, usage, model = r.get("lane_status"), r.get("usage"), r.get("observed_model")
    if lane == "opencode" and r.get("observed_session_id"):
        model, usage = reread_opencode(r["observed_session_id"])
        lane_status = "ok" if model == r.get("requested_model") else "substituted"
    if lane == "claude" and model and lane_status == "substituted":
        # Amendment 10: the transcript stores the bare model, the lane asks with `[1m]`.
        lane_status = "ok" if family(model) == family(r.get("requested_model") or "") else "substituted"
    if stalled:
        kind = "stall"
    elif "Rate limit reached" in (r.get("reason") or ""):
        kind = "excluded:rate-limit"
    elif r.get("excluded_reason"):
        kind = f"excluded:{r['excluded_reason']}"
    elif r.get("status") != "graded":
        kind = f"excluded:{r.get('status')}"
    elif lane_status == "substituted":
        kind = "excluded:substituted"
    else:
        kind = "graded"
    end = loadlog.ts(r["ts"])
    return {"lane": lane, "task": r["task_id"], "kind": kind, "pass": bool(r.get("pass")) and kind == "graded",
            "load_max": loadlog.max_load(end - (r.get("duration_s") or 0), end, LOAD),
            "lane_status": lane_status, "model": model, "usage": usage,
            "usd": round(usd(usage), 5) if usage else None, "wall_s": r.get("duration_s")}


def summary(rows: list) -> dict:
    scored = [r for r in rows if r["kind"] in ("graded", "stall")]
    graded = [r for r in rows if r["kind"] == "graded"]
    verified = [r for r in scored if r["lane_status"] == "ok" or r["kind"] == "stall"]
    k = sum(r["pass"] for r in scored)
    priced = [r for r in scored if r["usd"] is not None]
    by = {}
    for r in rows:
        if r["kind"].startswith("excluded:"):
            by[r["kind"][9:]] = by.get(r["kind"][9:], 0) + 1
    walls = sorted(r["wall_s"] for r in graded if r["wall_s"])
    # Post hoc (Amendment 5): the rate without scored attempts whose window saw load above 50.
    calm = [r for r in scored if r["load_max"] is None or r["load_max"] <= loadlog.HIGH]
    return {"attempts": len(rows), "scored": len(scored), "stalls": len(scored) - len(graded),
            "load_known": sum(r["load_max"] is not None for r in rows),
            "scored_high_load": len(scored) - len(calm),
            "stalls_high_load": sum(r["kind"] == "stall" for r in scored if r not in calm),
            "rate_without_high_load": round(sum(r["pass"] for r in calm) / len(calm), 3) if calm else None,
            "verified_scored": len(verified), "accepted": k,
            "rate": round(k / len(scored), 3) if scored else None, "ci95": wilson(k, len(scored)),
            "rate_without_stalls": round(k / len(graded), 3) if graded else None,
            "rate_rate_limit_scored": round(k / (len(scored) + by.get("rate-limit", 0)), 3) if scored else None,
            "excluded": by, "unmeasured": len(scored) - len(priced),
            "usd": round(sum(r["usd"] for r in priced), 3),
            "usd_per_accepted": round(sum(r["usd"] for r in priced) / k, 3) if k else None,
            "median_wall_s": walls[len(walls) // 2] if walls else None}


def main() -> int:
    rows = []
    for line in HISTORY.read_text().splitlines():
        try:
            r = json.loads(line)
        except ValueError:
            continue
        cohort = r.get("experiment_id") or ""
        if cohort in {f"harness-fit-{lane}" for lane in LANES}:
            rows.append(classify(r))
    lanes = {lane: summary([r for r in rows if r["lane"] == lane]) for lane in LANES}
    tasks = sorted({r["task"] for r in rows})
    per_task = {t: {lane: f"{sum(r['pass'] for r in rows if r['lane'] == lane and r['task'] == t)}/"
                           f"{sum(r['kind'] in ('graded', 'stall') for r in rows if r['lane'] == lane and r['task'] == t)}"
                    for lane in LANES} for t in tasks}
    json.dump({"lanes": lanes, "per_task": per_task, "rows": rows}, sys.stdout, indent=1)
    return 0


if __name__ == "__main__":
    sys.exit(main())

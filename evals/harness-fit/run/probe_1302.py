#!/usr/bin/env python3
"""Run 0: trials that did not time out yet carry a 1302 / rate-limit error in the agent log.

  probe_1302.py [arm...]  -> per arm: count, and task, reward, output tokens, exception for each.
These are graded under Amendment 2 (the exclusion needs a timeout). run-0.md reports them.
"""
import json
import sys

import score_run0 as s

for arm in sys.argv[1:] or s.ARMS:
    hits = []
    for res in sorted((s.RUNS / f"harness-fit-run0-{arm}").glob("*/result.json")):
        row = s.trial_row(arm, res.parent)
        if row["excluded"]:
            continue
        text = ""
        for f in (res.parent / "agent").rglob("*"):
            if f.is_file() and f.stat().st_size < 200_000_000:
                text += f.read_text(errors="ignore")
        if s.RATE_LIMIT.search(text):
            # pi: the run ended on a failed retry of a 1302 (3 tries, then the agent stops, exit 0).
            log = res.parent / "agent" / "pi.txt"
            tail = log.read_text(errors="ignore")[-3000:] if log.is_file() else ""
            died = '"type":"auto_retry_end","success":false' in tail and bool(s.RATE_LIMIT.search(tail))
            hits.append((row["task"], row["reward"], row["tokens"]["output"], row["exception"], "DIED" if died else ""))
    passed = sum(1 for h in hits if h[1] == 1.0)
    died = [h for h in hits if h[4]]
    print(json.dumps({"arm": arm, "graded_with_rate_limit_in_log": len(hits), "of_those_passed": passed,
                      "ended_on_failed_1302_retry": len(died), "died_passed": sum(1 for h in died if h[1] == 1.0)}))
    for h in hits:
        print("  ", *h)

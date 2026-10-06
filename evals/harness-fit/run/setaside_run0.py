#!/usr/bin/env python3
"""Amendment 12: set aside one Run 0 arm's 1302-excluded trials before their one retry.

  setaside_run0.py <arm>   move each 1302-excluded trial of the arm's job to
                           aside-amendment-12/run0-<arm>/ in the run workspace,
                           and append its record to logs/amendment12-setaside.jsonl

Then `launch.py continue <arm>` runs the missing trials again in the same job.
A trial already in the log is never set aside twice, so a retry that hits 1302
again stays in the job, excluded (Amendment 12: one retry). Run through
`uv run --with pyyaml` from this directory.
"""
import json
import shutil
import sys

import score_run0 as s
from paths import LOGS, RUNS, WS

LOG = LOGS / "amendment12-setaside.jsonl"


def main(arm: str) -> int:
    job = RUNS / f"harness-fit-run0-{arm}"
    dest = WS / "aside-amendment-12" / f"run0-{arm}"
    done = set()
    if LOG.is_file():
        done = {(r["arm"], r["task"]) for r in map(json.loads, LOG.read_text().splitlines())}
    dest.mkdir(parents=True, exist_ok=True)
    moved = []
    for res in sorted(job.glob("*/result.json")):
        row = s.trial_row(arm, res.parent)
        if row["excluded"] != "infra-1302" or (arm, row["task"]) in done:
            continue
        d = json.loads(res.read_text())
        moved.append({"arm": arm, "trial": res.parent.name, "task": row["task"],
                      "started_at": d.get("started_at"), "finished_at": d.get("finished_at"),
                      "tokens": row["tokens"], "usd": row["usd"], "exception": row["exception"]})
        shutil.move(str(res.parent), str(dest / res.parent.name))
    with LOG.open("a") as f:
        for rec in moved:
            f.write(json.dumps(rec) + "\n")
    print(json.dumps({"arm": arm, "set_aside": len(moved), "to": str(dest)}))
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(__doc__)
        raise SystemExit(2)
    raise SystemExit(main(sys.argv[1]))

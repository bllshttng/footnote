#!/usr/bin/env python3
"""Run 0 driver: smokes, then the arms in one seeded order, one Harbor job at a time.

  drive_run0.py <arm>...

An arm whose smoke fails reads unavailable and is skipped. A finished job is
skipped; a killed one continues in place. Any failed arm stops the driver.
Writes logs/run0-driver.jsonl in the run workspace.
"""
import json
import random
import subprocess
import sys
import time

from paths import HERE, LOGS, RUNS, WS

SEED = 272
LOG = LOGS / "run0-driver.jsonl"


def note(**row) -> None:
    row["at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    with LOG.open("a") as f:
        f.write(json.dumps(row) + "\n")


def launch(mode: str, arm: str, log: str) -> int:
    with (LOGS / log).open("a") as out:
        return subprocess.call(["uv", "run", "--quiet", "--with", "pyyaml", "python", str(HERE / "launch.py"), mode, arm],
                               cwd=WS, stdout=out, stderr=subprocess.STDOUT)


def smoke_ok(arm: str) -> bool:
    res = RUNS / f"harness-fit-smoke2-{arm}" / "result.json"
    if not res.exists():
        launch("smoke", arm, f"smoke-{arm}.log")
    if not res.exists():
        return False
    stats = json.loads(res.read_text()).get("stats") or {}
    # Reaching the endpoint is the smoke's question, not solving the task: any tokens back is a pass.
    if stats.get("n_output_tokens") or stats.get("n_input_tokens"):
        return True
    # Harbor counts tokens per finished turn. A turn that streams until the task timeout
    # reports zero, yet the model answered (Amendment 10): the agent log holds the stream.
    for log in res.parent.glob("*/agent/claude-code.txt"):
        stream = log.read_bytes()
        if b'"thinking_tokens"' in stream or b'"type":"assistant"' in stream:
            return True
    return False


def main(arms: list[str]) -> int:
    LOGS.mkdir(parents=True, exist_ok=True)
    order = sorted(arms)
    random.Random(SEED).shuffle(order)
    note(event="order", seed=SEED, order=order)
    for arm in order:
        job = RUNS / f"harness-fit-run0-{arm}"
        if (job / "result.json").exists() and json.loads((job / "result.json").read_text()).get("finished_at"):
            note(event="skip-finished", arm=arm)
            continue
        if not smoke_ok(arm):
            note(event="unavailable", arm=arm, reason="smoke failed")
            continue
        mode = "continue" if (job / "config.json").exists() else "full"
        note(event="start", arm=arm, mode=mode)
        rc = launch(mode, arm, f"run0-{arm}.log")
        note(event="end", arm=arm, exit=rc)
        if rc != 0:
            # 3 = ceiling. Anything else: the job died; never start the next arm on top of it.
            note(event="stop", reason="ceiling" if rc == 3 else f"arm exit {rc}")
            return rc
    note(event="done")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))

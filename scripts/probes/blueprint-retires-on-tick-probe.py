#!/usr/bin/env python3
"""Post-deploy probe: a finished blueprint retires on the daemon tick (x-d8bc).

Read-only. Scans the agents event journal inside the --since window and exits 0
only when all three conditions hold:

1. At least one `retire_holds` event with data.scheduler == "daemon". Only the
   new build writes it, so a pass proves the deployed daemon runs this change.
2. An `agent_row_reaped` event from source `daemon` whose row name parses as a
   blueprint dispatch name (`bp-...` or `<source>-bp-...`) and whose basis
   starts with `planning finished on` or `planning halted on`.
3. No `agent_removed` event with actor `operator` for that name earlier in the
   window - an operator hand-removal would fake the retirement.

The first missing condition prints to stderr and the probe exits 1.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import re
import sys

SINCE_RE = re.compile(r"^(\d+)h$")


def parse_since(raw: str) -> dt.datetime:
    match = SINCE_RE.match(raw.strip())
    if not match:
        raise ValueError(f"--since wants <N>h, got {raw!r}")
    hours = int(match.group(1))
    now = dt.datetime.now(dt.timezone.utc)
    return now - dt.timedelta(hours=hours)


def parse_ts(raw: str) -> dt.datetime | None:
    text = (raw or "").strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    try:
        parsed = dt.datetime.fromisoformat(text)
    except (ValueError, AttributeError, TypeError):
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed.astimezone(dt.timezone.utc)


def is_blueprint_name(name: str | None) -> bool:
    """The dispatch-grammar shape the plan names: `bp-...` or `<src>-bp-...`."""
    if not name:
        return False
    tokens = name.split("-")
    return tokens[0] == "bp" or (len(tokens) > 1 and tokens[1] == "bp")


def blueprint_reap_rows(events: list[dict]) -> list[dict]:
    rows = []
    for event in events:
        if event.get("type") != "agent_row_reaped":
            continue
        if event.get("source") != "daemon":
            continue
        data = event.get("data") or {}
        name = data.get("name")
        basis = data.get("basis") or ""
        if not is_blueprint_name(name):
            continue
        if not basis.startswith(("planning finished on", "planning halted on")):
            continue
        rows.append({"name": name, "ts": event.get("ts", ""), "basis": basis})
    return rows


def operator_removed_earlier(events: list[dict], name: str, reap_ts: str) -> bool:
    reap_at = parse_ts(reap_ts)
    for event in events:
        if event.get("type") != "agent_removed":
            continue
        data = event.get("data") or {}
        if data.get("actor") != "operator" or data.get("name") != name:
            continue
        removed_at = parse_ts(event.get("ts", ""))
        if removed_at is None or reap_at is None or removed_at < reap_at:
            return True
    return False


def run_probe(events_path: pathlib.Path, since: dt.datetime) -> tuple[bool, str, list[dict]]:
    events: list[dict] = []
    try:
        for line in events_path.read_text().splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    except OSError as error:
        return False, f"cannot read {events_path}: {error}", []

    window = []
    for event in events:
        ts = parse_ts(event.get("ts", ""))
        if ts is not None and ts >= since:
            window.append(event)

    holds = [
        e
        for e in window
        if e.get("type") == "retire_holds"
        and (e.get("data") or {}).get("scheduler") == "daemon"
    ]
    if not holds:
        return (
            False,
            "no daemon retire_holds event in the window: the deployed daemon "
            "is not running the x-d8bc build, or no tick held rows yet",
            [],
        )

    rows = blueprint_reap_rows(window)
    if not rows:
        return (
            False,
            "no agent_row_reaped event from source daemon with a blueprint "
            "name and a planning finished/halted basis in the window",
            [],
        )

    for row in rows:
        if operator_removed_earlier(window, row["name"], row["ts"]):
            return (
                False,
                f"{row['name']} was removed by an operator hand-removal before "
                "the daemon retired it; the tick cannot take the credit",
                rows,
            )
    return True, "", rows


def self_test() -> int:
    """The three conditions against inline fixtures: pass, and each failure."""
    now = dt.datetime.now(dt.timezone.utc)

    def ev(kind: str, source: str, minutes_ago: int, data: dict) -> dict:
        ts = (now - dt.timedelta(minutes=minutes_ago)).isoformat().replace("+00:00", "Z")
        return {"ts": ts, "type": kind, "source": source, "data": data}

    good = [
        ev("control_plane_tick", "loop", 40, {"arm": "retire"}),
        ev("retire_holds", "loop", 39, {"scheduler": "daemon", "holds": []}),
        ev(
            "agent_row_reaped",
            "daemon",
            38,
            {"name": "bp-x-1", "basis": "planning halted on x-1: turn ended with no plan"},
        ),
    ]
    ok, why, rows = run_probe_window(good, now - dt.timedelta(hours=2))
    assert ok and len(rows) == 1 and rows[0]["name"] == "bp-x-1", why or rows
    assert not is_blueprint_name("t-x-4-slug"), "non-bp names never parse as blueprint"
    assert is_blueprint_name("sob-bp-x-2-slug"), "sourced bp names parse"

    # Condition 1 missing: no retire_holds row.
    no_holds = [good[0], good[2]]
    ok, why, _ = run_probe_window(no_holds, now - dt.timedelta(hours=2))
    assert not ok and "retire_holds" in why, why

    # Condition 3: an operator hand-removal before the reaped event fails.
    with_op = good + [
        ev("agent_removed", "daemon", 39, {"actor": "operator", "name": "bp-x-1"})
    ]
    ok, why, _ = run_probe_window(with_op, now - dt.timedelta(hours=2))
    assert not ok and "operator" in why, why

    # Wrong basis never counts.
    wrong_basis = [
        good[1],
        ev("agent_row_reaped", "daemon", 38, {"name": "bp-x-1", "basis": "every named node done: x-1"}),
    ]
    ok, why, _ = run_probe_window(wrong_basis, now - dt.timedelta(hours=2))
    assert not ok and "agent_row_reaped" in why, why
    del ok, ev, now, good, no_holds, with_op, wrong_basis
    print("self-test: ok")
    return 0


def run_probe_window(events: list[dict], since: dt.datetime) -> tuple[bool, str, list[dict]]:
    """The probe over an in-memory event list (the self-test seam)."""
    window = [e for e in events if (parse_ts(e.get("ts", "")) or since) >= since]
    holds = [
        e
        for e in window
        if e.get("type") == "retire_holds"
        and (e.get("data") or {}).get("scheduler") == "daemon"
    ]
    if not holds:
        return False, "no daemon retire_holds event in the window", []
    rows = blueprint_reap_rows(window)
    if not rows:
        return False, "no agent_row_reaped event from source daemon with a blueprint name and a planning finished/halted basis in the window", []
    for row in rows:
        if operator_removed_earlier(window, row["name"], row["ts"]):
            return False, f"{row['name']} was removed by an operator hand-removal before the daemon retired it; the tick cannot take the credit", rows
    return True, "", rows


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--since", help="window lower bound as <N>h, e.g. 3h")
    ap.add_argument(
        "--events",
        type=pathlib.Path,
        default=pathlib.Path.home() / ".fno" / "agents" / "events.jsonl",
        help="agents event journal (default ~/.fno/agents/events.jsonl)",
    )
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="run the three conditions against inline fixtures and exit",
    )
    args = ap.parse_args()
    if not args.self_test and not args.since:
        ap.error("--since is required (e.g. --since 3h)")
    if args.self_test:
        return self_test()
    since = parse_since(args.since)
    ok, why, rows = run_probe(args.events, since)
    if not ok:
        print(f"FAIL: {why}", file=sys.stderr)
        return 1
    for row in rows:
        print(f"{row['ts']} retired blueprint {row['name']} ({row['basis']})")
    return 0


if __name__ == "__main__":
    sys.exit(main())

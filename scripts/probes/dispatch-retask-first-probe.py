#!/usr/bin/env python3
"""Post-deploy probe for the dispatch retask-first arm (x-3582).

Read-only. Reads the project events journal and the agents registry and
verifies, inside the --since window, both halves of one reused dispatch:

1. A `dispatch_spawned` row whose data carries `retask: retasked`. Only the
   new build writes that key, so its presence proves the deployed build runs
   the reuse arm.
2. The registry row named by that row's `agent_name` carries a non-empty
   `predecessor_session_ids`, the lineage a completed retask writes.

Either condition missing prints the first failure to stderr and exits 1.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import sys
import tempfile

DEFAULT_REGISTRY = pathlib.Path.home() / ".fno" / "agents" / "registry.json"


def parse_since(raw: str) -> dt.datetime:
    """Parse '<N>h' into a UTC lower bound (the plan's window form)."""
    text = raw.strip().lower()
    if text.endswith("h") and text[:-1].isdigit():
        return dt.datetime.now(dt.timezone.utc) - dt.timedelta(hours=int(text[:-1]))
    raise argparse.ArgumentTypeError(f"--since wants <N>h (e.g. 6h), got {raw!r}")


def parse_ts(raw: object) -> "dt.datetime | None":
    try:
        text = str(raw).strip()
        if text.endswith("Z"):
            text = text[:-1] + "+00:00"
        parsed = dt.datetime.fromisoformat(text)
        return parsed.replace(tzinfo=dt.timezone.utc) if parsed.tzinfo is None else parsed.astimezone(dt.timezone.utc)
    except (ValueError, TypeError):
        return None


def retasked_rows(events_path, since: dt.datetime) -> list[tuple[str, dict]]:
    """The dispatch_spawned rows in the window whose data names a retask."""
    try:
        lines = pathlib.Path(events_path).read_text().splitlines()
    except OSError:
        return []
    rows = []
    for line in lines:
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(row, dict) or row.get("type") != "dispatch_spawned":
            continue
        data = row.get("data") or {}
        if data.get("retask") != "retasked":
            continue
        ts = parse_ts(row.get("ts"))
        if ts is None or ts < since:
            continue
        rows.append((ts.isoformat(), data))
    return rows


def lineage_of(registry_path, agent_name: str) -> list:
    """The named row's predecessor list; unreadable is unproven ([])."""
    try:
        registry = json.loads(pathlib.Path(registry_path).read_text())
    except (OSError, json.JSONDecodeError):
        return []
    agents = registry.get("agents") if isinstance(registry, dict) else None
    for entry in agents or []:
        if isinstance(entry, dict) and entry.get("name") == agent_name:
            return entry.get("predecessor_session_ids") or []
    return []


def check(events_path, registry_path, since: dt.datetime) -> tuple[bool, str]:
    """Evaluate the two conditions once; (ok, line naming what held or failed)."""
    rows = retasked_rows(events_path, since)
    if not rows:
        return False, "no dispatch_spawned row with retask: retasked inside the window"
    ts, data = rows[0]
    agent_name = data.get("agent_name") or ""
    if not lineage_of(registry_path, agent_name):
        return False, (
            f"reused row {agent_name} has no predecessor_session_ids in the registry"
        )
    return True, f"{ts} reused {data.get('reused_worker')} as {agent_name}"


def _fixture_events(ts_iso: str) -> str:
    return json.dumps(
        {
            "ts": ts_iso,
            "type": "dispatch_spawned",
            "source": "backlog",
            "data": {
                "node_id": "x-bbbb",
                "retask": "retasked",
                "reused_worker": "ac-bp-x-aaaa-slug",
                "agent_name": "bp-x-bbbb-renamed",
                "harness": "claude",
                "substrate": "thread",
                "command": "/fno:blueprint x-bbbb",
                "caller": "advance",
            },
        }
    ) + "\n"


def self_test() -> int:
    """One pass case plus one case per missing condition, on inline fixtures."""
    now = dt.datetime.now(dt.timezone.utc)
    stale = (now - dt.timedelta(hours=8)).isoformat()
    fresh = now.isoformat()
    registry_pass = json.dumps(
        {
            "agents": [
                {
                    "name": "bp-x-bbbb-renamed",
                    "predecessor_session_ids": ["old-session"],
                }
            ]
        }
    )
    registry_bare = json.dumps({"agents": [{"name": "bp-x-bbbb-renamed"}]})
    cases = [
        ("pass", _fixture_events(fresh), registry_pass, True),
        ("no-retasked-row", _fixture_events(stale), registry_pass, False),
        (
            "no-lineage",
            _fixture_events(fresh),
            registry_bare,
            False,
        ),
    ]
    failed = False
    with tempfile.TemporaryDirectory() as tmp:
        for name, events_text, registry_text, want_ok in cases:
            events = pathlib.Path(tmp) / f"{name}-events.jsonl"
            registry = pathlib.Path(tmp) / f"{name}-registry.json"
            events.write_text(events_text)
            registry.write_text(registry_text)
            ok, line = check(events, registry, now - dt.timedelta(hours=6))
            verdict = "ok" if ok == want_ok else "FAIL"
            failed = failed or ok != want_ok
            print(f"self-test {name}: {verdict} ({line})")
    return 1 if failed else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--since", help="window lower bound, <N>h (e.g. 6h)")
    ap.add_argument(
        "--events",
        type=pathlib.Path,
        default=None,
        help="events journal (default: fno.paths.project_events_json())",
    )
    ap.add_argument(
        "--registry",
        type=pathlib.Path,
        default=DEFAULT_REGISTRY,
        help="agents registry (default ~/.fno/agents/registry.json)",
    )
    ap.add_argument("--self-test", action="store_true", help="run on inline fixtures")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    if not args.since:
        ap.error("--since is required (e.g. --since 6h) unless --self-test")

    events_path = args.events
    if events_path is None:
        try:
            from fno.paths import project_events_json

            events_path = project_events_json()
        except Exception as exc:  # noqa: BLE001 - no resolvable journal: name the flag
            print(
                f"no events journal resolvable ({exc}); pass --events <path>",
                file=sys.stderr,
            )
            return 2

    ok, line = check(events_path, args.registry, parse_since(args.since))
    if ok:
        print(line)
        return 0
    print(f"FAIL: {line}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())

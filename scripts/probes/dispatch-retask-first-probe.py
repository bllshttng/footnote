#!/usr/bin/env python3
"""Post-deploy probe for the dispatch retask-first arm.

Read-only. Verifies, inside the --since window, both halves of one reused
dispatch:

1. A `dispatch_spawned` row whose data carries `retask: retasked`. Only the
   new build writes that key, so its presence proves the deployed build runs
   the reuse arm.
2. The registry row named by a matching row's `agent_name` carries a
   non-empty `predecessor_session_ids`, the lineage a completed retask writes.

Matching rows come from `fno doctor event find`, which reads every journal
and retained rotation and owns the path rules (FNO_EVENTS_PATH, the spaces
config, the canonical checkout). A missing condition prints to stderr and
exits 1.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import subprocess
import sys
import tempfile

DEFAULT_REGISTRY = pathlib.Path.home() / ".fno" / "agents" / "registry.json"


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


def find_rows(since: str) -> tuple[list, bool, str]:
    """Retasked dispatch_spawned rows in the window, via the fno binary.

    The verb reads every journal the writers write, so the probe ships no
    second resolver and cannot disagree with it about where rows live.
    Returns (rows, truncated, err): truncated means the verb collected
    fewer rows than it matched, so a verdict drawn from rows alone could
    miss the one lineaged row.
    """
    try:
        proc = subprocess.run(
            [
                "fno", "doctor", "event", "find", "dispatch_spawned",
                "--field", "retask=retasked",
                "--since", since,
                "--limit", "200",  # per journal; existence needs any one match
                "--json",
            ],
            capture_output=True,
            text=True,
        )
    except OSError as exc:
        return [], False, f"could not run fno doctor event find: {exc}"
    if proc.returncode not in (0, 3):
        lines = [ln for ln in proc.stderr.splitlines() if ln.strip()]
        detail = next(
            (ln for ln in reversed(lines) if ln.startswith("error:")),
            " ".join(proc.stderr.split()) or f"exited {proc.returncode}",
        )
        return [], False, f"fno doctor event find: {detail}"
    try:
        payload = json.loads(proc.stdout)
    except json.JSONDecodeError:
        return [], False, "fno doctor event find printed no JSON"
    rows = payload.get("matches", [])
    if proc.returncode == 3 and not rows:
        # An unreadable journal could hide the row, so absence is unproven.
        unreadable = len(payload.get("unreadable_files") or [])
        return [], False, f"fno doctor event find hit {unreadable} unreadable journal(s)"
    if not payload.get("file_count"):
        return [], False, "fno resolved no event journals to search"
    return rows, payload.get("match_count", 0) > len(rows), ""


def check(rows: list, registry_path) -> tuple[bool, str]:
    """Evaluate the two conditions once; (ok, line naming what held or failed)."""
    named = [
        row for row in rows
        if isinstance(row.get("data"), dict) and row["data"].get("agent_name")
    ]
    for row in named:
        agent_name = row["data"]["agent_name"]
        if lineage_of(registry_path, agent_name):
            return True, (
                f"{row.get('ts')} reused {row['data'].get('reused_worker')} as {agent_name}"
            )
    if not rows:
        return False, "no dispatch_spawned row with retask: retasked inside the window"
    agent_name = named[0]["data"]["agent_name"] if named else "(no agent_name)"
    return False, f"reused row {agent_name} has no predecessor_session_ids in the registry"


def self_test() -> int:
    """One pass case plus one case per missing condition, on inline rows."""
    now = dt.datetime.now(dt.timezone.utc)

    def row(ts: str, agent_name: str) -> dict:
        return {
            "ts": ts,
            "type": "dispatch_spawned",
            "data": {
                "retask": "retasked",
                "reused_worker": "ac-bp-x-aaaa-slug",
                "agent_name": agent_name,
            },
        }

    fresh = now.isoformat()
    registry = json.dumps(
        {
            "agents": [
                {"name": "bp-x-bbbb-renamed", "predecessor_session_ids": ["old-session"]},
                {"name": "bp-x-bbbb-bare"},
            ]
        }
    )
    cases = [
        ("pass", [row(fresh, "bp-x-bbbb-renamed")], True),
        # A bare row ahead of the lineaged one must not hide it: rows are one
        # flat list, so journal splits cannot reorder the verdict.
        ("lineage-behind-a-bare-row", [row(fresh, "bp-x-bbbb-bare"), row(fresh, "bp-x-bbbb-renamed")], True),
        ("no-rows", [], False),
        ("no-lineage", [row(fresh, "bp-x-bbbb-bare")], False),
    ]
    failed = False
    with tempfile.TemporaryDirectory() as tmp:
        registry_path = pathlib.Path(tmp) / "registry.json"
        registry_path.write_text(registry)
        for name, rows, want_ok in cases:
            ok, line = check(rows, registry_path)
            verdict = "ok" if ok == want_ok else "FAIL"
            failed = failed or ok != want_ok
            print(f"self-test {name}: {verdict} ({line})")
    return 1 if failed else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--since", help="window lower bound, <N>h (e.g. 6h)")
    ap.add_argument(
        "--registry",
        type=pathlib.Path,
        default=DEFAULT_REGISTRY,
        help="agents registry (default ~/.fno/agents/registry.json)",
    )
    ap.add_argument("--self-test", action="store_true", help="run on inline rows")
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    if not args.since:
        ap.error("--since is required (e.g. --since 6h) unless --self-test")

    rows, truncated, err = find_rows(args.since)
    ok, line = (False, err) if err else check(rows, args.registry)
    if not ok and truncated:
        line = f"{line}; some in-window matches were not collected (--limit)"
    if ok:
        print(line)
        return 0
    print(f"FAIL: {line}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())

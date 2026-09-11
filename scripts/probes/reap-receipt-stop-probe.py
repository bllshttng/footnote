#!/usr/bin/env python3
"""Post-deploy probe over reap receipts (x-9485).

Passive only: reads ~/.fno/agents/reap-receipts/*.json and verifies, for every
native-stop effect written since --since, that the pid the effect names is
actually gone. A confirmed-removed effect that names a live pid exits 1. A
confirmed-removed effect that names no pid (pre-x9485 receipts) is counted as
UNPROBEABLE-no-pid and named, never silently passed. A failed effect's pid is
informational: the refusal never claimed the pid died.

The active rm-with-re-minted-pane-id check stays a manual recipe (plan
20260910-reap-live-checks-post-deploy-x-9485.md); a probe that spawns and
kills on its own is a liability, not a probe.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import subprocess
import sys

RECEIPTS_SUBDIR = "reap-receipts"


def parse_since(raw: str) -> dt.datetime:
    text = raw.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    parsed = dt.datetime.fromisoformat(text)
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return parsed.astimezone(dt.timezone.utc)


def parse_reaped_at(raw: str) -> dt.datetime | None:
    try:
        return parse_since(raw)
    except (ValueError, AttributeError, TypeError):
        return None


def pid_in_detail(detail: str | None) -> int | None:
    """The pid a native-stop detail names, e.g. '...; pid 22287 gone'."""
    if not detail or "pid " not in detail:
        return None
    token = detail.split("pid ")[-1].split()[0].rstrip(";,")
    return int(token) if token.isdigit() else None


def pid_alive(pid: int) -> bool:
    return subprocess.run(["ps", "-p", str(pid)], capture_output=True).returncode == 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--since", required=True, help="ISO8601 lower bound on reaped_at")
    ap.add_argument(
        "--home",
        type=pathlib.Path,
        default=pathlib.Path.home() / ".fno" / "agents",
        help="agents home (default ~/.fno/agents); point at a tempdir in tests",
    )
    ap.add_argument("--json", action="store_true", help="machine-readable rows")
    args = ap.parse_args()

    since = parse_since(args.since)
    receipts_dir = args.home / RECEIPTS_SUBDIR
    files = sorted(receipts_dir.glob("*.json")) if receipts_dir.is_dir() else []

    rows: list[dict] = []
    for path in files:
        try:
            receipt = json.loads(path.read_text())
        except (json.JSONDecodeError, OSError):
            continue
        reaped = parse_reaped_at(receipt.get("reaped_at", ""))
        if reaped is None or reaped < since:
            continue
        for effect in receipt.get("effects", []):
            if effect.get("op") != "native-stop":
                continue
            outcome = effect.get("outcome", "")
            detail = effect.get("detail")
            pid = pid_in_detail(detail)
            row = {
                "receipt": path.name,
                "row_name": receipt.get("row_name"),
                "reaped_at": receipt.get("reaped_at"),
                "outcome": outcome,
                "pid": pid,
            }
            if pid is None:
                row["verdict"] = "UNPROBEABLE-no-pid"
            elif pid_alive(pid):
                row["verdict"] = "PID-ALIVE"
                row["detail"] = detail
            elif outcome == "confirmed-removed":
                row["verdict"] = "pid-gone"
            else:
                # failed / other outcomes never claimed the pid died.
                row["verdict"] = "pid-gone-informative"
            rows.append(row)

    probed = [r for r in rows if r["pid"] is not None]
    alive = [r for r in rows if r["verdict"] == "PID-ALIVE"]
    unprobeable = [r for r in rows if r["verdict"] == "UNPROBEABLE-no-pid"]

    if args.json:
        print(
            json.dumps(
                {
                    "since": args.since,
                    "receipts_dir": str(receipts_dir),
                    "rows": rows,
                    "probed": len(probed),
                    "unprobeable_no_pid": len(unprobeable),
                    "pid_alive": len(alive),
                },
                indent=1,
            )
        )
    else:
        for r in rows:
            print(
                f"{r['reaped_at']} {r['verdict']} {r['outcome']} "
                f"pid={r['pid']} {r['row_name']} ({r['receipt']})"
            )
        print(
            f"summary: {len(probed)} probed, {len(unprobeable)} UNPROBEABLE-no-pid, "
            f"{len(alive)} PID-ALIVE"
        )

    if alive:
        for r in alive:
            print(f"FAIL: confirmed-removed but pid alive: {r['receipt']} pid={r['pid']}", file=sys.stderr)
        return 1
    if not probed:
        print(
            "FAIL: no pid-bearing native-stop effect since "
            f"{args.since}; the probe has nothing to verify",
            file=sys.stderr,
        )
        return 1
    if unprobeable:
        print(
            f"note: {len(unprobeable)} confirmed-removed effect(s) name no pid "
            "(pre-x9485 receipts); after the next deploy these must vanish",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""ledger-dedup-lookup.py -- decide whether the stop-hook fallback should fire.

Used by hooks/target-stop-hook.sh ensure_session_registered to know if a
session is already represented in ledger.json. The historic check only
looked for the Claude transcript UUID inside entry.sessions[], which
missed canonical entries written by /target's pre-promise sequence under
the target-minted session_id (the PR #174 incident, change #3 in
internal/fno/plans/2026-04-29-completion-stamp-ritual-fixes.md).

Match tiers (any tier passing means "already registered"):
  1. transcript UUID present in entry.sessions[]  (legacy regression coverage)
  2. target session_id matches entry.session_id (scalar) or
     target session_id present in entry.sessions[] (also legacy-safe)
  3. pr_number > 0 AND matches entry.pr_number (defense-in-depth - same
     PR can't legitimately be registered twice in the same hour)
  4. branch matches entry.branch AND entry.completed is within the last
     60 minutes (last-line defense for entries that somehow lost both
     identity fields)

Exit codes:
  0  - already registered (skip fallback)
  1  - not registered (fallback should fire)
  2  - ledger unreadable / malformed (caller should warn AND fire fallback;
       conflating corruption with "not registered" would hide an
       operational problem)
"""
from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timedelta


FRESHNESS_WINDOW = timedelta(minutes=60)


def _parse_iso(ts: str | None) -> datetime | None:
    if not ts or not isinstance(ts, str):
        return None
    # ledger.json writes datetime.now().isoformat() (no tz). Z-suffixed UTC
    # would also work for completeness; strip a trailing Z if present.
    raw = ts.rstrip("Z").rstrip()
    try:
        return datetime.fromisoformat(raw)
    except (ValueError, TypeError):
        return None


def _entry_matches(
    entry: dict,
    *,
    transcript_uuid: str,
    target_session_id: str,
    pr_number: int,
    branch: str,
    now: datetime,
) -> bool:
    sessions = entry.get("sessions") or []

    # Tier 1: transcript UUID in sessions[]  (existing semantics)
    if transcript_uuid and transcript_uuid in sessions:
        return True

    # Tier 2: target session_id - scalar OR sessions list
    if target_session_id:
        if entry.get("session_id") == target_session_id:
            return True
        if target_session_id in sessions:
            return True

    # Tier 3: pr_number scalar match (only when caller actually has one)
    if pr_number and pr_number > 0 and entry.get("pr_number") == pr_number:
        return True

    # Tier 4: branch + freshness window. Wrap the comparison in try/except
    # so a tz-aware completed timestamp (legacy or foreign-written entry
    # with `+00:00` offset) doesn't TypeError-crash the lookup. A naive-vs-
    # aware comparison would otherwise propagate as rc=1 (looks like "not
    # registered" to the stop hook) and trigger the exact duplicate write
    # this script exists to prevent. Falling through silently means tier 4
    # never fires for that entry, which is the safer default.
    if branch and entry.get("branch") == branch:
        completed = _parse_iso(entry.get("completed"))
        if completed is not None:
            try:
                if (now - completed) <= FRESHNESS_WINDOW:
                    return True
            except TypeError:
                pass

    return False


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ledger_path")
    parser.add_argument("--transcript-uuid", default="")
    parser.add_argument("--target-session-id", default="")
    parser.add_argument("--pr-number", type=int, default=0)
    parser.add_argument("--branch", default="")
    args = parser.parse_args(argv)

    try:
        with open(args.ledger_path) as f:
            data = json.load(f)
    except FileNotFoundError:
        return 1
    except (OSError, json.JSONDecodeError) as exc:
        print(f"{type(exc).__name__}: {exc}", file=sys.stderr)
        return 2

    if not isinstance(data, dict):
        return 1
    entries = data.get("entries", [])
    if not isinstance(entries, list):
        return 1

    now = datetime.now()
    for entry in entries:
        if not isinstance(entry, dict):
            continue
        if _entry_matches(
            entry,
            transcript_uuid=args.transcript_uuid,
            target_session_id=args.target_session_id,
            pr_number=args.pr_number,
            branch=args.branch,
            now=now,
        ):
            return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())

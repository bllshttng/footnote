#!/usr/bin/env python3
"""Read-only readiness verdict for the shared Codex daemon journey.

Prints a non-empty `codex_shared_daemon_ready` line ONLY when the newest
journey receipt exists, is fresh, and every required row reads `pass`. It
never runs anything: the journey (tests/codex-shared-daemon-version-journey.sh)
is the writer; this is the reader. AC24-HP's standing gate.
"""

import argparse
import json
import sys
import time
from pathlib import Path

REQUIRED_ROWS = (
    "create",
    "stale_detection",
    "safe_upgrade",
    "same_id_read",
    "new_message_delivery",
    "exactly_one_writer",
)


def newest_receipt(directory):
    candidates = sorted(directory.glob("codex_shared_daemon_*.json"))
    return candidates[-1] if candidates else None


def receipt_age_hours(path):
    stamp = path.stem.removeprefix("codex_shared_daemon_")
    try:
        seconds = time.mktime(time.strptime(stamp, "%Y%m%dT%H%M%SZ"))
    except ValueError:
        return None
    return (time.time() - seconds) / 3600.0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-latest", action="store_true")
    parser.add_argument("--max-age-hours", type=float, default=24)
    parser.add_argument("--receipt-dir", type=Path, default=Path.home() / ".fno" / "codex-journey")
    args = parser.parse_args()

    if not args.verify_latest:
        parser.error("only --verify-latest is implemented; the journey is the writer")

    receipt = newest_receipt(args.receipt_dir)
    if receipt is None:
        print("codex_shared_daemon_not_ready: no journey receipt under", args.receipt_dir, file=sys.stderr)
        return 1
    try:
        data = json.loads(receipt.read_text())
    except (OSError, json.JSONDecodeError) as error:
        print(f"codex_shared_daemon_not_ready: unreadable receipt {receipt}: {error}", file=sys.stderr)
        return 1

    age = receipt_age_hours(receipt)
    if age is None:
        print(f"codex_shared_daemon_not_ready: receipt name does not parse as a journey stamp: {receipt.name}", file=sys.stderr)
        return 1
    if age > args.max_age_hours:
        print(
            f"codex_shared_daemon_not_ready: newest receipt is {age:.1f}h old (max {args.max_age_hours}h)",
            file=sys.stderr,
        )
        return 1

    rows = data.get("rows", {})
    missing = [row for row in REQUIRED_ROWS if rows.get(row) != "pass"]
    if missing:
        print(f"codex_shared_daemon_not_ready: rows not passing: {', '.join(missing)}", file=sys.stderr)
        return 1

    print(f"codex_shared_daemon_ready {receipt.name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

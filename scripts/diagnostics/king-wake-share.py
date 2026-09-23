#!/usr/bin/env python3
"""Measure king assistant turns and the automated wake share in one transcript."""

from __future__ import annotations

import argparse
import json
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path


def parse_time(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def message_text(value: object) -> str:
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False)[:4000]


def classify(text: str) -> str:
    lower = text.lower()
    if "task-notification" in lower or "background task" in lower:
        return "task-notification"
    if "goal check-in" in lower or "/goal" in lower:
        return "Goal check-in / /goal feedback"
    if "fno_mail" in lower or "fno mail" in lower:
        return "fno_mail"
    if "reign check-in" in lower or "king checkin" in lower:
        return "reign check-in loop"
    return "human"


def measure(path: Path, since: datetime) -> tuple[Counter[str], Counter[str], datetime | None, datetime | None]:
    counts: Counter[str] = Counter()
    sources: Counter[str] = Counter()
    seen_assistant: set[str] = set()
    first: datetime | None = None
    last: datetime | None = None
    with path.open(errors="replace") as stream:
        for line in stream:
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            timestamp = row.get("timestamp")
            if not timestamp:
                continue
            try:
                moment = parse_time(timestamp)
            except ValueError:
                continue
            if moment < since:
                continue
            first = first or moment
            last = moment
            if row.get("type") == "user":
                text = message_text(row.get("message", {}).get("content"))
                source = classify(text)
                sources[source] += 1
                counts["turn_prompts"] += 1
            if row.get("type") != "assistant":
                continue
            message = row.get("message") or {}
            usage = message.get("usage")
            if not usage:
                continue
            identity = str(message.get("id") or row.get("uuid") or "")
            if identity in seen_assistant:
                continue
            seen_assistant.add(identity)
            counts["assistant_turns"] += 1
    return counts, sources, first, last


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("transcript", type=Path)
    parser.add_argument("--since", required=True, help="UTC ISO timestamp, for example 2026-09-20T11:00Z")
    args = parser.parse_args()
    since = parse_time(args.since)
    counts, sources, first, last = measure(args.transcript, since)
    seconds = max(0.0, (last - first).total_seconds()) if first and last else 0.0
    hours = seconds / 3600.0
    prompts = counts["turn_prompts"]
    automated = prompts - sources["human"]
    share = 100.0 * automated / prompts if prompts else 0.0
    per_hour = counts["assistant_turns"] / hours if hours else 0.0
    print(f"transcript={args.transcript}")
    print(f"since={since.isoformat().replace('+00:00', 'Z')}")
    print(f"hours_covered={hours:.2f}")
    print(f"assistant_turns={counts['assistant_turns']}")
    print(f"assistant_turns_per_hour={per_hour:.2f}")
    print(f"turn_prompts={prompts}")
    print(f"automated_prompts={automated}")
    print(f"automated_share={share:.2f}%")
    for source in ("task-notification", "Goal check-in / /goal feedback", "fno_mail", "reign check-in loop", "human"):
        print(f"source.{source}={sources[source]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

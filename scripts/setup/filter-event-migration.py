#!/usr/bin/env python3
"""Filter split-journal gate rows so migration cannot restore older approval."""

from __future__ import annotations

import json
import os
import re
import sys
from collections.abc import Iterator
from datetime import datetime, timedelta
from pathlib import Path


def _gate_key(row: object) -> tuple[object, ...] | None:
    if not isinstance(row, dict) or not isinstance(row.get("data"), dict):
        return None
    data = row["data"]
    if row.get("type") == "review_attestation":
        reviewer = data.get("reviewer")
        head_sha = data.get("head_sha")
        if not isinstance(reviewer, str) or not isinstance(head_sha, str):
            return None
        return ("review_attestation", reviewer.lstrip("/"), head_sha)
    if row.get("type") == "review_coverage":
        pr = data.get("pr")
        head_sha = data.get("head_sha")
        if (
            not isinstance(pr, int)
            or isinstance(pr, bool)
            or not isinstance(head_sha, str)
        ):
            return None
        return ("review_coverage", pr, head_sha)
    return None


def _favorable(row: dict[str, object]) -> bool:
    data = row.get("data")
    if not isinstance(data, dict):
        return False
    return (
        row.get("type") == "review_attestation" and data.get("verdict") == "pass"
    ) or (row.get("type") == "review_coverage" and data.get("coverage") == "covered")


def _timestamp(row: object) -> datetime | None:
    if not isinstance(row, dict):
        return None
    value = row["ts"] if "ts" in row else row.get("timestamp")
    if (
        not isinstance(value, str)
        or re.fullmatch(
            r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]{1,6})?(?:Z|\+00:00)",
            value,
        )
        is None
    ):
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed if parsed.utcoffset() == timedelta(0) else None


def _rows(path: Path) -> Iterator[tuple[bytes, object | None]]:
    with path.open("rb") as handle:
        for raw in handle:
            try:
                yield raw, json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                yield raw, None


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: filter-event-migration.py CANONICAL LOCAL", file=sys.stderr)
        return 2
    canonical, local = map(Path, sys.argv[1:])
    existing: dict[tuple[object, ...], datetime | None] = {}
    for _, row in _rows(canonical):
        key = _gate_key(row)
        if key is None:
            continue
        timestamp = _timestamp(row)
        previous = existing.get(key)
        if key not in existing or (
            timestamp is not None and (previous is None or timestamp > previous)
        ):
            existing[key] = timestamp
    local_size = local.stat().st_size
    try:
        local_cursor = int(
            Path(os.environ["EVENTS_MIGRATION_LOCAL_CURSOR"]).read_text(
                encoding="ascii"
            )
        )
    except (KeyError, OSError, UnicodeError, ValueError):
        local_cursor = 0
    if local_cursor < 0 or local_cursor > local_size:
        local_cursor = 0
    local_offset = 0
    mapped_cursor = 0
    for raw, row in _rows(local):
        line_end = local_offset + len(raw)
        key = _gate_key(row)
        timestamp = _timestamp(row)
        if (
            key is not None
            and isinstance(row, dict)
            and key in existing
            and (
                (
                    timestamp is not None
                    and existing[key] is not None
                    and timestamp <= existing[key]
                )
                or (
                    _favorable(row)
                    and (timestamp is None or existing[key] is None)
                )
            )
        ):
            local_offset = line_end
            continue
        sys.stdout.buffer.write(raw)
        if line_end <= local_cursor:
            mapped_cursor += len(raw)
        if key is not None:
            previous = existing.get(key)
            if key not in existing or (
                timestamp is not None and (previous is None or timestamp > previous)
            ):
                existing[key] = timestamp
        local_offset = line_end
    mapping_path = os.environ.get("EVENTS_MIGRATION_CURSOR_MAP")
    if mapping_path:
        Path(mapping_path).write_text(str(mapped_cursor), encoding="ascii")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

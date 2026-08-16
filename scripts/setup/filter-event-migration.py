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


def _attester_key(row: object) -> tuple[object, ...] | None:
    key = _gate_key(row)
    if key is None or key[0] != "review_attestation" or not isinstance(row, dict):
        return None
    data = row["data"]
    attester = data.get("attester_session_id")
    if not isinstance(attester, str) or not attester:
        attester = None
    return (*key, attester)


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


def _record_latest(
    records: dict[tuple[object, ...], tuple[datetime | None, bool]],
    key: tuple[object, ...] | None,
    row: object,
) -> None:
    if key is None or not isinstance(row, dict):
        return
    candidate = (_timestamp(row), _favorable(row))
    previous = records.get(key)
    if previous is None:
        records[key] = candidate
        return
    timestamp, favorable = candidate
    previous_timestamp, _ = previous
    if timestamp is not None and (
        previous_timestamp is None
        or timestamp > previous_timestamp
        or (timestamp == previous_timestamp and not favorable)
    ):
        records[key] = candidate


def _gate_row_is_stale(
    row: dict[str, object],
    timestamp: datetime | None,
    previous: tuple[datetime | None, bool],
) -> bool:
    previous_timestamp, _ = previous
    if timestamp is not None and previous_timestamp is not None:
        return timestamp < previous_timestamp or (
            timestamp == previous_timestamp and _favorable(row)
        )
    return _favorable(row)


def _preserves_distinct_attester_coverage(
    row: dict[str, object],
    timestamp: datetime | None,
    gate_previous: tuple[datetime | None, bool],
    attester_previous: tuple[datetime | None, bool] | None,
) -> bool:
    if not _favorable(row) or not gate_previous[1] or timestamp is None:
        return False
    if attester_previous is None:
        return True
    previous_timestamp, _ = attester_previous
    return previous_timestamp is None or timestamp > previous_timestamp


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: filter-event-migration.py CANONICAL LOCAL", file=sys.stderr)
        return 2
    canonical, local = map(Path, sys.argv[1:])
    existing: dict[tuple[object, ...], tuple[datetime | None, bool]] = {}
    attesters: dict[tuple[object, ...], tuple[datetime | None, bool]] = {}
    for _, row in _rows(canonical):
        _record_latest(existing, _gate_key(row), row)
        _record_latest(attesters, _attester_key(row), row)
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
        attester_key = _attester_key(row)
        timestamp = _timestamp(row)
        stale = (
            key is not None
            and isinstance(row, dict)
            and key in existing
            and _gate_row_is_stale(row, timestamp, existing[key])
        )
        if stale and not (
            isinstance(row, dict)
            and attester_key is not None
            and _preserves_distinct_attester_coverage(
                row,
                timestamp,
                existing[key],
                attesters.get(attester_key),
            )
        ):
            local_offset = line_end
            continue
        sys.stdout.buffer.write(raw)
        if line_end <= local_cursor:
            mapped_cursor += len(raw)
        _record_latest(existing, key, row)
        _record_latest(attesters, attester_key, row)
        local_offset = line_end
    mapping_path = os.environ.get("EVENTS_MIGRATION_CURSOR_MAP")
    if mapping_path:
        Path(mapping_path).write_text(str(mapped_cursor), encoding="ascii")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

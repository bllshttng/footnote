#!/usr/bin/env python3
"""Filter split-journal gate rows so migration cannot restore older approval."""

from __future__ import annotations

import json
import sys
from collections.abc import Iterator
from pathlib import Path


def _gate_key(row: object) -> tuple[object, ...] | None:
    if not isinstance(row, dict) or not isinstance(row.get("data"), dict):
        return None
    data = row["data"]
    if row.get("type") == "review_attestation":
        reviewer = data.get("reviewer")
        if isinstance(reviewer, str):
            reviewer = reviewer.lstrip("/")
        return ("review_attestation", reviewer, data.get("head_sha"))
    if row.get("type") == "review_coverage":
        return ("review_coverage", data.get("pr"), data.get("head_sha"))
    return None


def _favorable(row: dict[str, object]) -> bool:
    data = row.get("data")
    if not isinstance(data, dict):
        return False
    return (row.get("type") == "review_attestation" and data.get("verdict") == "pass") or (
        row.get("type") == "review_coverage" and data.get("coverage") == "covered"
    )


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
    existing = {key for _, row in _rows(canonical) if (key := _gate_key(row)) is not None}
    for raw, row in _rows(local):
        key = _gate_key(row)
        if key is not None and isinstance(row, dict) and _favorable(row) and key in existing:
            continue
        sys.stdout.buffer.write(raw)
        if key is not None:
            existing.add(key)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

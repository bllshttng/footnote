"""Versioned graph migrations that require an explicit operator action."""
from __future__ import annotations

from datetime import datetime, timezone


def _migration_marker(row: dict) -> bool:
    return any(
        isinstance(item, dict)
        and item.get("source") == "migrate-priorities"
        and item.get("from") == "p0"
        and item.get("to") == "p1"
        for item in (row.get("priority_history") or [])
    )


def migrate_legacy_p0(entries: list[dict], *, apply: bool = False) -> dict[str, int]:
    """Re-band unacknowledged legacy p0 rows to p1, once and audibly."""
    pending = [
        row for row in entries
        if isinstance(row, dict)
        and row.get("priority") == "p0"
        and not row.get("blocks_everything")
    ]
    already = sum(1 for row in entries if isinstance(row, dict) and _migration_marker(row))
    receipt = {
        "legacy_p0": len(pending),
        "rebanded_to_p1": len(pending) if apply else len(pending),
        "remaining_unacknowledged_p0": len(pending),
        "already_migrated": already,
    }
    if not apply:
        return receipt

    now = datetime.now(timezone.utc).isoformat()
    for row in pending:
        row.setdefault("priority_history", []).append(
            {
                "from": "p0",
                "to": "p1",
                "prior_priority": "p0",
                "source": "migrate-priorities",
                "ts": now,
            }
        )
        row["priority"] = "p1"
    receipt["remaining_unacknowledged_p0"] = 0
    return receipt


def rollback_legacy_p0(entries: list[dict]) -> dict[str, int]:
    """Restore rows changed by this migration, preserving the audit trail."""
    candidates = [
        row for row in entries
        if isinstance(row, dict)
        and row.get("priority") == "p1"
        and _migration_marker(row)
    ]
    now = datetime.now(timezone.utc).isoformat()
    for row in candidates:
        row.setdefault("priority_history", []).append(
            {
                "from": "p1",
                "to": "p0",
                "prior_priority": "p1",
                "source": "migrate-priorities-rollback",
                "ts": now,
            }
        )
        row["priority"] = "p0"
    return {"restored_to_p0": len(candidates)}

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


def _retired_band(row: dict) -> str | None:
    """The row's model_tier as a canonical band, or None if it is not one.

    AttributeError joins ValueError: a non-string value (a hand-edit's int,
    list) dies inside ``str.strip``/``.lower`` before the band check, which
    would surface as a traceback instead of the by-id refusal below.
    """
    from fno.graph._constants import normalize_difficulty

    try:
        return normalize_difficulty(row.get("model_tier"))
    except (ValueError, AttributeError, TypeError):
        return None


def split_retired_tier_rows(entries: list[dict]) -> tuple[list[str], list[str]]:
    """(migratable ids, both-spellings ids) for rows still carrying model_tier.

    ONE classifier, shared by the migration verb and the reconcile advisory,
    so the prescription the advisory prints can never drift from the verdict
    the verb delivers.
    """
    migratable: list[str] = []
    conflicted: list[str] = []
    for row in entries:
        if not isinstance(row, dict) or row.get("model_tier") is None:
            continue
        if row.get("difficulty") is not None:
            conflicted.append(str(row.get("id", "?")))
        else:
            migratable.append(str(row.get("id", "?")))
    return migratable, conflicted


def migrate_model_tier(entries: list[dict], *, apply: bool = False) -> dict:
    """Move the retired ``model_tier`` band onto ``difficulty``, once, audibly.

    The compat window closed with zero rows carrying both spellings (measured
    across the whole graph 2026-08-26), so a row holding both today means a
    writer re-added the retired key after the tombstone; that row is refused
    rather than guessed at. Bands run through ``normalize_difficulty`` like
    every other difficulty writer - a value that does not normalize is
    refused by id, never migrated verbatim under a success receipt.
    """
    _migratable, conflicted = split_retired_tier_rows(entries)
    if conflicted:
        raise ValueError(
            "rows carry both difficulty and model_tier; resolve by hand "
            "before migrating: " + ", ".join(sorted(conflicted))
        )
    invalid = sorted(
        f"{row.get('id', '?')}={row.get('model_tier')!r}"
        for row in entries
        if isinstance(row, dict)
        and row.get("model_tier") is not None
        and _retired_band(row) is None
    )
    if invalid:
        raise ValueError(
            "model_tier values that are not a difficulty band; fix or drop "
            "them by hand before migrating: " + ", ".join(invalid)
        )
    pending = [
        row for row in entries
        if isinstance(row, dict) and row.get("model_tier") is not None
    ]
    receipt: dict = {
        "candidates": [row.get("id") for row in pending],
        "candidate_count": len(pending),
        "apply": apply,
    }
    if not apply:
        return receipt

    now = datetime.now(timezone.utc).isoformat()
    for row in pending:
        band = _retired_band(row)
        row.pop("model_tier", None)
        row["difficulty"] = band
        # `or []`, not setdefault: a hand-edited row can carry
        # ``difficulty_history: null`` and setdefault would return that null
        # to .append (AttributeError) instead of the conflict-free path.
        history = row.get("difficulty_history") or []
        history.append({"value": band, "source": "migration", "ts": now})
        row["difficulty_history"] = history
    receipt["migrated"] = len(pending)
    return receipt

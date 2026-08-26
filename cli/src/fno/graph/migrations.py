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
    """(migratable ids, divergent-band ids) for rows still carrying model_tier.

    ONE classifier, shared by the migration verb and the reconcile advisory,
    so the prescription the advisory prints can never drift from the verdict
    the verb delivers. A row carrying both spellings with the SAME band is
    migratable: the retired ``--model-tier`` handler always wrote both keys
    with equal bands, so same-band rows are what machine-created leftovers
    actually look like, and only a DIVERGENT pair is a real conflict.
    """
    migratable: list[str] = []
    divergent: list[str] = []
    from fno.graph._constants import normalize_difficulty

    for row in entries:
        if not isinstance(row, dict) or row.get("model_tier") is None:
            continue
        rid = str(row.get("id", "?"))
        band = _retired_band(row)
        current = row.get("difficulty")
        if isinstance(current, str):
            try:
                current = normalize_difficulty(current)
            except ValueError:
                pass  # garbage canonical band: the divergent lane names both
        if current is not None and band is not None and band != current:
            divergent.append(rid)
        else:
            # model_tier-only rows, same-band pairs (case-folded), and a
            # garbage retired key under a live difficulty: all drain without
            # a judgment.
            migratable.append(rid)
    return migratable, divergent


def migrate_model_tier(entries: list[dict], *, apply: bool = False) -> dict:
    """Move the retired ``model_tier`` band onto ``difficulty``, once, audibly.

    Same-band both-spellings rows drain (the retired handler wrote both keys
    with equal bands, so those are the machine-created leftovers); only a
    DIVERGENT pair is refused rather than guessed at. Bands run through
    ``normalize_difficulty`` like every other difficulty writer - a
    model_tier-only value that does not normalize is refused by id, never
    migrated verbatim under a success receipt.
    """
    from fno.graph._constants import append_difficulty_history

    _migratable, divergent = split_retired_tier_rows(entries)
    if divergent:
        _divergent_ids = set(divergent)
        raise ValueError(
            "rows carry difficulty and a DIVERGENT model_tier; pick the band "
            "with `fno backlog update <id> --difficulty <band>` (which clears "
            "the retired key) before migrating: "
            + ", ".join(
                f"{row.get('id', '?')} model_tier={row.get('model_tier')!r} "
                f"difficulty={row.get('difficulty')!r}"
                for row in entries
                if isinstance(row, dict)
                and str(row.get("id", "?")) in _divergent_ids
            )
        )
    invalid = sorted(
        f"{row.get('id', '?')}={row.get('model_tier')!r}"
        for row in entries
        if isinstance(row, dict)
        and row.get("model_tier") is not None
        and row.get("difficulty") is None
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
    drained = 0
    for row in pending:
        band = _retired_band(row)
        row.pop("model_tier", None)
        if row.get("difficulty") is None and band is not None:
            row["difficulty"] = band
        if band is not None:
            append_difficulty_history(row, band, "migration", now)
        drained += 1
    receipt["migrated"] = drained
    return receipt

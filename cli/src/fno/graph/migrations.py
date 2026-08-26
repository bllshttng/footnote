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
    """(drainable ids, needs-decision ids) for rows still carrying model_tier.

    ONE classifier, shared by the migration verb and the reconcile advisory,
    so the prescription the advisory prints can never drift from the verdict
    the verb delivers. Drainable: a valid band with no canonical field, a
    same-band pair (the retired ``--model-tier`` handler wrote both keys with
    equal bands, so those are what machine-created leftovers look like), or a
    garbage retired key under a live difficulty. Needs a decision: a divergent
    pair, or a garbage band with no canonical field - both are fixed by the
    same one command, `fno backlog update <id> --difficulty <band>`.
    """
    drainable: list[str] = []
    needs_decision: list[str] = []
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
                pass  # garbage canonical band: the needs-decision lane names both
        if (current is None and band is None) or (
            current is not None and band is not None and band != current
        ):
            needs_decision.append(rid)
        else:
            drainable.append(rid)
    return drainable, needs_decision


def _retired_tier_pending(entries: list[dict]) -> list[dict]:
    """The rows still carrying model_tier - the ONE walk behind the ids and
    the mutation, so the classifier and the verb cannot disagree on what
    counts as a candidate."""
    return [
        row for row in entries
        if isinstance(row, dict) and row.get("model_tier") is not None
    ]


def migrate_model_tier(entries: list[dict], *, apply: bool = False) -> dict:
    """Move the retired ``model_tier`` band onto ``difficulty``, once, audibly.

    Same-band both-spellings rows drain (the retired handler wrote both keys
    with equal bands, so those are the machine-created leftovers). A row the
    classifier routes to needs-decision (a divergent pair, or a garbage band
    with no canonical field) is refused, named with its values and the one
    command that fixes it. Bands run through ``normalize_difficulty`` like
    every other difficulty writer - a value that does not normalize is never
    migrated verbatim under a success receipt.
    """
    from fno.graph._constants import append_difficulty_history

    pending = _retired_tier_pending(entries)
    _drainable, needs_decision = split_retired_tier_rows(entries)
    if needs_decision:
        _decision_ids = set(needs_decision)
        named = [
            f"{row.get('id', '?')} model_tier={row.get('model_tier')!r} "
            f"difficulty={row.get('difficulty')!r}"
            for row in pending
            if str(row.get("id", "?")) in _decision_ids
        ]
        raise ValueError(
            "rows need a hand-picked band before migrating; pick it with "
            "`fno backlog update <id> --difficulty <band>` (which also clears "
            "the retired key): "
            + ", ".join(named[:8])
            + (f" ... (+{len(named) - 8} more)" if len(named) > 8 else "")
        )
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

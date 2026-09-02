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
    pair, a garbage band with no canonical field, or a garbage canonical band
    under any retired key - all fixed by the same one command,
    `fno backlog update <id> --difficulty <band>`.
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
        current_is_band = False
        if isinstance(current, str):
            try:
                current = normalize_difficulty(current)
                current_is_band = True
            except ValueError:
                pass
        if current is not None and not current_is_band:
            # A garbage canonical band needs the same hand-picked decision as
            # a garbage retired one: draining would bless an invalid band with
            # a success receipt, and once the retired key is gone no later
            # sweep names the row again.
            needs_decision.append(rid)
        elif (current is None and band is None) or (
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


def migrate_updated_at(entries: list[dict], *, apply: bool = False) -> dict:
    """Remove the proven-unread ``__updated_at`` residue, once and audibly."""
    pending = [
        row
        for row in entries
        if isinstance(row, dict) and "__updated_at" in row
    ]
    receipt: dict = {
        "candidates": [row.get("id") for row in pending],
        "candidate_count": len(pending),
        "apply": apply,
        "removed": 0,
    }
    if not apply:
        return receipt

    for row in pending:
        row.pop("__updated_at", None)
    receipt["removed"] = len(pending)
    return receipt


_BACKFILL_SIZE_BANDS = {"S": "low", "M": "medium", "L": "high"}
_BACKFILL_PRIORITY_BANDS = {"p0": "high", "p1": "high", "p2": "medium", "p3": "low"}
_BACKFILL_TYPE_BANDS = {"bug": "high", "epic": "medium", "feature": "medium", "roadmap": "medium"}
_BAND_RANK = {"low": 0, "medium": 1, "high": 2}


def _backfill_band(row: dict) -> str | None:
    """Choose the strongest named signal on a legacy row, or no band."""
    signals: list[str] = []
    size = row.get("size")
    if isinstance(size, str):
        band = _BACKFILL_SIZE_BANDS.get(size.strip().upper())
        if band:
            signals.append(band)
    priority = row.get("priority")
    if isinstance(priority, str):
        band = _BACKFILL_PRIORITY_BANDS.get(priority.strip().lower())
        if band:
            signals.append(band)
    node_type = row.get("type")
    if isinstance(node_type, str):
        band = _BACKFILL_TYPE_BANDS.get(node_type.strip().lower())
        if band:
            signals.append(band)
    return max(signals, key=lambda candidate: _BAND_RANK[candidate]) if signals else None


def backfill_difficulty(entries: list[dict], *, apply: bool = False) -> dict:
    """Backfill missing difficulty from explicit legacy row signals.

    Size, priority, and type are signals already stored on the row. The
    strongest available signal wins; a row with none stays unbanded rather
    than receiving an invented default. Existing difficulty and history are
    untouched.
    """
    pending = [
        row for row in entries
        if isinstance(row, dict) and row.get("difficulty") is None
    ]
    planned: list[tuple[dict, str]] = []
    skipped: list[str] = []
    for row in pending:
        band = _backfill_band(row)
        if band is None:
            skipped.append(str(row.get("id", "?")))
        else:
            planned.append((row, band))

    receipt = {
        "apply": apply,
        "candidates": [str(row.get("id", "?")) for row, _band in planned],
        "written": [str(row.get("id", "?")) for row, _band in planned] if apply else [],
        "skipped": skipped,
        "already_band": sum(
            1 for row in entries if isinstance(row, dict) and row.get("difficulty") is not None
        ),
    }
    if not apply:
        return receipt

    now = datetime.now(timezone.utc).isoformat()
    from fno.graph._constants import append_difficulty_history

    for row, band in planned:
        row["difficulty"] = band
        append_difficulty_history(row, band, "backfill", now)
    return receipt

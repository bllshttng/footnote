"""The yard identity fold: species, rarity, first-sighting for the fleet.

Read-only over two files that already exist - the agent registry and the
graph archive - and nothing else. The mux's yard overlay shells out to
``fno yard --json`` and renders what this fold emits. The fold computes no
liveness and no status: the overlay reads those from the roster row it
already holds, so one status value feeds both the row and the sprite.

Two rules from the yard design that this module must never break:

- Species identity is the FULL unique id, hashed stably. The short mail
  handle and the per-restart display prefix collide (three live sessions
  once shared the handle ``01a00328``), and builtin ``hash()`` is salted
  per process, which would reassign a citizen's species on every render.
- Rarity is a rank, shown as outcome only. The original proposal named a
  four-axis tuple (harness, provider, model, account); the registry has
  carried only ``harness`` since schema v10 (x-880e removed ``provider``;
  model and account are not row fields), so v1 ranks the one axis that is
  recorded - ``put out the codex bowl and a different cat shows up`` is
  still the whole mechanic. Widen the tuple when the registry grows axes
  back; never invent them.

The first-sighting mark is species-dimension ONLY: rarity and hat are not
recomputed for archive rows (``source_harness`` survives on roughly 40% of
them) and are never faked.
"""
from __future__ import annotations

import hashlib
from collections import Counter
from typing import Iterable

SPECIES_COUNT = 18
RARITY_TIERS = ("common", "uncommon", "rare", "epic", "legendary")
# Cumulative population-share boundaries matching RARITY_WEIGHT's 60/25/10/4/1
# shape: tuples covering up to 60% of the population read common, the next 25%
# uncommon, and so on; whatever the boundaries never reach reads legendary.
_RARITY_BOUNDARIES = (0.60, 0.85, 0.95, 0.99)


def species_for(full_id: str) -> int:
    """Stable species index for a full unique id (sha256, never ``hash()``)."""
    digest = hashlib.sha256(full_id.encode("utf-8")).digest()
    return int.from_bytes(digest[:8], "big") % SPECIES_COUNT


def citizen_id(row) -> str:
    """The full unique key behind a registry row - never the short handle.

    ``harness_session_id`` is the canonical full id; a legacy row without one
    degrades to ``name@created_at``, which is still stable per row and does
    not collide across restarts the way a display prefix does.
    """
    sid = getattr(row, "harness_session_id", None)
    if sid:
        return sid
    return f"{getattr(row, 'name', '?')}@{getattr(row, 'created_at', '')}"


def rarity_tiers(population: Iterable[str]) -> dict[str, str]:
    """Rank ``population`` values by frequency, bucket against the weight shape.

    Deterministic: count-desc, then lex on the value for ties. The value that
    crosses a boundary opens the next bucket, so a majority value always reads
    common and a value seen once among many reads legendary.
    """
    counts = Counter(v for v in population if v)
    total = sum(counts.values())
    if total == 0:
        return {}
    tiers: dict[str, str] = {}
    tier_idx = 0
    cum = 0
    for value in sorted(counts, key=lambda v: (-counts[v], v)):
        cum += counts[value]
        tiers[value] = RARITY_TIERS[tier_idx]
        while tier_idx < len(_RARITY_BOUNDARIES) and cum >= _RARITY_BOUNDARIES[tier_idx] * total:
            tier_idx += 1
    return tiers


def seen_species(archive_entries: Iterable) -> set[int]:
    """Species hashes already recorded in the album (the archive's sessions)."""
    ids: set[str] = set()
    for e in archive_entries:
        if not isinstance(e, dict):
            continue
        for key in ("source_session_id", "session_id"):
            v = e.get(key)
            if isinstance(v, str) and v:
                ids.add(v)
        for s in e.get("sessions") or []:
            if isinstance(s, dict):
                v = s.get("session_id")
                if isinstance(v, str) and v:
                    ids.add(v)
    return {species_for(v) for v in ids}


def fold(rows: list, archive_entries: Iterable) -> list[dict]:
    """Fold registry rows into yard citizens, joined to the album for the mark.

    Sorted by name for a deterministic wire order (the overlay joins by name;
    a stable order keeps the spotlight cursor from jumping between renders).
    """
    seen = seen_species(archive_entries)
    tiers = rarity_tiers(getattr(r, "harness", None) for r in rows)
    citizens = []
    for r in rows:
        sp = species_for(citizen_id(r))
        citizens.append(
            {
                "id": citizen_id(r),
                "name": getattr(r, "name", ""),
                "harness": getattr(r, "harness", None),
                "species": sp,
                "rarity": tiers.get(getattr(r, "harness", None), RARITY_TIERS[0]),
                "crown_level": getattr(r, "crown_level", None) or 0,
                "first_sighting": sp not in seen,
            }
        )
    citizens.sort(key=lambda c: c["name"])
    return citizens

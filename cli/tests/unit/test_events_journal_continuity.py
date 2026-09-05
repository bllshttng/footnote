"""The journal continuity invariant (x-d2e9).

The real producer rotates by RENAME (``events.rs`` ROTATE_AT_BYTES): the
former live file becomes ``.1`` and the live file restarts empty, so there is
no overlap across a rotation boundary and never was. An earlier marker here
demanded ``oldest_live <= newest_rotated`` and could only pass on invented
overlap; that marker was retracted by its own author. The invariant this file
pins is the one a rename rotation actually satisfies, and the foreign-move
defect actually breaks:

- ORDER: the newest rotated row must not be LATER than the oldest live row.
  A live journal whose rows sit entirely BEFORE its rotation history is the
  older-window replace shape: the recent past is gone.
- ADJACENCY: the boundary gap (``oldest_live - newest_rotated``) must stay
  within ``MAX_ROTATION_GAP_HOURS``. A rename rotation's boundary gap is the
  writer's own cadence (seconds); a live journal replaced by an unrelated,
  newer-windowed file starts days after the rotation history ends - the hole
  this suite exists to catch. The bound is deliberately generous: an idle
  journal rotates only when a write crossed the size threshold, so the two
  sides of a real boundary are adjacent by construction.

The negative case applies the pre-fix ``migrate_from_checkout`` shape -
``os.replace(foreign, live)`` with a newer-windowed foreign file - and
asserts the invariant FAILS, so the suite can detect the defect it exists to
catch.

The fixture is built here, never read off the developer's machine: the
``event_journals()``-driven cases pin all three live roots at tmp files.

Absence is never a verdict. An empty file, an unreadable line, or a missing
sibling all pass the checker - only a PROVEN inversion or a PROVEN gap fails.
"""
from __future__ import annotations

import json
import os
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from fno import paths

# The boundary bound, stated so a reviewer can move it: a journal that
# rotated and then went silent still shows a small boundary gap, because the
# write that crossed the size threshold continued into the new file.
MAX_ROTATION_GAP_HOURS = 24


def _parse_ts(value: object) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return parsed if parsed.tzinfo else parsed.replace(tzinfo=timezone.utc)


def _row_timestamps(journal: Path) -> list[datetime]:
    """Every parseable row ts; unparseable lines are skipped, not failures."""
    found: list[datetime] = []
    try:
        text = journal.read_text(encoding="utf-8")
    except OSError:
        return found
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        ts = _parse_ts(row.get("ts") if isinstance(row, dict) else None)
        if ts is not None:
            found.append(ts)
    return found


def continuity_violations(journals: list[Path]) -> list[str]:
    """Name every proven inversion or proven boundary gap."""
    violations: list[str] = []
    for journal in journals:
        sibling = journal.with_name(journal.name + ".1")
        if not sibling.exists():
            continue
        live_ts = _row_timestamps(journal)
        rotated_ts = _row_timestamps(sibling)
        if not live_ts or not rotated_ts:
            continue
        oldest_live, newest_rotated = min(live_ts), max(rotated_ts)
        if newest_rotated > oldest_live:
            violations.append(
                f"{journal}: newest rotated row {newest_rotated.isoformat()} is later "
                f"than oldest live row {oldest_live.isoformat()}; the live file lost "
                f"the recent past (older-window replace)"
            )
        elif oldest_live - newest_rotated > timedelta(hours=MAX_ROTATION_GAP_HOURS):
            violations.append(
                f"{journal}: oldest live row {oldest_live.isoformat()} starts "
                f"{MAX_ROTATION_GAP_HOURS}h+ after the newest rotated row "
                f"{newest_rotated.isoformat()}; the span has a hole"
            )
    return violations


def _seed_journal(path: Path, timestamps: list[str]) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        "".join(
            json.dumps({"ts": ts, "type": "guard_decision", "data": {}}) + "\n"
            for ts in timestamps
        ),
        encoding="utf-8",
    )
    return path


# ── AC7-HP: a rename rotation keeps the span continuous ─────────────────────


def test_rename_rotation_boundary_passes(tmp_path: Path) -> None:
    """The sibling holds history up to the rotation instant; the live file
    continues from it seconds later - the shape events.rs rename produces."""
    live = _seed_journal(
        tmp_path / "spaces" / "space" / "events.jsonl",
        ["2026-09-05T12:00:00Z", "2026-09-05T12:30:00Z"],
    )
    _seed_journal(
        tmp_path / "spaces" / "space" / "events.jsonl.1",
        ["2026-09-05T11:00:00Z", "2026-09-05T11:59:59Z"],
    )
    assert continuity_violations([live]) == []


def test_empty_or_unreadable_sides_pass(tmp_path: Path) -> None:
    """A side the checker cannot judge is not a violation."""
    live = _seed_journal(tmp_path / "j" / "events.jsonl", ["2026-09-05T12:00:00Z"])
    _seed_journal(tmp_path / "j" / "events.jsonl.1", [])  # empty rotated sibling
    assert continuity_violations([live]) == []
    corrupt = _seed_journal(tmp_path / "k" / "events.jsonl", ["not json"])
    _seed_journal(tmp_path / "k" / "events.jsonl.1", ["2026-09-04T00:00:00Z"])
    assert continuity_violations([corrupt]) == []


def test_missing_sibling_passes(tmp_path: Path) -> None:
    live = _seed_journal(
        tmp_path / "j" / "events.jsonl", ["2026-09-05T12:00:00Z"]
    )
    assert continuity_violations([live]) == []


# ── AC7-HP via the real enumeration: event_journals() over pinned roots ─────


@pytest.fixture
def pinned_roots(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """Point the three event_journals() roots at fixture files only."""
    global_dir = tmp_path / "home" / ".fno"
    agents_home = tmp_path / "agents"
    space_dir = tmp_path / "spaces" / "space"
    monkeypatch.setattr(paths, "global_events_json", lambda: global_dir / "events.jsonl")
    monkeypatch.setattr(paths, "agents_home_dir", lambda: agents_home)
    monkeypatch.setattr(paths, "project_events_json", lambda: space_dir / "events.jsonl")
    return {"global": global_dir, "agents": agents_home, "space": space_dir}


def test_event_journals_enumeration_passes_on_seeded_pairs(
    tmp_path: Path, pinned_roots: dict[str, Path]
) -> None:
    space = pinned_roots["space"]
    _seed_journal(
        space / "events.jsonl", ["2026-09-05T12:00:00Z", "2026-09-05T12:30:00Z"]
    )
    _seed_journal(
        space / "events.jsonl.1", ["2026-09-05T11:00:00Z", "2026-09-05T11:59:59Z"]
    )
    journals = paths.event_journals()
    assert space / "events.jsonl" in journals
    assert continuity_violations(journals) == []


# ── AC8-ERR: the defect must be detectable ──────────────────────────────────


def test_pre_fix_foreign_move_violates_continuity(
    tmp_path: Path, pinned_roots: dict[str, Path]
) -> None:
    """The pre-fix migrate shape - os.replace(foreign, live) - leaves exactly
    the hole this invariant exists to catch. The foreign file's rows all sit
    AFTER the rotated sibling's, so the replaced live file starts where the
    rotation history ended: a days-wide hole."""
    space = pinned_roots["space"]
    live = _seed_journal(
        space / "events.jsonl", ["2026-09-05T11:00:00Z", "2026-09-05T11:59:59Z"]
    )
    _seed_journal(
        space / "events.jsonl.1", ["2026-09-04T10:00:00Z", "2026-09-05T10:59:59Z"]
    )
    assert continuity_violations([live]) == [], "fixture itself must start clean"

    foreign = _seed_journal(
        tmp_path / "sandbox" / "stray" / "events.jsonl",
        ["2026-09-06T20:00:00Z"],
    )
    os.replace(foreign, live)  # the pre-fix move, verbatim

    violations = continuity_violations([live])
    assert violations, "the invariant must FAIL on the pre-fix move shape"
    assert "hole" in violations[0]

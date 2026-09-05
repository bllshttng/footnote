"""The journal continuity invariant (x-d2e9).

For every journal that has a rotated sibling, the oldest ``ts`` in the live
file must not be later than the newest ``ts`` in that sibling: the live file
and its rotations together cover a continuous span. A reader that computes
merge eligibility from a journal cannot tolerate a hole, and a hole is what a
foreign-destination move leaves behind (the pre-fix ``migrate_from_checkout``
replaced a live journal with an unrelated, newer-windowed file).

The marker is deliberately STRICT: any positive gap between the sibling's
newest row and the live file's oldest row flags, including the seconds-wide
boundary a rename-style rotation (``events.rs`` ROTATE_AT_BYTES) produces.
Fail loud over quiet tolerance was the plan's call; a tolerance is a one-line
change here if a reviewer wants the boundary admitted instead.

The fixture is built here, never read off the developer's machine: the
``event_journals()``-driven cases pin all three live roots at tmp files.

Absence is never a verdict in this suite. An empty file, an unreadable line,
or a missing sibling all pass the checker - only a PROVEN inversion fails.
"""
from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path

import pytest

from fno import paths


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
    """Name every proven inversion across journal-plus-sibling pairs."""
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
        if oldest_live > newest_rotated:
            violations.append(
                f"{journal}: oldest live row {oldest_live.isoformat()} is newer "
                f"than newest rotated row {newest_rotated.isoformat()}; the "
                f"span has a hole"
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


# ── AC7-HP: a rotation keeps the span continuous ────────────────────────────


def test_continuous_rotation_passes(tmp_path: Path) -> None:
    """The sibling carries history up to the boundary instant; the live file
    continues from it (overlapping boundary: oldest live == newest rotated)."""
    live = _seed_journal(
        tmp_path / "spaces" / "space" / "events.jsonl",
        ["2026-09-05T12:00:00Z", "2026-09-05T12:30:00Z"],
    )
    _seed_journal(
        tmp_path / "spaces" / "space" / "events.jsonl.1",
        ["2026-09-05T11:00:00Z", "2026-09-05T12:00:00Z"],
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
        space / "events.jsonl.1", ["2026-09-05T11:00:00Z", "2026-09-05T12:00:00Z"]
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
    rotation history ends: a gap."""
    space = pinned_roots["space"]
    live = _seed_journal(
        space / "events.jsonl", ["2026-09-05T11:00:00Z", "2026-09-05T11:59:59Z"]
    )
    _seed_journal(
        space / "events.jsonl.1", ["2026-09-04T10:00:00Z", "2026-09-05T11:59:59Z"]
    )
    assert continuity_violations([live]) == [], "fixture itself must start clean"

    foreign = _seed_journal(
        tmp_path / "sandbox" / "stray" / "events.jsonl",
        ["2026-09-05T20:00:00Z"],
    )
    os.replace(foreign, live)  # the pre-fix move, verbatim

    violations = continuity_violations([live])
    assert violations, "the invariant must FAIL on the pre-fix move shape"
    assert "hole" in violations[0]

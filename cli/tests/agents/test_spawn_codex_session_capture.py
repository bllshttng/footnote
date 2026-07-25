"""US1 (x-86e4): capture a codex pane's session id at spawn.

Codex, like opencode, cannot be handed a pre-minted session id the way claude
can, so the id has to be discovered from the rollout store after the pane runs.
Without this capture every codex pane row carries ``harness_session_id=None``
forever, and ``_discover_from_registry`` drops a sid-less row before any name
matching runs -- so a live codex worker is unreachable by truth, peek, and the
mail name lane at once.

Coverage:
  - AC2-FR: a missed capture leaves the row live-only, never crashes the spawn.
  - The match is exact-directory + started-after-spawn, so a sibling worktree's
    session and a pre-existing session in the SAME cwd are both excluded.
  - Two same-cwd candidates stamp NEITHER row (the opencode ambiguity rule).

Every case drives an injected sessions dir: the suite must never read the
developer's real ``~/.codex``.
"""
from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path

from fno.agents.discover import codex_session_ids_started_in
from fno.agents.mux_spawn import _backfill_codex_session_id

SID_A = "019cc081-de0d-7283-97cc-751c46742a07"
SID_B = "019cc082-1111-7283-97cc-751c46742a08"

T_SPAWN = "2026-07-24T18:00:00.000Z"
T_EARLIER = "2026-07-24T17:00:00.000Z"


def _ms(iso: str) -> int:
    return int(datetime.fromisoformat(iso).timestamp() * 1000)


def _rollout(root: Path, sid: str, cwd: str, started: str) -> Path:
    """Write a rollout jsonl whose first line is codex's real session_meta shape."""
    day = root / "2026" / "07" / "24"
    day.mkdir(parents=True, exist_ok=True)
    path = day / f"rollout-{started.replace(':', '-')}-{sid}.jsonl"
    path.write_text(
        json.dumps(
            {
                "timestamp": started,
                "type": "session_meta",
                "payload": {"id": sid, "timestamp": started, "cwd": cwd},
            }
        )
        + "\n",
        encoding="utf-8",
    )
    return path


def _no_sleep(_seconds: float) -> None:
    """Collapse the retry delay so tests do not pay it."""


# ---------------------------------------------------------------------------
# codex_session_ids_started_in: the store query
# ---------------------------------------------------------------------------


def test_unique_candidate_is_found(tmp_path: Path) -> None:
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    assert codex_session_ids_started_in(
        Path("/w/proj"), _ms(T_SPAWN) - 1, sessions_dir=tmp_path
    ) == [SID_A]


def test_sibling_worktree_in_another_cwd_is_excluded(tmp_path: Path) -> None:
    """Worktrees of one repo differ only by directory, so cwd must match exactly."""
    _rollout(tmp_path, SID_A, "/w/proj-other", T_SPAWN)
    assert (
        codex_session_ids_started_in(
            Path("/w/proj"), _ms(T_SPAWN) - 1, sessions_dir=tmp_path
        )
        == []
    )


def test_session_started_before_the_spawn_is_excluded(tmp_path: Path) -> None:
    """A codex session already open in this cwd is NOT the one we just spawned.

    Mtime cannot make this call: an older session that is still being typed into
    has a fresh mtime. Only the session_meta start timestamp separates them.
    """
    _rollout(tmp_path, SID_A, "/w/proj", T_EARLIER)
    assert (
        codex_session_ids_started_in(
            Path("/w/proj"), _ms(T_SPAWN), sessions_dir=tmp_path
        )
        == []
    )


def test_unreadable_and_malformed_rollouts_are_skipped(tmp_path: Path) -> None:
    day = tmp_path / "2026" / "07" / "24"
    day.mkdir(parents=True)
    (day / "rollout-garbage.jsonl").write_text("not json\n", encoding="utf-8")
    (day / "rollout-wrongtype.jsonl").write_text(
        json.dumps({"type": "response_item", "payload": {}}) + "\n", encoding="utf-8"
    )
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    assert codex_session_ids_started_in(
        Path("/w/proj"), 1, sessions_dir=tmp_path
    ) == [SID_A]


def test_missing_store_returns_empty(tmp_path: Path) -> None:
    assert (
        codex_session_ids_started_in(
            Path("/w/proj"), 1, sessions_dir=tmp_path / "absent"
        )
        == []
    )


# ---------------------------------------------------------------------------
# _backfill_codex_session_id: the spawn-side rule
# ---------------------------------------------------------------------------


def test_backfill_returns_the_unique_id(tmp_path: Path) -> None:
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), 1, sessions_dir=tmp_path, sleep=_no_sleep
        )
        == SID_A
    )


def test_backfill_refuses_an_ambiguous_pair(tmp_path: Path) -> None:
    """Two panes racing in one cwd: stamping either row could point resume at
    the other pane's session, so neither gets an id."""
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    _rollout(tmp_path, SID_B, "/w/proj", "2026-07-24T18:00:01.000Z")
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), 1, sessions_dir=tmp_path, sleep=_no_sleep
        )
        is None
    )


def test_backfill_skips_a_same_cwd_sibling_started_earlier(tmp_path: Path) -> None:
    """The race a too-early spawn timestamp loses.

    A sibling pane that started its codex session before this spawn's pane run
    must not be stamped onto this row. since_ms is sampled at the pane run, so
    the earlier sibling is excluded and this spawn's own session is the unique
    candidate (and the one stamped), not the sibling.
    """
    _rollout(tmp_path, SID_B, "/w/proj", T_EARLIER)
    _rollout(tmp_path, SID_A, "/w/proj", T_SPAWN)
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), _ms(T_SPAWN), sessions_dir=tmp_path, sleep=_no_sleep
        )
        == SID_A
    )


def test_backfill_miss_returns_none_rather_than_raising(tmp_path: Path) -> None:
    """AC2-FR: the pane is already running; a missing id costs resume, not the spawn."""
    assert (
        _backfill_codex_session_id(
            Path("/w/proj"), 1, sessions_dir=tmp_path, sleep=_no_sleep
        )
        is None
    )

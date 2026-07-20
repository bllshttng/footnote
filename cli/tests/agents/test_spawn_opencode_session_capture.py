"""Task 1.2 (x-830c): capture an opencode pane's ``ses_`` id at spawn.

opencode's ``--session`` only continues an EXISTING session, so unlike claude's
pre-minted uuid the id must be discovered after the spawn. Without this capture
the session-id mapping and resume argv are dead code: every opencode row would
carry ``harness_session_id=None`` forever.

Coverage:
  - AC2-EDGE: two same-cwd candidates stamp NEITHER row.
  - AC2-FR: a missed capture leaves the row live-only, non-terminal, and visible.
  - The query matches on exact directory + created-after-spawn, never a project id.
  - Plugin banners and the column header on stdout never become session ids.

Every case drives a stubbed runner: the suite must never read the real
``~/.local/share/opencode`` store (same class as the FNO_AGENTS_HOME trap).
"""
from __future__ import annotations

import subprocess
from pathlib import Path

from fno.agents.mux_spawn import (
    _backfill_opencode_session_id,
    _query_opencode_sessions,
)

SES_A = "ses_09679f284ffeJv7NdBAoLQLnLZ"
SES_B = "ses_09bc06382ffe0u0TVPYYkOuj2N"


def _runner(stdout: str = "", returncode: int = 0, record: list | None = None):
    """A stubbed ``subprocess.run`` that returns fixed output."""

    def run(argv, **kwargs):
        if record is not None:
            record.append(argv)
        return subprocess.CompletedProcess(argv, returncode, stdout=stdout, stderr="")

    return run


def _no_sleep(_seconds: float) -> None:
    """Collapse the retry delay so tests do not pay it."""


# ---------------------------------------------------------------------------
# Unique match -> stamp
# ---------------------------------------------------------------------------


def test_unique_candidate_is_captured() -> None:
    out = _backfill_opencode_session_id(
        Path("/w/proj"), 1784483620000,
        runner=_runner(f"id\n{SES_A}\n"), sleep=_no_sleep,
    )
    assert out == SES_A


def test_plugin_banner_and_header_are_not_mistaken_for_ids() -> None:
    """opencode plugins print to stdout ahead of real output (verified live)."""
    noisy = f"[claude-mem] OpenCode plugin loading\nid\n{SES_A}\n"
    assert _backfill_opencode_session_id(
        Path("/w/proj"), 1, runner=_runner(noisy), sleep=_no_sleep,
    ) == SES_A


def test_query_filters_on_exact_directory_and_spawn_time() -> None:
    """Worktrees of one repo share an opencode project id but not a directory.

    Matching the directory string keeps a sibling worktree's session from being
    mis-attributed; the time bound keeps an already-open session in the same cwd
    from being claimed by this spawn.
    """
    seen: list = []
    _backfill_opencode_session_id(
        Path("/w/proj"), 1784483620000,
        runner=_runner(f"{SES_A}\n", record=seen), sleep=_no_sleep,
    )
    sql = seen[0][2]
    assert seen[0][:2] == ["opencode", "db"]
    assert "directory='/w/proj'" in sql
    assert "time_created >= 1784483620000" in sql
    assert "project_id" not in sql


def test_single_quote_in_cwd_is_escaped() -> None:
    seen: list = []
    _backfill_opencode_session_id(
        Path("/w/o'brien"), 1, runner=_runner("", record=seen), sleep=_no_sleep,
    )
    assert "directory='/w/o''brien'" in seen[0][2]


# ---------------------------------------------------------------------------
# AC2-EDGE — ambiguity stamps neither
# ---------------------------------------------------------------------------


def test_two_candidates_stamp_neither() -> None:
    """Two panes racing in one cwd: a wrong id is worse than no id."""
    assert _backfill_opencode_session_id(
        Path("/w/proj"), 1,
        runner=_runner(f"id\n{SES_A}\n{SES_B}\n"), sleep=_no_sleep,
    ) is None


def test_ambiguity_does_not_burn_retries() -> None:
    """Retrying cannot narrow two matches, so it returns on the first read."""
    calls: list = []
    _backfill_opencode_session_id(
        Path("/w/proj"), 1,
        runner=_runner(f"{SES_A}\n{SES_B}\n", record=calls), sleep=_no_sleep,
    )
    assert len(calls) == 1


# ---------------------------------------------------------------------------
# AC2-FR — a miss is bounded, silent to the pane, and non-terminal
# ---------------------------------------------------------------------------


def test_no_candidate_returns_none_after_bounded_retry() -> None:
    calls: list = []
    assert _backfill_opencode_session_id(
        Path("/w/proj"), 1, runner=_runner("", record=calls), sleep=_no_sleep,
    ) is None
    assert len(calls) == 2  # _OPENCODE_BACKFILL_ATTEMPTS, not unbounded


def test_late_session_row_is_caught_by_the_retry() -> None:
    """Open Q2: the TUI writes its row some time after start, so one try is thin."""
    seq = ["", f"{SES_A}\n"]

    def run(argv, **kwargs):
        return subprocess.CompletedProcess(argv, 0, stdout=seq.pop(0), stderr="")

    assert _backfill_opencode_session_id(
        Path("/w/proj"), 1, runner=run, sleep=_no_sleep,
    ) == SES_A


def test_missing_binary_never_raises() -> None:
    """The pane is already running; a store read failure must not break spawn."""

    def run(argv, **kwargs):
        raise FileNotFoundError("opencode")

    assert _backfill_opencode_session_id(
        Path("/w/proj"), 1, runner=run, sleep=_no_sleep,
    ) is None


def test_timeout_never_raises() -> None:
    def run(argv, **kwargs):
        raise subprocess.TimeoutExpired(cmd="opencode", timeout=5.0)

    assert _backfill_opencode_session_id(
        Path("/w/proj"), 1, runner=run, sleep=_no_sleep,
    ) is None


# ---------------------------------------------------------------------------
# Query layer: "ran clean, found nothing" vs "could not run"
# ---------------------------------------------------------------------------


def test_query_distinguishes_clean_empty_from_failure() -> None:
    assert _query_opencode_sessions("select 1", _runner("", 0)) == []
    assert _query_opencode_sessions("select 1", _runner("", 1)) is None

"""Tests for shared ambient harness session identity resolution."""

from __future__ import annotations

import pytest

from fno.harness_identity import (
    HarnessIdentity,
    canonical_handle,
    resolve_harness_identity,
)


@pytest.mark.parametrize(
    ("marker", "session_id", "harness"),
    [
        ("CODEX_THREAD_ID", "thread-1", "codex"),
        ("CLAUDE_CODE_SESSION_ID", "claude-1", "claude"),
        ("CODEX_SESSION_ID", "codex-1", "codex"),
        ("GEMINI_SESSION_ID", "gemini-1", "gemini"),
    ],
)
def test_resolves_each_supported_marker(marker, session_id, harness):
    assert resolve_harness_identity({marker: f"  {session_id}  "}) == HarnessIdentity(
        session_id=session_id,
        harness=harness,
    )


def test_precedence_favors_codex_thread_id():
    env = {
        "CODEX_THREAD_ID": "thread",
        "CLAUDE_CODE_SESSION_ID": "claude",
        "CODEX_SESSION_ID": "codex-session",
        "GEMINI_SESSION_ID": "gemini",
    }
    assert resolve_harness_identity(env) == HarnessIdentity("thread", "codex")


def test_whitespace_markers_are_skipped_in_precedence_order():
    env = {
        "CODEX_THREAD_ID": "  ",
        "CLAUDE_CODE_SESSION_ID": "\t",
        "CODEX_SESSION_ID": " codex-session ",
    }
    assert resolve_harness_identity(env) == HarnessIdentity("codex-session", "codex")


def test_no_marker_returns_empty_identity():
    assert resolve_harness_identity({}) == HarnessIdentity(None, None)


def test_canonical_handle_is_harness_prefixed_first8():
    assert canonical_handle("codex", "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4") == "codex-019f48e1"
    assert canonical_handle("claude", "abcdef01-2345") == "claude-abcdef01"


def test_ac1_fr_registry_name_equals_canonical_handle(tmp_path):
    """The registry row name a session registers under MUST equal the handle a
    sender resolves and the drain reads, or a queued message strands. Assert the
    registry derives its name via the same shared function (drift fails CI)."""
    from fno.agents.registry import register_existing_session

    sid = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    entry = register_existing_session(
        provider="codex",
        session_id=sid,
        cwd="/tmp",
        registry_path=tmp_path / "agents.json",
    )
    assert entry.name == canonical_handle("codex", sid) == "codex-019f48e1"

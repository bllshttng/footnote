"""Tests for shared ambient harness session identity resolution."""

from __future__ import annotations

import pytest

from fno.harness_identity import (
    HarnessIdentity,
    LEGACY_HANDLE_RE,
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


def test_ac1_hp_canonical_handle_is_bare_first8():
    """The generated mailbox id carries no harness prefix (AC1-HP)."""
    assert canonical_handle("019f48e1-5b09-72a0-9bc8-6b364bcf4ae4") == "019f48e1"
    assert canonical_handle("abcdef01-2345") == "abcdef01"


def test_canonical_handle_takes_no_harness():
    """The signature itself is the documentation: a mailbox id is derived from the
    session id alone. Harness is an envelope attribute and no code path may
    recover it from an address."""
    import inspect

    assert list(inspect.signature(canonical_handle).parameters) == ["session_id"]


def test_ac4_edge_session_id_shorter_than_eight():
    """Boundary: a sub-8-char session id is its own whole handle, never an error."""
    assert canonical_handle("abc") == "abc"
    assert canonical_handle("") == ""


def test_legacy_handle_re_matches_only_the_retired_form():
    """The pattern exists to RECOGNIZE a retired address so the send path can
    refuse it by name - never to accept one."""
    assert LEGACY_HANDLE_RE.match("claude-019f48e1")
    assert LEGACY_HANDLE_RE.match("codex-abcdef01")
    assert not LEGACY_HANDLE_RE.match("019f48e1")  # the real address
    assert not LEGACY_HANDLE_RE.match("fno-019f48e1")  # friendly project alias
    assert not LEGACY_HANDLE_RE.match("tgt-node-claude-g1")  # a mesh name


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
    assert entry.name == canonical_handle(sid) == "019f48e1"

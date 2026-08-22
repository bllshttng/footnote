"""The crown grantor resolves identity through the one owned-identity resolver.

A claude session carrying seven inherited CODEX_* names once stamped a crown
grant with the inherited codex thread id: the registry recorded a codex grantor
for a crown a claude session issued, while ``fno whoami`` and ``fno backlog
idea`` answered the claude session correctly in the same minute. One path of N
reached a different answer, and it was the path that stamps authority.

These tests hold the collapse. Every grantor derivation reaches
``resolve_self_identity``, so a second implementation added to any of them fails
here rather than in review.

The ordering rule the tests pin: proof this process minted a marker comes from
the process-tree walk (the nearest harness ancestor), and the self-set marker
the running binary wrote about itself (``CLAUDECODE``) answers only where the
walk has none. That marker survives a fork, so it names ancestry rather than
self and must never outrank a readable ancestry.
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest

from fno.harness_identity import AMBIENT_IDENTITY_ENV, canonical_handle, self_set_harness

CLAUDE_SID = "119e3c52-62bf-43b4-b3c4-3c7ce659f802"
CODEX_TID = "01a02125-4eb4-7bf1-b74e-d238887eb092"


@pytest.fixture
def poisoned_env(monkeypatch):
    """The measured environment: a real claude session whose env also carries an
    inherited codex thread, plus the marker the claude binary set for itself."""
    for name in AMBIENT_IDENTITY_ENV:
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("CLAUDECODE", "1")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", CLAUDE_SID)
    monkeypatch.setenv("CODEX_THREAD_ID", CODEX_TID)


def _walk(monkeypatch, harness):
    """Pin what the process-tree walk can prove about this process."""
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness", lambda *_a, **_k: harness
    )


# --- the self-set marker ----------------------------------------------------


def test_self_set_harness_reads_the_running_binarys_own_marker(monkeypatch):
    monkeypatch.setenv("CLAUDECODE", "1")
    assert self_set_harness() == "claude"

    monkeypatch.delenv("CLAUDECODE")
    assert self_set_harness() is None


def test_a_blank_marker_is_not_a_marker(monkeypatch):
    monkeypatch.setenv("CLAUDECODE", "  ")
    assert self_set_harness() is None


# --- the grantor derivation -------------------------------------------------


def test_grantor_is_the_claude_session_never_the_inherited_codex_thread(
    poisoned_env, monkeypatch
):
    """The measured defect, inverted into an assertion.

    ``_capture_parent_edge`` is what the spawn path stamps as ``crown_grantor``
    (``spawned_by_session or "human"``), so the session id it returns IS the
    recorded grantor.
    """
    from fno.agents.dispatch import _capture_parent_edge

    _walk(monkeypatch, "claude")
    session_id, harness, _cwd = _capture_parent_edge()

    assert session_id == CLAUDE_SID
    assert session_id != CODEX_TID
    assert harness == "claude"

    grantor = session_id or "human"
    assert grantor == CLAUDE_SID


def test_an_unreadable_ancestry_still_resolves_the_running_claude_session(
    poisoned_env, monkeypatch
):
    """psutil denied, no harness ancestor, a container hiding the parent chain:
    the walk answers nothing and the grant used to record "human" for a crown a
    real session issued. The marker the claude binary wrote settles it."""
    from fno.agents.dispatch import _capture_parent_edge

    _walk(monkeypatch, None)
    session_id, harness, _cwd = _capture_parent_edge()

    assert session_id == CLAUDE_SID
    assert harness == "claude"


def test_a_readable_ancestry_outranks_the_self_set_marker(poisoned_env, monkeypatch):
    """A codex session hand-started from a shell that had run claude inherits
    CLAUDECODE. The walk knows what this process actually runs under, so it
    wins; the fork-surviving marker never overrides it."""
    from fno.agents.dispatch import _capture_parent_edge

    _walk(monkeypatch, "codex")
    session_id, harness, _cwd = _capture_parent_edge()

    assert session_id == CODEX_TID
    assert harness == "codex"


def test_no_marker_and_no_walk_still_refuses_to_guess(poisoned_env, monkeypatch):
    """Removing the self-set marker restores the honest refusal: two families,
    nothing proven, so no lineage is stamped rather than a stranger's."""
    from fno.agents.dispatch import _capture_parent_edge

    monkeypatch.delenv("CLAUDECODE")
    _walk(monkeypatch, None)
    session_id, harness, _cwd = _capture_parent_edge()

    assert session_id is None
    assert harness is None


# --- the shared call path ---------------------------------------------------


def test_whoami_and_the_grantor_stamp_resolve_through_one_call_path(
    poisoned_env, monkeypatch
):
    """The pin against a second implementation.

    ``resolve_owned_identity`` is the single choke point every identity surface
    reaches through ``resolve_self_identity``. Replacing it with a sentinel must
    move whoami's reply handle AND the grantor stamp together. A path that grows
    its own resolver stops reflecting the sentinel and fails here.
    """
    from fno.agents.dispatch import _capture_parent_edge
    from fno.agent.cli import _mail_handle

    sentinel = SimpleNamespace(
        session_id="5e471e10-0000-4000-8000-00000000cafe",
        harness="claude",
        disposition="single",
        markers_present=(),
        rejected=(),
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_owned_identity", lambda *_a, **_k: sentinel
    )

    handle, whoami_session = _mail_handle()
    grantor_session, _harness, _cwd = _capture_parent_edge()

    assert whoami_session == sentinel.session_id
    assert handle == canonical_handle(sentinel.session_id)
    assert grantor_session == sentinel.session_id


def test_the_grant_verb_stamps_the_claude_sessions_row(poisoned_env, monkeypatch):
    """``fno agents crown grant`` names its grantor from ``calling_agent_row``,
    which resolves this session's registry row. With both families present it
    must find the claude row, never the codex one that shares the environment."""
    from fno.agents.crown import calling_agent_row
    from fno.agents.registry import AgentEntry

    claude_row = AgentEntry(
        name="king-claude",
        cwd="/tmp",
        log_path="/tmp/claude.log",
        harness="claude",
        harness_session_id=CLAUDE_SID,
    )
    codex_row = AgentEntry(
        name="king-codex",
        cwd="/tmp",
        log_path="/tmp/codex.log",
        harness="codex",
        harness_session_id=CODEX_TID,
    )
    monkeypatch.setattr(
        "fno.agents.registry.load_registry", lambda *_a, **_k: [codex_row, claude_row]
    )

    _walk(monkeypatch, "claude")
    caller = calling_agent_row()

    assert caller is claude_row
    grantor = "human" if caller is None else caller.name
    assert grantor == "king-claude"


# --- the deleted second mapping ---------------------------------------------


def test_the_setup_doctor_reads_the_shared_marker_table(monkeypatch):
    """doctor.py carried its own ("CLAUDECODE", "claude") literal. It now reads
    the shared table, so the mapping cannot drift into two answers again."""
    from fno.setup.doctor import _detected_harness

    for name in AMBIENT_IDENTITY_ENV:
        monkeypatch.delenv(name, raising=False)
    for name in ("CLAUDE_CONFIG_DIR", "CODEX_HOME"):
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("CLAUDECODE", "1")

    assert _detected_harness() == "claude"


# --- the names fno cannot unexport ------------------------------------------


def test_whoami_names_the_inherited_family_it_cannot_unexport(poisoned_env):
    """fno cannot unexport a variable from a parent that is still running, so
    the operator gets the clearing instruction instead of silence."""
    from fno.agent.cli import _foreign_identity_line

    line = _foreign_identity_line("claude")

    assert line is not None
    assert "inherited codex name" in line
    assert "-u CODEX_THREAD_ID" in line
    # Never offers to strip this session's own markers.
    assert "CLAUDE_CODE_SESSION_ID" not in line


def test_a_clean_session_says_nothing(monkeypatch):
    from fno.agent.cli import _foreign_identity_line

    for name in AMBIENT_IDENTITY_ENV:
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("CLAUDECODE", "1")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", CLAUDE_SID)

    assert _foreign_identity_line("claude") is None

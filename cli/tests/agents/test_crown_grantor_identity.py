"""The crown grantor resolves identity through the one owned-identity resolver.

A claude session carrying seven inherited CODEX_* names once stamped a crown
grant with the inherited codex thread id: the registry recorded a codex grantor
for a crown a claude session issued, while ``fno whoami`` and ``fno backlog
idea`` answered the claude session correctly in the same minute. One path of N
reached a different answer, and it was the path that stamps authority.

These tests hold the collapse. Every grantor derivation reaches
``resolve_self_identity``, so a second implementation added to any of them fails
here rather than in review.

The rule the tests pin: the process-tree walk is the ONLY prover. A self-set
marker such as ``CLAUDECODE`` survives a fork, so it names ancestry rather than
self and must never stand in for the walk - promoting it contradicts a codex
session's own marker and loses a valid identity. With no walk, resolution
refuses rather than guesses, and ``fno whoami`` names the inherited family.
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest

from fno.harness_identity import AMBIENT_IDENTITY_ENV, canonical_handle

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


# --- the self-set marker is not an identity input ---------------------------


def test_a_sole_codex_marker_survives_an_inherited_claude_marker(monkeypatch):
    """The trap this file exists to keep shut.

    CLAUDECODE looks like proof of a claude self: a shell that never ran claude
    cannot produce it. But it survives a fork, so a codex session started from a
    shell that HAD run claude carries it. Reading it as proof there contradicts
    that session's own CODEX_THREAD_ID and turns a cleanly resolving codex
    session into an ambiguous one, taking every identity consumer with it.
    """
    from fno.claims.self_identity import resolve_self_identity

    for name in AMBIENT_IDENTITY_ENV:
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("CODEX_THREAD_ID", CODEX_TID)
    monkeypatch.setenv("CLAUDECODE", "1")
    _walk(monkeypatch, None)

    identity = resolve_self_identity()

    assert identity.session_id == CODEX_TID
    assert identity.harness == "codex"


def test_the_marker_scrubs_so_a_child_never_inherits_it():
    """It resolves nothing, and it still scrubs: a codex child carrying its
    claude parent's CLAUDECODE sends `fno doctor` to the wrong settings file."""
    assert "CLAUDECODE" in AMBIENT_IDENTITY_ENV


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


def test_the_walk_decides_which_family_owns_a_mixed_environment(
    poisoned_env, monkeypatch
):
    """The same environment, two answers, and only ancestry separates them. A
    genuine codex-hosted claude carries the identical name set, which is why
    reordering the marker table cannot fix this and the walk has to."""
    from fno.agents.dispatch import _capture_parent_edge

    _walk(monkeypatch, "codex")
    session_id, harness, _cwd = _capture_parent_edge()

    assert session_id == CODEX_TID
    assert harness == "codex"


def test_no_walk_refuses_to_guess_rather_than_stamping_a_stranger(
    poisoned_env, monkeypatch
):
    """psutil denied, no harness ancestor, a container hiding the parent chain.

    Two families and nothing proven, so no lineage is stamped. The grant records
    "human" and `fno whoami` names the family to clear; that is the honest
    answer, because the environment alone cannot say which session this is.
    """
    from fno.agents.dispatch import _capture_parent_edge

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


def test_whoami_names_the_inherited_family_it_cannot_unexport(
    poisoned_env, monkeypatch
):
    """fno cannot unexport a variable from a parent that is still running, so
    the operator gets the clearing instruction instead of silence."""
    from fno.agent.cli import _foreign_identity_line

    _walk(monkeypatch, "claude")
    line = _foreign_identity_line()

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
    _walk(monkeypatch, "claude")

    assert _foreign_identity_line() is None


def test_an_opencode_session_is_never_told_to_delete_its_own_marker(monkeypatch):
    """The keep-family must come from the resolver, not from `_detect_harness`.

    That helper fails open to "claude" for any harness outside claude/codex/
    gemini, so a cleanly resolving opencode session would be handed the command
    to unset its own OPENCODE_SESSION_ID, costing it its mail handle and its
    claims. It prints on every whoami and every SessionStart injection, so the
    wrong advice is loud.
    """
    from fno.agent.cli import _foreign_identity_line

    for name in AMBIENT_IDENTITY_ENV:
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("OPENCODE_SESSION_ID", "ses_9f2a")
    _walk(monkeypatch, "opencode")

    assert _foreign_identity_line() is None


def test_the_remedy_never_offers_to_delete_the_hermes_spawn_guard(monkeypatch):
    """HERMES_SESSION_ID is a fail-closed guard, not a session identity:
    HermesCliAdapter reads it as "shell spawn is FORBIDDEN". It is never a
    keep-family, so an unguarded strip list would offer to delete it and turn
    that refusal into a real spawn."""
    from fno.harness_identity import ambient_identity_strip_flags

    for name in AMBIENT_IDENTITY_ENV:
        monkeypatch.delenv(name, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", CLAUDE_SID)
    monkeypatch.setenv("HERMES_SESSION_ID", "hermes-1")
    monkeypatch.setenv("CODEX_THREAD_ID", CODEX_TID)

    flags = ambient_identity_strip_flags("claude")

    assert "HERMES_SESSION_ID" not in flags
    assert "CODEX_THREAD_ID" in flags


def test_an_unclassified_name_is_never_suggested_for_deletion(monkeypatch):
    """The docstring has always promised that a name missing from
    AMBIENT_IDENTITY_FAMILY is not stripped. The old membership test compared
    None against the keep list, found no match, and stripped it anyway."""
    import fno.harness_identity as hi

    monkeypatch.setattr(hi, "AMBIENT_IDENTITY_ENV", ("MYSTERY_SESSION_ID",))
    monkeypatch.setenv("MYSTERY_SESSION_ID", "whatever")

    assert hi.ambient_identity_strip_flags("claude") == []

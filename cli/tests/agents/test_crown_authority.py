"""grant_error authority: every path that cannot place the caller fails CLOSED.

Three distinct "not a known agent" states must each refuse rather than collapse
to the attended-human ``None`` that authorizes any grant:

- ``REGISTRY_UNREADABLE`` - ``load_registry`` raised;
- ``AGENT_UNREGISTERED`` - identity present, but ``_find_by_session`` returned
  ``None`` on a clean miss (no row yet); it does NOT raise, so without this
  sentinel the miss flowed out as the human ``None``.
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest

from fno.agents.crown import (
    AGENT_UNREGISTERED,
    REGISTRY_UNREADABLE,
    calling_agent_row,
    grant_error,
)


def _agent_identity():
    """An identity with a session + harness, so calling_agent_row reaches the
    registry lookup instead of returning the attended-human None early."""
    return SimpleNamespace(session_id="ses-deadbeef", harness="claude")


def _owned_agent_identity():
    return SimpleNamespace(session_id="ses-deadbeef", harness="claude", disposition="single")


def test_registry_read_failure_refuses_grant(monkeypatch):
    """An agent whose registry cannot be read is NOT promoted to human authority."""
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity", _owned_agent_identity
    )

    def boom(*args, **kwargs):
        raise OSError("registry unreadable")

    monkeypatch.setattr("fno.agents.registry.load_registry", boom)

    caller = calling_agent_row()
    # Not None: None means attended human and would authorize any grant.
    assert caller is REGISTRY_UNREADABLE

    problem = grant_error("some-scope", caller)
    # Refused, not authorized: a non-None reason naming the unreadable registry.
    assert problem is not None
    assert "registry" in problem.lower()


def test_attended_human_still_authorized(monkeypatch):
    """The fail-closed fix must not break the legitimate attended-human path:
    no agent identity at all -> any grant authorized."""
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    assert calling_agent_row() is None
    assert grant_error("any-scope", None) is None


def test_known_agent_passes_through_to_crown_check(monkeypatch):
    """A readable registry with a matching row is NOT misread as unreadable:
    the sentinel is reserved for failures, not for found rows."""
    row = SimpleNamespace(
        crown_scope=None, crown_level=None, crown_grantor=None
    )
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity", _owned_agent_identity
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [row])
    monkeypatch.setattr(
        "fno.agents.whoami._find_by_session", lambda *a, **k: row
    )

    caller = calling_agent_row()
    assert caller is row
    # An uncrowned agent (crown_scope None) is refused on its own merits, not
    # because the registry was unreadable.
    problem = grant_error("some-scope", caller)
    assert problem is not None
    assert "registry" not in problem.lower()


def test_unregistered_agent_refuses_grant(monkeypatch):
    """An agent whose identity is present but has no registry row (a clean miss,
    NOT an exception) is not promoted to human authority. _find_by_session
    returns None without raising, so this never reached the REGISTRY_UNREADABLE
    except branch - it is the third fail-open path, one branch over."""
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity", _owned_agent_identity
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
    monkeypatch.setattr(
        "fno.agents.whoami._find_by_session", lambda *a, **k: None
    )

    caller = calling_agent_row()
    # Not None: a clean miss is an unregistered agent, not an attended human.
    assert caller is AGENT_UNREGISTERED

    problem = grant_error("some-scope", caller)
    assert problem is not None
    # The refusal names the heal, not just "registry".
    assert "/fno-me" in problem or "wait for the row" in problem


def test_is_caller_row_is_harness_scoped_and_delegates() -> None:
    """is_caller_row must not match a row of a DIFFERENT harness via a colliding
    id (a codex session id equal to a claude holder's cc_session_id), or the
    spawn would vacate an unrelated holder's crown. Delegating to _find_by_session
    makes the succession check agree with the auth check for free."""
    from fno.agents.registry import AgentEntry
    from fno.agents.whoami import is_caller_row

    holder = AgentEntry(
        name="k",
        cwd="/w",
        log_path="",
        harness="claude",
        harness_session_id=None,
        cc_session_id="collide-x",
        short_id="k",
        status="busy",
    )
    # A codex caller with the same id must NOT match the claude holder.
    assert is_caller_row(holder, "collide-x", harness="codex") is False
    # The claude caller itself matches (via cc_session_id, harness-scoped).
    assert is_caller_row(holder, "collide-x", harness="claude") is True

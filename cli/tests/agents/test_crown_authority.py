"""grant_error authority: a registry read failure fails CLOSED, not open.

``calling_agent_row`` returns ``REGISTRY_UNREADABLE`` (distinct from the
attended-human ``None``) when ``load_registry`` raises, and ``grant_error``
refuses on it rather than authorizing like a human. This is the first coverage
of the grantor-check path: previously a swallowed registry error promoted any
spawned worker to attended-human authority.
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest

from fno.agents.crown import REGISTRY_UNREADABLE, calling_agent_row, grant_error


def _agent_identity():
    """An identity with a session + harness, so calling_agent_row reaches the
    registry lookup instead of returning the attended-human None early."""
    return SimpleNamespace(session_id="ses-deadbeef", harness="claude")


def test_registry_read_failure_refuses_grant(monkeypatch):
    """An agent whose registry cannot be read is NOT promoted to human authority."""
    monkeypatch.setattr(
        "fno.harness_identity.resolve_harness_identity", _agent_identity
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
        "fno.harness_identity.resolve_harness_identity",
        lambda: SimpleNamespace(session_id=None, harness=None),
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
        "fno.harness_identity.resolve_harness_identity", _agent_identity
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

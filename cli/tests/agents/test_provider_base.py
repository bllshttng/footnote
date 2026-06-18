"""Tests for the lifted ReachabilityProbeError base class (US4-gemini Wave 1.1).

Coverage targets the invariants in the design doc:

- The base class carries ``provider`` (non-optional) + ``reason``.
- A broad ``except RuntimeError`` upstream still catches the lifted error
  (regression seed for AC1-ERR).
- Pickle round-trips (AC1-EDGE).
- Every provider's inconclusive probe raises the shared base class with
  its ``provider`` discriminator (the per-provider deprecated aliases
  ``ClaudeReachabilityProbeError`` / ``SessionIndexReadError`` were removed
  one release cycle after Wave 1.1).
"""
from __future__ import annotations

import pickle
from pathlib import Path

import pytest

from fno.agents.providers.base import ReachabilityProbeError


def test_base_class_has_provider_and_reason_attributes() -> None:
    """AC1-HP: the base class records both fields from its kw-only init."""
    err = ReachabilityProbeError(provider="gemini", reason="EACCES")
    assert err.provider == "gemini"
    assert err.reason == "EACCES"


def test_base_class_message_format() -> None:
    """The formatted message includes both provider and reason verbatim."""
    err = ReachabilityProbeError(provider="codex", reason="permission denied")
    assert "codex" in str(err)
    assert "permission denied" in str(err)


def test_base_class_is_runtime_error_subclass() -> None:
    """AC1-ERR seed: a broad ``except RuntimeError`` upstream still catches us.

    Sigma-review pattern hunt would flag a broad catch in dispatch.py as a
    silent failure risk; we assert here so the class hierarchy is stable
    if anyone refactors RuntimeError to BaseException.
    """
    assert issubclass(ReachabilityProbeError, RuntimeError)


def test_base_class_requires_keyword_arguments() -> None:
    """Locked Decision 3: provider + reason are kw-only; no positional shape."""
    with pytest.raises(TypeError):
        ReachabilityProbeError("gemini", "EACCES")  # type: ignore[misc]


def test_base_class_provider_is_non_optional() -> None:
    """Invariant: provider must always be supplied (defensive: no default)."""
    with pytest.raises(TypeError):
        ReachabilityProbeError(reason="EACCES")  # type: ignore[call-arg]


def test_base_class_reason_is_non_optional() -> None:
    """Invariant: reason must always be supplied (no default)."""
    with pytest.raises(TypeError):
        ReachabilityProbeError(provider="claude")  # type: ignore[call-arg]


def test_base_class_pickle_round_trip() -> None:
    """AC1-EDGE: pickle.dumps/loads survives the kw-only init signature."""
    err = ReachabilityProbeError(provider="gemini", reason="EACCES")
    restored = pickle.loads(pickle.dumps(err))
    assert isinstance(restored, ReachabilityProbeError)
    assert restored.provider == "gemini"
    assert restored.reason == "EACCES"
    assert str(restored) == str(err)


def test_base_class_repr_does_not_raise() -> None:
    """AC1-EDGE: ``repr(e)`` must not raise even with kw-only init."""
    err = ReachabilityProbeError(provider="claude", reason="X")
    # Bare repr should be safe — Python's default Exception.__repr__ inspects
    # args[0]; our super().__init__(formatted_msg) populates that slot.
    rendered = repr(err)
    assert "ReachabilityProbeError" in rendered


# ---------------------------------------------------------------------------
# AC1-FR — portability: a hypothetical third provider plugs in cleanly
# ---------------------------------------------------------------------------


def test_portability_third_provider_uses_base_directly() -> None:
    """A new provider (e.g. opencode) can raise the base class without
    a per-provider subclass. This is the load-bearing reason we lifted
    the class: extending the contract to N=3+ providers must not require
    edits to base.py.
    """
    err = ReachabilityProbeError(provider="opencode", reason="phase-7-pending")
    assert err.provider == "opencode"
    assert err.reason == "phase-7-pending"
    # The same catch-all dispatch.py path that handles claude+codex
    # catches the new provider's instance.
    try:
        raise err
    except ReachabilityProbeError as caught:
        assert caught.provider == "opencode"


@pytest.mark.parametrize("provider", ["claude", "codex", "gemini"])
def test_provider_reachability_probes_raise_shared_base_class(
    provider: str,
    tmp_path,
    monkeypatch,
) -> None:
    """Contract-drift guard: every provider's inconclusive probe path
    raises the shared base class and sets the provider discriminator.
    """
    if provider == "claude":
        from fno.agents.providers import claude as claude_mod

        def _raise_os_error(*args, **kwargs):
            raise OSError("simulated probe failure")

        monkeypatch.setattr(claude_mod, "_subprocess_run", _raise_os_error)

        def probe():
            return claude_mod.claude_logs_reachable("abc12345", timeout=0.01)
    elif provider == "codex":
        from fno.agents.providers import codex as codex_mod

        unreadable_index = tmp_path / "session_index.jsonl"
        unreadable_index.mkdir()

        def probe():
            return codex_mod.load_known_session_ids(session_index_path=unreadable_index)
    else:
        from fno.agents.providers import gemini as gemini_mod

        monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path / "home"))

        def probe():
            return gemini_mod.gemini_session_reachable(
                "aaaaaaaa-1111-2222-3333-444444444444",
                tmp_path / "project",
            )

    with pytest.raises(ReachabilityProbeError) as exc_info:
        probe()

    assert exc_info.value.provider == provider

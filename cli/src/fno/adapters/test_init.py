"""Registry tests for the adapter package."""
from __future__ import annotations

import pytest

from fno.adapters import _REGISTRY, get_adapter
from fno.adapters.claude_code import ClaudeCodeAdapter
from fno.adapters.codex import CodexCliAdapter
from fno.adapters.hermes import HermesCliAdapter


def test_registry_contains_claude_code_and_codex():
    """Both built-in adapters must be registered after import."""
    assert "claude-code" in _REGISTRY
    assert "codex" in _REGISTRY


def test_registry_contains_hermes():
    """AC2.1-HP: hermes adapter is registered after import."""
    assert "hermes" in _REGISTRY


def test_registry_contains_exactly_known_adapters():
    """A surprise registration would slip past a membership-only check; pin the shape."""
    assert set(_REGISTRY.keys()) == {"claude-code", "codex", "hermes"}


def test_get_adapter_returns_claude_code_instance():
    adapter = get_adapter("claude-code")
    assert isinstance(adapter, ClaudeCodeAdapter)


def test_get_adapter_returns_codex_instance():
    """AC3.1-HP: get_adapter('codex') returns a CodexCliAdapter without raising."""
    adapter = get_adapter("codex")
    assert isinstance(adapter, CodexCliAdapter)
    assert adapter.name == "codex"


def test_get_adapter_returns_hermes_instance():
    """AC2.1-HP: get_adapter('hermes') returns a HermesCliAdapter without raising."""
    adapter = get_adapter("hermes")
    assert isinstance(adapter, HermesCliAdapter)
    assert adapter.name == "hermes"


def test_get_adapter_unknown_raises_with_available_list():
    """Unknown name must raise ValueError and name the available adapters."""
    with pytest.raises(ValueError) as exc_info:
        get_adapter("nope")
    msg = str(exc_info.value)
    assert "nope" in msg
    assert "claude-code" in msg
    assert "codex" in msg
    assert "hermes" in msg


def test_registry_registration_is_idempotent():
    """Re-registering the same adapter does not duplicate entries.

    The _register function uses dict assignment so re-import is safe;
    this pins that invariant.
    """
    from fno.adapters import _register

    before = dict(_REGISTRY)
    _register("codex", CodexCliAdapter)
    after = dict(_REGISTRY)

    assert before.keys() == after.keys()
    assert after["codex"] is before["codex"]

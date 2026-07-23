"""Adapter registry for RuntimeAdapter implementations."""
from __future__ import annotations

from fno.adapters.base import AdapterHealth, RuntimeAdapter

_REGISTRY: dict[str, type] = {}


def _register(name: str, cls: type) -> None:
    _REGISTRY[name] = cls


def get_adapter(name: str) -> RuntimeAdapter:
    """Return an instantiated adapter by name.

    Raises ValueError for unknown adapter names.
    """
    if name not in _REGISTRY:
        raise ValueError(f"unknown adapter: {name!r}. Available: {list(_REGISTRY)}")
    return _REGISTRY[name]()


# Register built-in adapters
from fno.adapters.claude_code import ClaudeCodeAdapter  # noqa: E402

_register("claude-code", ClaudeCodeAdapter)

from fno.adapters.codex import CodexCliAdapter  # noqa: E402

_register("codex", CodexCliAdapter)

from fno.adapters.hermes import HermesCliAdapter  # noqa: E402

_register("hermes", HermesCliAdapter)

__all__ = ["get_adapter", "RuntimeAdapter", "AdapterHealth"]

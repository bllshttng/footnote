"""`fno research` - retrieve + store (ddgs backbone -> sources.jsonl)."""
from __future__ import annotations

__all__ = ["research_command"]


def __getattr__(name: str):
    # Lazy: defer the typer-dependent CLI import until the command is invoked
    # (mirrors fno.executor's lazy attribute pattern - keeps `fno --help` cheap).
    if name == "research_command":
        from fno.research.cli import research_command

        return research_command
    raise AttributeError(f"module 'fno.research' has no attribute {name!r}")

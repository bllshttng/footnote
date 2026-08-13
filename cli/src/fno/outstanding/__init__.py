"""`fno outstanding` - what is piled up waiting on a human.

Read-only over two stores that already exist: the carve-out ledger and the
operator-question events. This package never writes graph.json or
target-state.md, and the read verb never mutates anything at all.
"""
from fno.outstanding.cli import outstanding_app

__all__ = ["outstanding_app"]

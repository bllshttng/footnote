"""Leaf error types for the dispatch seam.

Harness modules raise the same refusal without importing dispatch.
"""

from __future__ import annotations


class DispatchAskError(RuntimeError):
    """Any callable failure, carrying the exit code the CLI propagates."""

    def __init__(self, message: str, *, exit_code: int) -> None:
        super().__init__(message)
        self.exit_code = exit_code

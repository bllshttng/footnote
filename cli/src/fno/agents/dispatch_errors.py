"""Leaf error types for the dispatch seam.

Dispatch raises these; harness modules raise them too, so a refusal can
be expressed at the store boundary without importing dispatch itself.
"""

from __future__ import annotations


class DispatchAskError(RuntimeError):
    """Raised by the dispatch helpers for any callable failure.

    Carries the exit code the CLI layer should propagate to the shell.
    """

    def __init__(self, message: str, *, exit_code: int) -> None:
        super().__init__(message)
        self.exit_code = exit_code

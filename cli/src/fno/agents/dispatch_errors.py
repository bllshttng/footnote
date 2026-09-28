"""Leaf error types for the dispatch seam.

Harness modules raise the same refusal without importing dispatch.
"""

from __future__ import annotations


class DispatchAskError(RuntimeError):
    """Any callable failure, carrying the exit code the CLI propagates."""

    def __init__(self, message: str, *, exit_code: int) -> None:
        super().__init__(message)
        self.exit_code = exit_code


class RouteRestoreRefused(DispatchAskError):
    """A resume relaunch refused because its recorded route cannot be restored.

    Carries the same exit 2 as every other route-composition refusal, but names
    the cause so a caller mapping exit 2 (the wake lane's name-collision answer)
    can tell "a concurrent wake won" from "nothing started, the route is gone".
    """

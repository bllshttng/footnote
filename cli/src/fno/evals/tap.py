"""TAP-lite assertion-output parser.

Accepts a subset of TAP (Test Anything Protocol) output:

    ok <name>
    not ok <name>

All other lines (comments, diagnostics, plan lines like ``1..N``) are
ignored.  An empty result list is intentionally returned for empty or
noise-only input - the caller should treat zero assertions as a failure
(the design contract: "an empty assertion script must not pass silently").
"""
from __future__ import annotations

from dataclasses import dataclass


@dataclass
class Assertion:
    """A single TAP-lite assertion result.

    Attributes:
        name: The assertion label (text after ``ok`` / ``not ok``).
        ok:   True if the assertion passed, False if it failed.
    """

    name: str
    ok: bool


def parse_tap(text: str) -> list[Assertion]:
    """Parse TAP-lite output and return a list of :class:`Assertion` objects.

    Accepted line forms::

        ok <name>
        not ok <name>

    Any other line is silently ignored.  An empty ``text`` or text with no
    recognised assertion lines yields an empty list.

    Args:
        text: Raw stdout from an ``assert.sh`` run.

    Returns:
        List of :class:`Assertion` objects in the order they appeared.
        Returns ``[]`` for empty or noise-only input.
    """
    results: list[Assertion] = []
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("not ok "):
            name = stripped[len("not ok "):].strip()
            results.append(Assertion(name=name, ok=False))
        elif stripped.startswith("ok "):
            name = stripped[len("ok "):].strip()
            results.append(Assertion(name=name, ok=True))
    return results

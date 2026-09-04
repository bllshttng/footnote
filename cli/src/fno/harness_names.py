"""Canonical harness-name list (L0 platform data).

The COMPLETE roster of harnesses footnote supports, held equal to the union of
three shipped evidence surfaces (setup docs, the Rust provider dispatch, the
Python adapter registry) by ``scripts/ci/check-harness-roster-parity.py``.
Pure data with no ``fno`` imports, so any layer may read it without a
cross-layer edge. The runtime capability table
(``fno.agents.harness_map._HARNESS_CAPS``) asserts its keys are a SUBSET of
this list at import time: a capability row naming a harness absent here fails
loudly, while a roster entry with no capability row (hermes, openclaw today)
is a supported identity without a native fno spawn - legal, and deliberately
not a capability.

This inverts the old derivation (names read FROM the capability table) so the
platform layer (``fno.harness_identity``) no longer reaches into the runtime
for the name set, which dragged ``fno.agents`` in at import time (x-cec8). The
name set is the source of truth; the capability table validates against it.
"""
from __future__ import annotations

# Every harness supported by shipped evidence, not every harness with a
# capability row. hermes and openclaw host real sessions per docs/SETUP-*.md
# but have no native fno dispatch, so they carry no row in
# fno.agents.harness_map._HARNESS_CAPS; the map asserts capability keys stay a
# subset of this tuple at import time. Order is capability-table order, then
# newly recognized hosts appended; readers that need sorted output call
# sorted() (as known_harnesses() does).
KNOWN_HARNESSES: tuple[str, ...] = (
    "claude",
    "codex",
    "gemini",
    "agy",
    "opencode",
    "pi",
    "hermes",
    "openclaw",
    "cursor-agent",
    "grok",
)

# Every harness with a BUILT thread-spawn arm: opencode through its launch
# seam, and cursor-agent, pi, grok and agy through the keeper lane. A name
# joins on journey evidence, never on roster growth, and the lane table in
# docs/architecture/thread-lanes.md carries the measurement behind each row.
#
# Membership answers "is there a seam arm". The capability row answers "is the
# lane measured". They are different questions, which is why pi and agy sit
# here while their HEADLESS lanes stay unmeasured and `_check_spawn_harness`
# refuses those by stance. kimi is absent for the same reason: its ACP mint
# lane is built and unit-tested, but the binary refuses every turn until its
# provider is configured, so nothing measured stands behind a seat.
SPAWN_HARNESSES: tuple[str, ...] = (
    "claude",
    "codex",
    "opencode",
    "cursor-agent",
    "pi",
    "grok",
    "agy",
)


def unknown_thread_harness_message(name: str) -> str:
    """The one refusal every thread-substrate seam raises.

    Both halves derive from the tuples in this module, so no seam can name a
    harness the accept list has since admitted. The ROSTER, not the capability
    table, decides the second sentence: the pane lane execs whatever is on
    PATH, so a capability row is not what earns one. An unrecognized name gets
    no such pointer, because nothing here knows the binary exists.

    A missing thread lane is a statement about what fno has BUILT, never about
    the harness: any harness can host a thread once its lane is measured
    (docs/architecture/thread-lanes.md).
    """
    accepted = ", ".join(SPAWN_HARNESSES)
    lines = [
        f"unknown harness {name!r} on the thread substrate (--harness names "
        f"the CLI BINARY); accepted here: {accepted}.",
    ]
    if name in KNOWN_HARNESSES:
        lines.append(f"{name} has no measured thread lane yet; use --substrate pane.")
    lines.append("If you meant a model VENDOR, that is -P/--provider.")
    return "\n".join(lines)

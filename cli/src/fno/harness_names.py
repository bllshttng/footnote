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

import tomllib
from pathlib import Path

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

# The spawn roster reads from the capability table, not from a literal here
# (x-a3e8). One answer used to live in two places - a Python tuple plus a
# comment saying which rows it meant - and the two could not be checked
# against each other. Now the row IS the answer: a harness belongs exactly
# when its [harness.<name>.features.spawn] stanza reads native, with the
# journey evidence cited on the row itself. Still pure stdlib file reading -
# the module's no-fno-imports contract above is what keeps this layer L0.
_PACKAGED_CONTRACT = Path(__file__).parent / "agents" / "harness_capabilities.toml"


def _capability_table() -> dict:
    """The packaged capability table as plain dicts. Loud on a broken
    install: a missing or unparseable table is an import-time failure, the
    same posture harness_map takes when it loads the same file."""
    with _PACKAGED_CONTRACT.open("rb") as handle:
        return tomllib.load(handle)


def _spawn_state(name: str, table: dict | None = None) -> str:
    """``features.spawn.state`` for one capability row, ``unmeasured`` when
    the row carries no features stanza for it. The absent-row default is
    the honest one: a feature nobody measured is a gap, never a seat."""
    row = (table or _capability_table()["harness"]).get(name) or {}
    claim = row.get("features") or {}
    return claim.get("spawn", {}).get("state", "unmeasured")


SPAWN_HARNESSES: tuple[str, ...] = tuple(
    sorted(
        name
        for name, row in _capability_table()["harness"].items()
        if (row.get("features") or {}).get("spawn", {}).get("state") == "native"
    )
)


def pane_only_harnesses() -> tuple[str, ...]:
    """Declared harnesses with NO wired spawn arm - the derived form of the
    sentence the two spawn refusals used to hardcode by hand and had to
    keep in agreement. Sorted, so a refusal's accepted text is stable."""
    table = _capability_table()["harness"]
    return tuple(
        sorted(name for name in table if _spawn_state(name, table) != "native")
    )


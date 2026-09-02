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
)

# Thread/headless accepts opencode through its launch seam. agy and gemini are
# pane-only and stay out of this tuple.
#
# pi is pane-only TODAY and stays out for the same reason opencode's thread bit
# once read false: the lane has to exist before the roster advertises it. pi's
# rpc transport is built and tested (`fno.agents.harnesses.pi.PiRpcSession`)
# and its keeper lane has a passing journey, but `dispatch_spawn` still has no
# pi arm, so listing pi here would send a `--substrate thread` spawn past this
# gate and into the terminal else-branch, which refuses by naming gemini's
# retirement - a harness the operator never mentioned. A wrong refusal is worse
# than an honest one, and the honest one is this tuple.
#
# cursor-agent joins on the keeper lane (x-61bc's generic thread lane): the
# dispatch_spawn arm mints the chat id through `create-chat`, hosts the TUI
# under `fno-agents-worker --keeper`, and the journey backs it. Its `thread`
# row reads true behind that same journey.
SPAWN_HARNESSES: tuple[str, ...] = ("claude", "codex", "opencode", "cursor-agent")

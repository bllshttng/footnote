"""Canonical harness-name list (L0 platform data).

The set of harness names footnote knows. Pure data with no ``fno`` imports, so
any layer may read it without a cross-layer edge. The runtime capability table
(``fno.agents.harness_map._HARNESS_CAPS``) asserts its keys stay in sync with
this list at import time, so adding a harness is one coupled change: the name
lands here AND the capability table provides it, or the table fails loudly.

This inverts the old derivation (names read FROM the capability table) so the
platform layer (``fno.harness_identity``) no longer reaches into the runtime
for the name set, which dragged ``fno.agents`` in at import time (x-cec8). The
name set is the source of truth; the capability table validates against it.
"""
from __future__ import annotations

# The canonical, capability-backed harness set. Add a harness here AND in
# fno.agents.harness_map._HARNESS_CAPS in the same change; the map asserts the
# two agree at import time. Order is capability-table order, not alphabetical;
# readers that need sorted output call sorted() (as known_harnesses() does).
KNOWN_HARNESSES: tuple[str, ...] = ("claude", "codex", "gemini", "agy", "opencode")

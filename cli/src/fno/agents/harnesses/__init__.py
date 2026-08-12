"""Harness adapters for fno agents (Phase 1 substrate).

Each maintained headless harness module (claude, codex) owns its subprocess
adapter. Gemini remains a readable/pane-hosted legacy identity, but has no
Python ask adapter.

The exported roster names below keep their ``PROVIDER`` spelling on purpose:
they are the wire vocabulary of the ``provider`` config field and the registry
rows, which the four-axis ruling deliberately leaves in place. Renaming the
package fixed the container; the field is a separate, ruled-out surface.
"""

# Harnesses Python can DISPATCH (select_provider + availability checks).
# THE dispatch gate (x-8dfc): enforced only at the spawn/ask seam (dispatch.py
# _check_known_provider, spawn_defaults, mux_spawn), never at registry LOAD --
# the load gate is a shape check now, so an alien harness reads fine and is
# refused only where a dispatchable provider is actually required.
KNOWN_PROVIDERS: tuple[str, ...] = ("claude", "codex")

# The spawn/pane read-tolerance roster: harnesses a pane can HOST even without a
# Python ask adapter. `agy` (Antigravity) and `opencode` (x-51f6) land pane rows
# via Rust spawn paths / the mux pane back half. NOTE (x-8dfc): this is no longer
# the registry LOAD gate -- load_registry now shape-checks identity, so a row
# with any provider reads without bricking. READABLE_PROVIDERS survives as the
# spawn-default / pane-host tolerance set (spawn_defaults.py), NOT a read gate.
# Mirrors Rust's KNOWN_PROVIDERS in provider.rs (a cli test pins the two lists).
READABLE_PROVIDERS: tuple[str, ...] = (
    "claude",
    "codex",
    "gemini",
    "agy",
    "opencode",
)

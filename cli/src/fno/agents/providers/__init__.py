"""Provider adapters for fno agents (Phase 1 substrate).

Each provider module (claude, codex, gemini) lands in its own file in
Phase 2 onward. Phase 1 only ships the shared ``ProviderResult``
dataclass plus the dispatch-layer availability detection.
"""

# Providers Python can DISPATCH (select_provider + availability checks).
# THE dispatch gate (x-8dfc): enforced only at the spawn/ask seam (dispatch.py
# _check_known_provider, spawn_defaults, mux_spawn), never at registry LOAD --
# the load gate is a shape check now, so an alien harness reads fine and is
# refused only where a dispatchable provider is actually required.
KNOWN_PROVIDERS: tuple[str, ...] = ("claude", "codex", "gemini")

# The spawn/pane read-tolerance roster: providers a pane can HOST even without a
# Python ask adapter. `agy` (Antigravity) and `opencode` (x-51f6) land pane rows
# via Rust spawn paths / the mux pane back half. NOTE (x-8dfc): this is no longer
# the registry LOAD gate -- load_registry now shape-checks identity, so a row
# with any provider reads without bricking. READABLE_PROVIDERS survives as the
# spawn-default / pane-host tolerance set (spawn_defaults.py), NOT a read gate.
# Mirrors Rust's KNOWN_PROVIDERS in provider.rs (a cli test pins the two lists).
READABLE_PROVIDERS: tuple[str, ...] = KNOWN_PROVIDERS + ("agy", "opencode")

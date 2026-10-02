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
# THE dispatch gate: enforced only at the spawn/ask seam (dispatch.py
# _check_known_provider, spawn_defaults, mux_spawn), never at registry LOAD --
# the load gate is a shape check now, so an alien harness reads fine and is
# refused only where a dispatchable provider is actually required.
KNOWN_PROVIDERS: tuple[str, ...] = ("claude", "codex")

# Harnesses the thread/headless spawn seam accepts. The pane-only roster lives
# in mux_spawn.PANE_HOSTABLE_PROVIDERS and remains wider than this tuple.
from fno.harness_names import SPAWN_HARNESSES as SPAWN_HARNESSES  # noqa: E402

# The spawn/pane read-tolerance roster: harnesses a pane can HOST even without a
# Python ask adapter. It is Rust's KNOWN_PROVIDERS (provider.rs), read once
# per process through `fno-agents harness-roster`; there is no Python copy.
# Not the registry LOAD gate: load_registry shape-checks identity.
from typing import Any  # noqa: E402


def __getattr__(name: str) -> Any:
    """PEP 562: serve ``READABLE_PROVIDERS`` from the Rust roster on first read."""
    if name == "READABLE_PROVIDERS":
        from fno.harness_names import known_providers

        return known_providers()
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")

"""Canonical harness-name door (L0 platform data).

The COMPLETE roster of harnesses footnote support lives in Rust, as
``KNOWN_HARNESSES`` in ``crates/fno-agents/src/provider.rs`` - the one list
(x-bd68). This module is its Python door: reading ``KNOWN_HARNESSES`` here
runs ``fno-agents harness-roster`` once per process and caches the answer in
the module globals, so the graph-store and doctor regexes that run per write
never pay a subprocess per call. There is no Python tuple copy to drift; a
name added in Rust alone reaches every Python reader. The read is
fail-closed: a missing binary, a failed verb, or an unusable answer raises
``VerbUnavailable`` naming the remedy, never an empty roster that reads as
agreement. ``scripts/ci/check-harness-roster-parity.py`` holds every evidence
surface (setup docs, the Rust ``for_name`` arms, the Python adapter registry)
as a subset of the roster it parses from the same Rust source.

Pure platform layer with no ``fno.agents`` import: the only import is
``fno.rust_binary`` (stdlib-only), so importing this module drags no runtime.
The runtime capability table (``fno.agents.harness_map._HARNESS_CAPS``)
asserts its keys are a SUBSET of this list at import time: a capability row
naming a harness absent from the roster fails loudly, while a roster entry
with no capability row (hermes, openclaw today) is a supported identity
without a native fno spawn - legal, and deliberately not a capability.

``SPAWN_HARNESSES`` stays Python: it is the set of BUILT thread/headless
seam arms (measured per docs/architecture/thread-lanes.md), not roster
membership.
"""
from __future__ import annotations

from typing import TYPE_CHECKING, Any

from fno.rust_binary import (
    VerbUnavailable,
    call_binary_json,
    find_dev_binary,
    resolve_binary,
)

if TYPE_CHECKING:  # the served attr, for type checkers; resolved lazily below
    KNOWN_HARNESSES: tuple[str, ...]

# Every harness with a BUILT spawn-seam arm: opencode through its launch
# joins on journey evidence, never on roster growth; the measurement behind
# each row is in docs/architecture/thread-lanes.md. Membership answers "is
# there a seam arm", the row answers "is the lane measured", which is why pi
# and agy sit here while their HEADLESS lanes stay unmeasured. kimi is absent
# because its ACP lane refuses every turn until a provider is configured.
SPAWN_HARNESSES: tuple[str, ...] = (
    "claude",
    "codex",
    "opencode",
    "cursor-agent",
    "pi",
    "grok",
    "agy",
    # zcode's arm is the headless one-shot seam (client.rs), not a thread
    # keeper; its row carries state_root_grant.headless measured.
    "zcode",
)

#: The subprocess bound for the one roster read: a fork plus a const print,
#: so anything slower is a wedged binary worth refusing fast.
_ROSTER_TIMEOUT_S = 15


def _load_known_harnesses() -> tuple[str, ...]:
    """One subprocess read of the Rust roster; fail-closed on every path."""
    error, payload = call_binary_json(
        "harness-roster",
        timeout=_ROSTER_TIMEOUT_S,
        # The dev checkout's own build outranks the installed copy: a
        # just-ported verb is unknown to the stale PATH binary.
        binary=find_dev_binary() or resolve_binary(),
    )
    if error is not None:
        raise VerbUnavailable(
            "the harness roster lives in the fno-agents binary"
            " (crates/fno-agents/src/provider.rs KNOWN_HARNESSES) and the read"
            f" failed: {error}; run `fno doctor update --rust` or set FNO_AGENTS_BIN"
        )
    names = payload.get("known") if isinstance(payload, dict) else None
    if not names or not all(isinstance(n, str) and n for n in names):
        raise VerbUnavailable(
            "fno-agents harness-roster answered no usable roster"
            f" (got: {payload!r:.200})"
        )
    return tuple(names)


def known_harnesses() -> tuple[str, ...]:
    """The roster, resolved at most once per process and cached in globals."""
    cached = globals().get("KNOWN_HARNESSES")
    if cached is None:
        globals()["KNOWN_HARNESSES"] = cached = _load_known_harnesses()
    return cached


def __getattr__(name: str) -> Any:
    """PEP 562: serve ``KNOWN_HARNESSES`` from the Rust roster on first read.

    Module-internal code goes through :func:`known_harnesses` (a global
    lookup never triggers this hook).
    """
    if name == "KNOWN_HARNESSES":
        return known_harnesses()
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


def unknown_thread_harness_message(name: str) -> str:
    """The one refusal every thread-substrate seam raises.

    Both halves derive from this module: the SPAWN tuple and the served
    roster, so no seam can name a harness the accept list has since admitted.
    The ROSTER decides the second sentence: the pane lane execs whatever is
    on PATH. A missing thread lane says what fno has BUILT, never what the
    harness can do (docs/architecture/thread-lanes.md).
    """
    accepted = ", ".join(SPAWN_HARNESSES)
    lines = [
        f"unknown harness {name!r} on the thread substrate (--harness names "
        f"the CLI BINARY); accepted here: {accepted}.",
    ]
    if name in known_harnesses():
        lines.append(f"{name} has no measured thread lane yet; use --substrate pane.")
    lines.append("If you meant a model VENDOR, that is -P/--provider.")
    return "\n".join(lines)

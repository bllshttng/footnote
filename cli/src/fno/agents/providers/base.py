"""Shared provider primitives for fno agents.

The ProviderResult dataclass is the return shape every provider adapter
(claude/codex/gemini) produces. Phase 1 only defines the dataclass; the
adapters land in Phase 2.

ReachabilityProbeError is the lifted tri-state base class used by every
provider's reachability probe. Returning True/False is the definitive
case (caller flips status); raising the exception is the inconclusive
case (caller preserves status). The provider tag lets reconcile route
the error with a per-provider reason discriminator without re-deriving
which probe failed.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Optional


@dataclass
class ProviderResult:
    """Result of a single provider subprocess invocation.

    Attributes:
        exit_code: Subprocess exit code.
        stdout: Captured stdout.
        stderr: Captured stderr.
        duration_ms: Wall-clock duration in milliseconds.
        session_id_out: Newly-created or resumed session id (claude short-id,
            codex session uuid, or gemini session id). ``None`` when the
            invocation did not produce one (e.g. errors before session create).
    """

    exit_code: int
    stdout: str
    stderr: str
    duration_ms: int
    session_id_out: Optional[str] = None


class ReachabilityProbeError(RuntimeError):
    """Raised when a provider reachability probe cannot classify definitively.

    Tri-state probe contract (callers MUST implement all three branches):

    - Return ``True`` -> session is definitively reachable. ``reconcile``
      MAY flip ``status`` to ``"live"``.
    - Return ``False`` -> session is definitively orphaned. ``reconcile``
      MUST flip ``status`` to ``"orphaned"``.
    - Raise ``ReachabilityProbeError`` -> inconclusive (transient I/O,
      permission denied on a parent dir, race against an active writer).
      ``reconcile`` MUST preserve the existing ``status`` unchanged and
      route the entry to ``errors`` with a reason discriminator.

    Every provider's reachability probe raises this class directly with
    its own ``provider`` tag; there are no per-provider subclasses. (The
    deprecated ``ClaudeReachabilityProbeError`` / ``SessionIndexReadError``
    aliases were removed one release cycle after US4-gemini Wave 1.1.)

    ``provider`` is non-optional so reconcile's error route always carries
    a discriminator. ``reason`` is the short human-readable cause and
    feeds the ``stderr`` WARN line + ``errors`` payload.
    """

    def __init__(self, *, provider: str, reason: str) -> None:
        super().__init__(
            f"{provider} reachability probe inconclusive: {reason}"
        )
        self.provider = provider
        self.reason = reason

    def __reduce__(self):
        # Default Exception pickling round-trips through __init__(args[0]),
        # which trips this class's kw-only (provider, reason) signature.
        # Override so pickle.dumps / pickle.loads survives (AC1-EDGE) by
        # reconstructing through a module-level helper that re-supplies the
        # keyword arguments. Providers raise this class directly with a
        # ``provider`` tag (no per-provider subclasses), so the single
        # base-class path covers every instance.
        return (_reconstruct_base, (self.provider, self.reason))


def _reconstruct_base(provider: str, reason: str) -> ReachabilityProbeError:
    """Re-instantiate ``ReachabilityProbeError`` from its pickled state."""
    return ReachabilityProbeError(provider=provider, reason=reason)

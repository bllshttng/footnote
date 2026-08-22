"""Owned ambient identity resolution for core and runtime callers."""

from __future__ import annotations

from typing import Mapping, Optional

from fno.harness_identity import resolve_owned_identity


def resolve_self_identity(env: Optional[Mapping[str, str]] = None):
    """Resolve the harness identity this process can prove it owns.

    The prover is the process-tree walk, and it is the ONLY prover. The nearest
    harness ancestor is what a process actually runs under, so it separates a
    marker this session minted from one it merely inherited.

    A self-set marker does NOT belong here, and the attempt is worth recording
    because it looks correct. ``CLAUDECODE`` is written by the claude binary at
    startup, so a shell that never ran claude cannot produce it; that reads like
    proof of a claude self. It is not. The variable survives a fork, so a codex
    session started from a shell that HAD run claude inherits it, and promoting
    it to a prover contradicts that session's own ``CODEX_THREAD_ID``: a sole
    codex marker that resolved cleanly degrades to ambiguous, and every identity
    consumer loses a valid codex session. Environment alone cannot tell the two
    cases apart, because they carry the identical name set. Only ancestry can,
    which is what the walk reads.

    So when the walk has no answer - psutil denied, no harness ancestor, a
    container that hides the parent chain - resolution refuses rather than
    guesses, and ``fno whoami`` names the inherited family so the operator can
    clear it. See :data:`fno.harness_identity.SELF_SET_HARNESS_MARKERS`.
    """
    from fno.claims.session_pid import resolve_session_harness

    true_harness = resolve_session_harness()
    prove = None if true_harness is None else (lambda harness, sid: harness == true_harness)
    return resolve_owned_identity(env, prove=prove)

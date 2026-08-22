"""Owned ambient identity resolution for core and runtime callers."""

from __future__ import annotations

from typing import Mapping, Optional

from fno.harness_identity import resolve_owned_identity, self_set_harness


def resolve_self_identity(env: Optional[Mapping[str, str]] = None):
    """Resolve the harness identity this process can prove it owns.

    The prover is the process-tree walk: the nearest harness ancestor is what a
    process actually runs under, so it separates a marker this session minted
    from one it merely inherited.

    When the walk has no answer - psutil denied, no harness ancestor, a
    container that hides the parent chain - it falls back to the self-set
    marker the running binary wrote about itself (CLAUDECODE for claude). That
    marker is weaker: it survives a fork, so it names ancestry rather than self,
    which is why it never runs first. It answers only where the alternative is
    resolving a live session to nothing, which is how a crown grant came to
    record grantor "human" for a grant a real session issued.
    """
    from fno.claims.session_pid import resolve_session_harness

    true_harness = resolve_session_harness() or self_set_harness(env)
    prove = None if true_harness is None else (lambda harness, sid: harness == true_harness)
    return resolve_owned_identity(env, prove=prove)

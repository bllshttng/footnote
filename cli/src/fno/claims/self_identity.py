"""Owned ambient identity resolution for core and runtime callers."""

from __future__ import annotations

from typing import Mapping, Optional

from fno.harness_identity import (
    parse_canonical_identity,
    resolve_attester_identity,
    resolve_owned_identity,
    session_identity_key,
)


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
    canonical = parse_canonical_identity(env)
    if canonical.disposition not in {"complete", "name_only"}:
        fallback_prove = (
            None if true_harness is None else (lambda harness, sid: harness == true_harness)
        )
        return resolve_owned_identity(env, prove=fallback_prove)

    try:
        attested_session_id, witness = resolve_attester_identity(env)
    except Exception:
        attested_session_id, witness = "", ""
    canonical_session_id = canonical.session_id or attested_session_id
    canonical_proven = bool(
        true_harness
        and canonical.harness == true_harness
        and witness == "process"
        and canonical_session_id
        and attested_session_id
        and session_identity_key(canonical_session_id)
        == session_identity_key(attested_session_id)
    )

    def prove(harness: str, session_id: str) -> Optional[bool]:
        if harness != true_harness:
            return False
        if not canonical_proven:
            return None
        return session_identity_key(session_id) == session_identity_key(canonical_session_id)

    def collide(_harness: str, session_id: str) -> Optional[str]:
        from fno.agents.registry import row_owning_session_id

        return row_owning_session_id(session_id)

    return resolve_owned_identity(
        env,
        prove=prove,
        collide=None if canonical_proven else collide,
    )

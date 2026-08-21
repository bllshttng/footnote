"""Owned ambient identity resolution for core and runtime callers."""

from __future__ import annotations

from typing import Mapping, Optional

from fno.harness_identity import resolve_owned_identity


def resolve_self_identity(env: Optional[Mapping[str, str]] = None):
    """Resolve the harness identity this process can prove it owns."""
    from fno.claims.session_pid import resolve_session_harness

    true_harness = resolve_session_harness()
    prove = None if true_harness is None else (lambda harness, sid: harness == true_harness)
    return resolve_owned_identity(env, prove=prove)

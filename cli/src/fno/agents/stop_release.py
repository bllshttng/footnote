"""Stopping an agent releases the claims it holds (x-9c91 change 5).

The stop stays in :mod:`fno.agents.dispatch`; the release leg lives here so
the oversized dispatch file keeps shrinking. The native op resolves the
stopped holder's session from the registry row a stop leaves.
"""
from __future__ import annotations

from typing import Any


def stop_agent(name: str, **kwargs: Any):
    """Stop an agent, then release the claims it holds. The codex/gemini
    no-op arm releases nothing, because no stop happened."""
    from fno.agents.dispatch import StopResult, _stop_agent_inner
    from fno.claims.io import claims_dir, global_claims_dir
    from fno.claims.verdict import run_op

    result: StopResult = _stop_agent_inner(name, **kwargs)
    if result.noop:
        return result
    receipt, error = run_op(
        ["release-stopped", "--name", name], [global_claims_dir(), claims_dir(None)]
    )
    if error:
        print(f"claims not released: {error}", flush=True)
        return result
    released = (receipt or {}).get("released") or []
    kept = (receipt or {}).get("kept") or []
    if released or kept:
        suffix = f"; released {len(released)} claim(s)"
        suffix += "".join(
            f"; kept {row.get('key')} ({row.get('observed')})" for row in kept
        )
        print(suffix, flush=True)
    return result

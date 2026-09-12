"""Write-time handling for per-node dispatch overrides (US3)."""

from typing import Optional

import typer

from fno.agents.harness_map import _BRIEF_MAX_BYTES


def apply(node: dict, dispatch_verb: Optional[str], dispatch_brief: Optional[str]) -> Optional[str]:
    """Store the overrides permissively; the resolver stays the trust boundary
    (allowlist + hard 8 KB cap at dispatch time). The brief additionally warns
    at write: the author is present here and absent at spawn, so surface the
    size now, in the spawn path's wording.

    Returns the warning text instead of printing it: the caller runs inside
    locked_mutate_graph, which re-runs the mutator on contention, so a print
    here would repeat per retry and still fire when every retry fails. The
    caller echoes it once, after the lock succeeds and the write lands.
    """
    if dispatch_verb is not None:
        node["dispatch_verb"] = None if dispatch_verb.lower() == "null" else dispatch_verb
    if dispatch_brief is not None:
        brief_val = None if dispatch_brief.lower() == "null" else dispatch_brief
        node["dispatch_brief"] = brief_val
        if brief_val is not None:
            n_bytes = len(brief_val.encode("utf-8"))
            if n_bytes > _BRIEF_MAX_BYTES:
                return (
                    f"warning: dispatch brief is {n_bytes} bytes, over the "
                    f"{_BRIEF_MAX_BYTES}-byte (8 KB) env budget; shorten it "
                    f"(no silent truncation) - spawn will refuse it"
                )
    return None


def emit(warning: Optional[str]) -> None:
    """Echo a warning returned by apply(), once, after the write lands."""
    if warning is not None:
        typer.echo(warning, err=True)

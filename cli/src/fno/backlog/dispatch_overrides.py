"""Write-time handling for per-node dispatch overrides (US3)."""

from typing import Optional

import typer

from fno.agents.harness_map import _BRIEF_MAX_BYTES


def apply(node: dict, dispatch_verb: Optional[str], dispatch_brief: Optional[str]) -> Optional[str]:
    """Store the overrides. The verb is checked with the drain's own name-mint
    predicate (``verb_code_for``), so a verb carrying an argument, or an
    unknown word, is refused here instead of failing three drains later under
    the auto-defer rule. The refusal raises and aborts locked_mutate_graph
    before any write lands. The brief additionally warns at write: the author
    is present here and absent at spawn, so surface the size now, in the
    spawn path's wording.

    Returns the warning text instead of printing it: the caller runs inside
    locked_mutate_graph, which re-runs the mutator on contention, so a print
    here would repeat per retry and still fire when every retry fails. The
    caller echoes it once, after the lock succeeds and the write lands.
    """
    if dispatch_verb is not None:
        from fno.agents.naming import AgentNameError, accepted_verb_words, verb_code_for

        verb_val = None if dispatch_verb.lower() == "null" else dispatch_verb
        if verb_val is None:
            node["dispatch_verb"] = None
        else:
            try:
                verb_code_for(verb_val)
            except AgentNameError as exc:
                accepted = ", ".join(accepted_verb_words())
                typer.echo(
                    f"Error: --dispatch-verb {dispatch_verb!r} refused at write: "
                    f"{exc}. The field takes one bare verb word; a verb with an "
                    "argument would fail every drain at name mint. accepted: "
                    f"{accepted} (bare, '/fno:'- or '$fno:'-prefixed)",
                    err=True,
                )
                raise typer.Exit(code=2) from exc
            node["dispatch_verb"] = verb_val
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

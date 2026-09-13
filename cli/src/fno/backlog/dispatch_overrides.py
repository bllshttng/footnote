"""Write-time handling for per-node dispatch overrides (US3)."""

import re
from typing import Optional

import typer

from fno.agents.harness_map import _BRIEF_MAX_BYTES


def _verb_refusal(value: str) -> tuple[Optional[str], Optional[str]]:
    """The write-side verb gate. Returns (refusal, warning).

    A refusal is echoed and the write exits 2. A warning rides back to the
    caller like the brief-size warning: the author is present at write and
    absent at drain, so say now what the drain will do.

    Three checks, in order:
    - shape: one bare token. A verb carrying an argument stores fine and then
      fails at the drain's agent-name mint, three failed drains after the
      auto-failure rule has deferred the node.
    - prefix: bare or '/fno:' only. The dispatch resolver canonicalizes
      '/fno:' and nothing else, so a '$fno:' value passes the name mint and
      still fails at the resolver - the same delayed failure.
    - membership: the word must resolve in the drain's name vocabulary
      (``verb_code_for``) OR in the configured allowlist/registry, which the
      resolver honors. A config-only word writes with a warning naming the
      gap: the name mint is a static table and does not read config.
    """
    from fno.agents.naming import AgentNameError, accepted_verb_words, verb_code_for

    if value.strip() != value or re.search(r"\s", value):
        accepted = ", ".join(accepted_verb_words())
        return (
            f"Error: --dispatch-verb {value!r} refused at write: not one bare "
            "verb word. The field takes a single verb token; a verb with an "
            f"argument fails every drain at name mint. accepted: {accepted} "
            "(bare or '/fno:'-prefixed)",
            None,
        )
    if value.startswith("$fno:"):
        return (
            f"Error: --dispatch-verb {value!r} refused at write: the stored "
            "field takes a bare verb or a '/fno:'-prefixed one; the dispatch "
            "resolver canonicalizes only '/fno:' and would reject this at "
            "drain.",
            None,
        )
    static_ok = True
    try:
        verb_code_for(value)
    except AgentNameError:
        static_ok = False
    allowed: list[str] = []
    registry: dict = {}
    try:
        from fno.config import load_settings, resolvable_verbs

        settings = load_settings()
        raw_allowed = getattr(settings.dispatch, "allowed_verbs", None)
        allowed = list(raw_allowed) if isinstance(raw_allowed, list) else []
        raw_registry = getattr(settings.dispatch, "verb_registry", None)
        registry = resolvable_verbs(
            raw_registry if isinstance(raw_registry, dict) else None, allowed
        )
    except Exception:  # noqa: BLE001 - unreadable config leaves the static set
        allowed, registry = [], {}
    chosen = "/" + value[len("/fno:"):] if value.startswith("/fno:") else value
    if static_ok or chosen in allowed or chosen in registry:
        if static_ok:
            return None, None
        return None, (
            f"warning: dispatch verb {value!r} resolves only in "
            "config.dispatch.allowed_verbs/verb_registry; the drain's "
            f"worker-name mint knows only {', '.join(accepted_verb_words())} "
            "and may refuse the dispatch"
        )
    accepted = ", ".join(
        sorted(set(accepted_verb_words()) | set(allowed) | set(registry))
    )
    return (
        f"Error: --dispatch-verb {value!r} refused at write: unknown dispatch "
        "verb. The field takes one bare verb word; accepted here: "
        f"{accepted} (bare or '/fno:'-prefixed), extend "
        "config.dispatch.allowed_verbs or verb_registry for an outside verb",
        None,
    )


def apply(node: dict, dispatch_verb: Optional[str], dispatch_brief: Optional[str]) -> Optional[str]:
    """Store the overrides. The verb is gated at write (see
    :func:`_verb_refusal`) so a verb the drain cannot run is refused here
    instead of failing three drains later under the auto-defer rule. A
    refusal raises and aborts locked_mutate_graph before any write lands.
    The brief additionally warns at write: the author is present here and
    absent at spawn, so surface the size now, in the spawn path's wording.

    Returns the warning text instead of printing it: the caller runs inside
    locked_mutate_graph, which re-runs the mutator on contention, so a print
    here would repeat per retry and still fire when every retry fails. The
    caller echoes it once, after the lock succeeds and the write lands.
    """
    warnings: list[str] = []
    if dispatch_verb is not None:
        verb_val = None if dispatch_verb.lower() == "null" else dispatch_verb
        if verb_val is not None:
            refusal, verb_warning = _verb_refusal(verb_val)
            if refusal is not None:
                typer.echo(refusal, err=True)
                raise typer.Exit(code=2)
            if verb_warning is not None:
                warnings.append(verb_warning)
        node["dispatch_verb"] = verb_val
    if dispatch_brief is not None:
        brief_val = None if dispatch_brief.lower() == "null" else dispatch_brief
        node["dispatch_brief"] = brief_val
        if brief_val is not None:
            n_bytes = len(brief_val.encode("utf-8"))
            if n_bytes > _BRIEF_MAX_BYTES:
                warnings.append(
                    f"warning: dispatch brief is {n_bytes} bytes, over the "
                    f"{_BRIEF_MAX_BYTES}-byte (8 KB) env budget; shorten it "
                    f"(no silent truncation) - spawn will refuse it"
                )
    return "\n".join(warnings) if warnings else None


def emit(warning: Optional[str]) -> None:
    """Echo a warning returned by apply(), once, after the write lands."""
    if warning is not None:
        typer.echo(warning, err=True)

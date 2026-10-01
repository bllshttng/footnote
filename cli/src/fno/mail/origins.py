"""Mail-origin vocabulary and the identity floor over it.

The origin is evidence about the channel, not an authority claim: only
operator-origin evidence can carry operator intent into a machine-readable
gate. Home of `MAIL_ORIGINS` and `enforce_origin_floor` since the decide
family ported to Rust (the two were born inside the deleted decide engine, which kept
them only because `record_decision` shared the gate; their callers were
always the mail lanes).
"""

from __future__ import annotations


MAIL_ORIGINS: tuple[str, ...] = (
    "operator",
    "peer",
    "scheduler",
    "recovery",
)


def enforce_origin_floor(origin: str | None) -> str | None:
    """An ambient agent identity cannot declare an origin above peer.

    The one gate every explicit origin claim routes through (d-02625dda,
    d-8f396483): mail send's classify_origin and the dispatch ctx builder
    both ask this before trusting a caller-stated origin. Identity is proven
    by ancestry (a process-tree walk, never env markers), so a caller that
    resolves an agent identity is an agent claiming a channel it does not
    own, scheduler and recovery included; a detached scheduler or recovery
    daemon has no harness ancestor and keeps its honest declaration.
    """
    if origin is None or origin == "peer":
        return origin
    from fno.agents.self_stamp import resolve_self_identity

    ident = resolve_self_identity()
    if ident.session_id and ident.harness:
        return "peer"
    return origin

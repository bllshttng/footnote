"""Annotation core: operator review findings recorded to events.jsonl.

Record-then-deliver, record is the transaction (Locked Decision 1): the
``review_finding`` event is the source of truth loop-check gates on; the mail
live-inject to the claim-holding session is best-effort with the durable bus as
the fallback, so a delivery miss is never an error exit.

CLI-first layering (Locked Decision 4): this module is the core and works
without mux. ``fno mux block annotate`` (Rust porcelain) shells the CLI on top.
"""
from __future__ import annotations

import re as _re
import secrets
from pathlib import Path
from typing import Any, Optional

import fno.events as _events

# x-9ed6: node/operator free text embedded in an injected frame could carry a
# literal delimiter and break out into the recipient's next prompt. Defang the
# reminder + mail delimiters (case/whitespace-insensitive) before framing. The
# RECORDED event keeps the original text; only the injected frame is defanged.
_DELIM = _re.compile(r"<\s*(/?)\s*(system-reminder|fno_mail)\s*>", _re.IGNORECASE)


def defang(text: str) -> str:
    """Neutralize reminder/mail delimiters in operator text before it is framed."""
    return _DELIM.sub(r"[\1\2]", text)


def mint_finding_id() -> str:
    """Collision-safe short id (8 hex). Minted here, never operator-supplied."""
    return secrets.token_hex(4)


def _events_path(events_path: Optional[Path]) -> Path:
    return events_path if events_path is not None else Path(".fno/events.jsonl")


# -- Delivery -----------------------------------------------------------------


def _resolve_holder(node: str) -> dict[str, Any]:
    """Return the ``claim_status`` dict for ``node:<id>`` via the routed root.

    Never raises: claim_status is total. A free/absent claim yields
    ``state == 'free'`` with no holder.
    """
    from fno.claims import claim_status
    from fno.claims.io import claims_root_for

    key = f"node:{node}"
    return claim_status(key, root=claims_root_for(key))


_LIVE_HOLDER_STATES = {"live", "suspect", "stale"}


def _deliver(node: str, finding_id: str, text: str, excerpt: Optional[str]) -> str:
    """Best-effort live-inject to the claim holder. Returns a delivery outcome:

    ``delivered`` | ``deferred`` | ``no-holder``. Never raises - a delivery
    failure degrades to ``deferred`` (the event is already durable).
    """
    status = _resolve_holder(node)
    holder = status.get("holder")
    if status.get("state") not in _LIVE_HOLDER_STATES or not holder:
        return "no-holder"

    # holder is "target-session:<sid>"; the mail ladder addresses the raw sid.
    sid = holder.split(":", 1)[1] if ":" in holder else holder
    harness = status.get("harness") or "claude-code"

    from fno.mail.envelope import wrap_fno_mail

    body = f"review finding {finding_id} on {node}:\n{defang(text)}"
    if excerpt:
        body += f"\n\n--- block ---\n{defang(excerpt)}"
    body += f"\n\n(resolve with: fno annotate resolve {finding_id})"
    frame = wrap_fno_mail(body, from_="annotate", harness="claude-code", model="operator", node=node)

    try:
        from fno.agents.dispatch import _mail_inject_claude, _mail_inject_codex

        if harness == "codex":
            delivered = _mail_inject_codex(sid, frame)
        else:
            delivered = _mail_inject_claude(sid, frame)
    except Exception:
        # The ladder is best-effort; any import/runtime miss is a deferral, not
        # an error - the durable event still reaches the worker at the gate.
        delivered = False
    return "delivered" if delivered else "deferred"


# -- Verbs --------------------------------------------------------------------


class AnnotateError(ValueError):
    """A pre-write refusal (empty text). Nothing is recorded."""


def add_finding(
    node: str,
    text: str,
    *,
    block_cmd: Optional[str] = None,
    block_excerpt: Optional[str] = None,
    events_path: Optional[Path] = None,
) -> dict[str, Any]:
    """Record a ``review_finding`` and attempt delivery. Record is the transaction.

    Returns ``{finding_id, node, recorded: True, delivery}`` where ``delivery``
    is one of ``delivered | deferred | no-holder``. Raises ``AnnotateError``
    (writing nothing) only on empty/whitespace text.
    """
    text = text.strip()
    if not text:
        raise AnnotateError("annotation text is empty")

    finding_id = mint_finding_id()
    data: dict[str, Any] = {"finding_id": finding_id, "node": node, "text": text}
    if block_cmd:
        data["block_cmd"] = block_cmd
    if block_excerpt:
        data["block_excerpt"] = block_excerpt

    event = _events._build("review_finding", "observer", data)
    _events.append_event(event, _events_path(events_path))

    # Record is the transaction; nothing after the append may fail it. Any
    # delivery-side exception degrades to a deferral - the event is already
    # durable and reaches the worker at the gate.
    try:
        delivery = _deliver(node, finding_id, text, block_excerpt)
    except Exception:
        delivery = "deferred"
    return {"finding_id": finding_id, "node": node, "recorded": True, "delivery": delivery}


def _scan(events_path: Optional[Path]) -> list[dict[str, Any]]:
    import json

    path = _events_path(events_path)
    if not path.is_file():
        return []
    out: list[dict[str, Any]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            out.append(json.loads(line))
        except ValueError:
            # A structurally-unparseable line is our own writer's output; skip
            # it for listing (loop-check surfaces the malformed count for gating).
            continue
    return out


def list_findings(
    node: Optional[str] = None,
    *,
    events_path: Optional[Path] = None,
) -> list[dict[str, Any]]:
    """Open + resolved findings, optionally scoped to ``node``.

    Each entry: ``{finding_id, node, text, open, block_cmd?}``. A finding is open
    until an explicit ``review_finding_resolved`` clears it (no head-pin, no TTL).
    """
    resolved: set[str] = set()
    findings: dict[str, dict[str, Any]] = {}
    for ev in _scan(events_path):
        # A valid-JSON but non-object line (a bare list/number) must be skipped,
        # not crash on .get (gemini review). _scan already drops unparseable lines.
        if not isinstance(ev, dict):
            continue
        etype = ev.get("type")
        data = ev.get("data") or {}
        fid = data.get("finding_id")
        if not fid:
            continue
        if etype == "review_finding":
            findings[fid] = {
                "finding_id": fid,
                "node": data.get("node"),
                "text": data.get("text", ""),
                "block_cmd": data.get("block_cmd"),
            }
        elif etype == "review_finding_resolved":
            resolved.add(fid)

    out = []
    for fid, f in findings.items():
        if node is not None and f.get("node") != node:
            continue
        f["open"] = fid not in resolved
        out.append(f)
    return out


def resolve_finding(
    finding_id: str,
    *,
    events_path: Optional[Path] = None,
) -> dict[str, Any]:
    """Append a ``review_finding_resolved``. Idempotent: resolving an unknown or
    already-resolved id is a warning no-op (no second event), never an error.

    Returns ``{finding_id, resolved: bool, warning?}``.
    """
    known = {f["finding_id"]: f for f in list_findings(events_path=events_path)}
    f = known.get(finding_id)
    if f is None:
        return {"finding_id": finding_id, "resolved": False, "warning": "unknown finding id"}
    if not f["open"]:
        return {"finding_id": finding_id, "resolved": False, "warning": "already resolved"}

    data = {"finding_id": finding_id, "node": f.get("node")}
    event = _events._build("review_finding_resolved", "observer", data)
    _events.append_event(event, _events_path(events_path))
    return {"finding_id": finding_id, "resolved": True}

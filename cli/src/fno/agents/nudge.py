"""fno.agents.nudge — P2 loop-boundary inbox nudge (ab-098967b4).

An autonomous `/target` loop submits no user prompt and starts no new session
between iterations, so neither the per-prompt nor the session-start inbox
reminder fires; a message addressed to it sits unseen until the loop yields to
a human. P2 closes that gap: when the loop-check verb emits a `block` decision
(the loop-yield boundary), the Rust side shells out to `fno agents nudge-peek`,
which returns a one-line nudge for the OLDEST unread message addressed to this
session's project and records that it was surfaced — so it appears exactly once
(US3) while the durable copy stays in the inbox for the drain / human (AC3-FR).

Idempotency uses a per-session set of already-nudged message ids, pruned each
call to the currently-unread set (bounded, and re-surface-proof even if the bus
consumer cursor moves under us). The bus consumer cursor is NEVER advanced here
(that is `fno mail ack`'s job): surfacing a nudge is not the same as reading.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Optional

from fno import paths

_SUMMARY_MAX = 80


def _cursor_path(session_id: str) -> Path:
    safe = session_id.replace("/", "_") or "unknown"
    return paths.state_dir() / "nudge-cursors" / f"{safe}.json"


def _load_nudged(path: Path) -> set[str]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError, UnicodeDecodeError):
        return set()
    ids = data.get("nudged") if isinstance(data, dict) else None
    return {str(x) for x in ids} if isinstance(ids, list) else set()


def _save_nudged(path: Path, nudged: set[str]) -> None:
    import tempfile

    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_str = tempfile.mkstemp(
        dir=str(path.parent), prefix=f".{path.name}.tmp.", suffix=".part"
    )
    tmp = Path(tmp_str)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump({"nudged": sorted(nudged)}, f)
        os.replace(str(tmp), str(path))
    except Exception:
        try:
            tmp.unlink()
        except OSError:
            pass
        # Best-effort: a failed cursor write means at worst a future re-surface,
        # never a crash of the loop-check that called us.


def _render(envelope) -> str:
    """One-line nudge, attributed + actionable (AC3-UI / AC4-UI).

    Names the sender, a bounded one-line summary, and how to read/reply. A
    reply (in_reply_to set) is labelled as such so the round-trip is legible.
    """
    sender = getattr(envelope, "from_", None) or "unknown"
    body = (getattr(envelope, "body", None) or "").strip().replace("\n", " ")
    if len(body) > _SUMMARY_MAX:
        body = body[: _SUMMARY_MAX - 1] + "…"
    label = "reply from" if getattr(envelope, "in_reply_to", None) else "from"
    return (
        f'inbox: {label} {sender}: "{body}" '
        f"— read: fno mail unread; reply: fno mail send {sender} \"...\""
    )


def _resolve_self_name(cwd: str) -> Optional[str]:
    """Best-effort: this worker's registry name, resolved by unique live cwd.

    A sender addresses a worker by its registry name (``fno mail send <name>``),
    so to drain by-name mail the worker must know its own name. We match the live
    registry entry whose cwd equals this worker's cwd (the worktree-per-worker
    norm makes this unique). Zero or many live entries at the same cwd -> return
    None and degrade to project-only delivery rather than guess a recipient.
    Fail-open: any registry error yields None.
    """
    try:
        from fno.agents.registry import load_registry

        target = os.path.normpath(os.path.expanduser(cwd))
        live = [
            e for e in load_registry()
            if getattr(e, "status", None) == "live"
            and os.path.normpath(os.path.expanduser(e.cwd)) == target
        ]
        return live[0].name if len(live) == 1 else None
    except Exception:  # noqa: BLE001 - identity resolution is best-effort
        return None


def peek_nudge(session_id: str, cwd: str) -> Optional[str]:
    """Return a one-line nudge for the oldest un-nudged unread msg, or None.

    Drains the worker's addressed mail at the loop boundary: direct by-name mail
    to this worker (cv-d54ddd45) plus project broadcasts NOT sent by it
    (sender-excluded). Side effect: records the surfaced message id in the
    per-session cursor so it is not surfaced again (US3/AC3-FR). Fail-open:
    returns None on any error so the loop-check that shells out to us is never
    broken by inbox state.
    """
    if not cwd:
        return None
    try:
        from fno.agents.discover import resolve_project_for_cwd

        project = resolve_project_for_cwd(cwd)
        my_name = _resolve_self_name(cwd)
        if not project and not my_name:
            return None

        from fno.bus.cursor import scan_unread

        # Direct by-name mail to this worker (no self-echo by construction), plus
        # project broadcasts excluding this worker's own sends. Each scan honors
        # its own per-recipient cursor (advanced by `fno mail ack`).
        matched: list = []
        if my_name:
            matched += scan_unread(my_name, warn=False)
        if project:
            # Sender-exclusion: my_name is the load-bearing key (a self-broadcast's
            # from_ equals this worker's registry name). session_id is a secondary
            # match against from_session that only fires when the two id namespaces
            # agree; it is belt-and-suspenders, never the sole guard.
            exclude = {x for x in (my_name, session_id) if x}
            matched += scan_unread(project, warn=False, exclude_from=exclude)
        if not matched:
            return None

        # Dedup + restore oldest->newest order across the two scans WITHOUT a
        # third full bus scan (peek_nudge runs at every loop boundary, and the
        # bus can be tens of MB). Both scans already returned parsed envelopes in
        # global order; sorting their union by ts is correct (ISO-8601 sorts
        # chronologically) and a stable sort keeps same-second order intact.
        seen: set[str] = set()
        msgs = []
        for m in matched:
            if m.id not in seen:
                seen.add(m.id)
                msgs.append(m)
        msgs.sort(key=lambda m: m.ts)
        if not msgs:
            return None

        cursor_path = _cursor_path(session_id)
        nudged = _load_nudged(cursor_path)
        unread_ids = {m.id for m in msgs}
        # Prune ids that left the unread set so the cursor file stays bounded
        # and a recycled id can never falsely suppress a fresh message.
        nudged &= unread_ids

        fresh = [m for m in msgs if m.id not in nudged]
        if not fresh:
            _save_nudged(cursor_path, nudged)  # persist the prune
            return None

        oldest = fresh[0]
        nudged.add(oldest.id)
        _save_nudged(cursor_path, nudged)
        return _render(oldest)
    except Exception:  # noqa: BLE001 — fail-open: never break the loop-check
        return None

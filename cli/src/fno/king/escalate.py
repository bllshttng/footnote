"""Escalate a stalled king board to the operator, exactly once per stalled set.

A king terminating ``NoProgress`` exits quietly: work pending, nothing moving,
nobody told. The escalation is the telling, a question in the operator queue.
Idempotence keys on the stalled id set within the king's own channel: a
respawned king meeting the same board records no second question, and a
changed board supersedes only that king's stale row.

The operator-facing text renders in the ``fno-agents`` crate
(``king-escalation-text``, x-ff27); this module keeps the fold and the
liveness read.
"""
from __future__ import annotations

from pathlib import Path

MARKER = "king-escalation"


def _render(
    ids: "list[str]",
    key: str,
    reason: str,
    *,
    live: "bool | None" = None,
    unknown_reason: "str | None" = None,
    verdict: "str | None" = None,
    scope: "str | None" = None,
) -> dict:
    """One round-trip with the crate renderer (d-b6cc1a2a: new code in
    ``crates/``). ``ok: false`` is a REFUSAL, not a failure: the caller must
    raise, never fall back to Python text - the fallback is the defect.
    ``verdict`` (first word = verdict name, x-4d4f) and ``scope`` ride along.
    """
    from fno.rust_binary import verb_call

    return verb_call(
        "king-escalation-text",
        {
            "stalled": ids,
            "key": key,
            "reason": reason,
            "live": live,
            "unknown_reason": unknown_reason,
            "verdict": verdict,
            "scope": scope,
        },
    )


def escalate(stalled_ids: "list[str]", reason: str, root: Path, session_id: "str | None",
             cwd: Path, *, live: "bool | None" = None,
             unknown_reason: "str | None" = None,
             verdict: "str | None" = None,
             scope: "str | None" = None) -> "tuple[str, str]":
    """Record one operator question for this stalled set.

    Returns ``(outcome, question_id)``; raises rather than failing quiet.
    """
    from fno.agents.stale_escalate import dedupe_key, reconcile_channel
    from fno.harness_identity import canonical_handle

    ids = sorted(set(stalled_ids))
    key = dedupe_key(ids)
    marker, render_key = _escalation_channel(session_id, key)
    # Render BEFORE the fold: a refusal must raise while the channel is still
    # untouched. The channel's empty branch closes open asks, so reaching it
    # with a refused set would read as a clean board.
    answer = _render(
        ids, render_key, reason, live=live, unknown_reason=unknown_reason,
        verdict=verdict, scope=scope,
    )
    if not answer.get("ok"):
        raise ValueError(answer.get("message", "king escalation refused"))
    outcome, qid = reconcile_channel(
        ids,
        root=root,
        session_id=session_id,
        cwd=cwd,
        marker=marker,
        subject="king-escalation",
        identities=ids,
        question=lambda _key: answer["question"],
        # No ask line. A stalled row is queue-qualified (`undispatched:x-1234`)
        # and not every queue holds backlog nodes, so any single clearing
        # command here would be a guess. The question text names the rows.
        ask=lambda _key: "",
        # The delivery address for the eventual answer. The king that asked
        # is dead by then, but the durable mail tier reaches its successor;
        # an asker-less row can only ever be answered into the void.
        asker=canonical_handle(session_id) if session_id else None,
    )
    return ("recorded", qid) if outcome == "asked" else (outcome, qid)


def _escalation_channel(session_id: "str | None", key: str) -> "tuple[str, str]":
    """The ask's marker and render key from the crate; legacy on failure."""
    if not session_id:
        return MARKER, key
    try:
        from fno.rust_binary import verb_call

        answer = verb_call(
            "king-escalation-scope", {"session_id": session_id, "key": key}
        )
    except Exception:  # noqa: BLE001 - an unreadable crate answer never blocks the ask
        return MARKER, key
    marker, out_key = answer.get("marker"), answer.get("key")
    if isinstance(marker, str) and marker and isinstance(out_key, str) and out_key:
        return marker, out_key
    return MARKER, key


def resolve_presiding_king(session_id: "str | None") -> "dict | None":
    """The crown above the escalating session's own, or None (x-3ecf AC4-HP)."""
    if not session_id:
        return None
    try:
        from fno.agents.court import find_presiding_crown, gather_court
        from fno.agents.crown import _graph_index
        from fno.agents.registry import load_registry
        from fno.harness_identity import session_identity_key

        needle = session_identity_key(session_id)

        def _keyed(r: object) -> "str | None":
            sid = getattr(r, "harness_session_id", None)
            return session_identity_key(sid) if sid else None

        own = next((r for r in load_registry() if _keyed(r) == needle), None)
        if own is None or own.crown_level is None or not own.crown_scope:
            return None
        crowns = gather_court().get("crowns")
        if not crowns:
            return None
        return find_presiding_crown(own.crown_scope, own.crown_level, crowns, _graph_index())
    except Exception:  # noqa: BLE001 - a read failure falls through to the operator
        return None


def mail_presiding_king(holder: str, stalled_ids: "list[str]", reason: str) -> bool:
    """True only on a confirmed send; any failure reads False."""
    import shutil
    import subprocess

    fno_bin = shutil.which("fno")
    if not fno_bin:
        return False
    from fno.agents.stale_escalate import dedupe_key

    ids = sorted(set(stalled_ids))
    message = _render(ids, dedupe_key(ids), reason)["mail"]
    try:
        proc = subprocess.run(
            [fno_bin, "agents", "mail", "send", holder, message],
            capture_output=True, timeout=15, check=False, text=True,
        )
        return proc.returncode == 0 and "delivered (hosted)" in (proc.stdout or "")
    except (OSError, subprocess.SubprocessError):
        return False

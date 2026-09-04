"""Thread-specific transport helpers for the mail force lane."""

from __future__ import annotations

import copy
import sys

import typer


def resolve_pane_entry(resolved, recipient: str | None, token: str | None):
    """Return the registry row behind a name-lane address, if any."""
    from fno.agents.registry import AgentResolutionError, resolve_agent

    candidates = [
        getattr(resolved, "session_id", None) if resolved is not None else None,
        token,
        recipient,
    ]
    for candidate in candidates:
        if not candidate:
            continue
        try:
            return resolve_agent(candidate).entry
        except (AgentResolutionError, OSError):
            continue
    return None


def prepare_forced_entry(entry, *, recipient: str, reservation):
    """Resolve a thread row to its dedicated viewport, or return its pane ref."""
    from fno.mail import cli as mail_cli

    mux = getattr(entry, "mux", None) or {}
    if getattr(entry, "substrate", None) != "thread":
        return entry, mux.get("session"), mux.get("pane_id"), False

    from fno.agents.retask import RetaskTransportError, resolve_thread_viewport

    try:
        mux_session, pane_id = resolve_thread_viewport(entry)
    except RetaskTransportError as exc:
        mail_cli._release_budget(reservation)
        print(f"error: --force cannot open the thread viewport for {recipient}: {exc}. The row needs a logical thread reference from spawn.", file=sys.stderr)
        raise typer.Exit(code=1) from exc
    transport_entry = copy.copy(entry)
    transport_entry.mux = {"session": mux_session, "pane_id": pane_id}
    return transport_entry, mux_session, pane_id, True


def send_by_thread_identity(name: str, *, message: str, from_name, harness, style_exception, origin):
    """Force-send to a thread by its durable identity when discovery missed it."""
    from fno.mail import cli as mail_cli
    from fno.harness_identity import canonical_handle

    entry = resolve_pane_entry(None, None, name)
    if entry is None:
        return False
    session = next((getattr(entry, f, None) for f in ("harness_session_id", "fno_id", "session_id")), None)
    if not session or mail_cli._self_recipient(name, resolved_session_id=session):
        return False
    mail_cli._name_lane_send(message, from_name=from_name, resolved=None, token=None, recipient=canonical_handle(session), provider=getattr(entry, "harness", None) or harness, style_exception=style_exception, force=True, origin=origin)
    return True

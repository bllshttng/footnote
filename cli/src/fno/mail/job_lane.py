"""``fno agents mail send node:<id>``: mail addressed to the WORK, not a session.

Lifted out of ``fno.mail.cli`` unchanged.
"""
from __future__ import annotations

import sys
from typing import Optional

import typer


def job_lane_send(
    message: str,
    token: str,
    *,
    from_name: Optional[str],
    style_exception: Optional[str] = None,
    origin: Optional[str] = None,
) -> None:
    """Deliver to a JOB address, resolved to whoever holds the claim RIGHT NOW.

    A holder live-injects; a live miss floors to a durable envelope addressed to
    the job; no holder REFUSES (exit 16) and queues nothing. Lane and receipts:
    ``docs/architecture/cross-agent-bus-log.md#job-address-lane``."""
    from fno.mail.cli import (
        _live_miss_age_suffix,
        _release_budget,
        _reply_session_for,
        _reserve_budget,
    )
    from fno.agents.dispatch import (
        BUS_ONLY_POLICY,
        _mail_inject_claude,
        _mail_inject_codex,
    )
    from fno.agents.self_stamp import resolve_self_model, stamp_from
    from fno.dispatch_flags import infer_invoking_harness
    from fno.inbox.store import DurableOwner, generate_msg_id, write_new_thread
    from fno.mail.envelope import harness_for_provider, wrap_fno_mail
    from fno.mail.job_address import resolve_job_address

    job = resolve_job_address(token)
    if job is None:
        # Not a job address -- should not reach here (cmd_send gates on the
        # prefix), but fail closed rather than misaddress.
        print(f"error: not a job address: {token!r}", file=sys.stderr)
        raise typer.Exit(code=2)

    if not job.has_holder:
        note = f" ({job.note})" if job.note else ""
        print(
            f"mail: {token}{note} has no live holder "
            f"(claim state: {job.state}); not queued.\n"
            f"  a job address with no holder would strand. "
            f"Retry when a /target session holds {job.address}.",
            file=sys.stderr,
        )
        raise typer.Exit(code=16)
    # A local so the type-checker narrows Optional -> str past has_holder.
    session_id = job.session_id
    assert session_id is not None  # has_holder is True iff session_id is set

    msg_id = generate_msg_id()
    # The durable recipient is the JOB, never the holder's handle: that is what
    # makes the address outlive the session.
    recipient = job.address
    sender = stamp_from(from_name)
    _reservation, authored_words = _reserve_budget(
        sender=sender,
        recipient=recipient,
        body=message,
        msg_id=msg_id,
        allow_reason=style_exception,
    )
    sender_harness = infer_invoking_harness()
    sender_model = resolve_self_model()
    # The collision-safe reply address the name lane also carries; both durable
    # records below read it, and a reply consults THOSE, not the envelope.
    sender_session = _reply_session_for(from_name)
    def _envelope(to_session: Optional[str] = None) -> str:
        return wrap_fno_mail(
            message,
            from_=sender,
            harness=harness_for_provider(sender_harness) if sender_harness else "cli",
            model=sender_model,
            to=recipient,
            node=job.node_id,
            id=msg_id,
            from_session=sender_session,
            origin=origin,
            to_session=to_session,
        )

    # A job address outlives its holder, so only the live envelope names a
    # crown; a queued body can be drained by a successor (x-6346).
    wrapped = _envelope(session_id)

    provider = job.harness or "claude"
    _job_reason: list = []
    if provider == "codex":
        injected = _mail_inject_codex(session_id, wrapped, reason_out=_job_reason)
    else:
        injected = _mail_inject_claude(session_id, wrapped, reason_out=_job_reason)
    bus_only = not injected and BUS_ONLY_POLICY in _job_reason

    holder_tag = f" [holder {provider} {session_id[:8]}]"
    if injected:
        from fno.bus.log import record_hosted_delivery

        try:
            record_hosted_delivery(
                msg_id=msg_id,
                sender=sender,
                recipient=recipient,
                body=wrapped,
                provider_from=sender_harness,
                provider_to=provider,
                from_session=sender_session,
                from_model=sender_model,
                to_kind="node",
                word_count=authored_words,
                to_session=session_id,
                to_harness=provider,
            )
        except Exception as exc:  # noqa: BLE001 - delivery already succeeded
            print(
                "delivery succeeded; outbox record failed; "
                f"do not retry: {exc}",
                file=sys.stderr,
            )
        print(f"delivered (hosted) to {recipient}{holder_tag} id:{msg_id}")
        return

    owner = DurableOwner.WAKE_DAEMON
    try:
        th = write_new_thread(
            recipient=recipient,
            sender=stamp_from(from_name),
            kind="send",
            body=_envelope(),
            msg_id=msg_id,
            provider_to=provider,
            to_kind="node",
            owner=owner.value,
            from_session=sender_session,
            origin=origin,
            word_count=authored_words,
        )
    except (OSError, ValueError, RuntimeError) as exc:
        _release_budget(_reservation)
        print(
            f"durable envelope write failed for {recipient!r}: {exc}",
            file=sys.stderr,
        )
        raise typer.Exit(code=12) from exc
    if bus_only:
        print(
            "mail: holder is DND (bus-only by delivery policy); queued durable "
            "until a holder drains",
            file=sys.stderr,
        )
        print(
            f"{th.thread_id} queued (durable) for {recipient} "
            f"[bus-only: a holder drains it by policy]{holder_tag}"
        )
        return
    print(f"mail: {recipient} live-inject missed; durable until a holder drains",
          file=sys.stderr)
    suffix = _live_miss_age_suffix(recipient)
    print(f"{th.thread_id} queued (durable) for {recipient} [job-live-miss{suffix}]{holder_tag}")

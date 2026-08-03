"""Hidden operator surface for approvals: inspect and decide, never execute.

Every command here is inert with respect to the outside world. ``ls`` and
``show`` read. ``decide`` records a decision. None of them dispatch an effect,
and none of them can report an acknowledgment, because acknowledgment is
something a destination does and this process never talks to one.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Optional

import typer

from fno.approvals.models import (
    ApprovalRequest,
    DecisionKind,
    EffectState,
    RefusedError,
    utcnow,
)
from fno.approvals.policy import ConfigAuthority, load_authority
from fno.approvals.store import EffectStore
from fno.handoff.exit_codes import ExitCode

approvals_app = typer.Typer(
    name="approvals",
    help="Inspect and decide pending approvals for consequential effects.",
    no_args_is_help=True,
)


def _open(db: Optional[Path], authority: ConfigAuthority | None = None) -> EffectStore:
    return EffectStore(db, authority=authority or load_authority())


def _fail(message: str, *, code: ExitCode = ExitCode.ERROR) -> None:
    typer.secho(message, err=True, fg=typer.colors.RED)
    raise typer.Exit(code)


def _refuse(exc: RefusedError, as_json: bool) -> None:
    """Print a stable refusal. Same shape for every reason, so callers can parse."""
    refusal = exc.refusal
    if as_json:
        typer.echo(json.dumps({"result": "refused", **refusal.model_dump()}, indent=2))
    else:
        typer.secho(f"REFUSED [{refusal.reason.value}] {refusal.detail}", err=True, fg=typer.colors.RED)
        if refusal.fields:
            typer.secho(f"  conflicting fields: {', '.join(refusal.fields)}", err=True)
        if refusal.authority_source:
            typer.secho(f"  authority consulted: {refusal.authority_source}", err=True)
        if refusal.recovery:
            typer.secho(f"  recovery: {refusal.recovery}", err=True)
    raise typer.Exit(ExitCode.ERROR)


def _describe(store: EffectStore, request: ApprovalRequest) -> dict[str, Any]:
    """Every bound field, the decision, and the current effect state."""
    decision = store.get_decision(request.request_digest)
    attempts = [
        attempt.model_dump(mode="json")
        for attempt in store.attempts_for_request(request.request_digest)
    ]
    return {
        "request_digest": request.request_digest,
        "request_id": request.request_id,
        "principal_id": request.principal_id,
        "work_order_id": request.work_order_id,
        "work_order_attempt": request.attempt_id,
        "effect_id": request.effect_id,
        "effect_class": request.effect_class,
        "destination": request.destination,
        "action_digest": request.action_digest,
        "created_at": request.created_at.isoformat(),
        "expires_at": request.expires_at.isoformat(),
        "expired": request.is_expired(utcnow()),
        "decision": decision.decision.value if decision else None,
        "deciding_principal_id": decision.deciding_principal_id if decision else None,
        "decided_at": decision.decided_at.isoformat() if decision else None,
        "decision_transport": decision.transport if decision else None,
        "effect_attempts": attempts,
    }


def _print_human(record: dict[str, Any]) -> None:
    typer.echo(f"request     {record['request_digest']}")
    for label, key in (
        ("principal", "principal_id"),
        ("work order", "work_order_id"),
        ("attempt", "work_order_attempt"),
        ("effect", "effect_id"),
        ("class", "effect_class"),
        ("destination", "destination"),
        ("action", "action_digest"),
        ("expires", "expires_at"),
    ):
        typer.echo(f"  {label:<12}{record[key]}")

    if record["decision"] is None:
        typer.echo(f"  {'decision':<12}pending")
    else:
        typer.echo(
            f"  {'decision':<12}{record['decision']} by {record['deciding_principal_id']}"
            f" at {record['decided_at']}"
        )
        if record["decision_transport"]:
            typer.echo(
                f"  {'transport':<12}{record['decision_transport']} (informational, not authority)"
            )
    if record["expired"]:
        typer.echo(f"  {'expired':<12}yes - a new request is required")

    if not record["effect_attempts"]:
        typer.echo(f"  {'effect':<12}not prepared")
        return
    for attempt in record["effect_attempts"]:
        typer.echo(f"  {'effect':<12}{attempt['idempotency_key']}: {attempt['state']}")
        if attempt["state"] == EffectState.UNKNOWN.value:
            typer.echo(
                "               outcome ambiguous: reconcile with the destination, or retry"
                " only through an adapter that enforces this idempotency key remotely"
            )
        if attempt["external_ref"]:
            typer.echo(f"               destination ref: {attempt['external_ref']}")


@approvals_app.command("ls")
def list_requests(
    pending: bool = typer.Option(False, "--pending", help="Only requests with no decision."),
    as_json: bool = typer.Option(False, "--json", "-J", help="Machine-readable output."),
    db: Optional[Path] = typer.Option(None, "--db", hidden=True, help="Override the store path."),
) -> None:
    """List approval requests. Reads only; never executes an effect."""
    with _open(db) as store:
        records = [_describe(store, request) for request in store.list_requests(pending_only=pending)]

    if as_json:
        typer.echo(json.dumps(records, indent=2))
        return
    if not records:
        typer.echo("no pending approval requests" if pending else "no approval requests")
        return
    for record in records:
        state = record["decision"] or "pending"
        typer.echo(
            f"{record['request_digest'][:12]}  {state:<9} {record['effect_class']:<26}"
            f" {record['destination']}"
        )


@approvals_app.command("show")
def show_request(
    request_digest: str = typer.Argument(..., help="Full request digest."),
    as_json: bool = typer.Option(False, "--json", "-J", help="Machine-readable output."),
    db: Optional[Path] = typer.Option(None, "--db", hidden=True, help="Override the store path."),
) -> None:
    """Show every bound field of one request. Reads only; never executes an effect."""
    with _open(db) as store:
        request = store.get_request(request_digest)
        if request is None:
            _fail(f"no request matches digest {request_digest}")
            return
        record = _describe(store, request)

    if as_json:
        typer.echo(json.dumps(record, indent=2))
    else:
        _print_human(record)


@approvals_app.command("decide")
def decide_request(
    request_digest: str = typer.Argument(..., help="Full request digest."),
    principal: str = typer.Option(..., "--as", help="Deciding principal id."),
    approve: bool = typer.Option(False, "--approve", help="Approve this exact effect."),
    decline: bool = typer.Option(False, "--decline", help="Decline this exact effect."),
    transport: Optional[str] = typer.Option(
        None, "--transport", help="Where the decision arrived from. Informational only."
    ),
    as_json: bool = typer.Option(False, "--json", "-J", help="Machine-readable output."),
    db: Optional[Path] = typer.Option(None, "--db", hidden=True, help="Override the store path."),
) -> None:
    """Decide one exact request.

    The deciding principal is authorized against independent policy
    (config.approvals.authorized_principals). Naming a principal here does not
    make it authorized. Approving records a decision and executes nothing.
    """
    if approve == decline:
        _fail("choose exactly one of --approve or --decline")

    decision = DecisionKind.APPROVED if approve else DecisionKind.DECLINED
    authority = load_authority()
    if not authority.is_configured:
        _fail(
            "no approval policy is configured, so nobody may decide. Set "
            "config.approvals.authorized_principals before deciding."
        )

    with _open(db, authority) as store:
        try:
            recorded = store.decide(
                request_digest=request_digest,
                deciding_principal_id=principal,
                decision=decision,
                transport=transport,
            )
        except RefusedError as exc:
            _refuse(exc, as_json)
            return
        record = _describe(store, store.get_request(request_digest))  # type: ignore[arg-type]

    if as_json:
        typer.echo(
            json.dumps(
                {
                    "result": recorded.decision.value,
                    "request_digest": recorded.request_digest,
                    "deciding_principal_id": recorded.deciding_principal_id,
                    "authority_source": authority.source,
                    "executed": False,
                    "acknowledged": False,
                    "request": record,
                },
                indent=2,
            )
        )
        return

    typer.echo(f"{recorded.decision.value} by {recorded.deciding_principal_id}")
    typer.echo(f"  authority   {authority.source}")
    if decision is DecisionKind.APPROVED:
        typer.echo("  effect      not executed - approval is not execution")
        typer.echo("  delivery    not acknowledged - only the destination can acknowledge")
    else:
        typer.echo("  effect      terminal - a declined request never executes")

"""`fno decide` - record a decision with no question on file, and recover one.

That is the case that loses today: the operator states a ruling in chat, it
touches no file, emits no event, and dies with the context. This verb is the
explicit write path because automatic recording would require classifying a
ruling from a truncated view.

Machine-first, mirroring `fno outstanding`: stdout carries the value (the new
decision id, or the decision history as JSON), guidance goes to stderr.
"""
from __future__ import annotations

import json
from typing import List, Optional

import typer

decide_app = typer.Typer(
    help=(
        "Record an operator decision so it survives the session, and recover "
        "the decision history for a subject. `fno decide --subject <subject> "
        "--decision \"...\"` records; `fno decide list --subject <subject>` "
        "recovers, newest first, including superseded ones (marked, not "
        "hidden). The subject is any string - a node id/slug, a PR (`pr-923`), "
        "a file, an area - and the reader takes every one the writer takes. "
        "`fno decide list` with no subject shows the recent ones; "
        "`fno decide reindex` backfills decisions recorded before the index."
    ),
)


@decide_app.callback(invoke_without_command=True)
def record(
    ctx: typer.Context,
    subject: Optional[str] = typer.Option(
        None, "--subject", help="What the decision governs: a node id/slug, file, or area."
    ),
    decision: Optional[str] = typer.Option(
        None, "--decision", help="What was chosen."
    ),
    question_id: Optional[str] = typer.Option(
        None,
        "--question-id",
        help="The operator_question this decision answers, when one is on file. "
        "Without it a decision recorded to settle an already-closed question "
        "cannot match the stop gate, which keys on this id.",
    ),
    rationale: Optional[str] = typer.Option(
        None, "--rationale", help="One line: the reason, not a restatement."
    ),
    option: List[str] = typer.Option(
        [], "--option", help="What was on the table; repeatable."
    ),
    supersedes: Optional[str] = typer.Option(
        None, "--supersedes", help="Decision id this one overturns."
    ),
    decided_by: Optional[str] = typer.Option(
        None,
        "--decided-by",
        help="Who decided. Defaults to the operator; name the agent under a beastmode grant.",
    ),
) -> None:
    """Record a decision as a durable event plus a graph projection."""
    if ctx.invoked_subcommand is not None:
        return
    if not decision or not subject:
        typer.echo(
            "decide: --subject and --decision are required to record", err=True
        )
        raise typer.Exit(1)

    from fno.decide import record_decision

    try:
        result = record_decision(
            decision=decision,
            subject=subject,
            question_id=question_id,
            decided_by=decided_by or "operator",
            authority_source="operator" if not decided_by else "beastmode",
            rationale=rationale,
            options=list(option) or None,
            supersedes=supersedes,
        )
    except Exception as exc:  # noqa: BLE001 - a failed capture is never a silent success
        typer.echo(f"decide: failed to record: {exc}", err=True)
        raise typer.Exit(1)

    did = result["decision_id"]
    # The receipt names the recall command in BOTH branches. A subject that
    # names no node loses only the graph projection; it is indexed and
    # recoverable exactly like one that does.
    if result["node_id"] is None:
        typer.echo(
            f"decide: recorded {did}; subject names no graph node, so no "
            f"projection was written (the event and the index are the record). "
            f"Recover with: fno decide list --subject {subject}",
            err=True,
        )
    else:
        typer.echo(
            f"decide: recorded {did} on {result['node_id']}. "
            f"Recover with: fno decide list --subject {result['node_id']}",
            err=True,
        )
    # stdout carries the value: the new decision id.
    typer.echo(did)


@decide_app.command("list")
def list_cmd(
    subject: Optional[str] = typer.Option(
        None,
        "--subject",
        help="What the decision governs: a node id/slug, a PR, a file, an area. "
        "Omit it to see the recent decisions across every subject.",
    ),
    limit: int = typer.Option(
        20, "--limit", help="Most recent N. 0 means no cap."
    ),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit one JSON object instead of the human block."
    ),
) -> None:
    """Recover the decision history for a subject, newest first."""
    from fno.decide import list_decisions

    label, decisions = list_decisions(subject, limit=limit)

    if as_json:
        typer.echo(
            json.dumps(
                {"subject": label, "decisions": decisions}, separators=(",", ":")
            )
        )
        return

    if not decisions:
        # Exit 0: a read that answered "none" is a successful read. Only a read
        # that could not run is a failure.
        typer.echo(f"decide list: no decisions recorded for '{label}'", err=True)
        return

    for d in decisions:
        superseded = str(d.get("superseded_by") or "")
        marker = f"  [superseded by {superseded}]" if superseded else ""
        # Across subjects the subject IS the column that tells the rows apart;
        # scoped to one it is the same word on every line.
        scope = "" if subject else f"{d.get('subject') or '(none)'}  "
        typer.echo(
            f"{d.get('decision_id')}  {d.get('ts', '')}  {scope}"
            f"{d.get('decided_by', '')}  {d.get('decision', '')}{marker}"
        )
        if d.get("rationale"):
            typer.echo(f"    rationale: {d['rationale']}")
        if d.get("question"):
            typer.echo(f"    question: {d['question']}")
        if d.get("options"):
            typer.echo(f"    options: {', '.join(str(o) for o in d['options'])}")
        if d.get("supersedes"):
            typer.echo(f"    supersedes: {d['supersedes']}")


@decide_app.command("reindex")
def reindex_cmd() -> None:
    """Backfill the recall index from the graph projections and the journals.

    A decision recorded before the index existed is durable but unreadable
    until this runs. Idempotent by decision id, so running it twice is free.
    """
    from fno.decide import reindex

    try:
        counts = reindex()
    except Exception as exc:  # noqa: BLE001 - a partial backfill must not read as done
        typer.echo(f"decide reindex: failed: {exc}", err=True)
        raise typer.Exit(1)

    note = f"reindex: +{counts['added']} decisions ({counts['already']} already indexed)"
    if counts.get("invalid"):
        note += f", {counts['invalid']} unreadable rows skipped"
    typer.echo(note, err=True)
    # stdout carries the value: the number of decisions now recoverable.
    typer.echo(counts["total"])

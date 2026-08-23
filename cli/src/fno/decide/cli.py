"""Decision commands for ``fno backlog`` and the one-release old-spelling shim.

That is the case that loses today: the operator states a ruling in chat, it
touches no file, emits no event, and dies with the context. This verb is the
explicit write path because automatic recording would require classifying a
ruling from a truncated view.

Machine-first, mirroring `fno inbox outstanding`: stdout carries the value (the new
decision id, or the decision history as JSON), guidance goes to stderr.
"""
from __future__ import annotations

import json
from typing import List, Optional

import typer

shim_app = typer.Typer(
    help=(
        "Deprecated compatibility shim for the decision commands now under "
        "fno backlog."
    ),
)
decide_app = shim_app

_DEPRECATION_NOTICE = (
    "fno decide is now `fno backlog decide` (`fno decide list` -> "
    "`fno backlog decisions`, `fno decide reindex` -> `fno backlog "
    "decide-reindex`). This spelling is removed next release."
)


def _render_claim_receipt(node_id: str, event: dict) -> None:
    """Print advisory claim context for a recorded node decision."""
    key = f"node:{node_id}"
    caller_label = event.get("data", {}).get("decided_by") or "unknown caller"
    try:
        from fno.agents.self_stamp import resolve_self_identity
        from fno.claims.core import claim_status
        from fno.claims.io import claims_root_for

        status = claim_status(key, root=claims_root_for(key))
    except Exception as exc:  # noqa: BLE001 - receipt failure cannot undo a ruling
        typer.echo(
            f"backlog decide: claim {key} unavailable for caller {caller_label}: "
            f"{exc}; caller comparison unavailable",
            err=True,
        )
        return

    state = status.get("state") or "unavailable"
    line = f"backlog decide: claim {key}; claim state: {state}; caller: {caller_label}"
    if state == "free":
        typer.echo(line, err=True)
        return

    holder = status.get("holder")
    if holder:
        line += f"; holder: {holder}"

    comparison = "caller comparison unavailable"
    if state in {"live", "suspect"} and isinstance(holder, str):
        prefix, separator, holder_session = holder.partition(":")
        try:
            identity = resolve_self_identity()
            caller_session = (identity.session_id or "").strip()
        except Exception:  # noqa: BLE001 - missing identity is an honest unknown
            caller_session = ""
        if prefix == "target-session" and separator and holder_session and caller_session:
            if holder_session == caller_session:
                comparison = "this caller holds the node"
            else:
                comparison = "another session holds the node, not this caller"

    error = status.get("error")
    if error:
        line += f"; error: {error}"
    typer.echo(f"{line}; {comparison}", err=True)


@shim_app.callback(invoke_without_command=True)
def legacy_record(
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
        help="A name to RELAY for someone else, when you are recording their "
        "ruling rather than your own. Inside a session it lands in relayed_by; "
        "decided_by is always stamped from the session itself. Outside one it "
        "is the decider.",
    ),
    authority: Optional[str] = typer.Option(
        None,
        "--authority",
        help="How the decider was entitled to decide: 'operator', 'crown', "
        "'agent', or 'beastmode'. Omit to resolve it from the current session.",
    ),
) -> None:
    """Warn once, then delegate the old spelling to the backlog leaf."""
    typer.echo(_DEPRECATION_NOTICE, err=True)
    if ctx.invoked_subcommand is not None:
        return
    _record(
        subject=subject,
        decision=decision,
        question_id=question_id,
        rationale=rationale,
        option=option,
        supersedes=supersedes,
        decided_by=decided_by,
        authority=authority,
    )


def _record(
    *,
    subject: Optional[str],
    decision: Optional[str],
    question_id: Optional[str],
    rationale: Optional[str],
    option: List[str],
    supersedes: Optional[str],
    decided_by: Optional[str],
    authority: Optional[str],
) -> None:
    """Record a decision as a durable event plus a graph projection."""
    if not decision or not subject:
        typer.echo(
            "backlog decide: subject and decision are required to record", err=True
        )
        raise typer.Exit(1)

    from fno.decide import (
        AUTHORITY_SOURCES,
        IndexWriteError,
        RefusedAuthorityError,
        UnattributedAuthorityError,
        record_decision,
    )

    # Validated here, on the write path, and deliberately NOT in schema.yaml:
    # rows already on disk carry invented `crown-l2-<node>` spellings, and a
    # schema enum would make `fno backlog decide-reindex` reject them and drop recall
    # for real rulings.
    if authority is not None and authority not in AUTHORITY_SOURCES:
        typer.echo(
            f"backlog decide: --authority '{authority}' is not one of "
            f"{', '.join(AUTHORITY_SOURCES)}. Nothing was recorded. Use 'crown' "
            "for a king ruling inside its own scope; omit the flag to resolve it "
            "from this session.",
            err=True,
        )
        raise typer.Exit(2)

    try:
        result = record_decision(
            decision=decision,
            subject=subject,
            question_id=question_id,
            decided_by=decided_by,
            # Stamped, never stated. `_resolve_decider` reads the ambient
            # session and puts a supplied name in relayed_by, because a reader
            # who quotes only decided_by must never be handed a name an agent
            # typed. Authority stays stated, validated against the enum above.
            authority_source=authority,
            rationale=rationale,
            options=list(option) or None,
            supersedes=supersedes,
        )
    except RefusedAuthorityError as exc:
        typer.echo(
            f"backlog decide: refused. This session is agent {exc.agent_handle}, so it "
            "cannot record under operator authority. If only the operator can "
            "settle this, use `fno inbox outstanding ask`. If ruling as an agent, "
            "drop --authority operator; it records agent coordination.",
            err=True,
        )
        raise typer.Exit(3)
    except UnattributedAuthorityError:
        typer.echo(
            "backlog decide: refused. This process has no session identity and no "
            "terminal, so nothing here shows the operator ruled. Operator "
            "authority is never inherited by silence. Run it from a terminal, "
            "join the session so a handle can be stamped, or drop --authority "
            "operator to record it in the unattributed lane.",
            err=True,
        )
        raise typer.Exit(3)
    except IndexWriteError as exc:
        # Exit 1, because the ruling is not recoverable yet. But name the right
        # remedy: the durable event HAS landed, so re-running this command
        # mints a second id for one ruling.
        typer.echo(
            f"backlog decide: recorded {exc.decision_id} to the project journal, but the "
            f"recall index write failed: {exc}. Run `fno backlog decide-reindex` to "
            f"recover it. Do NOT re-run decide; that records it twice.",
            err=True,
        )
        raise typer.Exit(1)
    except Exception as exc:  # noqa: BLE001 - a failed capture is never a silent success
        typer.echo(f"backlog decide: failed to record: {exc}", err=True)
        raise typer.Exit(1)

    did = result["decision_id"]
    if supersedes:
        # A transposed digit is otherwise a silent no-op: the older ruling
        # keeps reading as current, in a verb whose contract is that a reader
        # of an overturned decision can tell it is not.
        from fno.decide import list_decisions

        _, everything, _ = list_decisions()
        if supersedes not in {d.get("decision_id") for d in everything}:
            typer.echo(
                f"backlog decide: warning - no decision {supersedes} is on record, so "
                f"nothing was marked superseded. Check the id with "
                f"`fno backlog decisions {subject}`.",
                err=True,
            )
    # The receipt names the recall command in BOTH branches. A subject that
    # names no node loses only the graph projection; it is indexed and
    # recoverable exactly like one that does.
    if result["node_id"] is None:
        typer.echo(
            f"backlog decide: recorded {did}; subject names no graph node, so no "
            f"projection was written (the event and the index are the record). "
            f"Recover with: fno backlog decisions {subject}",
            err=True,
        )
    else:
        typer.echo(
            f"backlog decide: recorded {did} on {result['node_id']}. "
            f"Recover with: fno backlog decisions {result['node_id']}",
            err=True,
        )
        _render_claim_receipt(result["node_id"], result["event"])
    # stdout carries the value: the new decision id.
    typer.echo(did)


def backlog_decide(
    subject: Optional[str] = typer.Argument(
        None, help="Node or subject governed by the decision."
    ),
    decision: Optional[str] = typer.Argument(
        None, help="What was chosen."
    ),
    question_id: Optional[str] = typer.Option(
        None,
        "--question-id",
        help="The operator_question this decision answers.",
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
        None, "--decided-by", help="A name to relay for someone else."
    ),
    authority: Optional[str] = typer.Option(
        None,
        "--authority",
        help="How the decider was entitled to decide.",
    ),
    subject_legacy: Optional[str] = typer.Option(
        None, "--subject", hidden=True, help="Deprecated alias for the subject argument."
    ),
    decision_legacy: Optional[str] = typer.Option(
        None,
        "--decision",
        hidden=True,
        help="Deprecated alias for the decision argument.",
    ),
) -> None:
    from fno._flag_aliases import merge_deprecated_alias

    subject = merge_deprecated_alias(
        subject,
        subject_legacy,
        canonical_flag="<subject>",
        legacy_flag="--subject",
    )
    decision = merge_deprecated_alias(
        decision,
        decision_legacy,
        canonical_flag="<decision>",
        legacy_flag="--decision",
    )
    _record(
        subject=subject,
        decision=decision,
        question_id=question_id,
        rationale=rationale,
        option=option,
        supersedes=supersedes,
        decided_by=decided_by,
        authority=authority,
    )


@shim_app.command("list")
def list_cmd(
    subject: Optional[str] = typer.Option(
        None,
        "--subject",
        help="What the decision governs. Omit it for recent decisions.",
    ),
    limit: int = typer.Option(
        20, "--limit", help="Most recent N. 0 or less means no cap."
    ),
    lane: Optional[str] = typer.Option(
        None,
        "--lane",
        metavar="law|coord|grant|unattributed",
        help="Show only one authority lane.",
    ),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit one JSON object instead of the human block."
    ),
) -> None:
    _list_decisions(subject, limit, lane, as_json)


def backlog_decisions(
    subject: Optional[str] = typer.Argument(
        None,
        help="Node or subject whose decisions to recover. Omit for recent decisions.",
    ),
    limit: int = typer.Option(
        20, "--limit", help="Most recent N. 0 or less means no cap."
    ),
    lane: Optional[str] = typer.Option(
        None,
        "--lane",
        metavar="law|coord|grant|unattributed",
        help="Show only one authority lane.",
    ),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit one JSON object instead of the human block."
    ),
    subject_legacy: Optional[str] = typer.Option(
        None, "--subject", hidden=True, help="Deprecated alias for the subject argument."
    ),
) -> None:
    from fno._flag_aliases import merge_deprecated_alias

    subject = merge_deprecated_alias(
        subject,
        subject_legacy,
        canonical_flag="<subject>",
        legacy_flag="--subject",
    )
    _list_decisions(subject, limit, lane, as_json)


def _list_decisions(
    subject: Optional[str],
    limit: int,
    lane: Optional[str],
    as_json: bool,
) -> None:
    """Recover the decision history for a subject, newest first."""
    from fno.decide import (
        list_decisions,
        looks_like_decision_id,
        near_miss_subjects,
    )
    from fno.tracker.metadata import ExternalMetadataUnavailable

    if lane not in {None, "law", "coord", "grant", "unattributed"}:
        typer.echo(
            "backlog decisions: --lane must be law, coord, grant, or unattributed",
            err=True,
        )
        raise typer.Exit(2)

    try:
        # No cap on the read. The cap is applied HERE so the total is known,
        # and a truncated answer can say so - a silent cut on a recall verb is
        # the same lie as a missing record.
        label, found, damaged = list_decisions(subject, limit=None, lane=lane)
    except (OSError, ValueError) as exc:
        # ValueError covers UnicodeDecodeError, which a torn multi-byte append
        # raises and which is NOT an OSError.
        typer.echo(f"backlog decisions: cannot read the decision index: {exc}", err=True)
        raise typer.Exit(1)
    except ExternalMetadataUnavailable as exc:
        typer.echo(f"backlog decisions: {exc}", err=True)
        raise typer.Exit(1)

    decisions = found[:limit] if limit > 0 else found
    truncated = len(decisions) < len(found)
    # Computed for EVERY subject read, not only an empty one. The specimen this
    # exists for returns one row: `--subject x-f7b9` matched a wave plan and hid
    # four rulings filed under `x-f7b9 scope`. A near-miss scan that only runs
    # when the answer is empty would have stayed silent on exactly that case,
    # and a partial answer reads as a whole one.
    near = near_miss_subjects(subject) if subject else []

    if as_json:
        # matched_by tells a machine reader WHICH key answered, so an id lookup
        # is never mistaken for a subject hit. A LIST, because the read is a
        # union: `--subject d-XXXX` returns that decision AND any ruling filed
        # about it, so a single value would deny that the subject key answered.
        # Computed over `found`, never the --limit slice, or a cap silently
        # drops one of the two keys from the answer.
        matched: "list[str]" = []
        if subject:
            want = subject.strip().casefold()
            if any(str(d.get("decision_id") or "").casefold() == want for d in found):
                matched.append("decision_id")
            if any(
                str(d.get("decision_id") or "").casefold() != want for d in found
            ):
                matched.append("subject")
        typer.echo(
            json.dumps(
                {
                    "subject": label,
                    "decisions": decisions,
                    "total": len(found),
                    "truncated": truncated,
                    # This surface is machine-first, so an under-count that
                    # looks complete is the same lie "truncated" prevents.
                    "damaged": damaged,
                    "matched_by": matched if subject else None,
                    "near_misses": [{"subject": s, "count": n} for s, n in near],
                },
                separators=(",", ":"),
            )
        )
        return

    if not decisions:
        # Exit 0: a read that answered "none" is a successful read. Only a read
        # that could not run is a failure.
        #
        # But an install that predates the index has NO index, and every
        # decision it holds lives in the graph projection this reader no longer
        # consults. "None recorded" would then be the absence-reads-as-success
        # shape this verb exists to police, on its own upgrade path. So the
        # empty answer names the backfill whenever the index is missing.
        from fno.decide import _index_path

        if lane is not None and _index_path().exists():
            # The LANE emptied the answer, not the store. Saying nothing is
            # indexed under this subject would be false, and the reader acts on
            # it.
            #
            # ONE branch for every lane. `law` used to have its own, naming
            # only the pre-cutover rows, so a subject with 2 unattributed and 3
            # coord rulings heard about the 2 and never the 3: the more
            # specific branch gave the less complete answer.
            _, unfiltered, _ = list_decisions(subject, limit=None)
            if unfiltered:
                noun = "decision" if len(unfiltered) == 1 else "decisions"
                counts: "dict[str, int]" = {}
                for d in unfiltered:
                    key = str(d.get("lane") or "unattributed")
                    counts[key] = counts.get(key, 0) + 1
                lanes = ", ".join(f"{n} {k}" for k, n in sorted(counts.items()))
                hint = ""
                if lane == "law" and counts.get("unattributed"):
                    hint = (
                        " The unattributed ones are pre-cutover, recorded "
                        "before authority was an earned value."
                    )
                typer.echo(
                    f"backlog decisions: 0 {lane} decisions for '{label}', but "
                    f"{len(unfiltered)} {noun} sit under it: {lanes}."
                    f"{hint} Drop --lane to read them.",
                    err=True,
                )
                return

        hint = (
            "" if _index_path().exists()
            else " (no index yet on this machine - run `fno backlog decide-reindex` to "
            "backfill what is already on disk)"
        )
        # NEVER "no decisions recorded". That is a claim about the world, and
        # only a claim about the QUERY is true here. Say what is not indexed,
        # then name what is searchable.
        if near:
            listed = "; ".join(f"'{s}' ({n})" for s, n in near)
            typer.echo(
                f"backlog decisions: nothing is indexed under the exact subject "
                f"'{label}'{hint}. Nearly matching subjects: {listed}. Read one "
                f"with: fno backlog decisions '{near[0][0]}'",
                err=True,
            )
        elif subject and looks_like_decision_id(subject):
            typer.echo(
                f"backlog decisions: '{label}' is shaped like a decision id, and no "
                f"decision on this machine carries it{hint}. It is not indexed "
                "as a subject either. Browse the store with: fno backlog decisions",
                err=True,
            )
        else:
            typer.echo(
                f"backlog decisions: no decision is indexed under the subject "
                f"'{label}'{hint}. Rulings on other subjects are unaffected; "
                "browse them with: fno backlog decisions",
                err=True,
            )
        return

    for d in decisions:
        superseded = str(d.get("superseded_by") or "")
        marker = f"  [superseded by {superseded}]" if superseded else ""
        # Across subjects the subject IS the column that tells the rows apart;
        # scoped to one it is the same word on every line.
        scope = "" if subject else f"{d.get('subject') or '(none)'}  "
        lane_marker = "LAW" if d.get("lane") == "law" else d.get("lane", "")
        # Provenance travels ON the row. A citation is quoted without the lane
        # column far more often than it is read here, so the authority and the
        # attestation have to be part of what a reader copies.
        attested = "  [attested]" if d.get("attested_by") else ""
        # Parenthesised only when there IS one. Law is no longer defaulted, so
        # most rows claim no authority, and `operator ()` on every line is
        # noise on the one line added to carry provenance. `or ""` rather than
        # a .get default: an explicit JSON null would otherwise print "None".
        authority = str(d.get("authority_source") or "")
        authority = f" ({authority})" if authority else ""
        typer.echo(
            f"{lane_marker}  {d.get('decision_id')}  {d.get('ts') or ''}  {scope}"
            f"{d.get('decided_by') or ''}{authority}"
            f"{attested}  {d.get('decision') or ''}{marker}"
        )
        if d.get("relayed_by"):
            typer.echo(
                f"    relayed: {d['relayed_by']} (a name this caller supplied, "
                "not a stamped one)"
            )
        if d.get("rationale"):
            typer.echo(f"    rationale: {d['rationale']}")
        if d.get("question"):
            typer.echo(f"    question: {d['question']}")
        if d.get("options"):
            typer.echo(f"    options: {', '.join(str(o) for o in d['options'])}")
        if d.get("supersedes"):
            typer.echo(f"    supersedes: {d['supersedes']}")

    if truncated:
        typer.echo(
            f"backlog decisions: showing {len(decisions)} of {len(found)}; "
            f"--limit 0 for all.",
            err=True,
        )

    if near:
        # An answer that arrived is not an answer that is whole. `--subject
        # x-f7b9` returned one wave plan while four rulings sat under
        # `x-f7b9 scope`, and nothing said so.
        #
        # It states the near misses and nothing else. `len(found)` is already
        # lane-filtered, so calling it the count for this subject under-reports
        # the record - a wrong number inside the message that exists to stop a
        # reader trusting a short answer.
        listed = "; ".join(f"'{s}' ({n})" for s, n in near)
        typer.echo(
            f"backlog decisions: decisions also sit under subjects that nearly match "
            f"'{label}': {listed}",
            err=True,
        )


def _reindex() -> None:
    """Backfill the recall index from the graph projections and the journals.

    A decision recorded before the index existed is durable but unreadable
    until this runs. Idempotent by decision id, so running it twice is free.
    """
    from fno.decide import reindex

    from fno.decide import _index_path

    try:
        counts = reindex()
    except Exception as exc:  # noqa: BLE001 - a partial backfill must not read as done
        typer.echo(
            f"backlog decide-reindex: failed on the index at {_index_path()}: {exc}", err=True
        )
        raise typer.Exit(1)

    note = f"reindex: +{counts['added']} decisions ({counts['already']} already indexed)"
    if counts.get("repaired"):
        note += f", {counts['repaired']} damaged row(s) moved aside"
    if counts.get("unusable"):
        note += f", {counts['unusable']} row(s) the schema will not accept"
    if counts.get("invalid"):
        note += f", {counts['invalid']} rows could not be written"
    typer.echo(note, err=True)
    # stdout carries the value: the number of decisions now recoverable.
    typer.echo(counts["total"])

    # Exit 1 on ANY write failure, not only on a total one. The counter cannot
    # tell an unusable legacy row from a store that went unwritable partway
    # through, and a caller gating on the exit code (`fno backlog decide-reindex && ...`,
    # or an agent following the recovery an IndexWriteError named) must not read
    # success while decisions stay unrecoverable. Fail safe on the ambiguity.
    if counts.get("invalid"):
        typer.echo(
            f"backlog decide-reindex: {counts['invalid']} row(s) could not be written, "
            f"so the backfill is incomplete. Check that {_index_path()} is "
            f"writable, then run it again.",
            err=True,
        )
        raise typer.Exit(1)


@shim_app.command("reindex")
def reindex_cmd() -> None:
    _reindex()


def backlog_decide_reindex() -> None:
    _reindex()

"""Decision commands: canonical at ``fno inbox decide``, silent
alias at ``fno backlog decide``.

That is the case that loses today: the operator states a ruling in chat, it
touches no file, emits no event, and dies with the context. This verb is the
explicit write path because automatic recording would require classifying a
ruling from a truncated view.

Machine-first, mirroring `fno inbox outstanding`: stdout carries the value (the new
decision id, or the decision history as JSON), guidance goes to stderr.
"""
from __future__ import annotations

import json
import os
from typing import List, Optional

import typer

from fno.decide import READ_HELP
from fno.decide.graduation import REFERENCE_HELP as grad_reference_help

shim_app = typer.Typer(
    help=(
        "One-release root registration for `decide`. Unreachable "
        "via the CLI - VERB_MOVES intercepts the old spelling and prints "
        "its own notice - kept only so the registry holds a hidden, not a "
        "phantom, root until the release clock expires."
    ),
)
decide_app = shim_app

_DEPRECATION_NOTICE = (
    "fno decide is now `fno inbox decide` (`fno decide list` -> "
    "`fno inbox decisions`, `fno decide reindex` -> `fno backlog "
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
            f"decide: claim {key} unavailable for caller {caller_label}: "
            f"{exc}; caller comparison unavailable",
            err=True,
        )
        return

    state = status.get("state") or "unavailable"
    line = f"decide: claim {key}; claim state: {state}; caller: {caller_label}"
    if state == "free":
        typer.echo(line, err=True)
        return

    holder = status.get("holder")
    if holder:
        line += f"; holder: {holder}"

    comparison = "caller comparison unavailable"
    if state in {"live", "suspect"} and isinstance(holder, str):
        prefix, separator, holder_session = holder.partition(":")
        # Target init gives the driver-assigned run id precedence over the
        # harness session id when it stamps a target-session claim. Keep that
        # precedence here so the receipt does not call the current run foreign.
        caller_session = os.environ.get("TARGET_SESSION_ID", "").strip()
        if not caller_session:
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
    graduation: Optional[str] = typer.Option(
        None,
        "--graduation",
        help="enforced, guidance, or should-be-enforced-but-i-did-not.",
    ),
    graduation_ref: Optional[str] = typer.Option(
        None, "--graduation-ref", help=grad_reference_help
    ),
    read: List[str] = typer.Option([], "--read", help=READ_HELP),
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
        origin=None,
        graduation=graduation,
        graduation_ref=graduation_ref,
        read=read,
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
    origin: Optional[str],
    graduation: Optional[str],
    graduation_ref: Optional[str],
    read: List[str],
) -> None:
    """Record a decision as a durable event plus a graph projection."""
    if not decision or not subject:
        typer.echo(
            "decide: subject and decision are required to record", err=True
        )
        raise typer.Exit(1)

    from fno.decide import (
        AUTHORITY_SOURCES,
        IndexWriteError,
        RefusedAuthorityError,
        UnknownOriginError,
        UnattributedAuthorityError,
        WaiverAuthorityRefusedError,
        record_decision,
    )
    from fno.decide import (
        UnmeasuredClaimError,
        UnresolvableCitationError,
    )
    from fno.rust_binary import VerbUnavailable
    from fno.decide.graduation import InvalidGraduationError, graduation_or_guidance

    # Validated here, on the write path, and deliberately NOT in schema.yaml:
    # rows already on disk carry invented `crown-l2-<node>` spellings, and a
    # schema enum would reject them and drop recall for real rulings.
    if authority is not None and authority not in AUTHORITY_SOURCES:
        typer.echo(
            f"decide: --authority '{authority}' is not one of "
            f"{', '.join(AUTHORITY_SOURCES)}. Nothing was recorded. Use 'crown' "
            "for a king ruling inside its own scope; omit the flag to resolve it "
            "from this session.",
            err=True,
        )
        raise typer.Exit(2)

    try:
        graduation_data = graduation_or_guidance(graduation, graduation_ref)
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
            graduation=graduation_data,
            origin=origin,
            rationale=rationale,
            options=list(option) or None,
            supersedes=supersedes,
            reads=list(read) or None,
        )
    except (UnmeasuredClaimError, UnresolvableCitationError, VerbUnavailable) as exc:
        # Same ladder as the law door: the ruling was refused before any
        # write, so the caller must not re-run it expecting a different id.
        # VerbUnavailable rides it: a gate that cannot run refuses too.
        typer.echo(f"decide: refused. {exc} Nothing was recorded.", err=True)
        raise typer.Exit(3)
    except UnknownOriginError as exc:
        typer.echo(f"decide: refused. {exc}", err=True)
        raise typer.Exit(3)
    except InvalidGraduationError as exc:
        typer.echo(f"backlog decide: refused. {exc}. Nothing was recorded.", err=True)
        raise typer.Exit(2)
    except WaiverAuthorityRefusedError as exc:
        typer.echo(f"decide: refused. {exc}", err=True)
        raise typer.Exit(3)
    except RefusedAuthorityError as exc:
        typer.echo(
            f"backlog decide: refused. This session is agent {exc.agent_handle}, so it "
            "cannot record decisions HERE. The law door is open to it and "
            "the terms are narrow, so read them before you use it: "
            "`fno inbox law set <subject> <decision> --rationale <why>` "
            "(the operator types `/fno:law <the ruling>` for the same "
            "thing) records a chat_attested row, never an operator row, "
            "and it cannot supersede the operator's own law. That door is "
            "for a durable rule the OPERATOR asked for. It is not a way to "
            "route your own ruling around this refusal. "
            "Append agent findings without replacing node details with "
            "`fno backlog note <node> <text>`.",
            err=True,
        )
        raise typer.Exit(3)
    except UnattributedAuthorityError:
        typer.echo(
            "decide: refused. This process has no session identity and no "
            "terminal, so nothing here shows the operator ruled. Operator "
            "authority is never inherited by silence. Run "
            "`fno inbox law set <subject> <decision> --rationale <why>` from an "
            "attended operator "
            "terminal, or have the operator type `/fno:law <the ruling>` in "
            "chat, which records in one step. Append agent findings with "
            "`fno backlog note <node> <text>`.",
            err=True,
        )
        raise typer.Exit(3)
    except IndexWriteError as exc:
        # Exit 1, because the ruling is not recoverable yet. But name the right
        # remedy: the durable event HAS landed, so re-running this command
        # mints a second id for one ruling.
        typer.echo(
            f"decide: recorded {exc.decision_id} to the project journal, but the "
            f"recall store write failed: {exc}. Run `fno backlog decide-reindex` "
            "to recover it. Do NOT re-run decide; that records it twice.",
            err=True,
        )
        raise typer.Exit(1)
    except Exception as exc:  # noqa: BLE001 - a failed capture is never a silent success
        typer.echo(f"decide: failed to record: {exc}", err=True)
        raise typer.Exit(1)

    did = result["decision_id"]
    if supersedes:
        # A transposed digit is otherwise a silent no-op: the older ruling
        # keeps reading as current, in a verb whose contract is that a reader
        # of an overturned decision can tell it is not.
        from fno.decide import list_decisions

        _, everything, _ = list_decisions(state="all")
        if supersedes not in {d.get("decision_id") for d in everything}:
            typer.echo(
                f"decide: warning - no decision {supersedes} is on record, so "
                f"nothing was marked superseded. Check the id with "
                f"`fno backlog decisions {subject}`.",
                err=True,
            )
    # The receipt names the recall command in BOTH branches. A subject that
    # names no node loses only the graph projection; it is indexed and
    # recoverable exactly like one that does.
    if result["node_id"] is None:
        typer.echo(
            f"decide: recorded {did}; no projection was written because "
            f"{result['projection']} (the event and the index are the record). "
            f"Recover with: fno backlog decisions {subject}",
            err=True,
        )
    else:
        typer.echo(
            f"decide: recorded {did} on {result['node_id']}. "
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
    graduation: Optional[str] = typer.Option(
        None,
        "--graduation",
        help="enforced, guidance, or should-be-enforced-but-i-did-not.",
    ),
    graduation_ref: Optional[str] = typer.Option(
        None, "--graduation-ref", help=grad_reference_help
    ),
    read: List[str] = typer.Option([], "--read", help=READ_HELP),
    origin: Optional[str] = typer.Option(
        None,
        "--origin",
        hidden=True,
        help="Carried mail origin used by the machine law gate.",
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
        origin=origin,
        graduation=graduation,
        graduation_ref=graduation_ref,
        read=read,
    )


def backlog_decide_retract(
    decision_id: str = typer.Argument(..., help="Subject or decision id to retract."),
    reason: Optional[str] = typer.Option(None, "--reason", "-R", help="Why it no longer counts."),
    authority: Optional[str] = typer.Option(None, "--authority", help="Authority lane."),
    origin: Optional[str] = typer.Option(None, "--origin", hidden=True),
) -> None:
    """Compatibility forward to the native retract door (fno-agents).

    The retraction logic is native; this leaf exists so the old spellings
    keep resolving and the pinned surface sets do not shift. Retractions
    are append-only and have no inverse.
    """
    import os

    if origin is not None:
        typer.echo(
            "backlog decide-retract: --origin is retired: the native door "
            "takes no origin. Nothing was recorded.",
            err=True,
        )
        raise typer.Exit(2)
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "backlog decide-retract: refused: the fno-agents binary is "
            "unavailable (set FNO_AGENTS_BIN, or install fno-agents beside "
            "fno).",
            err=True,
        )
        raise typer.Exit(3)
    # A binary older than the native arm forwards right back here; break the
    # cycle with the one line that names the fix instead of exec-spinning.
    # The sentinel value is one this leaf mints, never a bare truthy flag, so
    # an unrelated export of the variable cannot trip the guard.
    if os.environ.get("FNO_BACKLOG_FORWARD") == "backlog-decide-retract":
        typer.echo(
            "backlog decide-retract: the resolved fno-agents binary predates "
            "the native decide-retract arm. Run `fno doctor update --rust` or "
            "rebuild fno-agents, then retry.",
            err=True,
        )
        raise typer.Exit(2)
    argv = [str(binary), "backlog", "decide-retract", decision_id]
    if reason:
        argv += ["--reason", reason]
    if authority:
        argv += ["--authority", authority]
    os.environ["FNO_BACKLOG_FORWARD"] = "backlog-decide-retract"
    os.execv(str(binary), argv)


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
    state: Optional[str] = typer.Option(
        None,
        "--state",
        metavar="live|retired|expired|superseded|retracted|unscoped|all",
        help="Filter by the derived lifecycle state.",
    ),
    review_list: bool = typer.Option(
        False,
        "--review-list",
        help="Report subjects with multiple unrelated live rulings without changing data.",
    ),
    output: Optional[str] = typer.Option(None, "--output", help="Write the full report to PATH."),
    output_format: Optional[str] = typer.Option(
        None, "--format", help="Export format: markdown or json."
    ),
    as_json: bool = typer.Option(
        False, "--json", "-J", help="Emit one JSON object instead of the human block."
    ),
    subject_legacy: Optional[str] = typer.Option(
        None, "--subject", hidden=True, help="Deprecated alias for the subject argument."
    ),
) -> None:
    """Compatibility forward to the native listing (fno-agents).

    The listing logic is native; this leaf exists so the old spellings keep
    resolving and the pinned surface sets do not shift. Reads never mutate
    the stores.
    """
    if subject_legacy is not None:
        if subject is not None:
            typer.echo(
                "backlog decisions: pass either <subject> or --subject "
                "(deprecated), not both",
                err=True,
            )
            raise typer.Exit(2)
        subject = subject_legacy
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "backlog decisions: refused: the fno-agents binary is "
            "unavailable (set FNO_AGENTS_BIN, or install fno-agents beside "
            "fno).",
            err=True,
        )
        raise typer.Exit(3)
    # A binary older than the native arm forwards right back here; break the
    # cycle with the one line that names the fix instead of exec-spinning.
    # The sentinel value is one this leaf mints, never a bare truthy flag.
    import os

    if os.environ.get("FNO_BACKLOG_FORWARD") == "backlog-decisions":
        typer.echo(
            "backlog decisions: the resolved fno-agents binary predates the "
            "native decisions arm. Run `fno doctor update --rust` or rebuild "
            "fno-agents, then retry.",
            err=True,
        )
        raise typer.Exit(2)
    argv = [str(binary), "backlog", "decisions"]
    if subject is not None:
        argv.append(subject)
    if limit != 20:
        argv += ["--limit", str(limit)]
    if lane is not None:
        argv += ["--lane", lane]
    if state is not None:
        argv += ["--state", state]
    if review_list:
        argv.append("--review-list")
    if output is not None:
        argv += ["--output", output]
    if output_format is not None:
        argv += ["--format", output_format]
    if as_json:
        argv.append("--json")
    os.environ["FNO_BACKLOG_FORWARD"] = "backlog-decisions"
    os.execv(str(binary), argv)


@shim_app.command("decide-reindex", hidden=True)
def decide_reindex_cmd() -> None:
    """Backfill the pre-wave-12 JSONL decision index."""
    from fno.decide import reindex

    try:
        typer.echo(json.dumps(reindex(), separators=(",", ":")))
    except (OSError, ValueError) as exc:
        typer.echo(f"backlog decide-reindex: failed: {exc}", err=True)
        raise typer.Exit(1)


backlog_decide_reindex = decide_reindex_cmd


@shim_app.command("reindex", hidden=True)
def reindex_compat_cmd() -> None:
    from fno import paths
    from fno.decide import reindex

    try:
        counts = reindex()
    except Exception as exc:  # noqa: BLE001 - recovery must name its refusal
        typer.echo(
            f"backlog decide-reindex: failed on the index at {paths.decisions_jsonl()}: {exc}",
            err=True,
        )
        raise typer.Exit(1)
    note = f"reindex: +{counts['added']} decisions ({counts['already']} already indexed)"
    if counts.get("unusable"):
        note += f", {counts['unusable']} row(s) the schema will not accept"
    if counts.get("invalid"):
        note += f", {counts['invalid']} rows could not be written"
    typer.echo(note, err=True)
    typer.echo(str(counts.get("total", 0)))
    if counts.get("invalid"):
        raise typer.Exit(1)

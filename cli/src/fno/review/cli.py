"""``fno do review`` - the review family's CLI surface.

The panel stays the default invocation (``fno do review`` with no
subcommand runs it unchanged, via the callback registration in
``fno.do_cli``); this module adds the subcommands the attestation record
needs. ``classify`` is the one shell entry point producers use to turn a
findings payload into the bounded record an event carries, so the rule is
never reimplemented in bash.
"""
from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any, Optional

import typer

from fno.review.findings import (
    FindingsNormalizeError,
    normalize,
    resolve_nonblocking_categories,
    shrink_summary,
    summarize,
)

review_app = typer.Typer(
    name="review",
    invoke_without_command=True,
    add_completion=False,
)

#: Cap on the per-finding primitives the event carries. The event envelope
#: already refuses data over ``limits.max_data_bytes`` (64 KiB); this budget
#: keeps the findings share under it with room for the attestation's own
#: fields. An overflow truncates the array and sets ``findings_truncated``,
#: which the gate reads as BLOCKING for the truncated remainder, so a
#: truncation can never quietly drop a finding into harmlessness.
_RECORD_BYTE_BUDGET = 48_000
_FINDINGS_COUNT_CAP = 200

#: The disposition enum on an attestation's ``dispositions`` array. The
#: writer is the producer that dispositioned the findings; the gate re-reads
#: it as the terminal-state input of wave 5.
_DISPOSITIONS = ("fixed", "declined", "nonblocking")


class RecordBuildError(ValueError):
    """The findings file cannot become a record; the emitter must refuse."""


def _resolved_categories() -> frozenset[str]:
    try:
        from fno.config import load_settings

        return resolve_nonblocking_categories(
            getattr(load_settings().review, "nonblocking_categories", None)
        )
    except Exception:  # noqa: BLE001 - unreadable config keeps the shipped default
        return resolve_nonblocking_categories(None)


def build_emit_record(payload: Any) -> dict[str, Any]:
    """A findings payload into the bounded record ``--emit-record`` prints.

    Accepts a bare array of finding records, or an object carrying a
    ``findings`` array plus optional ``dispositions`` and ``review_round``
    pass-through keys (the producer-supplied disposition trail and the
    branch-scoped round this verdict is). Raises :class:`RecordBuildError`
    on any shape the classifier cannot read: a refusal, never an empty
    record.
    """
    dispositions: list[dict[str, Any]] = []
    review_round: Optional[int] = None
    if isinstance(payload, dict):
        dispositions_raw = payload.get("dispositions")
        if dispositions_raw is not None:
            if not isinstance(dispositions_raw, list):
                raise RecordBuildError("dispositions must be an array")
            for entry in dispositions_raw:
                if not isinstance(entry, dict):
                    raise RecordBuildError("each disposition must be an object")
                key = entry.get("finding_key")
                disposition = entry.get("disposition")
                reason = entry.get("reason")
                if (
                    not isinstance(key, str)
                    or not key.strip()
                    or disposition not in _DISPOSITIONS
                    or not isinstance(reason, str)
                    or not reason.strip()
                ):
                    raise RecordBuildError(
                        "each disposition needs finding_key, a disposition in "
                        f"{list(_DISPOSITIONS)}, and a non-empty reason"
                    )
                dispositions.append(
                    {
                        "finding_key": key,
                        "disposition": disposition,
                        "reason": reason,
                    }
                )
        round_raw = payload.get("review_round")
        if round_raw is not None:
            if not isinstance(round_raw, int) or isinstance(round_raw, bool) or round_raw < 0:
                raise RecordBuildError("review_round must be a non-negative integer")
            review_round = round_raw
        source = "codex_review_output"
    else:
        source = "records"
    try:
        records = normalize(payload, source)
    except FindingsNormalizeError as exc:
        raise RecordBuildError(str(exc)) from exc
    summary = summarize(records, _resolved_categories())
    record = summary.as_dict()
    truncated = False
    findings = record["findings"]
    if len(findings) > _FINDINGS_COUNT_CAP:
        findings = findings[:_FINDINGS_COUNT_CAP]
        truncated = True
    record["findings"] = findings
    # Shed summary DETAIL before dropping any finding. Carrying the producer's
    # text must never reduce how many findings the gate sees: a truncated
    # array sets findings_truncated, and the gate reads that remainder as
    # BLOCKING with the literal key "(truncated remainder)", which no
    # disposition can clear. Losing a sentence is recoverable; losing a
    # finding into an unclearable blocker is not.
    for limit in (120, 60, 0):
        if len(json.dumps(record, ensure_ascii=False)) <= _RECORD_BYTE_BUDGET:
            break
        for item in findings:
            item["summary"] = shrink_summary(item.get("summary"), limit)
    while len(json.dumps(record, ensure_ascii=False)) > _RECORD_BYTE_BUDGET and findings:
        findings = findings[:-1]
        truncated = True
        record["findings"] = findings
    if truncated:
        record["findings_truncated"] = True
    if dispositions:
        record["dispositions"] = dispositions
    if review_round is not None:
        record["review_round"] = review_round
    return record


@review_app.command("classify", hidden=True)
def classify(
    findings_file: Path = typer.Option(
        ..., "--findings-file", help="JSON findings payload to classify."
    ),
    emit_record: bool = typer.Option(
        False, "--emit-record", help="Print the bounded attestation record as JSON."
    ),
) -> None:
    """Classify a findings payload; the one shell entry point producers share."""
    try:
        text = findings_file.read_text(encoding="utf-8")
    except OSError as exc:
        typer.secho(f"classify: cannot read {findings_file}: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    try:
        payload = json.loads(text)
    except ValueError as exc:
        typer.secho(f"classify: {findings_file} is not valid JSON: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    try:
        record = build_emit_record(payload)
    except RecordBuildError as exc:
        typer.secho(f"classify: {findings_file}: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    if emit_record:
        sys.stdout.write(json.dumps(record, ensure_ascii=False) + "\n")
        return
    typer.echo(
        f"classified: {record['findings_blocking']} blocking, "
        f"{record['findings_nonblocking']} nonblocking"
    )


#: The machine-parseable marker every round comment carries: idempotency key
#: and reader contract in one string, keyed on the FULL attested head.
_ROUND_MARKER = "<!-- fno-review-round head={head} round={round} reviewer={reviewer} -->"


def _render_round_comment(
    record: dict[str, Any], head: str, round_no: int, reviewer: str
) -> str:
    """The record as one human- and machine-readable comment: one line per
    finding, an undisposed finding listed as such (it keeps the PR uncovered,
    rendered where the human reading the PR can see it)."""
    findings = record.get("findings") or []
    dispositions = {
        entry.get("finding_key"): entry
        for entry in (record.get("dispositions") or [])
        if isinstance(entry, dict)
    }
    disposed = sum(1 for f in findings if f.get("finding_key") in dispositions)
    lines = [
        _ROUND_MARKER.format(head=head, round=round_no, reviewer=reviewer),
        f"Review round {round_no} at {head[:7]} ({reviewer}): "
        f"{len(findings)} findings, {disposed} disposed.",
    ]
    for finding in findings:
        key = finding.get("finding_key") or "(unkeyed)"
        entry = dispositions.get(key)
        if entry is None:
            lines.append(f"- {key}: no disposition")
        else:
            lines.append(
                f"- {key}: {entry.get('disposition')} ({entry.get('reason')})"
            )
    return "\n".join(lines)


@review_app.command("post-dispositions", hidden=True)
def post_dispositions(
    findings_file: Path = typer.Option(
        ..., "--findings-file", help="JSON findings payload with dispositions."
    ),
    head: str = typer.Option(
        ..., "--head", help="The full head sha the attestation pinned."
    ),
    reviewer: str = typer.Option(
        "code-review", "--reviewer", help="The reviewer name the round is attributed to."
    ),
    pr: Optional[int] = typer.Option(
        None, "--pr", help="The PR number; resolved from the current branch when omitted."
    ),
) -> None:
    """Post ONE per-round disposition comment on the PR, over REST.

    The comment is the human-visible index of a round's outcomes: one line per
    finding, each naming its disposition or its absence. Idempotent at
    (pr, head) via the marker line; a round with no dispositions posts nothing.
    """
    try:
        payload = json.loads(findings_file.read_text(encoding="utf-8"))
    except OSError as exc:
        typer.secho(f"post-dispositions: cannot read {findings_file}: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    except ValueError as exc:
        typer.secho(f"post-dispositions: {findings_file} is not valid JSON: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    try:
        record = build_emit_record(payload)
    except RecordBuildError as exc:
        typer.secho(f"post-dispositions: {findings_file}: {exc}", err=True)
        raise typer.Exit(code=2) from exc
    dispositions = record.get("dispositions") or []
    if not dispositions:
        typer.echo("post-dispositions: no dispositions in the record; nothing posted")
        return

    # Slug, PR number, and PR head all resolve through the pr REST module -
    # one implementation, not a review-side twin.
    from fno.pr._proc import run
    from fno.pr._rest import (
        _slug_or_reason,
        fetch_pr_info_rest,
        resolve_current_pr_number_rest,
    )

    slug, why = _slug_or_reason(None)
    if not slug:
        typer.secho(f"post-dispositions: {why}", err=True)
        raise typer.Exit(code=3)
    if pr is None:
        # No PR yet: the attestation stands alone, the comment waits for the
        # PR. Not an error - a pre-push emit is a legitimate shape.
        pr, why = resolve_current_pr_number_rest()
        if pr is None:
            typer.echo(f"post-dispositions: no PR to post on ({why}); nothing posted")
            return
    # Head-pin check, the same rule request-self-review applies: the comment
    # describes one head and must never attach to another.
    info, why = fetch_pr_info_rest(str(pr))
    if info is None:
        typer.secho(f"post-dispositions: PR read failed: {why}", err=True)
        raise typer.Exit(code=3)
    pr_head = info.get("head_sha")
    if pr_head and pr_head != head:
        typer.secho(
            f"post-dispositions: refusing: PR {pr} head {str(pr_head)[:9]} is not the "
            f"attested head {head[:9]}; re-request review at the new head",
            err=True,
        )
        raise typer.Exit(code=3)

    # Idempotent at (pr, head): the marker line is the key. A failed read
    # skips the post rather than risking a duplicate: the attestation is the
    # gate evidence, and the comment can be re-posted by the next round's
    # emit - a duplicate is the one outcome this command cannot undo.
    existing = run(["gh", "api", f"repos/{slug}/issues/{pr}/comments?per_page=100"])
    if not existing.ok:
        typer.echo(
            "post-dispositions: comments read failed; nothing posted "
            f"(re-run the emit to retry): {(existing.stderr or '').strip()[:200]}"
        )
        return
    if f"<!-- fno-review-round head={head} " in existing.stdout:
        typer.echo(f"post-dispositions: round comment for {head[:9]} already posted")
        return

    round_no = int(record.get("review_round") or 1)
    body = _render_round_comment(record, head, round_no, reviewer)
    write = run(
        [
            "gh", "api", "-X", "POST", f"repos/{slug}/issues/{pr}/comments",
            "-f", f"body={body}",
        ]
    )
    if not write.ok:
        typer.secho(
            f"post-dispositions: comment post failed: {(write.stderr or '').strip()}",
            err=True,
        )
        raise typer.Exit(code=3)
    typer.echo(f"post-dispositions: posted round comment on PR {pr} at {head[:9]}")


@review_app.command("invocations", hidden=True)
def invocations() -> None:
    """Settle lost review invocations into the attestation ledger.

    A sent invocation with no answering attestation after
    `review.invocation_ttl_minutes` is a dispatch the fleet paid for that
    coverage never saw; each becomes one `lost` attestation row, so the gate
    reads a named refusal instead of waiting on silence. Idempotent: an
    invocation with any answering attestation settles once and never again.
    """
    from fno.review.invocation import invocation_ttl_minutes, settle_lost_invocations

    ttl = invocation_ttl_minutes()
    rows = settle_lost_invocations(ttl_minutes=ttl)
    if not rows:
        typer.echo(f"invocations: none lost beyond the {ttl}m TTL")
        return
    settled = sum(1 for row in rows if row.get("settled"))
    typer.echo(f"invocations: {len(rows)} lost, {settled} settled now")
    for row in rows:
        if row.get("settled"):
            typer.echo(f"  settled {row['invocation_id']} at {str(row.get('head'))[:9]}")
        else:
            typer.echo(f"  refused {row['invocation_id']}: {row.get('reason')}")


@review_app.command("resolve-level", hidden=True)
def resolve_level(
    level: str = typer.Argument(
        None,
        help="Explicit level token (low medium high xhigh max); omit to size from the diff.",
    ),
    provider: str = typer.Option(
        None, "--provider", help="Route provider scope; defaults to the session stamp."
    ),
) -> None:
    """Resolve a review level to (band, effort, model); the seam the skill,
    the tests and the invocation event all call."""
    from fno.review_level import resolve_review_level

    resolution = resolve_review_level(
        level.strip().lower() if level else None,
        provider=provider,
        project_root=Path.cwd(),
    )
    sys.stdout.write(json.dumps(resolution.as_dict(), ensure_ascii=False) + "\n")

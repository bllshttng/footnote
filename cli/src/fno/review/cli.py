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


#: The machine-parseable marker every round comment carries. The head is the
#: FULL sha the attestation pinned, so idempotency and reader parsing key on
#: one string.
_ROUND_MARKER = "<!-- fno-review-round head={head} round={round} reviewer={reviewer} -->"


def _render_round_comment(
    record: dict[str, Any], head: str, round_no: int, reviewer: str
) -> str:
    """The classified record into one human- and machine-readable comment.

    The marker line is the idempotency key and the parser contract; the body
    names each finding's outcome, and a finding with no disposition is listed
    as such - the law's sentence that an undisposed finding keeps the PR
    uncovered, rendered where the human reading the PR can see it.
    """
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


def _gh_api(args: list[str], *, timeout: float = 30.0) -> tuple[int, str, str]:
    """One bounded `gh api` REST call: `(returncode, stdout, stderr)`."""
    import subprocess

    proc = subprocess.run(
        ["gh", "api", *args], capture_output=True, text=True, timeout=timeout
    )
    return proc.returncode, proc.stdout, proc.stderr


def _pr_for_head_branch(slug: str, branch: str) -> tuple[Optional[int], str]:
    """The open PR whose head branch is `branch`, over REST. `(None, reason)`
    on a miss - the caller posts nothing, which is the no-PR posture."""
    owner = slug.split("/")[0]
    rc, out, err = _gh_api(
        [
            "-X", "GET",
            f"repos/{slug}/pulls?head={owner}:{branch}&state=open&per_page=10",
        ]
    )
    if rc != 0:
        return None, (err.strip() or "pull list read failed")
    try:
        rows = json.loads(out)
    except ValueError:
        return None, "pull list returned output that is not JSON"
    if not isinstance(rows, list) or not rows:
        return None, f"no open PR for branch {branch}"
    number = rows[0].get("number")
    return (int(number), "") if isinstance(number, int) else (None, "pull list malformed")


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
    branch: Optional[str] = typer.Option(
        None, "--branch", help="The head branch to resolve the PR from (default: current)."
    ),
) -> None:
    """Post ONE per-round disposition comment on the PR, over REST.

    The comment is the human-visible index of a round's outcomes: one line per
    finding, each naming its disposition or its absence. Idempotent at
    (pr, head): the marker line is the key, and a second post at the same head
    exits 0 having written nothing. A round with no dispositions posts nothing
    - the comment exists to carry outcomes.
    """
    import subprocess
    from pathlib import Path as _Path

    try:
        text = findings_file.read_text(encoding="utf-8")
        payload = json.loads(text)
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

    # The round number rides the record when the producer supplied one.
    round_no = record.get("review_round") or 1

    # The repo identity is LOCAL (a git remote parse), never a network read.
    from fno.paths import repo_identity_from_remote_url

    try:
        remote = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            capture_output=True, text=True, timeout=10, check=True,
        ).stdout.strip()
    except (OSError, subprocess.SubprocessError) as exc:
        typer.secho(f"post-dispositions: no origin remote to resolve the repo: {exc}", err=True)
        raise typer.Exit(code=3) from exc
    identity = repo_identity_from_remote_url(remote)
    # The REST path wants `owner/repo`; the identity carries the host as its
    # first segment, and gh's own auth decides whether it is reachable.
    slug = identity.split("/", 1)[1] if identity and identity.count("/") >= 2 else identity
    if not slug or slug.count("/") != 1:
        typer.secho(f"post-dispositions: origin remote does not name a GitHub repo: {remote}", err=True)
        raise typer.Exit(code=3)

    if pr is None:
        if not branch:
            try:
                branch = subprocess.run(
                    ["git", "rev-parse", "--abbrev-ref", "HEAD"],
                    capture_output=True, text=True, timeout=10, check=True,
                ).stdout.strip()
            except (OSError, subprocess.SubprocessError):
                branch = ""
        if not branch or branch == "HEAD":
            typer.secho("post-dispositions: no PR given and no branch to resolve one from", err=True)
            raise typer.Exit(code=3)
        pr, reason = _pr_for_head_branch(slug, branch)
        if pr is None:
            # No PR yet: the attestation stands alone, the comment waits for
            # the PR. Not an error - a pre-push emit is a legitimate shape.
            typer.echo(f"post-dispositions: no PR to post on ({reason}); nothing posted")
            return

    # Head-pin check, the same rule request-self-review applies: the comment
    # describes one head and must never attach to another.
    rc, out, err = _gh_api(["-X", "GET", f"repos/{slug}/pulls/{pr}"])
    if rc != 0:
        typer.secho(f"post-dispositions: PR read failed: {err.strip()}", err=True)
        raise typer.Exit(code=3)
    try:
        pr_head = (json.loads(out) or {}).get("head", {}).get("sha")
    except ValueError:
        typer.secho("post-dispositions: PR read returned output that is not JSON", err=True)
        raise typer.Exit(code=3)
    if pr_head and pr_head != head:
        typer.secho(
            f"post-dispositions: refusing: PR {pr} head {str(pr_head)[:9]} is not the "
            f"attested head {head[:9]}; re-request review at the new head",
            err=True,
        )
        raise typer.Exit(code=3)

    # Idempotent at (pr, head): the marker line is the key.
    rc, out, _ = _gh_api(
        ["-X", "GET", f"repos/{slug}/issues/{pr}/comments?per_page=100"]
    )
    if rc == 0 and f"<!-- fno-review-round head={head} " in out:
        typer.echo(f"post-dispositions: round comment for {head[:9]} already posted")
        return

    body = _render_round_comment(record, head, int(round_no), reviewer)
    write = subprocess.run(
        ["gh", "api", "-X", "POST", f"repos/{slug}/issues/{pr}/comments", "-f", f"body={body}"],
        capture_output=True, text=True, timeout=30,
    )
    if write.returncode != 0:
        typer.secho(f"post-dispositions: comment post failed: {write.stderr.strip()}", err=True)
        raise typer.Exit(code=3)
    typer.echo(f"post-dispositions: posted round comment on PR {pr} at {head[:9]}")


@review_app.command("invocations", hidden=True)
def invocations(
    action: str = typer.Argument(
        "list",
        help="`list` reports lost invocations; `settle` emits one lost "
        "attestation per unanswered one older than the TTL.",
    ),
) -> None:
    """Report or settle lost review invocations.

    A sent invocation with no answering attestation after
    `review.invocation_ttl_minutes` is a dispatch the fleet paid for that
    coverage never saw. `settle` turns each into one `lost` attestation row,
    so the gate reads a named refusal instead of waiting on silence.
    Idempotent: an invocation with any answering attestation settles once
    and never again.
    """
    from fno.review.invocation import settle_lost_invocations

    if action not in ("list", "settle"):
        typer.secho(f"invocations: unknown action '{action}' (list | settle)", err=True)
        raise typer.Exit(code=2)
    try:
        from fno.config import load_settings

        ttl = int(getattr(load_settings().review, "invocation_ttl_minutes", 15))
    except Exception:  # noqa: BLE001 - unreadable config keeps the shipped default
        ttl = 15
    rows = settle_lost_invocations(ttl_minutes=ttl, emit=action == "settle")
    if not rows:
        typer.echo(f"invocations: none lost beyond the {ttl}m TTL")
        return
    if action == "list":
        for row in rows:
            typer.echo(f"  lost {row['invocation_id']}: {row.get('reason', 'unanswered')}")
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

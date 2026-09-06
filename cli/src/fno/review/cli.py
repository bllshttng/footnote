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
import os
import subprocess
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

#: The record keys an emitted attestation carries beside its own fields: the
#: same projection emit-attestation.sh merges onto the event, so a row written
#: here and a row written there read identically downstream.
_ATTEST_RECORD_KEYS = (
    "findings_blocking",
    "findings_nonblocking",
    "findings",
    "findings_truncated",
    "review_round",
    "dispositions",
)


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


def _git_out(*args: str) -> str:
    try:
        proc = subprocess.run(
            ["git", *args], capture_output=True, text=True, timeout=10
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    return proc.stdout.strip() if proc.returncode == 0 else ""


def _attest_from_record(
    record: dict[str, Any],
    reviewer: str,
    reviewer_context: str,
    findings_file: Path,
    execution_context: str = "inline",
    output_contract: str = "json_block",
) -> str:
    """Write the ``review_attestation`` row for the record classify just built.

    The verdict is MEASURED, never typed: pass only when the classifier
    counted zero blocking findings, so the row cannot be milder than what the
    review produced. Returns the verdict for the receipt. Every refusal exit
    names the missing input, and nothing is emitted on any of them.

    The emitted row carries the same fields the shell producer writes, the
    reviewed ranges included: a row without ``reviewed_base_sha`` contributes
    no tile to the coverage chain, so a lane attest that skipped the
    measurement would never tile and never reach covered below the cap.
    """
    verdict = "pass" if record.get("findings_blocking", 0) == 0 else "fail"
    if output_contract == "prose_unparseable":
        # An unreadable answer must never read as a pass, whatever an empty
        # findings array classifies to: the array cannot represent what the
        # reviewer said, so the contract itself withholds the pass.
        verdict = "fail"

    from fno.review.invocation import _settle_head_pin

    head_sha, branch = _settle_head_pin(Path.cwd())
    if not head_sha:
        typer.secho(
            "classify --attest: no readable head to pin (not a git repo or detached "
            "HEAD); no event emitted",
            err=True,
        )
        raise typer.Exit(code=3)

    base = _git_out("symbolic-ref", "--short", "refs/remotes/origin/HEAD")
    base = base.rsplit("/", 1)[-1] if base else "main"
    reviewed_base = _git_out("merge-base", "HEAD", f"origin/{base}") or _git_out(
        "merge-base", "HEAD", base
    )
    if not reviewed_base:
        typer.secho(
            f"classify --attest: cannot resolve the base branch '{base}' to a "
            "merge-base; the diff under review is unmeasurable, no event emitted",
            err=True,
        )
        raise typer.Exit(code=3)
    span = f"{reviewed_base}..HEAD"
    file_count = len(
        [ln for ln in _git_out("diff", "--name-only", span).splitlines() if ln.strip()]
    )
    # Files, not lines, decide emptiness: numstat prints "-" for binaries and
    # 0 for renames, so a binary-only diff is a real diff with an honest 0.
    line_count = 0
    for line in _git_out("diff", "--numstat", span).splitlines():
        parts = line.split("\t")
        if len(parts) >= 2 and parts[0].isdigit() and parts[1].isdigit():
            line_count += int(parts[0]) + int(parts[1])
    if file_count == 0:
        typer.secho(
            f"classify --attest: the diff under review is empty ({reviewed_base[:9]}.."
            f"{head_sha[:9]}); a review with nothing to read is not a pass, "
            "no event emitted",
            err=True,
        )
        raise typer.Exit(code=3)

    # session_id/harness/harness_session_id: the same manifest grep the
    # retraction verb performs; a bare-shell attest leaves them empty and the
    # row says so.
    session_id = ""
    harness = ""
    harness_session_id = ""
    try:
        from fno.paths import resolve_repo_root

        manifest = resolve_repo_root() / ".fno" / "target-state.md"
        if manifest.is_file():
            for line in manifest.read_text(encoding="utf-8").splitlines():
                stripped = line.strip()
                for key in ("session_id", "harness", "harness_session_id"):
                    prefix = f"{key}:"
                    if stripped.startswith(prefix):
                        value = stripped[len(prefix) :].strip()
                        if value and value != "null":
                            if key == "session_id":
                                session_id = value
                            elif key == "harness_session_id":
                                harness_session_id = value
                            else:
                                harness = value
    except OSError:
        pass

    # The invocation join, the shell producer's preference order: the live
    # hold's metadata first, then the session sidecar, then UNJOINED.
    invocation_id = ""
    try:
        from fno.claims.core import claim_status
        from fno.pr._review_hold import review_hold_key

        status = claim_status(review_hold_key(branch)) or {}
        invocation_id = str((status.get("metadata") or {}).get("invocation_id") or "")
    except Exception:  # noqa: BLE001 - claims-root resolution shells out and can fail
        invocation_id = ""
    if not invocation_id and harness_session_id:
        sidecar = (
            Path(os.environ.get("FNO_HOME", str(Path.home() / ".fno")))
            / "review-invocations"
            / f"{harness_session_id}.json"
        )
        try:
            invocation_id = str(
                json.loads(sidecar.read_text(encoding="utf-8")).get("invocation_id")
                or ""
            )
        except (OSError, ValueError):
            invocation_id = ""
    if not invocation_id:
        invocation_id = "UNJOINED"

    from fno.events import ValidationError, _build, append_event
    from fno.events.cli import mirror_to_global_log
    from fno.harness_identity import AttesterIdentityConflict, resolve_attester_identity
    from fno.paths import project_log, resolve_repo_root

    try:
        resolved_id, witness = resolve_attester_identity()
    except AttesterIdentityConflict as exc:
        typer.secho(
            f"classify --attest: {exc}. No event emitted under an identity the "
            "caller overrode.",
            err=True,
        )
        raise typer.Exit(code=1) from exc

    data: dict[str, Any] = {
        "reviewer": reviewer.lstrip("/"),
        "head_sha": head_sha,
        "verdict": verdict,
        "session_id": session_id,
        "branch": branch,
        "reviewer_context": reviewer_context,
        "execution_context": execution_context,
        "output_contract": output_contract,
        "attester_session_id": resolved_id,
        "attester_witness": witness,
        "invocation_id": invocation_id,
        "reviewed_base_sha": reviewed_base,
        "reviewed_head_sha": head_sha,
        "reviewed_line_count": line_count,
        "reviewed_file_count": file_count,
    }
    if harness:
        data["harness"] = harness
    for key in _ATTEST_RECORD_KEYS:
        if record.get(key) is not None:
            data[key] = record[key]

    repo_root = resolve_repo_root()
    events_path = project_log("events.jsonl", project_root=repo_root)
    try:
        event = _build("review_attestation", "target" if session_id else "test", data)
    except ValidationError as exc:
        typer.secho(f"classify --attest: {exc}", err=True)
        raise typer.Exit(code=3) from exc
    try:
        append_event(event, events_path=events_path)
    except Exception as exc:  # noqa: BLE001 - surface the append failure verbatim
        typer.secho(f"classify --attest: failed to append event: {exc}", err=True)
        raise typer.Exit(code=3) from exc
    try:
        mirror_to_global_log(event, events_path, repo_root)
    except Exception:  # noqa: BLE001 - the project append already stands
        pass

    # The per-round comment is the row's human-visible index. Best-effort and
    # idempotent at (pr, head); a repo with no PR yet posts nothing.
    if record.get("dispositions"):
        try:
            post_dispositions(
                findings_file=findings_file, head=head_sha, reviewer=data["reviewer"]
            )
        except Exception:  # noqa: BLE001 - the attestation is the gate evidence
            pass

    typer.secho(
        f"review_attestation emitted: reviewer={data['reviewer']} "
        f"head_sha={head_sha[:8]} branch={branch} verdict={verdict} "
        f"session={session_id or 'none'} attester={resolved_id or 'unattributed'} "
        f"lines={line_count} files={file_count}",
        err=True,
    )
    return verdict


@review_app.command("classify", hidden=True)
def classify(
    findings_file: Path = typer.Option(
        ..., "--findings-file", help="JSON findings payload to classify."
    ),
    emit_record: bool = typer.Option(
        False, "--emit-record", help="Print the bounded attestation record as JSON."
    ),
    attest: str = typer.Option(
        "",
        "--attest",
        help="Reviewer name: also write the head-pinned review_attestation row, "
        "with the verdict the classifier measured (pass only on zero blocking "
        "findings). Empty: classify only, no event.",
    ),
    reviewer_context: str = typer.Option(
        "unknown",
        "--reviewer-context",
        help="Positive context evidence carried on the attested row "
        "(fresh | shared | unknown).",
    ),
    execution_context: str = typer.Option(
        "inline",
        "--execution-context",
        help="Where the review ran (inline | fork).",
    ),
    output_contract: str = typer.Option(
        "json_block",
        "--output-contract",
        help="The contract the review's result surfaced under "
        "(json_block | report_findings | prose_unparseable); prose_unparseable "
        "always rides a fail verdict.",
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
    if attest:
        if not attest.strip():
            typer.secho(
                "classify: --attest needs a reviewer name; no event emitted", err=True
            )
            raise typer.Exit(code=2)
        if reviewer_context not in ("fresh", "shared", "unknown"):
            typer.secho(
                "classify: --reviewer-context must be fresh, shared, or unknown "
                f"(got '{reviewer_context}')",
                err=True,
            )
            raise typer.Exit(code=2)
        if execution_context not in ("inline", "fork"):
            typer.secho(
                "classify: --execution-context must be inline or fork "
                f"(got '{execution_context}')",
                err=True,
            )
            raise typer.Exit(code=2)
        if output_contract not in ("json_block", "report_findings", "prose_unparseable"):
            typer.secho(
                "classify: --output-contract must be json_block, report_findings, "
                f"or prose_unparseable (got '{output_contract}')",
                err=True,
            )
            raise typer.Exit(code=2)
        _attest_from_record(
            record,
            attest.strip(),
            reviewer_context,
            findings_file,
            execution_context=execution_context,
            output_contract=output_contract,
        )
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
        None, "--pr-number", "--pr",
        help="The PR number; resolved from the current branch when omitted.",
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

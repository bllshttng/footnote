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

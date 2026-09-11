"""``fno backlog note``: append + deliver a progress note; registered here so the file-budget gate keeps ``graph/cli.py`` shrinking."""
from __future__ import annotations

import json
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import typer

from fno.decide import READ_HELP
from fno.graph import cli as graph_cli


# Route the graph path through graph_cli's namespace so the tests' existing
# `monkeypatch.setattr("fno.graph.cli._graph_path", ...)` seam keeps working.
@graph_cli.cli.command("note")
def cmd_note(
    task_id: str = typer.Argument(..., help="Node id to append a progress note to."),
    text: Optional[str] = typer.Argument(None, help="Progress note text (one line)."),
    body_file: Optional[Path] = typer.Option(
        None,
        "--body-file",
        help="Read the note text from a file ('-' = stdin). Same length guidance applies.",
    ),
    quiet: bool = typer.Option(
        False, "--quiet", "-q", help="Annotate silently: write it, mail nobody."
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the appended note as JSON."),
    read: list[str] = typer.Option([], "--read", help=READ_HELP),
) -> None:
    """Append a timestamped progress note to a backlog node, and DELIVER it.

    Delivery is the DEFAULT: a worker reads its node once, at dispatch, so the
    verb mails a pointer to the node's holder, the owner's holder and the epic's
    king. ``--quiet`` is the deliberate silent annotation. Contract, and why the
    fanout's own stamps never mail: docs/architecture/backlog-graph-verb-contracts.md.
    """
    from fno.decide import (
        UnmeasuredClaimError,
        UnresolvableCitationError,
        note_evidence,
        unmeasured_note_warning,
        warn_if_note_is_long,
    )
    from fno.graph.store import append_progress_note
    from fno.claims.self_identity import resolve_self_identity
    from fno.rust_binary import VerbUnavailable
    from fno.text_or_file import read_text_arg

    text = (read_text_arg(text, body_file, what="the note text") or "").strip()
    if not text:
        typer.echo("Error: note text is empty", err=True)
        raise typer.Exit(code=1)

    # A contradicted citation refuses BEFORE the append; an unmeasured claim
    # only warns (this verb advises, never refuses a body).
    try:
        read_rows, claims = note_evidence(text, list(read))
    except (UnresolvableCitationError, UnmeasuredClaimError, VerbUnavailable) as exc:
        typer.echo(f"Error: note refused: {exc}", err=True)
        raise typer.Exit(code=1)

    note: "dict[str, Any]" = {"ts": datetime.now(timezone.utc).isoformat(), "text": text}
    if read_rows:
        note["reads"] = read_rows
    try:
        identity = resolve_self_identity()
    except Exception:  # noqa: BLE001 - an unprovable identity must not lose the note
        identity = None
    if identity is not None and identity.session_id:
        note["source_session_id"] = identity.session_id
    if identity is not None and identity.harness:
        note["source_harness"] = identity.harness
    entries: list[dict] = []
    found, _ = append_progress_note(graph_cli._graph_path(), task_id, note, entries_out=entries)
    if not found:
        typer.echo(f"Error: no node resolves to '{task_id}'", err=True)
        raise typer.Exit(code=1)
    warn_if_note_is_long(text)
    if claims:
        typer.echo(unmeasured_note_warning(claims), err=True)
    if json_output:
        typer.echo(json.dumps({"id": task_id, "note": note}, separators=(",", ":")))
    else:
        typer.echo(f"noted {task_id}: {text}")
    if not quiet:
        try:
            from fno.backlog.note_notify import deliver_note

            receipts = deliver_note(task_id, text, graph_cli._graph_path(), entries or None)
        except Exception as exc:  # noqa: BLE001 - the note is written, delivery is not
            receipts = [(f"notify FAILED {task_id}: {exc}", True)]
        for line, undelivered in receipts:
            typer.echo(line, err=undelivered or json_output)

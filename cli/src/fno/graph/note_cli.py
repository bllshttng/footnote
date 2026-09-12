"""``fno backlog note``: append + deliver a progress note; registered here so the file-budget gate keeps ``graph/cli.py`` shrinking."""
from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

import typer

from fno.decide import READ_HELP
from fno.graph import cli as graph_cli
from fno.graph.cli import cli


# Registered through graph_cli's namespace so the tests' existing
# `monkeypatch.setattr("fno.graph.cli._graph_path", ...)` seam keeps working.
@cli.command("note")
def cmd_note(
    task_id: str = typer.Argument(..., help="Node id to append a progress note to."),
    text: Optional[str] = typer.Argument(None, help="Progress note text (one line)."),
    body_file: Optional[Path] = typer.Option(
        None,
        "--body-file",
        help="Read the note text from a file ('-' = stdin). Same length guidance applies.",
    ),
    quiet: bool = typer.Option(
        False,
        "--quiet",
        "-q",
        help="Annotate silently: write it, mail nobody. The acknowledgment "
        "when the verb would otherwise refuse.",
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the appended note as JSON."),
    read: list[str] = typer.Option([], "--read", help=READ_HELP),
) -> None:
    """Append a timestamped progress note to a backlog node, and DELIVER it.

    Delivery is the DEFAULT: a worker reads its node once, at dispatch, so the
    verb mails a pointer to every bound reader (claim, graph session, registry
    workers, then the crown walk out to the project). When nobody bound would
    be told, the verb REFUSES before the append and exits 3 with nothing
    written; when no send confirms, it exits 4 with the note written.
    ``--quiet`` writes it anyway and mails nobody. Contract:
    docs/architecture/backlog-graph-verb-contracts.md.
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
    from fno.backlog.note_notify import Refused, readers_before_append

    # Refuse BEFORE the append: a note nobody bound would hear is a silent
    # drop wearing a receipt, so it costs the write instead.
    readers = None
    if not quiet:
        resolved = readers_before_append(task_id, graph_cli._graph_path())
        if isinstance(resolved, Refused):
            typer.echo(resolved.message, err=True)
            raise typer.Exit(code=resolved.exit_code)
        readers = resolved
    found, _ = append_progress_note(
        graph_cli._graph_path(), readers.node_id if readers is not None else task_id, note
    )
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
    if readers is not None:
        from fno.backlog.note_notify import deliver

        raise typer.Exit(code=deliver(readers, text, json_output=json_output))

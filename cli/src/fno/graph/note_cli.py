"""``fno backlog note``: the Rust note action's public bridge (x-920a).

The native action owns state policy, history routing, and the budget; this
bridge keeps the recipient walk, evidence checks, identity, archived
refusal, and mail transport - each owned by exactly one implementation.
"""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional

import typer

from fno.decide import READ_HELP
from fno.graph import cli as graph_cli
from fno.graph.cli import cli


# Registered through graph_cli's namespace so the tests' existing
# `monkeypatch.setattr("fno.graph.cli._graph_path", ...)` seam keeps working.
@cli.command("note")
def cmd_note(
    task_id: str = typer.Argument(..., help="Node id (or slug) whose current state the note replaces."),
    text: Optional[str] = typer.Argument(None, help="The note body (replaces current state)."),
    body_file: Optional[Path] = typer.Option(
        None,
        "--body-file",
        help="Read the note text from a file ('-' = stdin). Same length guidance applies.",
    ),
    quiet: bool = typer.Option(
        False, "--quiet", "-q",
        help="Write it, mail nobody: the acknowledgment when the verb would refuse.",
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the state receipt as JSON."),
    read: list[str] = typer.Option([], "--read", help=READ_HELP),
) -> None:
    """Record progress on a node by REPLACING its current state.

    The exact prior state lands in permanent history. Nobody bound refuses
    BEFORE the write: exit 3, nothing written. No send confirmed: exit 4,
    note written. ``--quiet`` writes it anyway.
    """
    from fno.decide import (
        UnmeasuredClaimError,
        UnresolvableCitationError,
        note_evidence,
        unmeasured_note_warning,
        warn_if_note_is_long,
    )
    from fno.claims.self_identity import resolve_self_identity
    from fno.text_or_file import read_text_arg

    text = (read_text_arg(text, body_file, what="the note text") or "").strip()
    if not text:
        typer.echo("Error: note text is empty", err=True)
        raise typer.Exit(code=1)

    # A contradicted citation refuses BEFORE the write; an unmeasured claim
    # only warns (this verb advises, never refuses a body).
    try:
        read_rows, claims = note_evidence(text, list(read))
    except (UnresolvableCitationError, UnmeasuredClaimError) as exc:
        typer.echo(f"Error: note refused: {exc}", err=True)
        raise typer.Exit(code=1)

    try:
        identity = resolve_self_identity()
    except Exception:  # noqa: BLE001 - an unprovable identity must not lose the note
        identity = None
    session_id = identity.session_id if identity is not None and identity.session_id else None

    graph_path = graph_cli._graph_path()

    # Archived refusal BEFORE the write, exact PR 1871 remedy (AC16); quiet
    # mode never bypasses it (it guards the write, not the delivery).
    from fno.graph._archive_lookup import refuse_update_if_archived

    if refuse_update_if_archived(task_id):
        raise typer.Exit(code=1)

    # Refuse BEFORE the write: an unread note is a silent drop wearing a
    # receipt. The shipped walk answers for the non-quiet path.
    from fno.backlog.note_notify import Refused, NoteReaders, deliver, readers_before_append

    resolved = None if quiet else readers_before_append(task_id, graph_path)
    if isinstance(resolved, Refused):
        typer.echo(resolved.message, err=True)
        raise typer.Exit(code=resolved.exit_code)
    readers = resolved

    node_target = readers.node_id if readers is not None else task_id
    code, receipt = _write_state(
        node_target,
        text,
        quiet=quiet,
        session_id=session_id,
        graph_path=graph_path,
        reads=read_rows,
    )
    if code != 0:
        # 1 = budget/history refusal, 3 = a stale revision conflict. The
        # child printed the reason on stderr.
        raise typer.Exit(code=code)

    if claims:
        typer.echo(unmeasured_note_warning(claims), err=True)

    if receipt is None:
        typer.echo("Error: the note action returned no receipt", err=True)
        raise typer.Exit(code=1)
    if json_output:
        note = {
            "id": receipt.get("node_id") or task_id,
            "text": text,
            "revision": receipt.get("revision"),
            "routed": receipt.get("routed"),
        }
        typer.echo(json.dumps(note, separators=(",", ":")))
    else:
        typer.echo(f"noted {receipt.get('node_id') or task_id}: {text}")
    warn_if_note_is_long(text)
    # Terminal-routed notes delivered too: the write went to history, but the
    # bound readers are still the people to tell.
    if not isinstance(readers, NoteReaders):
        return
    raise typer.Exit(code=deliver(readers, text, json_output=json_output))


def _receipt(stdout: str) -> Optional[dict]:
    """The one JSON receipt line the native action prints on stdout."""
    for line in reversed((stdout or "").strip().splitlines()):
        if line.startswith("{"):
            try:
                return json.loads(line)
            except ValueError:
                continue
    return None


def _write_state(
    node_id: str,
    text: str,
    *,
    quiet: bool,
    session_id: Optional[str],
    graph_path,
    reads=None,
) -> "tuple[int, Optional[dict]]":
    """One native `backlog-note` invocation. Returns `(exit, receipt)`; the
    receipt is parsed from the child's stdout when the exit is 0."""
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo("Error: the fno-agents binary is required for `fno backlog note`", err=True)
        raise typer.Exit(code=1)
    argv = [str(binary), "backlog-note", "--graph", str(graph_path), "--stdin",
            "--json", "--node", node_id]
    if reads:
        argv.extend(["--reads", json.dumps(reads, separators=(",", ":"))])
    if session_id:
        argv.append("--self-session")
        argv.append(session_id)
    if quiet:
        argv.append("--quiet")
    proc = subprocess.run(argv, input=text, text=True, check=False, capture_output=True)
    if proc.returncode != 0:
        import sys

        sys.stderr.write(proc.stderr or "")
        return proc.returncode, None
    return 0, _receipt(proc.stdout)

"""``fno backlog note``: the Rust note action's public bridge.

The native action owns state policy, history routing, and the budget; this
bridge keeps the recipient walk, evidence checks, identity, and transport.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Optional

import typer

from fno.decide import READ_HELP
from fno.graph import cli as graph_cli
from fno.graph.cli import cli


# Registered through graph_cli's namespace so the tests' existing
# `monkeypatch.setattr("fno.graph.cli._graph_path", ...)` seam keeps working.
@cli.command("note", context_settings={"allow_extra_args": True, "ignore_unknown_options": True})
def cmd_note(
    ctx: typer.Context,
    task_id: Optional[str] = typer.Argument(None, help="Node id (or slug) whose current state the note replaces."),
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

    The prior state lands in permanent history. Read it with
    `fno backlog notes history <id>`. Nobody bound refuses BEFORE
    the write: exit 3. No send confirmed: exit 4. ``--quiet`` writes anyway.
    """
    from fno.decide import (
        UnmeasuredClaimError,
        UnresolvableCitationError,
        note_evidence,
        unmeasured_note_warning,
        warn_if_note_is_long,
    )
    from fno.text_or_file import read_text_arg

    # Finding routes ride ctx.args to the native action unparsed (law
    # d-b6cc1a2a: the bridge adds no flags; Rust owns the vocabulary). Only
    # routing reads them here.
    extra = list(ctx.args)
    resolve_id = None
    if "--resolve" in extra:
        i = extra.index("--resolve")
        resolve_id = extra[i + 1] if i + 1 < len(extra) else ""
    blocking = "--blocking" in extra
    if resolve_id is None and not task_id:
        typer.echo("Error: a node id is required (or pass --resolve <finding-id>)", err=True)
        raise typer.Exit(code=2)

    graph_path = graph_cli._graph_path()
    if resolve_id is not None:
        code, receipt = _write_resolve(resolve_id, session_id=_session_id(), graph_path=graph_path)
        if code != 0:
            raise typer.Exit(code=code)
        _echo_receipt(json.dumps(receipt, separators=(",", ":")) if json_output else receipt.get("line", ""))
        return

    text = (read_text_arg(text, body_file, what="the note text") or "").strip()
    # An empty body refuses in the native action, which owns the message.

    # A contradicted citation refuses BEFORE the write; an unmeasured claim
    # only warns (this verb advises, never refuses a body).
    try:
        read_rows, claims = note_evidence(text, list(read))
    except (UnresolvableCitationError, UnmeasuredClaimError) as exc:
        typer.echo(f"Error: note refused: {exc}", err=True)
        raise typer.Exit(code=1)

    session_id = _session_id()

    # Archived refusal BEFORE the write, exact PR 1871 remedy (AC16); quiet
    # mode never bypasses it (it guards the write, not the delivery). Live
    # first, then the archive: a live id is live whatever the archive holds
    # (the same order update, reopen, and unarchive take).
    from fno.graph._archive_lookup import refuse_update_if_archived
    from fno.graph._intake import _find_node
    from fno.graph.api import wire_rows

    try:
        live = _find_node(wire_rows(path=graph_path), task_id)
    except Exception:  # noqa: BLE001 - an unreadable store is the write path's error to report
        live = None
    if live is None and refuse_update_if_archived(task_id):
        raise typer.Exit(code=1)

    # Refuse BEFORE the write: an unread note is a silent drop wearing a
    # receipt. The shipped walk answers for the non-quiet path.
    from fno.backlog.note_notify import (
        Refused,
        NoteReaders,
        deliver,
        deliver_finding,
        readers_before_append,
    )

    resolved = None if quiet else readers_before_append(task_id, graph_path)
    if isinstance(resolved, Refused):
        # A blocking finding skips only the nobody-bound refusal: with no
        # live reader it still writes and gates the next worker.
        if not (blocking and resolved.exit_code == 3):
            typer.echo(resolved.message, err=True)
            raise typer.Exit(code=resolved.exit_code)
        readers = None
    else:
        readers = resolved

    node_target = readers.node_id if readers is not None else task_id
    code, receipt = _write_state(
        node_target,
        text,
        quiet=quiet,
        session_id=session_id,
        graph_path=graph_path,
        reads=read_rows,
        extra=extra,
    )
    if code != 0:
        # 1 = budget refusal, 3 = a stale revision conflict; the child
        # printed the reason on stderr.
        raise typer.Exit(code=code)

    if claims:
        typer.echo(unmeasured_note_warning(claims), err=True)

    if receipt is None:
        typer.echo("Error: the note action returned no receipt", err=True)
        raise typer.Exit(code=1)
    shown = json.dumps(receipt, separators=(",", ":")) if json_output else receipt.get("line", "")
    _echo_receipt(shown)
    if receipt.get("routed") == "finding" and isinstance(readers, NoteReaders):
        raise typer.Exit(
            code=deliver_finding(readers, str(receipt.get("finding_id")), json_output=json_output)
        )
    warn_if_note_is_long(text)
    # Terminal-routed notes delivered too: the write went to history, but the
    # bound readers are still the people to tell.
    if not isinstance(readers, NoteReaders):
        return
    raise typer.Exit(code=deliver(readers, text, json_output=json_output))


def _session_id() -> Optional[str]:
    """The caller's claimed session id, or None when unprovable."""
    from fno.claims.self_identity import resolve_self_identity

    try:
        identity = resolve_self_identity()
    except Exception:  # noqa: BLE001 - an unprovable identity must not lose the note
        return None
    return identity.session_id if identity is not None and identity.session_id else None


def _write_resolve(
    finding_id: str,
    *,
    session_id: Optional[str],
    graph_path,
) -> "tuple[int, Optional[dict]]":
    """One native `backlog-note --resolve` invocation: no node, no body."""
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo("Error: the fno-agents binary is required for `fno backlog note`", err=True)
        raise typer.Exit(code=1)
    argv = [str(binary), "backlog-note", "--graph", str(graph_path), "--json", "--resolve", finding_id]
    if session_id:
        argv += ["--self-session", session_id]
    proc = subprocess.run(argv, text=True, check=False, capture_output=True)
    if proc.returncode != 0:
        import sys

        sys.stderr.write(proc.stderr or "")
        return proc.returncode, None
    return 0, _receipt(proc.stdout)


def _echo_receipt(line: str) -> None:
    """Print the note receipt, tolerating a reader that closed the pipe.
    The store write has landed by the time this runs; a display failure
    must not turn a landed note into a reported failure."""
    try:
        typer.echo(line)
        sys.stdout.flush()
    except BrokenPipeError:
        try:
            devnull = os.open(os.devnull, os.O_WRONLY)
            os.dup2(devnull, sys.stdout.fileno())
        except OSError:
            pass


def _receipt(stdout: str) -> Optional[dict]:
    for line in reversed((stdout or "").strip().splitlines()):
        if line.startswith("{"):
            try:
                return json.loads(line)
            except ValueError:
                continue
    return None


def native_update(
    node_id: str,
    args: list[str],
    *,
    graph_path,
    json_out: bool = True,
) -> "tuple[int, Optional[dict]]":
    """One native `backlog-update` invocation : the patch door.

    Returns `(exit, receipt)`; the receipt is parsed from the child's stdout
    when `json_out` and the exit is 0. Without `json_out` the child's text
    receipt streams to the caller's stdout verbatim. The child's stderr is
    relayed either way - a refusal line is the answer, never a detail.
    """
    import sys

    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo("Error: the fno-agents binary is required for `fno backlog update`", err=True)
        raise typer.Exit(code=1)
    argv = [str(binary), "backlog-update", "--graph", str(graph_path), "--node", node_id]
    if json_out:
        argv.append("--json")
    argv.extend(args)
    proc = subprocess.run(argv, text=True, check=False, capture_output=True)
    if json_out:
        if proc.returncode != 0:
            sys.stderr.write(proc.stderr or "")
            return proc.returncode, None
        return 0, _receipt(proc.stdout)
    if proc.stdout:
        typer.echo(proc.stdout.rstrip("\n"))
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr or "")
    return proc.returncode, None


def _write_state(
    node_id: str,
    text: str,
    *,
    quiet: bool,
    session_id: Optional[str],
    graph_path,
    reads=None,
    extra: Optional[list[str]] = None,
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
    argv.extend(extra or [])
    if reads:
        argv.extend(["--reads", json.dumps(reads, separators=(",", ":"))])
    if session_id:
        argv.extend(["--self-session", session_id])
    if quiet:
        argv.append("--quiet")
    proc = subprocess.run(argv, input=text, text=True, check=False, capture_output=True)
    if proc.returncode != 0:
        import sys

        sys.stderr.write(proc.stderr or "")
        return proc.returncode, None
    return 0, _receipt(proc.stdout)


@cli.command(
    "notes",
    hidden=True,  # the advertised backlog menu caps at 12; `fno backlog note` help names this reader
    context_settings={"allow_extra_args": True, "ignore_unknown_options": True},
    add_help_option=False,
)
def cmd_notes(ctx: typer.Context) -> None:
    """Read the note history the note verb archives (passthrough to the Rust reader)."""
    from fno._subprocess_util import propagate_returncode
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo(
            "fno backlog notes: the fno-agents binary was not found. Reinstall fno, run "
            "`fno doctor update --rust`, or set FNO_AGENTS_BIN.",
            err=True,
        )
        raise typer.Exit(code=2)
    proc = subprocess.run([str(binary), "backlog-notes", *ctx.args], check=False)
    raise typer.Exit(code=propagate_returncode(proc.returncode))

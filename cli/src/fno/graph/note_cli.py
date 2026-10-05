"""``fno backlog note``: the Rust note action's public bridge.

The native action owns the note feed (every note appends a comment
row to the node's thread, stamped with the writer's identity); this bridge
keeps the recipient walk, evidence checks, identity, and transport.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Optional

import typer

from fno.graph import cli as graph_cli
from fno.graph.cli import cli


# Registered through graph_cli's namespace so the tests' existing
# `monkeypatch.setattr("fno.graph.cli._graph_path", ...)` seam keeps working.
READ_HELP = (
    "The command that produced a code fact in this ruling. It is RUN at "
    "record time and its output stored on the row. Repeatable; pair a zero "
    "with a control: a second read aimed at something known to be present."
)


class UnresolvableCitationError(ValueError):
    """A citation the repo contradicts."""


class UnmeasuredClaimError(ValueError):
    """A code fact stated with no read attached."""


def _note_gate_error(answer):
    kind = answer.get("kind")
    message = answer.get("message") or "the evidence gate refused without a reason"
    if kind == "citation":
        return UnresolvableCitationError(message)
    return UnmeasuredClaimError(message)


def note_evidence(text, reads):
    """Note-lane gate: (rows, claims), each None when not applicable.

    A contradicted citation RAISES (a note is a fact on the node even when
    --quiet); a claim with no read only reports - the note verb advises,
    never refuses a body.
    """
    from fno.paths import resolve_repo_root
    from fno.rust_binary import verb_call

    answer = verb_call(
        "evidence-gate",
        {
            "lane": "note",
            "text": text,
            "reads": list(reads) if reads else None,
            "root": str(resolve_repo_root()),
            "timeout": 20,
        },
        timeout=120,
    )
    if not answer.get("ok"):
        raise _note_gate_error(answer)
    return answer.get("rows") or None, answer.get("claims") or None


def unmeasured_note_warning(claims):
    return (
        f"note appended with an unmeasured code fact ('{claims[0]}'): a reader "
        "cannot tell measured from assumed. Attach --read <command> - it runs "
        "at record time and its output is stored; pair a zero with a control."
    )


def warn_if_note_is_long(text, *, stream=None):
    """Advise on a long note, never refuse one.

    Why uncapped, and why the blunt multiplier:
    docs/architecture/backlog-graph-verb-contracts.md.
    """
    import sys

    from fno import rust_binary

    try:
        from fno.config import load_settings

        cap = load_settings().style.word_cap.encounter
    except Exception:  # noqa: BLE001 - an advisory must never break a write
        return
    count = rust_binary.style_word_count(text)
    if count <= cap * 4:
        return
    print(
        f"note appended ({count} words). Long evidence belongs in a plan doc; "
        "a note carrying a path is cheaper for every later reader.",
        file=stream or sys.stderr,
    )


@cli.command("note", context_settings={"allow_extra_args": True, "ignore_unknown_options": True})
def cmd_note(
    ctx: typer.Context,
    task_id: Optional[str] = typer.Argument(None, help="Node id (or slug) the note appends to."),
    text: Optional[str] = typer.Argument(None, help="The note body (appended to the node's thread)."),
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
    """Append a note to the node's thread (one feed, oldest first).

    A note is a comment row in the node's thread, stamped with this
    session's identity, never a replacement of current_state. ``--kind``
    names the feed kind (progress, finding, ruling, collision; default
    progress). Read the feed with `fno backlog note comment <id>
    --list`; the newest row is the live reading. Nobody bound refuses
    BEFORE the write: exit 3. No send confirmed: exit 4. ``--quiet``
    writes anyway.
    """
    from fno.claims.self_identity import resolve_self_identity
    from fno.text_or_file import read_text_arg

    extra = list(ctx.args)
    # --kind (the feed kind) rides the passthrough: the Python flag surface
    # is shrink-only (the registry ratchet), so the flag is read out of
    # extra rather than declared as an option parameter.
    kind = None
    if "--kind" in extra:
        at = extra.index("--kind")
        kind = extra[at + 1] if at + 1 < len(extra) else None
    graph_path = graph_cli._graph_path()
    # The comment thread owns its word on both entries. The native router
    # wants `comment` as the first tail word, ahead of --graph, so this
    # forward keeps that order: the receipts' own suggested command
    # (`fno backlog note comment <id> --list`) must run here too.
    if task_id == "comment":
        from fno.rust_binary import resolve_binary

        binary = resolve_binary()
        if binary is None:
            typer.echo("Error: the fno-agents binary is required for `fno backlog note`", err=True)
            raise typer.Exit(code=1)
        argv = [str(binary), "backlog", "note", "comment", "--graph", str(graph_path)]
        if json_output:
            argv.append("--json")
        if text:
            argv.append(text)
        argv += extra
        proc = subprocess.run(argv, check=False)
        raise typer.Exit(code=proc.returncode)
    if not task_id or "--blocking" in extra or "--resolve" in extra:
        from fno.rust_binary import resolve_binary

        binary = resolve_binary()
        if binary is None:
            typer.echo("Error: the fno-agents binary is required for `fno backlog note`", err=True)
            raise typer.Exit(code=1)
        argv = [str(binary), "backlog", "note", "--graph", str(graph_path)]
        if json_output:
            argv.append("--json")
        if body_file:
            argv += ["--body-file", str(body_file)]
        argv += [a for a in (task_id, text) if a]
        argv += extra
        proc = subprocess.run(argv, check=False)
        raise typer.Exit(code=proc.returncode)

    # --replace is retired, not swallowed: a note appends and cannot
    # clobber, so the flag would silently do nothing on this route.
    if "--replace" in extra:
        typer.echo(
            "--replace is retired: a note appends to the thread and cannot "
            f"clobber anything. Read the feed: fno backlog note comment {task_id} --list",
            err=True,
        )
        raise typer.Exit(code=3)

    text = (read_text_arg(text, body_file, what="the note text") or "").strip()
    # An empty body refuses in the native action, which owns the message.

    # A body that looks like a flag is a mistyped flag, not a note: a bare
    # `note <id> --list` once wrote the literal text "--list" over state.
    # Refuse BEFORE any write; file bodies are deliberate and exempt.
    if body_file is None and text.startswith("--"):
        typer.echo(
            f"Error: note refused: the body starts with '--' ({text}), so it is "
            "a mistyped flag, not a note. Nothing was written. Read the feed: "
            f"`fno backlog notes history {task_id}` or `fno backlog get {task_id}`. "
            "To write flag-shaped text, pass --body-file.",
            err=True,
        )
        raise typer.Exit(code=2)

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
        kind=kind,
    )
    if code != 0:
        # 2 = usage (an unknown --kind); 3 = a refusal that wrote nothing (a
        # retired --replace, or a refused --clear). The child printed the
        # reason on stderr.
        raise typer.Exit(code=code)

    if claims:
        typer.echo(unmeasured_note_warning(claims), err=True)

    if receipt is None:
        typer.echo("Error: the note action returned no receipt", err=True)
        raise typer.Exit(code=1)
    shown = json.dumps(receipt, separators=(",", ":")) if json_output else receipt.get("line", "")
    _echo_receipt(shown)
    warn_if_note_is_long(text)
    # Terminal-routed notes delivered too: the write went to the thread, but
    # the bound readers are still the people to tell.
    if not isinstance(readers, NoteReaders):
        return
    raise typer.Exit(code=deliver(readers, text, json_output=json_output))


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
    """One native `backlog update` invocation : the patch door.

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
    argv = [str(binary), "backlog", "update", "--graph", str(graph_path), "--node", node_id]
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
    kind: Optional[str] = None,
) -> "tuple[int, Optional[dict]]":
    """One native `backlog-note` invocation. Returns `(exit, receipt)`; the
    receipt is parsed from the child's stdout when the exit is 0."""
    from fno.rust_binary import resolve_binary

    binary = resolve_binary()
    if binary is None:
        typer.echo("Error: the fno-agents binary is required for `fno backlog note`", err=True)
        raise typer.Exit(code=1)
    argv = [str(binary), "backlog", "note", "--graph", str(graph_path), "--stdin",
            "--json", "--node", node_id]
    if reads:
        argv.extend(["--reads", json.dumps(reads, separators=(",", ":"))])
    if session_id:
        argv.extend(["--self-session", session_id])
    if kind:
        argv.extend(["--kind", kind])
    if quiet:
        argv.append("--quiet")
    proc = subprocess.run(argv, input=text, text=True, check=False, capture_output=True)
    if proc.returncode != 0:
        import sys

        sys.stderr.write(proc.stderr or "")
        return proc.returncode, None
    return 0, _receipt(proc.stdout)

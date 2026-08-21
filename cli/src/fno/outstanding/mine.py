"""Grouped CLI for the operator-owned ``my-priorities.md`` lane.

Parsing stays in :mod:`fno.king.lane`. This module only serializes mutations,
holding the lane's sidecar lock across the read/modify/replace transaction so
two writers cannot lose each other's changes.
"""

from __future__ import annotations

import json
import os
import re
import tempfile
from pathlib import Path
from typing import Callable

import filelock
import typer

from fno.graph._constants import NODE_ID_BODY
from fno.king.lane import LaneItem, LaneRead, read_lane


mine_app = typer.Typer(
    help="Read or edit the operator's ranked lane.",
    invoke_without_command=True,
)

_NODE_RE = re.compile(rf"^(?:{NODE_ID_BODY})$")
_LOCK_TIMEOUT_SECONDS = 10


class MineError(RuntimeError):
    """A lane read or mutation failed without changing the original file."""


def _payload(read: LaneRead) -> dict[str, list[dict[str, object]]]:
    return {
        "mine": [
            {"n": n, "text": item.text, "done": item.done, "node": item.node}
            for n, item in enumerate(read.items, start=1)
        ]
    }


def _load() -> LaneRead:
    read = read_lane()
    if read.error:
        raise MineError(read.error)
    return read


def _selected(read: LaneRead, visible_index: int) -> LaneItem:
    if visible_index < 1 or visible_index > len(read.items):
        raise MineError(
            f"mine item {visible_index} does not exist; "
            f"choose 1 through {len(read.items)}"
        )
    return read.items[visible_index - 1]


def _ending(raw_line: str) -> str:
    if raw_line.endswith("\r\n"):
        return "\r\n"
    if raw_line.endswith("\n") or raw_line.endswith("\r"):
        return raw_line[-1]
    return ""


def _replace(path: Path, content: str) -> None:
    tmp_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="w",
            dir=path.parent,
            prefix=f".{path.name}.",
            suffix=".tmp",
            delete=False,
            encoding="utf-8",
            newline="",
        ) as tmp:
            tmp.write(content)
            tmp_path = Path(tmp.name)
        os.replace(tmp_path, path)
        tmp_path = None
    finally:
        if tmp_path is not None:
            tmp_path.unlink(missing_ok=True)


def _mutate(change: Callable[[list[str], LaneRead], None]) -> LaneRead:
    from fno import paths

    path = paths.operator_lane()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with filelock.FileLock(str(path) + ".lock", timeout=_LOCK_TIMEOUT_SECONDS):
            read = read_lane(path)
            if read.error:
                raise MineError(read.error)
            try:
                content = path.read_bytes().decode("utf-8")
            except FileNotFoundError:
                content = ""
            except (OSError, UnicodeDecodeError) as exc:
                raise MineError(f"cannot read operator lane {path}: {exc}") from exc
            lines = content.splitlines(keepends=True)
            change(lines, read)
            _replace(path, "".join(lines))
            updated = read_lane(path)
            if updated.error:
                raise MineError(updated.error)
            return updated
    except MineError:
        raise
    except (filelock.Timeout, OSError) as exc:
        raise MineError(f"cannot write operator lane {path}: {exc}") from exc


def _run_mutation(change: Callable[[list[str], LaneRead], None]) -> None:
    try:
        updated = _mutate(change)
    except MineError as exc:
        typer.echo(f"mine: failed: {exc}", err=True)
        raise typer.Exit(1)
    typer.echo(str(len(updated.items)))


@mine_app.callback(invoke_without_command=True)
def list_mine(
    ctx: typer.Context,
    as_json: bool = typer.Option(False, "--json", "-J", help="Emit the lane as JSON."),
) -> None:
    """List the operator's lane in visible item order."""
    if ctx.invoked_subcommand is not None:
        return
    try:
        read = _load()
    except MineError as exc:
        typer.echo(f"mine: failed: {exc}", err=True)
        raise typer.Exit(1)
    payload = _payload(read)
    if as_json:
        typer.echo(json.dumps(payload, separators=(",", ":")))
        return
    for row in payload["mine"]:
        mark = "x" if row["done"] else " "
        node = f" -> {row['node']}" if row["node"] else ""
        typer.echo(f"{row['n']}. [{mark}] {row['text']}{node}")


def _add(text: str) -> None:
    if not text.strip() or "\n" in text or "\r" in text:
        typer.echo("mine: failed: item text must be one non-empty line", err=True)
        raise typer.Exit(2)

    def change(lines: list[str], _read: LaneRead) -> None:
        if lines and not lines[-1].endswith(("\n", "\r")):
            lines[-1] += "\n"
        lines.append(f"- [ ] {text.strip()}\n")

    _run_mutation(change)


def _parse_visible_index(raw: str) -> int:
    try:
        return int(raw)
    except ValueError:
        typer.echo(f"mine: failed: not a number: {raw}", err=True)
        raise typer.Exit(2)


def _done(raw_index: str) -> None:
    visible_index = _parse_visible_index(raw_index)

    def change(lines: list[str], read: LaneRead) -> None:
        item = _selected(read, visible_index)
        idx = item.line - 1
        raw = lines[idx]
        mark = " " if item.done else "x"
        lines[idx] = raw[:3] + mark + raw[4:]

    _run_mutation(change)


def _drop(raw_index: str) -> None:
    visible_index = _parse_visible_index(raw_index)

    def change(lines: list[str], read: LaneRead) -> None:
        item = _selected(read, visible_index)
        del lines[item.line - 1]

    _run_mutation(change)


def _link(raw_index: str, node_id: str) -> None:
    visible_index = _parse_visible_index(raw_index)
    if _NODE_RE.fullmatch(node_id) is None:
        typer.echo(f"mine: failed: invalid node id: {node_id}", err=True)
        raise typer.Exit(2)

    def change(lines: list[str], read: LaneRead) -> None:
        item = _selected(read, visible_index)
        idx = item.line - 1
        ending = _ending(lines[idx])
        mark = "x" if item.done else " "
        lines[idx] = f"- [{mark}] {item.text} -> {node_id}{ending}"

    _run_mutation(change)


# One dispatched leaf instead of four (add/done/drop/link): the CLI-surface
# ratchet (scripts/ci/verb-collapse-map.tsv, test_verb_collapse_map.py) caps
# total registered Click leaves; four separate commands, doubled by the
# pre-existing bare `outstanding` alias, would blow that budget.
_ACTIONS = {"add": 1, "done": 1, "drop": 1, "link": 2}


@mine_app.command("do", context_settings={"ignore_unknown_options": True})
def do(
    action: str = typer.Argument(..., help="add | done | drop | link"),
    args: list[str] = typer.Argument(None, help="add TEXT | done N | drop N | link N NODE_ID"),
) -> None:
    """Dispatch one MINE mutation: add TEXT | done N | drop N | link N NODE_ID."""
    args = args or []
    want = _ACTIONS.get(action)
    if want is None:
        typer.echo(f"mine: failed: unknown action {action!r} (add|done|drop|link)", err=True)
        raise typer.Exit(2)
    if len(args) != want:
        typer.echo(
            f"mine: failed: {action} takes {want} argument(s), got {len(args)}", err=True
        )
        raise typer.Exit(2)
    if action == "add":
        _add(args[0])
    elif action == "done":
        _done(args[0])
    elif action == "drop":
        _drop(args[0])
    else:
        _link(args[0], args[1])

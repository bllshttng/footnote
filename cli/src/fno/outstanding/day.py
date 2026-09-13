"""Explicit morning and end-of-day readbacks for the operator inbox."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Any

import typer

from fno import paths
from fno.events import append_event, day_boundary
from fno.outstanding.core import events_path, questions_path
from fno.rust_binary import resolve_binary

day_app = typer.Typer(name="day", help="Read and record a daily boundary.")


class DayIndexWriteError(RuntimeError):
    """The project journal accepted a boundary but the recall index did not."""


def _run_native(kind: str) -> dict[str, Any]:
    binary = resolve_binary()
    if binary is None:
        raise RuntimeError("fno-agents binary not found; install or set FNO_AGENTS_BIN")
    argv = [str(binary), "day", "--kind", kind, "--json"]
    for event_path in paths.event_journals():
        argv.extend(("--events-path", str(event_path)))
    result = subprocess.run(argv, capture_output=True, text=True, check=False)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or f"fno-agents day exited {result.returncode}")
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"fno-agents day returned invalid JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise RuntimeError("fno-agents day returned a non-object payload")
    return payload


def _record_boundary(kind: str) -> dict[str, Any]:
    payload = _run_native(kind)
    if payload.get("reused") is True:
        return payload
    boundary_id = str(payload.get("boundary_id") or "")
    if not boundary_id:
        raise RuntimeError("fno-agents day returned no boundary_id")
    questions = payload.get("questions") or {}
    event = day_boundary(
        boundary_id=boundary_id,
        kind=kind,
        cutoff=str(payload.get("cutoff") or ""),
        prior_boundary_id=payload.get("prior_boundary_id"),
        featured=list(questions.get("featured") or []),
        completed=questions_count(payload.get("completed"), "count"),
        open_count=questions.get("open"),
        opened=questions_count(questions, "opened"),
        closed=questions_count(questions, "closed"),
        retractions=length(payload.get("retractions")),
    )
    append_event(event, events_path=events_path(resolve_carveout_root()))
    try:
        append_event(event, events_path=questions_path())
    except Exception as exc:  # noqa: BLE001 - name the durable boundary on failure
        raise DayIndexWriteError(boundary_id) from exc
    return payload


def questions_count(value: Any, key: str) -> int | None:
    if isinstance(value, dict) and isinstance(value.get(key), int):
        return value[key]
    return None


def length(value: Any) -> int:
    return len(value) if isinstance(value, list) else 0


def resolve_carveout_root() -> Path:
    from fno.carveout.core import resolve_carveout_root as resolve

    return resolve()


def _render(payload: dict[str, Any]) -> str:
    questions = payload.get("questions") or {}
    featured = questions.get("featured") or []
    if payload.get("questions_state") == "missing":
        first = f"open questions: unknown (questions store missing: {payload.get('questions_path')})"
    elif featured:
        first = f"{'Start: answer' if payload.get('kind') == 'start' else 'Tomorrow: resume'} {featured[0]}"
    else:
        first = f"Nothing waits on you: {questions.get('open', 0)} open questions"
    return "\n".join(
        [
            first,
            f"Completed: {(payload.get('completed') or {}).get('count', 0)}",
            f"Questions: +{questions.get('opened', 0)} / -{questions.get('closed', 0)}; showing {len(featured)} of {questions.get('open', 0)} open",
            f"Retractions: {length(payload.get('retractions'))}",
            f"Window: {(payload.get('window') or {}).get('from')} to {(payload.get('window') or {}).get('to')}",
        ]
    )


def _command(kind: str, as_json: bool) -> None:
    try:
        payload = _record_boundary(kind)
    except DayIndexWriteError as exc:
        typer.echo(f"day boundary {exc.args[0]} recorded in the project journal but not the question index", err=True)
        raise typer.Exit(code=1) from exc
    except RuntimeError as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(code=1) from exc
    typer.echo(json.dumps(payload, indent=2) if as_json else _render(payload))


@day_app.command()
def start(json_output: bool = typer.Option(False, "--json")) -> None:
    """Read and record the start-of-day boundary."""
    _command("start", json_output)


@day_app.command()
def end(json_output: bool = typer.Option(False, "--json")) -> None:
    """Read and record the end-of-day boundary."""
    _command("end", json_output)

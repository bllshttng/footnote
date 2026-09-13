"""Explicit morning and end-of-day readbacks for the operator inbox."""

import json
import subprocess
from typing import Any

import typer

from fno import paths
from fno.carveout.core import resolve_carveout_root
from fno.events import append_event, day_boundary
from fno.outstanding.core import events_path, questions_path
from fno.rust_binary import resolve_binary

day_app = typer.Typer(name="day", help="Read and record a daily boundary.")


class DayIndexWriteError(RuntimeError):
    pass


def _run_native(kind: str) -> dict[str, Any]:
    binary = resolve_binary()
    if binary is None:
        raise RuntimeError("fno-agents binary not found; install or set FNO_AGENTS_BIN")
    argv = [str(binary), "day", "--kind", kind, "--json"]
    for path in paths.event_journals():
        argv.extend(("--events-path", str(path)))
    result = subprocess.run(argv, capture_output=True, text=True, check=False)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or f"fno-agents day exited {result.returncode}")
    return json.loads(result.stdout)


def _record_boundary(kind: str) -> dict[str, Any]:
    payload = _run_native(kind)
    if payload.get("reused") is True:
        return payload
    boundary_id = str(payload.get("boundary_id") or "")
    questions = payload.get("questions") or {}
    event = day_boundary(
        boundary_id=boundary_id, kind=kind, cutoff=str(payload.get("cutoff") or ""),
        prior_boundary_id=payload.get("prior_boundary_id"), featured=list(questions.get("featured") or []),
        completed=(payload.get("completed") or {}).get("count"), open_count=questions.get("open"),
        opened=questions.get("opened"), closed=questions.get("closed"), retractions=len(payload.get("retractions") or []),
    )
    append_event(event, events_path=events_path(resolve_carveout_root()))
    try:
        append_event(event, events_path=questions_path())
    except Exception as exc:  # noqa: BLE001 - name the durable boundary on failure
        raise DayIndexWriteError(boundary_id) from exc
    return payload


def _command(kind: str, as_json: bool) -> None:
    try:
        payload = _record_boundary(kind)
    except DayIndexWriteError as exc:
        typer.echo(f"day boundary {exc.args[0]} recorded in the project journal but not the question index", err=True)
        raise typer.Exit(code=1) from exc
    except (RuntimeError, json.JSONDecodeError) as exc:
        typer.echo(str(exc), err=True)
        raise typer.Exit(code=1) from exc
    if as_json:
        typer.echo(json.dumps(payload, indent=2))
        return
    questions = payload.get("questions") or {}
    featured = questions.get("featured") or []
    line = (f"open questions: unknown (questions store missing: {payload.get('questions_path')})"
            if payload.get("questions_state") == "missing" else
            f"{'Start: answer' if kind == 'start' else 'Tomorrow: resume'} {featured[0]}"
            if featured else f"Nothing waits on you: {questions.get('open', 0)} open questions")
    typer.echo(line)


@day_app.command()
def start(json_output: bool = typer.Option(False, "--json")) -> None:
    _command("start", json_output)


@day_app.command()
def end(json_output: bool = typer.Option(False, "--json")) -> None:
    _command("end", json_output)

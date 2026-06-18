"""fno log subcommand - append structured progress entries to agent-progress.jsonl.

Agents use this to record activity, milestones, warnings, and user notes.
The walker reads these entries for mid-phase observability in 'fno megawalk status'.
"""
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

import typer

app = typer.Typer(
    name="log",
    help="Append a progress entry to the per-worktree agent-progress.jsonl",
    no_args_is_help=True,
)


def _resolve_worktree_progress_file(cwd: Path | None = None) -> Path:
    """Return the path to agent-progress.jsonl, creating parent dirs if needed."""
    cwd = cwd or Path.cwd()
    progress = cwd / ".fno" / "agent-progress.jsonl"
    progress.parent.mkdir(parents=True, exist_ok=True)
    return progress


def _emit(kind: str, summary: str, details: dict | None = None) -> None:
    """Write a single JSON entry to the progress file."""
    entry: dict = {
        "ts": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "session_id": (
            os.environ.get("CLAUDECODE_SESSION_ID")
            or f"manual-{os.environ.get('USER', 'unknown')}"
        ),
        "kind": kind,
        "summary": summary,
    }
    if details:
        entry["details"] = details
    path = _resolve_worktree_progress_file()
    try:
        with path.open("a", encoding="utf-8") as f:
            f.write(json.dumps(entry) + "\n")
    except OSError as exc:
        print(f"fno log: write failed to {path}: {exc}", file=sys.stderr)
        raise typer.Exit(code=2)


@app.command()
def activity(
    summary: str = typer.Argument(..., help="Short description of the activity"),
    details: str = typer.Option(
        None, "--details", help="JSON-encoded details object"
    ),
) -> None:
    """Log an activity (frequent, ongoing work)."""
    parsed_details = json.loads(details) if details else None
    _emit("activity", summary, parsed_details)


@app.command()
def milestone(
    summary: str = typer.Argument(..., help="Short description of the milestone"),
    details: str = typer.Option(
        None, "--details", help="JSON-encoded details object"
    ),
) -> None:
    """Log a milestone (significant completion)."""
    parsed_details = json.loads(details) if details else None
    _emit("milestone", summary, parsed_details)


@app.command()
def warning(
    summary: str = typer.Argument(..., help="Short description of the warning"),
    details: str = typer.Option(
        None, "--details", help="JSON-encoded details object"
    ),
) -> None:
    """Log a warning (concerning but not yet a failure)."""
    parsed_details = json.loads(details) if details else None
    _emit("warning", summary, parsed_details)


@app.command(name="user-note")
def user_note(
    summary: str = typer.Argument(..., help="Free-form note from a human"),
) -> None:
    """Log a user_note (free-form note from a human, non-actionable for agents)."""
    _emit("user_note", summary)

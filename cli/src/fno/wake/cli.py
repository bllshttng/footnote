"""fno wake CLI - admin verbs for inspecting and managing wake-signals.

Commands:
    list   - list pending wake-signals (table or --json)
    clear  - delete all wake-signals
    drop   - inject a signal for testing / manual wakeup

Exit codes:
    0  success
"""
from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import typer

from fno.wake.signal import (
    WakeSignal,
    drop_signal,
    read_signals,
    signals_dir,
)

wake_app = typer.Typer(help="Wake-signal admin commands", no_args_is_help=True)


# ---------------------------------------------------------------------------
# Repo root resolution
# ---------------------------------------------------------------------------

def _repo_root() -> Path:
    """Return the repo root for signal storage.

    Overridden by FNO_WAKE_REPO_ROOT env var (for tests).
    Default: current working directory (same convention as other CLI modules).
    """
    override = os.environ.get("FNO_WAKE_REPO_ROOT")
    if override:
        return Path(override)
    return Path.cwd()


# ---------------------------------------------------------------------------
# list
# ---------------------------------------------------------------------------

@wake_app.command(name="list")
def list_cmd(
    json_output: bool = typer.Option(False, "--json", "-J", help="output JSON array"),
) -> None:
    """List pending wake-signals (non-destructive)."""
    root = _repo_root()
    signals = read_signals(root)

    if json_output:
        # Strip internal _path field before emitting
        cleaned = [{k: v for k, v in s.items() if k != "_path"} for s in signals]
        typer.echo(json.dumps(cleaned))
        return

    if not signals:
        typer.echo("no signals")
        return

    # Simple aligned-column table - grep-able and pipeable
    header = f"{'signal_id':<20s}  {'kind':<10s}  {'source':<20s}  {'from':<20s}  summary"
    typer.echo(header)
    typer.echo("-" * len(header))
    for s in signals:
        sid = s.get("signal_id", "")
        kind = s.get("kind", "")
        source = s.get("source", "")
        from_ = s.get("from_project", "")
        summary = s.get("summary", "")
        typer.echo(f"{sid:<20s}  {kind:<10s}  {source:<20s}  {from_:<20s}  {summary}")


# ---------------------------------------------------------------------------
# clear
# ---------------------------------------------------------------------------

@wake_app.command()
def clear() -> None:
    """Delete all pending wake-signals."""
    root = _repo_root()
    wake_dir = signals_dir(root)
    if not wake_dir.is_dir():
        typer.echo("cleared 0 signals")
        return

    files = list(wake_dir.glob("wake-*.json"))
    for f in files:
        try:
            f.unlink()
        except OSError:
            pass

    typer.echo(f"cleared {len(files)} signals")


# ---------------------------------------------------------------------------
# drop
# ---------------------------------------------------------------------------

@wake_app.command()
def drop(
    source: str = typer.Option(..., "--source", help="signal source identifier"),
    kind: str = typer.Option(..., "--kind", help="signal kind: question|lesson|supervisor|brief"),
    msg_id: str = typer.Option(..., "--msg-id", help="back-reference to inbox msg-id"),
    from_: str = typer.Option(..., "--from", help="originating project"),
    summary: str = typer.Option(..., "--summary", help="one-line human summary"),
) -> None:
    """Inject a wake-signal (for testing or manual wakeup)."""
    root = _repo_root()
    signal = WakeSignal(
        source=source,
        kind=kind,  # type: ignore[arg-type]  # Typer passes a str; WakeSignal validates at runtime
        msg_id=msg_id,
        from_project=from_,
        summary=summary,
        ts=datetime.now(tz=timezone.utc),
    )
    drop_signal(root, signal)
    typer.echo(signal.signal_id)

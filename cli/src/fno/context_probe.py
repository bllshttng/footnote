"""Python transport for the Rust-owned context-window reader."""
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

import typer

from fno.agents.self_stamp import resolve_own_transcript, resolve_self_identity


@dataclass(frozen=True)
class ContextReading:
    """One context-window reading returned by the native probe."""

    used_tokens: int
    window_tokens: int
    used_pct: int
    model: str


def probe_context(transcript_path: Optional[Path] = None) -> Optional[ContextReading]:
    """One context-window reading, or None (unreadable).

    With ``transcript_path`` it reads that file - the door hooks use, which are
    handed an authoritative path in their payload. Without one it self-resolves
    through the same harness-aware locator the model resolver uses, so ``fno
    whoami`` and ``fno whoami context`` answer from the same transcript. The number is
    derived, never stored: the transcript is the only source.
    """
    if transcript_path is None:
        ident = resolve_self_identity()
        if not ident.session_id or not ident.harness:
            return None
        transcript_path = resolve_own_transcript(ident.session_id, ident.harness)
        if transcript_path is None:
            return None
    from fno.rust_binary import call_binary_json

    probe = ["--probe", "--transcript", str(transcript_path), "--json"]
    error, native = call_binary_json("context-run", probe, timeout=5)
    if error or not isinstance(native, dict):
        return None
    try:
        keys = ("used_tokens", "window_tokens", "used_pct")
        return ContextReading(*(int(native[k]) for k in keys), model=str(native["model"]))
    except (KeyError, TypeError, ValueError):
        return None


def context_command(
    transcript: Optional[Path] = typer.Option(
        None, "--transcript", help="probe this transcript jsonl instead of self-resolving"
    ),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="emit the reading as one JSON line"
    ),
) -> None:
    """Context-window usage for this session, or a given transcript.

    Hidden: hooks are handed a ``transcript_path`` and probe THAT file rather
    than whatever this process's env resolves to, which ``fno whoami`` cannot
    do without a flag that makes no sense on an orientation verb. Not a second
    model-facing surface; it is what makes the number reachable from a codex,
    agy, or opencode worker and from every hook. Exits 3 when unreadable,
    matching the shell probe's contract.
    """
    reading = probe_context(transcript_path=transcript)
    if reading is None:
        raise typer.Exit(code=3)
    if json_output:
        typer.echo(
            json.dumps(
                {
                    "used_tokens": reading.used_tokens,
                    "window_tokens": reading.window_tokens,
                    "used_pct": reading.used_pct,
                    "model": reading.model,
                }
            )
        )
        return
    typer.echo(
        f"{reading.used_pct}% used ({reading.used_tokens:,} of "
        f"{reading.window_tokens:,} tokens), model {reading.model}"
    )

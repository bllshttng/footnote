"""Transcript reads the Rust stop gate shells for.

Registered on the agents app by importing this module from ``cli.py`` (after
``agents_app`` exists), so the file-budget gate keeps ``cli.py`` shrinking
while the verb stays one hop from its reader in ``peek.py``.
"""

from __future__ import annotations

import sys
from pathlib import Path

import typer

from fno.agents.cli import agents_app


@agents_app.command("newest-assistant-text", hidden=True)
def cmd_newest_assistant_text(
    transcript: str = typer.Option(
        ..., "--transcript", help="Path to a claude or codex JSONL transcript."
    ),
) -> None:
    """Print the newest assistant turn's text from one transcript file.

    loopcheck's distress read routes here instead of keeping its own parser:
    this is the one reader that speaks both harness shapes (x-6aca). Exit 1
    when the file yields no assistant text.
    """
    from fno.agents.peek import newest_assistant_text

    text = newest_assistant_text(Path(transcript))
    if not text:
        raise typer.Exit(code=1)
    sys.stdout.write(text)

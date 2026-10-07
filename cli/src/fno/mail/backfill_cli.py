"""The `fno agents mail backfill` transport.

While `fno mail send` was down, agent-to-agent messages went over the
harness's native cross-session transport (the SendMessage tool call in the
sender transcript, the <cross-session-message> block in the receiver
transcript). Both halves live in the harness transcripts, so the traffic
can be joined and written back as durable audit-only rows
(delivery=cross-session) that never re-deliver.

The scan, the join, and the write live behind the hidden
`fno-agents mail-backfill run` verb (the mail-receipt pattern: Rust owns
the engine, Python keeps transports). The file-budget law bars new Python
in cli/src/fno, so this module stays a thin transport: resolve defaults,
run the verb, print what it says.
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Optional

import typer

# The mail-send outage window: the default backfill scope. Every message
# the engine joins between these bounds is traffic the store never saw.
# Overrides: --since / --until.
OUTAGE_SINCE = "2026-10-07T08:42:47Z"
OUTAGE_UNTIL = "2026-10-07T12:52:39Z"


def _verb(argv: list[str], stdin_text: Optional[str] = None) -> str:
    """One mail-backfill read through the native door. A missing binary
    refuses rather than falling back to a second implementation."""
    from fno.rust_binary import VerbUnavailable, resolve_binary

    binary = resolve_binary()
    if binary is None:
        raise VerbUnavailable("fno-agents binary not found; run fno doctor update")
    proc = subprocess.run(
        [str(binary), "mail-backfill", *argv],
        input=stdin_text,
        capture_output=True,
        text=True,
        timeout=60,
    )
    if proc.returncode != 0:
        raise VerbUnavailable(
            (proc.stderr or "fno-agents mail-backfill failed").strip()[:200]
        )
    return proc.stdout.rstrip("\n")


def cmd_mail_backfill(
    since: Optional[str] = typer.Option(None, "--since", help="Window start (ISO)."),
    until: Optional[str] = typer.Option(None, "--until", help="Window end (ISO)."),
    root: Optional[list[Path]] = typer.Option(
        None, "--root", help="Transcript store root(s). Default: this host's store."
    ),
    apply: bool = typer.Option(False, "--apply", help="Write the joined rows."),
) -> None:
    """Backfill outage-era cross-session traffic into the mail store.

    The engine scans the harness transcripts for native SendMessage calls,
    joins each with its receiver-side block, and writes audit-only rows
    (delivery=cross-session) that never re-deliver. Idempotent by msg_id:
    the archive id is deterministic, so a re-run skips what already landed.
    """
    from fno.agents.discover import default_projects_dir
    from fno.bus.log import bus_log_path

    argv = [
        "run",
        "--since", since or OUTAGE_SINCE,
        "--until", until or OUTAGE_UNTIL,
    ]
    for r in root or [default_projects_dir()]:
        argv += ["--root", str(r)]
    if apply:
        argv += ["--apply", "--live", str(bus_log_path())]
    print(_verb(argv))

"""`fno agents mail backfill`: thin transport for the hidden engine.

The engine (crates/fno-agents/src/mail_backfill.rs) scans the harness
transcripts, joins native SendMessage calls with their receiver-side
blocks, and writes audit-only rows that never re-deliver. The transport
resolves this host's transcript store and the live bus, runs the verb,
prints its JSON summary. Flags beyond --apply (--since/--until/--root)
stay on the binary verb itself.
"""

import json

import typer


def cmd_mail_backfill(
    apply: bool = typer.Option(False, "--apply", help="Write the joined rows."),
) -> None:
    """Backfill outage-era cross-session traffic into the mail store.

    Dry run by default. Idempotent by msg_id: a re-run skips what the
    archive already holds.
    """
    from fno.agents.discover import default_projects_dir
    from fno.bus.log import bus_log_path
    from fno.rust_binary import call_binary_json

    argv = ["run", "--root", str(default_projects_dir())]
    if apply:
        argv += ["--apply", "--live", str(bus_log_path())]
    error, parsed = call_binary_json("mail-backfill", argv)
    if error is not None:
        print(f"backfill failed: {error}")
        raise typer.Exit(code=1)
    print(json.dumps(parsed, ensure_ascii=False))

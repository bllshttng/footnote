"""Thin transport for the hidden `fno-agents mail-backfill run` engine."""
import json

import typer


def cmd_mail_backfill(
    apply: bool = typer.Option(False, "--apply"),
) -> None:
    from fno.agents.discover import default_projects_dir
    from fno.bus.log import bus_log_path
    from fno.rust_binary import call_binary_json

    argv = ["run", "--root", str(default_projects_dir())]
    if apply:
        argv += ["--apply", "--live", str(bus_log_path())]
    error, parsed = call_binary_json("mail-backfill", argv)
    if error is not None:
        raise SystemExit(f"backfill failed: {error}")
    print(json.dumps(parsed, ensure_ascii=False))

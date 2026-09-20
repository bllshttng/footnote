"""fno agents restart - restart running fno processes onto freshly-installed binaries.

`fno doctor update && fno agents restart` is the reboot loop: `update` installs new binaries,
`restart` swaps the RUNNING processes onto them.

The verb itself lives in the Rust `fno-agents restart`; this module is a pass-through
that execs it, so the flag spellings and the receipt cannot drift between the doors.

x-6648: after a proven `--mux` kill, this adapter also runs the Rust
`restart --keepers-only` leg. The full restart cycles stale store keepers
BEFORE the mux kill, so a keeper the old server spawned (measured: one
survived under launchd) missed that pass; the post-kill sweep reaches it.
"""

import json
import subprocess
import sys

import typer

KEEPERS_PREFIX = "fno agents restart: keepers "


def _keepers_summary(stdout: str | None) -> dict | None:
    """Parse the `fno agents restart: keepers {...}` machine line (last wins).

    None when the line is absent or unparsable: the caller treats that as
    "not proven", never as success.
    """
    if not stdout:
        return None
    for line in reversed(stdout.splitlines()):
        if line.startswith(KEEPERS_PREFIX):
            try:
                return json.loads(line[len(KEEPERS_PREFIX) :])
            except json.JSONDecodeError:
                return None
    return None


def _any_mux_killed(summary: dict | None) -> bool:
    """True only when the summary's mux leg PROVES at least one server kill."""
    sessions = ((summary or {}).get("mux") or {}).get("sessions") or []
    return any(s.get("killed") is True for s in sessions)


def restart_command(
    force: bool = typer.Option(False, "--force", "-F", help="Break-glass daemon restart."),
    mux: bool = typer.Option(False, "--mux", help="Also restart live mux servers."),
    json_out: bool = typer.Option(False, "--json", "-J", help="JSON summary on stdout."),
) -> None:
    """Restart running fno processes onto freshly-installed binaries."""
    from fno import rust_binary

    binary = rust_binary.resolve_installed_binary()
    if binary is None:
        typer.echo(
            "fno agents restart: no installed fno-agents binary; skipping daemon restart",
            err=True,
        )
        raise typer.Exit(0)
    daemon_cmd = [str(binary), "restart"]
    if force:
        daemon_cmd.append("--force")
    if mux:
        daemon_cmd.append("--mux")
    if json_out:
        daemon_cmd.append("--json")
    try:
        proc = subprocess.run(daemon_cmd, capture_output=True, text=True, timeout=600)
    except (OSError, subprocess.SubprocessError) as exc:
        typer.echo(f"fno agents restart: could not run fno-agents restart ({exc})", err=True)
        raise typer.Exit(1)
    if proc.stderr:
        typer.echo(proc.stderr, err=True)
    if not json_out and proc.stdout:
        typer.echo(proc.stdout, err=True)

    exit_code = proc.returncode
    summary = _keepers_summary(proc.stdout)
    # x-6648: the post-mux keeper refresh. Runs only after a PROVEN kill (a
    # parsed summary naming a killed session); daemon-only restarts,
    # report-only rows, failed kills, and unparsable summaries skip the leg.
    folded = False
    if mux and proc.returncode == 0 and _any_mux_killed(summary):
        keeper_cmd = [str(binary), "restart", "--keepers-only", "--json"]
        try:
            keeper = subprocess.run(keeper_cmd, capture_output=True, text=True, timeout=300)
        except (OSError, subprocess.SubprocessError) as exc:
            typer.echo(f"fno agents restart: post-mux keeper leg could not run ({exc})", err=True)
            raise typer.Exit(1)
        if keeper.stderr:
            if json_out:
                typer.echo(keeper.stderr, err=True)
            else:
                typer.echo(keeper.stderr)
        ksummary = _keepers_summary(keeper.stdout)
        proved = ksummary is not None and ksummary.get("ok") is True
        if json_out and summary is not None:
            summary["post_mux_store_keepers"] = (ksummary or {}).get("store_keepers", [])
            summary["post_mux_keeper_refresh"] = "proved" if proved else "unproven"
            if not proved:
                summary["ok"] = False
                summary["verdict"] = "FAILED"
            sys.stdout.write(KEEPERS_PREFIX + json.dumps(summary) + "\n")
            folded = True
        if not proved:
            typer.echo(
                "fno agents restart: post-mux keeper refresh unproven; a stale store keeper survived the mux kill",
                err=True,
            )
            exit_code = 1
    if json_out and not folded and proc.stdout:
        sys.stdout.write(proc.stdout)
    raise typer.Exit(exit_code)

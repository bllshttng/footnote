"""fno agents restart - restart running fno processes onto freshly-installed binaries.

`fno doctor update && fno agents restart` is the reboot loop: `update` installs new binaries,
`restart` swaps the RUNNING processes onto them.

The verb itself lives in the Rust `fno-agents restart`; this module is a pass-through
that execs it, so the flag spellings and the receipt cannot drift between the doors.
"""

import subprocess
import sys

import typer


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
    if json_out and proc.stdout:
        sys.stdout.write(proc.stdout)
    elif proc.stdout:
        typer.echo(proc.stdout, err=True)
    raise typer.Exit(proc.returncode)

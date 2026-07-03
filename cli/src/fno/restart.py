"""fno restart - restart running fno processes onto freshly-installed binaries (x-69b3).

`fno update && fno restart` is the reboot loop: `update` installs new binaries,
`restart` swaps the RUNNING processes onto them.

- Agents daemon: ALWAYS restarted - SIGTERM the stale daemon and lazy-start a
  fresh one from the current binary; PTY workers survive. Safe: the daemon holds
  no user session state.
- Mux servers: reported by default, restarted only with --mux. A live mux server
  holds real shells/panes and deliberately survives `cargo install` upgrades, so
  restarting it ENDS those sessions - hence opt-in, not the default.
"""
from __future__ import annotations

import json
import shutil
import subprocess
from typing import Any, Optional

import typer


def _mux_sessions() -> Optional[list[dict[str, Any]]]:
    """Live mux sessions via `fno mux ls --json`, or None when the mux front door
    is unavailable / the call fails. Best-effort: never raises."""
    fno = shutil.which("fno")
    if not fno:
        return None
    try:
        proc = subprocess.run(
            [fno, "mux", "ls", "--json"], capture_output=True, text=True, timeout=10
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    try:
        data = json.loads(proc.stdout or "[]")
    except json.JSONDecodeError:
        return None
    return data if isinstance(data, list) else None


def restart_command(
    mux: bool = typer.Option(
        False,
        "--mux",
        help="Also restart live mux servers (DESTRUCTIVE: ends their shells/panes).",
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit a single JSON summary on stdout; text to stderr."
    ),
) -> None:
    """Restart running fno processes onto freshly-installed binaries.

    The agents daemon restarts always (PTY workers survive). Live mux servers are
    reported by default and restarted only with --mux (killing a server ends its
    live sessions).
    """
    result: dict[str, Any] = {
        "daemon": None,
        "mux_sessions": [],  # LIVE session names (the restart targets)
        "mux_other": [],  # non-live rows (stale/unqueryable): reported, never killed
        "mux_restarted": [],
    }
    failures: list[str] = []  # non-empty -> exit 1

    def say(msg: str, err: bool = False) -> None:
        # In --json mode all human text goes to stderr so stdout stays one object.
        if json_out or err:
            typer.echo(msg, err=True)
        else:
            typer.echo(msg)

    # 1. Agents daemon (safe: PTY workers survive). The primary action - an actual
    # restart FAILURE fails the command so a chained `fno update && fno restart`
    # surfaces it. An absent binary is "nothing to restart", not a failure.
    from fno.agents import rust_runtime

    binary = rust_runtime.resolve_installed_binary()
    if binary is None:
        result["daemon"] = "skipped-no-binary"
        say("fno restart: no installed fno-agents binary; skipping daemon restart", err=True)
    else:
        try:
            rc = subprocess.run([str(binary), "restart"], timeout=120).returncode
        except (OSError, subprocess.SubprocessError) as exc:
            result["daemon"] = "failed"
            say(f"fno restart: could not run fno-agents restart ({exc})", err=True)
            failures.append(f"daemon: {exc}")
        else:
            if rc == 0:
                result["daemon"] = "restarted"
                say("fno restart: agents daemon restarted (PTY workers survive).")
            else:
                result["daemon"] = f"failed:{rc}"
                say(f"fno restart: fno-agents restart exited {rc}", err=True)
                failures.append(f"daemon: exit {rc}")

    # 2. Mux servers. ONLY live sessions are restart targets; stale/unqueryable
    # rows are reported, never killed (killing a non-live socket is meaningless
    # and could unlink a socket that `kill-server` owns).
    sessions = _mux_sessions()
    if sessions is None:
        say("fno restart: mux front door unavailable; skipped mux check.")
    else:
        live = [
            s["session"]
            for s in sessions
            if isinstance(s, dict) and s.get("session") and s.get("state") == "live"
        ]
        other = [
            s["session"]
            for s in sessions
            if isinstance(s, dict) and s.get("session") and s.get("state") != "live"
        ]
        result["mux_sessions"] = live
        result["mux_other"] = other
        if other:
            say(f"fno restart: {len(other)} non-live mux row(s) (not restarted): {other}.")
        if not live:
            say("fno restart: no live mux sessions.")
        elif mux:
            fno = shutil.which("fno")
            if not fno:
                say(
                    "fno restart: --mux requested but the `fno` mux binary is not on PATH; "
                    "cannot restart mux sessions.",
                    err=True,
                )
                failures.append("mux: fno not on PATH")
            else:
                for name in live:
                    try:
                        kc = subprocess.run(
                            [fno, "mux", "kill-server", name],
                            capture_output=True,
                            text=True,
                            timeout=10,
                        ).returncode
                    except (OSError, subprocess.SubprocessError):
                        kc = 1
                    if kc == 0:
                        result["mux_restarted"].append(name)
                        say(
                            f"fno restart: mux session '{name}' killed; the next attach starts a "
                            "fresh server on the new binary."
                        )
                    else:
                        say(f"fno restart: could not kill mux session '{name}' (exit {kc}).", err=True)
                        failures.append(f"mux: kill {name} exit {kc}")
        else:
            say(
                f"fno restart: {len(live)} live mux session(s) still on the current binary: "
                f"{live}. Killing a mux server ends its shells/panes and the next attach starts a "
                "fresh server on the new binary; that stays opt-in. Do it with "
                "`fno mux kill-server <name>`, or `fno restart --mux` for all."
            )

    result["ok"] = not failures
    if json_out:
        typer.echo(json.dumps(result))
    if failures:
        raise typer.Exit(1)

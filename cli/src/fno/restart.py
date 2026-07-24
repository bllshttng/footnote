"""fno restart - restart running fno processes onto freshly-installed binaries (x-69b3).

`fno update && fno restart` is the reboot loop: `update` installs new binaries,
`restart` swaps the RUNNING processes onto them.

- Agents daemon: ALWAYS restarted - SIGTERM the stale daemon and lazy-start a
  fresh one from the current binary; PTY workers survive. Safe: the daemon holds
  no user session state.
- Mux servers: a STALE-wire server (its `.ver` sidecar predates the running
  binary, so a new client's handshake is rejected - already unreachable) is
  auto-restarted, healing the pair-deploy skew (x-1a85). A CURRENT-wire server
  holds real reachable shells/panes, so restarting it ENDS those sessions and
  stays opt-in behind --mux (reported by default).
- Worker revival: killing a mux server also kills the worker PTYs it hosted.
  After a kill, registered claude workers that died with the server are
  respawned onto their recorded session (`fno agents spawn --resume`, the
  revive-in-place lane) so the restart reconnects them instead of orphaning
  them. Best-effort and opt-out via --no-revive; bg workers survive the kill
  and are never touched.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import time
from typing import Any, Callable, Optional

import typer

# Killed panes' worker processes take a moment to die; probing liveness too
# early would read them as survivors and skip the revive.
_REVIVE_SETTLE_SECS = 3.0


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
    except subprocess.TimeoutExpired:
        typer.echo(
            "fno restart: `fno mux ls --json` gave up after 10s; skipped mux check "
            "(a wedged server? `fno mux doctor`).",
            err=True,
        )
        return None
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    try:
        data = json.loads(proc.stdout or "[]")
    except json.JSONDecodeError:
        return None
    return data if isinstance(data, list) else None


def _agents_rows() -> list[dict[str, Any]]:
    """Registered agents via `fno agents list --json`; [] on any failure."""
    fno = shutil.which("fno")
    if not fno:
        return []
    try:
        proc = subprocess.run(
            [fno, "agents", "list", "--json"], capture_output=True, text=True, timeout=15
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if proc.returncode != 0:
        return []
    try:
        data = json.loads(proc.stdout or "[]")
    except json.JSONDecodeError:
        return []
    if isinstance(data, list):
        rows = data
    elif isinstance(data, dict):
        rows = data.get("agents", [])
    else:
        rows = []
    return [r for r in rows if isinstance(r, dict)]


def _revive_orphans(
    pre_live: dict[str, dict[str, Any]],
    say: Callable[..., None],
    result: dict[str, Any],
) -> None:
    """Respawn workers orphaned by a mux-server kill onto their recorded claude
    sessions. Best-effort: a failed revive is reported in the summary but never
    fails the restart (which already succeeded); revive-in-place refuses to
    double-spawn if the worker is actually still alive."""
    fno = shutil.which("fno")
    if not fno:
        return
    time.sleep(_REVIVE_SETTLE_SECS)
    try:
        subprocess.run([fno, "agents", "reconcile"], capture_output=True, timeout=30)
    except (OSError, subprocess.SubprocessError):
        pass
    now_live = {r.get("name") for r in _agents_rows() if r.get("status") == "live"}
    for name, row in pre_live.items():
        if name in now_live:
            continue
        session = row.get("session_id")
        if row.get("provider") != "claude" or not session:
            result["agents_revive_skipped"].append(name)
            say(
                f"fno restart: worker '{name}' died with its mux server and has no "
                "resumable claude session; not revived.",
                err=True,
            )
            continue
        # Pin the provider explicitly: a bare spawn inherits
        # config.agents.defaults.provider, and a non-claude default makes the
        # spawn seam inject --provider <that>, which the --resume guard then
        # rejects - every revive would fail in such an environment. --substrate
        # bg is what --resume implies; naming it is belt-and-suspenders.
        cmd = [
            fno, "agents", "spawn", "--name", name,
            "--harness", "claude", "--substrate", "bg", "--resume", str(session),
        ]
        if row.get("cwd"):
            cmd += ["--cwd", str(row["cwd"])]
        try:
            rc = subprocess.run(cmd, capture_output=True, text=True, timeout=120).returncode
        except (OSError, subprocess.SubprocessError):
            rc = 1
        if rc == 0:
            result["agents_revived"].append(name)
            say(f"fno restart: revived worker '{name}' onto session {session}.")
        else:
            result["agents_revive_failed"].append(name)
            say(
                f"fno restart: could not revive worker '{name}' (spawn --resume "
                f"exited {rc}); resume it manually: fno agents resume {name}",
                err=True,
            )


def restart_command(
    mux: bool = typer.Option(
        False,
        "--mux",
        help="Also restart live mux servers (DESTRUCTIVE: ends their shells/panes).",
    ),
    revive: bool = typer.Option(
        True,
        "--revive/--no-revive",
        help="After killing a mux server, respawn the claude workers that died with "
        "it onto their recorded sessions (spawn --resume). Workers that survive "
        "(bg substrate) or have no recorded session are left alone.",
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit a single JSON summary on stdout; text to stderr."
    ),
) -> None:
    """Restart running fno processes onto freshly-installed binaries.

    The agents daemon restarts always (PTY workers survive). A stale-wire mux
    server (predates the running binary; unreachable by a new client) is
    auto-restarted; a current-wire server is reported by default and restarted
    only with --mux (killing a server ends its live sessions). Claude workers
    that die with a killed server are respawned onto their recorded sessions
    (--no-revive to skip).
    """
    result: dict[str, Any] = {
        "daemon": None,
        "mux_sessions": [],  # LIVE session names (the restart targets)
        "mux_wedged": [],  # wedged rows: actionable failures (holds socket, not accepting)
        "mux_other": [],  # other non-live rows (stale/unqueryable): reported, never killed
        "mux_restarted": [],
        "agents_revived": [],
        "agents_revive_failed": [],
        "agents_revive_skipped": [],
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
        live_rows = [
            s
            for s in sessions
            if isinstance(s, dict) and s.get("session") and s.get("state") == "live"
        ]
        live = [s["session"] for s in live_rows]
        # (x-1a85) A stale-wire live server predates the running binary (its
        # `.ver` sidecar version != ours, or is absent), so a new client's
        # handshake is REJECTED - it is already unreachable. Restart those
        # UNCONDITIONALLY (killing them loses nothing a current client could
        # still reach, and it heals the pair-deploy skew with no manual --mux).
        # A current-wire server has healthy, reachable panes, so restarting it
        # stays opt-in behind --mux.
        stale_live = [s["session"] for s in live_rows if s.get("stale")]
        current_live = [s["session"] for s in live_rows if not s.get("stale")]
        # A wedged server holds its socket but never accepts (x-82c6): it is a
        # broken server, NOT a benign non-live socket. Reporting it as
        # "restart succeeded" is the ok:true lie this fixes -- surface each one
        # (name + log) and fail the command so `fno restart` exits non-zero.
        wedged = [
            s
            for s in sessions
            if isinstance(s, dict) and s.get("session") and s.get("state") == "wedged"
        ]
        other = [
            s["session"]
            for s in sessions
            if isinstance(s, dict)
            and s.get("session")
            and s.get("state") not in ("live", "wedged")
        ]
        result["mux_sessions"] = live
        result["mux_stale"] = stale_live
        result["mux_wedged"] = [w["session"] for w in wedged]
        result["mux_other"] = other
        for w in wedged:
            name = w["session"]
            log = w.get("log") or "(server log path unknown)"
            say(
                f"fno restart: mux session '{name}' is WEDGED (holds its socket but is not "
                f"accepting connections); the server is stuck. Kill the server process directly "
                f"(its log: {log}).",
                err=True,
            )
            failures.append(f"mux: {name} wedged")
        if other:
            say(f"fno restart: {len(other)} non-live mux row(s) (not restarted): {other}.")

        # Always restart the stale-wire servers; add the current-wire ones only
        # when --mux is given.
        to_restart = list(stale_live) + (current_live if mux else [])
        if not live:
            say("fno restart: no live mux sessions.")
        # Snapshot live workers BEFORE the kill: the kill is what orphans them,
        # so this is the only moment their pre-kill liveness is observable.
        pre_live: dict[str, dict[str, Any]] = {}
        if to_restart and revive:
            pre_live = {
                r["name"]: r
                for r in _agents_rows()
                if r.get("name") and r.get("status") == "live"
            }
        if to_restart:
            fno = shutil.which("fno")
            if not fno:
                say(
                    "fno restart: mux session(s) need restarting but the `fno` mux binary is "
                    "not on PATH; cannot restart them.",
                    err=True,
                )
                failures.append("mux: fno not on PATH")
            else:
                for name in to_restart:
                    reason = "stale wire version" if name in stale_live else "requested"
                    try:
                        kc = subprocess.run(
                            [fno, "mux", "kill-server", name],
                            capture_output=True,
                            text=True,
                            timeout=10,
                        ).returncode
                    except subprocess.TimeoutExpired:
                        say(f"fno restart: gave up on mux session '{name}' after 10s.", err=True)
                        failures.append(f"mux: kill {name} timed out")
                        continue
                    except (OSError, subprocess.SubprocessError):
                        kc = 1
                    if kc == 0:
                        result["mux_restarted"].append(name)
                        say(
                            f"fno restart: mux session '{name}' killed ({reason}); the next "
                            "attach starts a fresh server on the new binary."
                        )
                    else:
                        say(f"fno restart: could not kill mux session '{name}' (exit {kc}).", err=True)
                        failures.append(f"mux: kill {name} exit {kc}")
        if revive and result["mux_restarted"] and pre_live:
            _revive_orphans(pre_live, say, result)
        # Current-wire servers left running (opt-in): report so the operator can
        # restart them deliberately. Skipped when --mux already restarted them.
        if current_live and not mux:
            say(
                f"fno restart: {len(current_live)} live mux session(s) on the current wire: "
                f"{current_live}. Killing one ends its shells/panes; that stays opt-in. Do it "
                "with `fno mux kill-server <name>`, or `fno restart --mux` for all."
            )

    result["ok"] = not failures
    if json_out:
        typer.echo(json.dumps(result))
    if failures:
        raise typer.Exit(1)

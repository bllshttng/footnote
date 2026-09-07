"""Spawn bridges that delegate a worker spawn to the fno-agents Rust client.

The held app-server child and the registry row are owned by the Rust runtime,
so both runtimes share recovery and full-session identity semantics. Each
bridge builds one client argv, overlays the caller's env, and parses the
client's JSON receipt.
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Mapping, Optional


def _opencode_serve_spawn(
    *,
    name: str,
    message: str,
    cwd: Path,
    from_name: str,
    model: Optional[str],
    node: Optional[str] = None,
) -> str:
    """Delegate an opencode bg spawn to the Rust serve lane; return short_id.

    The lane (shared serve, session mint, writable-dirs grant, registry row,
    detached writer) is implemented once, in the fno-agents runtime; forking it
    here would fork the registry contract too. The subprocess runs while this
    process holds the per-agent flock, which is safe: the serve dispatch takes
    no per-agent lock, only its serve-boot sidecar.
    """
    from fno.agents.dispatch import DispatchAskError

    from fno import rust_binary

    binary = rust_binary.resolve_binary()
    if binary is None:
        raise DispatchAskError(
            "opencode bg spawn needs the fno-agents runtime; install it "
            "(cargo build --release -p fno-agents) or use --substrate pane",
            exit_code=13,
        )
    argv = [
        str(binary),
        "spawn",
        "--name",
        name,
        "--harness",
        "opencode",
        "--substrate",
        "bg",
        "--cwd",
        str(cwd),
    ]
    if from_name:
        argv += [f"--from-name={from_name}"]
    if model:
        argv += [f"--model={model}"]
    if node:
        argv += ["--node", node]
    # The seed rides as the fenced positional tail, never a bare flag value:
    # with --name set the whole tail is the message, and a hyphen-leading
    # message as the value of `--message <msg>` would die as an unknown flag
    # (the argv-fence gate's exact trap).
    argv += ["--", message]
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, timeout=180)
    except subprocess.TimeoutExpired as exc:
        raise DispatchAskError(
            "opencode serve spawn timed out after 180s", exit_code=2
        ) from exc
    if proc.returncode != 0:
        detail = proc.stderr.strip() or proc.stdout.strip() or f"exit {proc.returncode}"
        raise DispatchAskError(
            f"opencode serve spawn failed: {detail}", exit_code=2
        )
    try:
        receipt = json.loads(proc.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError) as exc:
        raise DispatchAskError(
            f"opencode serve spawn printed no receipt: {proc.stdout[:200]!r}",
            exit_code=2,
        ) from exc
    short_id = receipt.get("short_id") or receipt.get("session_id")
    if not short_id:
        raise DispatchAskError(
            f"opencode serve receipt carries no session id: {receipt!r}", exit_code=2
        )
    return str(short_id)


def _codex_thread_spawn(
    name: str,
    message: str,
    cwd: Path,
    from_name: str,
    model: Optional[str],
    yolo: bool,
    node: Optional[str] = None,
    account_env: Optional[Mapping[str, str]] = None,
    route_env: Optional[Mapping[str, str]] = None,
) -> str:
    """Delegate a Codex thread spawn to the Rust daemon lane.

    The Python runtime remains a compatibility front door; the held app-server
    child and registry row are owned by the Rust supervisor so both runtimes
    share recovery and full-session identity semantics.

    ``account_env``/``route_env`` overlay the client subprocess environment.
    They reach the app-server child only when this client also lazy-starts the
    daemon (the child inherits the daemon's env); a warm daemon keeps its own.
    The two state roots that have an env carrier are pinned around the overlay
    by ``seal_state_root``; that docstring names what still follows HOME.
    """
    from fno.agents.account_env import seal_state_root
    from fno.agents.dispatch import DispatchAskError

    from fno import rust_binary

    binary = rust_binary.resolve_binary()
    if binary is None:
        raise DispatchAskError(
            "codex thread spawn needs the fno-agents runtime; install it "
            "(cargo build --release -p fno-agents) or use --substrate pane",
            exit_code=13,
        )
    argv = [
        str(binary),
        "spawn",
        "--name",
        name,
        "--harness",
        "codex",
        "--substrate",
        "thread",
        "--cwd",
        str(cwd),
    ]
    if from_name:
        argv += [f"--from-name={from_name}"]
    if model:
        argv += [f"--model={model}"]
    if yolo:
        argv += ["--yolo"]
    if node:
        argv += ["--node", node]
    argv += ["--", message]
    env = dict(os.environ)
    for overlay in (route_env, account_env):
        if overlay:
            env.update(overlay)
    # This client IS a footnote process and a non-claude oauth_dir overlay is a
    # HOME override, so seal it or the client's own reads resolve under the
    # account's home and go unfindable (x-c33e). Env is the only channel to the
    # app-server child, so the override stays; seal_state_root's docstring says
    # what that leaves open.
    env = seal_state_root(env)
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, timeout=180, env=env)
    except subprocess.TimeoutExpired as exc:
        raise DispatchAskError(
            "codex thread spawn timed out after 180s", exit_code=2
        ) from exc
    if proc.returncode != 0:
        detail = proc.stderr.strip() or proc.stdout.strip() or f"exit {proc.returncode}"
        raise DispatchAskError(f"codex thread spawn failed: {detail}", exit_code=2)
    try:
        receipt = json.loads(proc.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError) as exc:
        raise DispatchAskError(
            f"codex thread spawn printed no receipt: {proc.stdout[:200]!r}",
            exit_code=2,
        ) from exc
    session_id = (
        receipt.get("harness_session_id")
        or receipt.get("session_id")
        or receipt.get("short_id")
    )
    if not session_id:
        raise DispatchAskError(
            f"codex thread receipt carries no full session id: {receipt!r}",
            exit_code=2,
        )
    return str(session_id)

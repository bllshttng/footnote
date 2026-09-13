"""Helpers the codex pane lane needs for daemon-owned threads.

Remote Control is served by the shared ``codex app-server`` daemon, and a
daemon can load only threads it owns. A TUI launched bare owns its thread
in-process, so the pane lane starts the daemon (the interactive_create
form's ``pre_exec``) and launches against it (``--remote unix://``).
Measured 2026-09-13 on codex-cli 0.154.0.
"""

from __future__ import annotations

import json
import re
import subprocess
from typing import Callable, Optional, Sequence

from fno.agents.dispatch import DispatchAskError

#: `daemon start` is a no-op when a daemon is already running, so the bound
#: only has to cover a cold start.
_CODEX_DAEMON_START_TIMEOUT_S = 15

#: A key outside this shape would write a different config path: dotted or
#: quoted keys are table paths in TOML, not one leaf.
_ENV_KEY_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def ensure_codex_daemon(
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
    env: Optional[dict[str, str]] = None,
) -> None:
    """Start (or confirm) the shared app-server daemon before a codex pane.

    The command is read from the codex interactive_create form's
    ``pre_exec`` through ``capabilities("codex")``, never hardcoded: the
    capability toml is the one place the daemon contract is declared. A
    failure raises before any pane exists, so no TUI opens to mint a thread
    the daemon could never serve.
    """
    from fno.agents.harness_map import capabilities

    form = capabilities("codex")["resume_strategy"]["forms"]["interactive_create"]
    cmd = [str(tok) for tok in (form.get("pre_exec") or [])]
    display = " ".join(cmd)
    if not cmd:
        raise DispatchAskError(
            "codex interactive_create declares no pre_exec daemon start; the "
            "pane would mint a thread the shared app-server daemon cannot "
            "serve",
            exit_code=2,
        )
    try:
        proc = runner(
            cmd,
            capture_output=True,
            text=True,
            timeout=_CODEX_DAEMON_START_TIMEOUT_S,
            **({"env": env} if env is not None else {}),
        )
    except subprocess.TimeoutExpired:
        raise DispatchAskError(
            f"codex pane launch refused: `{display}` timed out after "
            f"{_CODEX_DAEMON_START_TIMEOUT_S}s; the shared app-server daemon "
            "did not come up",
            exit_code=2,
        ) from None
    except OSError as exc:
        raise DispatchAskError(
            f"codex pane launch refused: `{display}` failed: {exc}",
            exit_code=2,
        ) from None
    if proc.returncode != 0:
        stderr = (proc.stderr or proc.stdout or "no output").strip()
        raise DispatchAskError(
            f"codex pane launch refused: `{display}` exited "
            f"{proc.returncode}: {stderr}",
            exit_code=2,
        )


def codex_shell_env_args(pairs: Sequence[str]) -> list[str]:
    """Render ``K=V`` pairs as config-set leaves the daemon delivers to tools.

    A daemon-run tool inherits the daemon's environment, not the TUI's, so
    the env(1) wrapper's worker identity stops at the TUI (measured
    2026-09-13). Each pair becomes
    ``-c shell_environment_policy.set.K="<V>"``; the JSON string is a valid
    TOML basic string, and the leaf merges with the
    ``[shell_environment_policy.set]`` table in config.toml (measured).
    """
    args: list[str] = []
    for pair in pairs:
        key, sep, value = pair.partition("=")
        if not sep or not _ENV_KEY_RE.fullmatch(key):
            raise DispatchAskError(
                f"refusing mesh pair {pair!r}: the key must match "
                "[A-Za-z_][A-Za-z0-9_]* to name one "
                "shell_environment_policy config path",
                exit_code=2,
            )
        args += ["-c", f"shell_environment_policy.set.{key}={json.dumps(value)}"]
    return args

"""The codex pane lane's daemon-owned-thread helpers: start the shared
``codex app-server`` daemon, launch against it, name the worker to its
tools."""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
from pathlib import Path
from typing import Callable, Optional, Sequence

from fno.agents.dispatch import DispatchAskError

_CODEX_DAEMON_START_TIMEOUT_S = 15  # `daemon start` no-ops when running.

_ENV_KEY_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def ensure_codex_daemon(
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
    env: Optional[dict[str, str]] = None,
) -> None:
    """Start (or confirm) the shared app-server daemon before a codex pane.

    The command is the create form's ``pre_exec``; a failure raises before
    any pane exists.
    """
    from fno.agents.harness_map import capabilities

    form = capabilities("codex")["resume_strategy"]["forms"]["interactive_create"]
    cmd = [str(tok) for tok in (form.get("pre_exec") or [])]
    display = " ".join(cmd)
    if not cmd:
        raise DispatchAskError(
            "codex interactive_create declares no pre_exec daemon start; the "
            "pane would mint a thread the shared app-server daemon cannot serve",
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
            f"{_CODEX_DAEMON_START_TIMEOUT_S}s",
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
    """Render the worker's own identity as ONE ``-c`` config-set leaf.

    A daemon-run tool inherits the daemon's env, not the TUI's. Measured
    2026-09-13 on codex-cli 0.154.0: only the FIRST ``-c`` per config key
    applies, so one leaf carries ``FNO_AGENT_SELF`` and merges with the
    operator's set table; the other pairs stay on the env(1) wrapper.
    """
    chosen = next((p for p in pairs if p.startswith("FNO_AGENT_SELF=")), None)
    chosen = chosen or (pairs[0] if pairs else "")
    if not chosen:
        return []
    key, sep, value = chosen.partition("=")
    if not sep or not _ENV_KEY_RE.fullmatch(key):
        raise DispatchAskError(
            f"refusing mesh pair {chosen!r}: the key must match "
            "[A-Za-z_][A-Za-z0-9_]* to name one "
            "shell_environment_policy config path",
            exit_code=2,
        )
    return ["-c", f"shell_environment_policy.set.{key}={json.dumps(value)}"]


_CODEX_DAEMON_PROBE_INTERVAL_S = 2.0  # a websocket round trip; rate-limit it


def _codex_session_ids_loaded(
    cwd: Path, *, codex_home: Optional[Path] = None
) -> Optional[set[str]]:
    """Session ids the app-server daemon reports as loaded for ``cwd``.

    None (daemon could not answer) is distinct from an empty set, so an
    unreachable daemon never reads as "nothing new here". Paths compare
    ``realpath``-normalized: the daemon may report a symlink-resolved cwd.
    """
    from fno.agents.discover import _codex_daemon_threads_raw

    if codex_home is None:
        threads = _codex_daemon_threads_raw()
    else:
        daemon_env = dict(os.environ)
        daemon_env["CODEX_HOME"] = str(codex_home)
        threads = _codex_daemon_threads_raw(env=daemon_env)
    if threads is None:
        return None
    resolved_cwd = os.path.realpath(str(cwd))
    ids: set[str] = set()
    for thread in threads:
        if not isinstance(thread, dict):
            continue
        sid = thread.get("session_id")
        thread_cwd = thread.get("cwd")
        if (
            isinstance(sid, str)
            and sid
            and isinstance(thread_cwd, str)
            and os.path.realpath(thread_cwd) == resolved_cwd
        ):
            ids.add(sid)
    return ids


def _codex_daemon_candidate(
    cwd: Path,
    baseline_ids: Optional[set[str]],
    *,
    codex_home: Optional[Path] = None,
    observed: Optional[list] = None,
) -> Optional[str]:
    """The single session id the app-server daemon reports as new for ``cwd``.

    This arm binds on the modern pane lane: the TUI launches with
    ``--remote unix://``, the thread is the daemon's, and the fd probe finds
    no rollout in the pane's own tree. The fd probe still runs first for any
    build that owns its rollout (and for ``codex exec``).

    ``baseline_ids`` None (no pre-spawn snapshot) is refused, never treated
    as empty: an empty stand-in would let any loaded stranger for this cwd
    read as "the one new id". Only ONE new id answers - two or more is a
    sibling race, and a healthy stranger is worse than unbound. Single-shot
    read; the caller owns the repeat-before-trust gate. ``observed`` is the
    out-of-band sink for WHY a poll declined, written in place so it holds
    the last observation: a blown window must name what it saw, not the
    clock.
    """

    def _note(text: str) -> None:
        if observed is not None:
            observed[:] = [text]

    if baseline_ids is None:
        # No window fixes this: the oracle is off for the whole spawn.
        _note("daemon: no pre-spawn baseline, oracle disabled for this spawn")
        return None
    loaded = (
        _codex_session_ids_loaded(cwd)
        if codex_home is None
        else _codex_session_ids_loaded(cwd, codex_home=codex_home)
    )
    if loaded is None:
        _note("daemon: app-server unreachable")
        return None
    new_ids = loaded - baseline_ids
    if len(new_ids) == 1:
        return next(iter(new_ids))
    if not new_ids:
        _note("daemon: no new codex session for this cwd")
    else:
        _note(f"daemon: {len(new_ids)} new codex sessions for this cwd, ambiguous")
    return None


def _make_codex_bind_probe(
    *,
    cwd: Path,
    spawn_started_ms: int,
    child_pid: int,
    codex_sessions_dir: Optional[Path],
    daemon_baseline_ids: Optional[set[str]],
    mux: dict,
    runner: Callable[..., "subprocess.CompletedProcess[str]"],
    oracle_used: Optional[list] = None,
    daemon_codex_home: Optional[Path] = None,
    condition: Optional[list] = None,
) -> Callable[[], Optional[str]]:
    """Build the two-oracle ``bind_probe`` :func:`_await_pane_binding` polls.

    Rollout-fd first (still correct for any build that owns its rollout, and
    for ``codex exec``), then the daemon oracle rate-limited to one call per
    ``_CODEX_DAEMON_PROBE_INTERVAL_S``. The daemon path needs the SAME
    single candidate on two consecutive probes before it binds: two panes
    racing into one cwd can otherwise show only one of them as "the new id"
    on a given poll, and the repeat turns a false single candidate into a
    correctly-ambiguous pair. A daemon-sourced candidate is also
    liveness-checked, because the detached daemon seeing a session does not
    prove the pane is still up. ``condition`` collects WHY a poll declined,
    newest last, so a blown window names a condition, not a clock.
    """
    used = oracle_used if oracle_used is not None else []
    last_probe_s = [0.0]
    prev_candidate: list = [None]

    def _mark_oracle(name: str) -> None:
        used[:] = [name]

    def _note(text: str) -> None:
        if condition is not None:
            condition[:] = [text]

    def _probe() -> Optional[str]:
        # Lazy: mux_spawn imports this module; a top-level import loops.
        from fno.agents.mux_spawn import (
            _PROBE_TIMEOUT_S,
            _backfill_codex_session_id,
            _mux_pane_alive,
        )

        sid = _backfill_codex_session_id(
            cwd,
            spawn_started_ms,
            sessions_dir=codex_sessions_dir,
            child_pid=child_pid,
            attempts=1,
        )
        if sid:
            _mark_oracle("rollout-fd")
            return sid
        _note(f"fd: no codex rollout on pane child pid {child_pid}")
        now_s = time.monotonic()
        if now_s - last_probe_s[0] < _CODEX_DAEMON_PROBE_INTERVAL_S:
            # A rate-limit skip is not an observation; the fd note stands.
            return None
        last_probe_s[0] = now_s
        candidate = _codex_daemon_candidate(
            cwd,
            daemon_baseline_ids,
            codex_home=daemon_codex_home,
            observed=condition,
        )
        if candidate is None or candidate != prev_candidate[0]:
            if candidate is not None:
                _note(f"daemon: candidate {candidate} awaiting a repeat probe")
            prev_candidate[0] = candidate
            return None
        # The candidate repeated - trust it unless the pane is gone.
        if _mux_pane_alive(mux, runner, timeout=_PROBE_TIMEOUT_S) is False:
            _note("daemon: candidate dropped, pane already gone")
            return None
        _mark_oracle("daemon")
        return candidate

    return _probe

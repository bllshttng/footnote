"""Helpers the codex pane lane needs for daemon-owned threads.

Remote Control is served by the shared ``codex app-server`` daemon, and a
daemon can load only threads it owns. A TUI launched bare owns its thread
in-process, so the pane lane starts the daemon (the interactive_create
form's ``pre_exec``) and launches against it (``--remote unix://``).
Measured 2026-09-13 on codex-cli 0.154.0.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
from pathlib import Path
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


#: The rollout-fd probe (in mux_spawn) stays cheap because it is a local
#: process-tree walk. The daemon oracle below costs a subprocess plus a
#: websocket round trip, so the binding loop's caller rate-limits it to this
#: interval even though the loop itself ticks every ``_BINDING_POLL_S``
#: (0.75s).
_CODEX_DAEMON_PROBE_INTERVAL_S = 2.0


def _codex_session_ids_loaded(
    cwd: Path, *, codex_home: Optional[Path] = None
) -> Optional[set[str]]:
    """Session ids the app-server daemon reports as loaded for ``cwd``.

    None means the daemon could not answer (missing binary, dead socket,
    timeout, malformed reply) - distinct from an empty set, which means it
    answered and this cwd currently has zero loaded threads. The correlation
    in :func:`_codex_daemon_candidate` needs that distinction: an unreachable
    daemon must leave the fd oracle deciding alone, never manufacture a false
    "nothing new here".

    Compares ``realpath``-normalized paths, not raw strings: the daemon may
    report a symlink-resolved cwd (e.g. macOS's ``/tmp`` -> ``/private/tmp``)
    that never equals our own unresolved ``str(cwd)``, which would silently
    zero out this oracle for every thread and reintroduce the 0/20-bind
    defect this correlation exists to fix.
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

    This is the oracle that binds on the modern pane lane. Since x-a095 the
    pane TUI launches with ``--remote unix://`` against the shared daemon, so
    the thread is the daemon's: the fd probe finds no rollout in the pane's
    own process tree (measured 2026-09-13 on codex-cli 0.154.0, daemon
    0.153.4: 0 rollout fds in the pane tree, thread 01a09c05 in the daemon's
    loaded set 4.3s after launch), and this oracle answers instead, via the
    already-wired :func:`fno.agents.discover._discover_from_codex_daemon`
    RPC. The fd probe still runs first for any build that owns its rollout
    (and for ``codex exec``); on the remote lane it declines and this arm
    decides. The condition sink says out loud which of the two answered, so a
    blown window names the arm that failed rather than the clock.

    ``baseline_ids`` is None when the pre-spawn snapshot could not be taken
    (daemon unreachable at that moment) - refused outright rather than
    treated as an empty set, because an empty stand-in would let ANY
    already-loaded stranger session for this cwd read as "the one new id"
    and bind to it.

    Accepts only when exactly ONE session id is new for this cwd since the
    baseline - the same rule :func:`_codex_session_id_for_pid` applies to a
    same-cwd sibling and the comment at line ~3160 states for the no-pid
    fallback: binding this row to a healthy stranger is worse than leaving it
    unbound, so two-or-more new ids keeps polling rather than guesses.

    This is a single-shot read with no cross-call memory; the caller
    (:func:`_make_codex_bind_probe`) is the one that requires the same
    candidate to repeat before trusting it.

    ``observed`` is an out-of-band sink for the CONDITION behind a decline,
    following the ``oracle_used`` idiom below. Four different states all return
    the same bare ``None`` here, and a bind window that expires after 60s of
    them used to report only the clock - which is how four measured failures
    produced one word and three wrong hypotheses. Written in place
    (``observed[:] = [...]``) so it always holds the LAST observation rather
    than growing across a poll. Never a return-value change: ``observed=None``
    leaves behavior byte-identical for every existing caller.
    """

    def _note(text: str) -> None:
        if observed is not None:
            observed[:] = [text]

    if baseline_ids is None:
        # No window length can fix this one: the daemon oracle is disabled for
        # the whole spawn, so saying so beats any timeout.
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

    Tries the rollout-fd probe first (cheap, still correct for any build that
    owns its rollout, and for ``codex exec``). Falls back to the app-server
    daemon oracle, rate-limited to one call per
    ``_CODEX_DAEMON_PROBE_INTERVAL_S`` even though the binding loop itself
    ticks every ``_BINDING_POLL_S``.

    The daemon path is stability-gated the same way
    :func:`_backfill_codex_session_id`'s no-pid fallback is: the SAME single
    candidate must repeat across two consecutive daemon probes before it
    binds. Two panes racing into one cwd can otherwise show only one of them
    as "the new id" on a given poll (the other's session not yet registered
    with the daemon) - requiring a repeat means that by the second probe,
    2s later, the correlating pane's own session has very likely also
    registered, turning a false single candidate into a correctly-ambiguous
    pair rather than a mis-bind.

    A daemon-sourced candidate is also liveness-checked before it is
    trusted: unlike the fd oracle (whose id can only come from a live
    process's open file), the daemon is detached from the pane, so seeing a
    session there does not by itself prove the pane is still up. Shared with
    ``fno doctor --codex-bind`` so a regression here shows up on the canary,
    not silently.

    ``condition`` is the out-of-band sink carrying WHY a poll declined, so the
    refusal a blown window raises can name a condition instead of a clock. Each
    decline overwrites the previous one, so the sink holds the newest
    observation when the window closes.
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
        # Lazy: mux_spawn imports this module for its re-exports, so a
        # module-level import here would be circular.
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
            # A rate-limit skip is not an observation, so the fd note above
            # stands as the newest thing anything actually looked at.
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
        # The same single candidate repeated - trust it, unless the mux has
        # just told us the pane itself is already gone.
        if _mux_pane_alive(mux, runner, timeout=_PROBE_TIMEOUT_S) is False:
            _note("daemon: candidate dropped, pane already gone")
            return None
        _mark_oracle("daemon")
        return candidate

    return _probe

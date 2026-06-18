"""fno.agents.providers.codex - codex exec adapter for US4.

Surface:

- :func:`create` - invoke ``codex exec --json --cd <cwd> ...``, parse the JSONL
  stream, capture session_id from the ``thread.started`` event and last
  assistant text from the ``item.completed`` (item.type=``agent_message``)
  events. Returns :class:`CodexResult`.
- :func:`resume` - invoke ``codex exec resume <session_id> --json ...`` from
  the registry-recorded cwd. Same JSONL loop; session_id is NOT re-captured.
- :func:`inject_from_name` - bracket-prefix prompt injection.
- :func:`sandbox_flag` - argv tokens for the sandbox mode (mutually exclusive
  with the ``--dangerously-bypass-...`` flag).

Locked Decision 13: the _EVENT_TYPES / _ITEM_TYPES dicts below are pinned
from a real capture by ``scripts/smoke/capture-codex-jsonl.sh`` against
codex 0.130.0. Parser code references these by key; literal string drift
in codex's vocabulary is meant to surface via the smoke integration test
(Wave 2.2), not be papered over inline.

Locked Decision 12: ``stderr=subprocess.STDOUT`` merges codex's stderr into
the stdout pipe so a single drainer handles both. Two-pipe deadlock (which
sigma-review caught on PR #299's claude logs --follow path) is avoided by
construction.
"""
from __future__ import annotations

import json
import os
import signal as _signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

from fno.agents.providers.base import ReachabilityProbeError


# Pinned from a real codex 0.130.0 capture (scripts/smoke/capture-codex-jsonl.sh).
# DO NOT reference these literal string values outside this constants block;
# parser code below references _EVENT_TYPES / _ITEM_TYPES by key. A regression
# test asserts the values do not appear elsewhere in this file.
_EVENT_TYPES = {
    "session": "thread.started",       # carries .thread_id (UUID)
    "complete": "turn.completed",      # end-of-turn; break the read loop
    "item_envelope": "item.completed", # discriminator: see .item.type
}

_ITEM_TYPES = {
    "message": "agent_message",   # has .item.text (assistant reply text)
    "error": "error",             # has .item.message (soft, not always fatal)
}


# Indirection so unit tests can monkeypatch subprocess.Popen without
# touching the global module's API surface.
_subprocess_popen = subprocess.Popen


@dataclass(frozen=True)
class CodexResult:
    """Return shape for :func:`create` / :func:`resume`.

    Attributes:
        exit_code: codex subprocess exit code (0 on happy path).
        session_id: UUID captured from ``thread.started`` (None on resume,
            since the caller already has it).
        last_msg: last assistant text seen on the stream; "" if no
            ``agent_message`` event fired.
        duration_ms: wall-clock elapsed since Popen returned.
    """

    exit_code: int
    session_id: Optional[str]
    last_msg: str
    duration_ms: int


class CodexInvocationError(RuntimeError):
    """codex exited non-zero and no assistant reply was captured.

    Carries the exit code so the dispatcher can propagate it to the shell.
    """

    def __init__(self, exit_code: int) -> None:
        super().__init__(f"codex exited {exit_code} with no captured reply")
        self.exit_code = exit_code


class NoSessionIdError(RuntimeError):
    """codex JSONL stream ended without a ``thread.started`` event.

    Locked Decision 14 warn-on-drift: carries the set of event-type names
    that DID appear so the dispatcher can surface forensic info to the
    operator. Exit 11 still fires after the warning.
    """

    def __init__(self, types_seen: set[str]) -> None:
        expected = _EVENT_TYPES["session"]
        self.types_seen = types_seen
        super().__init__(
            f"codex did not emit session id; saw events: "
            f"{sorted(types_seen)}; expected one of: [{expected!r}]"
        )


class CodexTimeoutError(RuntimeError):
    """codex did not finish within the configured timeout."""

    def __init__(self, timeout_sec: float) -> None:
        super().__init__(f"codex timed out after {timeout_sec}s")
        self.timeout_sec = timeout_sec


def inject_from_name(prompt: str, from_name: str) -> str:
    """Return ``"[from: <from_name>]\\n\\n<prompt>"`` (Locked Decision 7).

    The caller is responsible for validating ``from_name`` upstream (the
    dispatch layer's US2 validator runs before this function is called).
    This is a pure string concatenation; the prefix is the documented
    contract codex's prompt model sees.
    """
    return f"[from: {from_name}]\n\n{prompt}"


def sandbox_flag(yolo: bool) -> list[str]:
    """Return the argv tokens selecting codex's create-path SANDBOX posture.

    ``--sandbox`` is an ``exec``-subcommand flag, so these tokens go AFTER
    ``exec`` in the argv. The approval policy is a separate GLOBAL flag emitted
    before ``exec`` - see :func:`approval_flag`.

    - bounded (``yolo=False``, default): ``--sandbox workspace-write`` -
      workspace sandbox.
    - full yolo (``yolo=True``, explicit opt-in):
      ``--dangerously-bypass-approvals-and-sandbox`` - unsandboxed bypass. The
      two are mutually exclusive; never combine the workspace sandbox with the
      bypass.
    """
    if yolo:
        return ["--dangerously-bypass-approvals-and-sandbox"]
    return ["--sandbox", "workspace-write"]


def approval_flag(yolo: bool) -> list[str]:
    """Return the argv tokens selecting codex's create-path APPROVAL policy.

    ``--ask-for-approval never`` means "never ask; a blocked action is returned
    to the model", so the bounded posture never prompts (no hang).

    - bounded (``yolo=False``, default): ``--ask-for-approval never``.
    - full yolo (``yolo=True``): ``[]`` - the bypass flag emitted by
      :func:`sandbox_flag` already disables approval, so this stays empty.

    CRITICAL: in codex >= 0.133.0 ``-a/--ask-for-approval`` is a GLOBAL flag on
    the top-level ``codex`` command, NOT a flag on the ``exec`` subcommand. It
    MUST be emitted BEFORE ``exec`` in the argv; placing it after ``exec`` makes
    clap reject it with ``error: unexpected argument '--ask-for-approval'
    found``, aborting the spawn before any session id is emitted.

    Note: ``codex exec resume`` accepts neither ``--sandbox`` nor
    ``--ask-for-approval``; resume INHERITS the create-time posture, so neither
    this nor :func:`sandbox_flag_resume` emits approval tokens on resume.
    """
    if yolo:
        return []
    return ["--ask-for-approval", "never"]


def sandbox_flag_resume(yolo: bool) -> list[str]:
    """Return the argv tokens for codex resume's restricted sandbox surface.

    Resume only supports ``--dangerously-bypass-approvals-and-sandbox`` (no
    ``--sandbox`` flag on the resume subcommand). When ``yolo=False`` the
    session's original sandbox mode applies, so this returns an empty list.
    """
    if yolo:
        return ["--dangerously-bypass-approvals-and-sandbox"]
    return []


def _effective_yolo(yolo: bool, headless_yolo: Optional[bool] = None) -> bool:
    """Resolve the effective sandbox-bypass for the autonomous exec lane (ab-994222ee).

    The create/resume path is the headless (MODE==exec) lane: a worker no
    operator is watching. Returns whether this launch is FULL yolo (unsandboxed
    bypass) vs the BOUNDED default (sandboxed + never-prompt). Both never prompt,
    so a headless codex cannot wedge on an approval either way; the bounded
    default additionally keeps the workspace sandbox. ``config.agents.codex.
    headless_yolo: true`` opts into full yolo.

    ``headless_yolo`` is the full-yolo selector, injectable for deterministic
    tests; ``None`` reads it from config (degrading to the hang-safe BOUNDED
    default False). An explicit caller ``yolo=True`` always wins. The
    interactive ``host``/``drive`` lane does NOT call this (a human is driving),
    so the posture is correctly scoped to autonomous workers only.
    """
    if headless_yolo is None:
        # Best-effort: the config read pulls in fno.config (pydantic).
        # Degrade to the hang-safe BOUNDED default (False) on ANY failure -
        # including an ImportError when the provider runs in a minimal env
        # without the config deps (e.g. the bare-python3 Rust<->Python parity
        # harness) - so create()/resume() never crash on the config lookup.
        try:
            from fno.config import agents_headless_yolo

            headless_yolo = agents_headless_yolo("codex")
        except Exception:
            headless_yolo = False
    return yolo or headless_yolo


def _open_tee(log_path: Path):
    """Open the JSONL tee in append mode, line-buffered, ensuring parent exists.

    Locked Decision 8: ``output.jsonl`` is the read source for
    ``fno agents logs <name>`` (US3) so append-only / line-buffered is
    the contract a tail reader can rely on.

    Raises OSError (incl. PermissionError, FileNotFoundError) on mkdir or
    open failure. Callers in :func:`_run_codex` wrap the call in
    try/except and map to :class:`CodexInvocationError` so structured
    exit codes survive to the dispatch layer.
    """
    log_path.parent.mkdir(parents=True, exist_ok=True)
    return open(log_path, "a", buffering=1, encoding="utf-8")


def _parse_stream(
    proc: subprocess.Popen,
    tee_fh,
    types_seen: set[str],
    on_warning: Callable[[str], None],
) -> tuple[Optional[str], str, str, bool]:
    """Drain proc.stdout line-by-line until turn.completed or EOF.

    Returns:
        ``(session_id, last_msg, last_error_msg, broke_on_complete)``.

    Every line is tee'd before any control-flow logic runs. Tee write
    failures are reported via ``on_warning`` once per ``(errno, strerror)``
    tuple so a recurring failure mode warns ONCE but subsequent writes
    that fail with a DIFFERENT errno still surface (e.g. ENOSPC followed
    by EACCES). Tee failures NEVER crash the dispatch — the user-facing
    reply takes precedence over log persistence.

    ``last_error_msg`` captures the most recent ``item.completed`` /
    ``item.type=error`` envelope's ``.item.message`` so callers can
    surface a soft-error diagnostic when the model never emitted an
    ``agent_message`` but codex still exited 0 (silent-failure-hunter
    finding row 4: a soft error item with no model reply was previously
    only visible in the tee file, not the return value).
    """
    session_id: Optional[str] = None
    last_msg: str = ""
    last_error_msg: str = ""
    broke_on_complete = False
    # Track which (errno, strerror) tuples we've already warned about so
    # a recurring failure warns ONCE per unique mode but distinct modes
    # each get their own surface.
    tee_warned_modes: set[tuple] = set()

    assert proc.stdout is not None
    for raw in iter(proc.stdout.readline, ""):
        if not raw:
            break
        # Tee EVERY line (Locked Decision 8); failure is non-fatal.
        try:
            tee_fh.write(raw)
            tee_fh.flush()
        except OSError as exc:
            mode = (exc.errno, str(exc))
            if mode not in tee_warned_modes:
                tee_warned_modes.add(mode)
                on_warning(f"codex provider: tee write failed: {exc}")
            # Continue draining the pipe even if the tee is degraded;
            # the user-facing reply is what matters.

        line = raw.rstrip("\n")
        if not line or not line.startswith("{"):
            # codex emits non-JSON banner lines (e.g. "Reading additional
            # input from stdin...") and may emit Rust panics on stderr-
            # merged stdout. Skip them for control flow; the tee preserves
            # them for forensics.
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue

        if not isinstance(event, dict):
            continue

        ev_type = event.get("type")
        if isinstance(ev_type, str):
            types_seen.add(ev_type)

        if ev_type == _EVENT_TYPES["session"]:
            tid = event.get("thread_id")
            # cv-dcd823ce: an EMPTY thread_id ("") must NOT be captured as the
            # session id. Capturing it writes codex_session_id="" to the
            # registry and makes every subsequent resume fail opaquely with
            # "no codex_session_id; cannot follow up". `types_seen` already
            # recorded "thread.started" above, so an all-empty stream still
            # fails closed with NoSessionIdError (exit 11). (Mirrored in
            # codex_ask.rs's run_codex capture guard.)
            if isinstance(tid, str) and tid and session_id is None:
                session_id = tid
        elif ev_type == _EVENT_TYPES["item_envelope"]:
            item = event.get("item")
            if isinstance(item, dict):
                item_type = item.get("type")
                if item_type == _ITEM_TYPES["message"]:
                    text = item.get("text")
                    if isinstance(text, str):
                        last_msg = text
                elif item_type == _ITEM_TYPES["error"]:
                    # Soft error items (e.g. codex_hooks deprecation) are
                    # not fatal — codex still emits turn.completed and
                    # exits 0. Capture the message so callers can surface
                    # it if no agent_message ever arrived.
                    err_text = item.get("message")
                    if isinstance(err_text, str):
                        last_error_msg = err_text
        elif ev_type == _EVENT_TYPES["complete"]:
            broke_on_complete = True
            break

    return session_id, last_msg, last_error_msg, broke_on_complete


def _wait_with_grace(
    proc: subprocess.Popen, grace_sec: float = 5.0
) -> tuple[int, bool]:
    """Wait for proc to exit; SIGTERM after grace, SIGKILL after a further 5s.

    Returns ``(exit_code, sigkill_escalated)`` where ``sigkill_escalated``
    is True iff the function had to send SIGKILL (force-kill) to reap the
    process. Callers MUST honor the escalation flag: a partial captured
    reply combined with a force-kill is NOT a successful invocation, no
    matter what the exit code resembles. silent-failure-hunter row 4
    caught the prior bug where a partial agent_message + SIGKILL exit
    surfaced as a successful CodexResult.

    Termination signals target the codex PROCESS GROUP (see
    ``start_new_session=True`` on the Popen call). ``proc.terminate()`` /
    ``proc.kill()`` would only signal the wrapper bash and leave sandbox
    subshells orphaned, blocking the read loop on a never-EOF'd pipe.
    Gemini code review on PR #305 caught the inconsistency between this
    function and the watchdog / KeyboardInterrupt branches.
    """
    def _killpg(sig) -> None:
        try:
            os.killpg(os.getpgid(proc.pid), sig)
        except (OSError, ProcessLookupError):
            # Process group already gone (race with normal exit, or
            # already reaped by an earlier signal).
            pass

    try:
        return (proc.wait(timeout=grace_sec), False)
    except subprocess.TimeoutExpired:
        _killpg(_signal.SIGTERM)
        try:
            return (proc.wait(timeout=5.0), False)
        except subprocess.TimeoutExpired:
            _killpg(_signal.SIGKILL)
            try:
                return (proc.wait(timeout=2.0), True)
            except subprocess.TimeoutExpired:
                # Last resort: kernel hasn't reaped after SIGKILL+2s
                # (extremely rare). Report sentinel + escalation flag.
                return (-9, True)


def _stderr_warn(msg: str) -> None:
    """Emit a single-line stderr WARN. Used by the tee fallback."""
    print(msg, file=sys.stderr)


def _run_codex(
    argv: list[str],
    output_path: Path,
    timeout: Optional[float],
    expect_session: bool,
    popen_cwd: Optional[Path] = None,
    agent_self: Optional[str] = None,
) -> CodexResult:
    """Shared subprocess driver for :func:`create` and :func:`resume`.

    Wires up Popen with ``stdin=DEVNULL`` (Locked Decision 11) and
    ``stderr=subprocess.STDOUT`` (Locked Decision 12). Streams the
    JSONL output via :func:`_parse_stream`, applies a wall-clock
    timeout via a watchdog timer that sends ``SIGTERM`` (then ``SIGKILL``
    on grace overrun), and bounds the final wait via :func:`_wait_with_grace`.
    """
    started = time.monotonic()
    timed_out: dict[str, bool] = {"flag": False}

    # Open the tee BEFORE Popen so a tee-path failure (EACCES on the
    # state dir, ENOSPC on the parent fs, etc.) maps to a structured
    # CodexInvocationError exit instead of a raw Python traceback
    # surfacing through the dispatch layer (silent-failure-hunter row 2).
    try:
        tee_fh = _open_tee(output_path)
    except (PermissionError, FileNotFoundError, OSError) as exc:
        _stderr_warn(
            f"codex provider: cannot open output tee {output_path}: {exc}"
        )
        # Exit 12 mirrors dispatch.py's mapping for "registry write failed
        # / path-layer problems"; the caller's hint will be the stderr
        # WARN line above.
        raise CodexInvocationError(12) from exc

    # Outer try/finally guarantees tee_fh.close() AND proc reap on every
    # exit path. Gemini PR #305 rounds 2+3 hardened against three leak
    # classes: (a) tee_fh leak on non-KeyboardInterrupt exceptions,
    # (b) watchdog timer race where sigkill_followup escapes cancellation,
    # (c) un-reaped subprocess when _parse_stream raises unexpectedly.
    #
    # Timers go into a list so the cancel-all loop in finally is
    # race-free: appending to the list happens before .start(), and the
    # finally iterates the same list under the GIL (list.append /
    # list iteration are atomic in CPython).
    timers: list[threading.Timer] = []
    proc: Optional[subprocess.Popen] = None
    types_seen: set[str] = set()
    session_id: Optional[str] = None
    last_msg: str = ""
    last_error_msg: str = ""
    exit_code: int = -1
    sigkill_escalated: bool = False
    keyboard_interrupted = False

    # Inject FNO_AGENT_* env vars so nested `fno agents ask` calls
    # from inside this codex session attribute back to the parent agent.
    # When agent_self is None (direct caller, no parent context) we pass
    # env=None so the child inherits the parent process env unchanged.
    if agent_self is not None:
        spawn_env: Optional[dict[str, str]] = dict(os.environ)
        spawn_env["FNO_AGENT_SELF"] = agent_self
        spawn_env["FNO_AGENT_PROVIDER"] = "codex"
    else:
        spawn_env = None

    try:
        try:
            proc = _subprocess_popen(
                argv,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,  # Locked Decision 12
                text=True,
                bufsize=1,                  # line-buffered
                cwd=str(popen_cwd) if popen_cwd is not None else None,
                # Put the child in its own process group so timeout-driven
                # SIGTERM / SIGKILL propagates to descendants (codex spawns
                # subshells for sandbox tooling; a flat .terminate() leaves
                # them running and blocks the read loop on a never-EOF'd pipe).
                start_new_session=True,
                env=spawn_env,
            )
        except FileNotFoundError:
            raise CodexInvocationError(127)
        except OSError as exc:
            _stderr_warn(f"codex provider: OSError invoking codex: {exc}")
            raise CodexInvocationError(1) from exc

        def _killpg(sig) -> None:
            assert proc is not None
            try:
                os.killpg(os.getpgid(proc.pid), sig)
            except (OSError, ProcessLookupError):
                pass

        # Watchdog: if timeout fires before the read loop exits, send
        # SIGTERM to the codex process GROUP. A short follow-up timer
        # escalates to SIGKILL on the group if the child ignores SIGTERM.
        # Both timers are appended to the list BEFORE .start() so the
        # finally cancel-all sees them even if the watchdog has fired
        # by the time we cancel — list.append in CPython is GIL-atomic.
        if timeout is not None and timeout > 0:
            def _on_timeout() -> None:
                # Codex PR #305 round 4 (P2): guard against a race where
                # _on_timeout fires concurrent with normal process exit.
                # The threading.Timer can be scheduled but not yet cancelled
                # when proc returns 0 and the main thread is mid-finally.
                # Without the poll check, we'd set timed_out["flag"]=True
                # and the post-finally branch would raise CodexTimeoutError
                # for a successful run.
                if proc.poll() is not None:
                    return
                timed_out["flag"] = True
                _killpg(_signal.SIGTERM)
                # Schedule SIGKILL escalation in case codex / its subshells
                # ignore SIGTERM. 2s grace matches the Locked Decision 11
                # discussion in Claude's Discretion. The list append is
                # the load-bearing primitive that closes the prior race
                # where the main thread's finally read sigkill_followup
                # as None mid-creation.
                followup = threading.Timer(2.0, lambda: _killpg(_signal.SIGKILL))
                followup.daemon = True
                timers.append(followup)
                followup.start()

            watchdog = threading.Timer(float(timeout), _on_timeout)
            watchdog.daemon = True
            timers.append(watchdog)
            watchdog.start()

        try:
            session_id, last_msg, last_error_msg, _ = _parse_stream(
                proc, tee_fh, types_seen, _stderr_warn
            )
        except KeyboardInterrupt:
            keyboard_interrupted = True
            # Forward SIGINT to the codex process group (subshells too).
            _killpg(_signal.SIGINT)
            # Best-effort wait so the child can clean up before we exit.
            try:
                proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                _killpg(_signal.SIGKILL)
                try:
                    proc.wait(timeout=1.0)
                except subprocess.TimeoutExpired:
                    pass
            raise

        # Happy path: read loop completed. Reap proc here so its
        # (exit_code, sigkill_escalated) is captured. The outer finally
        # below also calls _wait_with_grace as a no-op safety net on
        # exception paths — _wait_with_grace tolerates already-reaped
        # procs (proc.wait returns the cached returncode without
        # spawning a new wait).
        exit_code, sigkill_escalated = _wait_with_grace(proc)
    finally:
        # Cancel timers first so any pending SIGKILL doesn't fire after
        # the proc is reaped (or after the pid is recycled).
        for t in timers:
            t.cancel()

        # Close stdout pipe; safe to call multiple times.
        if proc is not None and proc.stdout is not None:
            try:
                proc.stdout.close()
            except OSError:
                pass

        # Reap proc on every exit path so we never leak a zombie even
        # when _parse_stream raises an unexpected exception. This is a
        # no-op if proc already exited normally above (proc.wait caches
        # the returncode); it's the load-bearing reap on exception paths.
        if proc is not None and proc.poll() is None:
            _wait_with_grace(proc, grace_sec=2.0)

        try:
            tee_fh.close()
        except OSError as close_exc:
            # Close failure on the tee is rare but observable (final
            # buffered bytes may be lost on disk-full). Surface via
            # stderr WARN — silent-failure-hunter row 3 specifically
            # called out the prior silent swallow.
            _stderr_warn(
                f"codex provider: tee close failed on {output_path}: {close_exc}"
            )

    duration_ms = int((time.monotonic() - started) * 1000)

    if timed_out["flag"]:
        raise CodexTimeoutError(float(timeout))  # type: ignore[arg-type]

    if expect_session and session_id is None:
        # Locked Decision 14: surface what we DID see before failing closed.
        raise NoSessionIdError(types_seen)

    # silent-failure-hunter row 4: a force-killed run with a partially
    # captured reply previously surfaced as a "successful" CodexResult.
    # SIGKILL escalation is always a failure mode regardless of what the
    # parser observed; raise so the dispatch layer maps to exit 1 with
    # the output.jsonl tee for diagnostics.
    if sigkill_escalated:
        raise CodexInvocationError(exit_code if exit_code != 0 else 1)

    if exit_code != 0 and not last_msg:
        # No reply captured and non-zero exit: propagate to caller. The
        # stderr diagnostic is in output.jsonl (stderr=STDOUT merge).
        raise CodexInvocationError(exit_code)

    # silent-failure-hunter row 5: a soft-error item with no agent_message
    # and exit 0 left callers with no error context. Promote the captured
    # error text to last_msg ONLY when nothing else was captured — soft
    # errors that precede a real reply are still teed for forensics.
    effective_last_msg = last_msg if last_msg else last_error_msg

    return CodexResult(
        exit_code=exit_code,
        session_id=session_id,
        last_msg=effective_last_msg,
        duration_ms=duration_ms,
    )


def create(
    *,
    cwd: Path,
    prompt: str,
    from_name: str,
    yolo: bool,
    output_path: Path,
    timeout: Optional[float] = None,
    agent_self: Optional[str] = None,
    headless_yolo: Optional[bool] = None,
) -> CodexResult:
    """Spawn ``codex exec --json --cd <cwd> --skip-git-repo-check ...``.

    Captures the session UUID from ``thread.started`` and the last
    assistant text from ``item.completed`` (item.type=agent_message).
    Tees every stdout line to ``output_path`` in append mode.

    Raises:
        :class:`NoSessionIdError`: JSONL stream ended without
            ``thread.started`` event (warn-on-drift forensics in the
            attached ``types_seen`` set).
        :class:`CodexInvocationError`: codex exited non-zero and no
            assistant reply was captured.
        :class:`CodexTimeoutError`: wall-clock exceeded ``timeout``.
    """
    full_prompt = inject_from_name(prompt, from_name)
    eff_yolo = _effective_yolo(yolo, headless_yolo)
    # Approval is a GLOBAL flag and must precede `exec`; sandbox is an `exec`
    # flag and follows it. See `approval_flag` / `sandbox_flag`.
    argv = [
        "codex",
        *approval_flag(eff_yolo),
        "exec", "--json",
        "-C", str(cwd),
        "--skip-git-repo-check",
        *sandbox_flag(eff_yolo),
        full_prompt,
    ]
    return _run_codex(
        argv=argv,
        output_path=output_path,
        timeout=timeout,
        expect_session=True,
        popen_cwd=None,
        agent_self=agent_self,
    )


def resume(
    *,
    session_id: str,
    cwd: Path,
    prompt: str,
    from_name: str,
    yolo: bool,
    output_path: Path,
    timeout: Optional[float] = None,
    headless_yolo: Optional[bool] = None,
) -> CodexResult:
    """Spawn ``codex exec resume <session_id> --json ...`` from ``cwd``.

    codex's ``exec resume`` does NOT accept ``--cd`` (verified against
    0.130.0). cwd-pinning is enforced by setting the subprocess's
    working directory via ``Popen(cwd=...)``; codex's session lookup
    filters by the current cwd by default.

    Args:
        session_id: UUID from the registry's ``codex_session_id`` field.
        cwd: registry-recorded cwd for the agent; the call-time cwd is
            ignored (parent design domain pitfall: cwd-pinned sessions).
        yolo: sandbox bypass; default False.

    Raises:
        :class:`CodexInvocationError`: codex exits non-zero (e.g. session
            not found, model error). The exit code is propagated; stderr
            went into the tee (output.jsonl) via Locked Decision 12.
        :class:`CodexTimeoutError`: wall-clock exceeded ``timeout``.
    """
    full_prompt = inject_from_name(prompt, from_name)
    # `codex exec resume` does not accept `--sandbox`; only the dangerous
    # bypass flag toggles sandbox behavior. Sandbox mode otherwise inherits
    # from the original session's settings.
    argv = [
        "codex", "exec", "resume", session_id,
        "--json",
        "--skip-git-repo-check",
        *sandbox_flag_resume(_effective_yolo(yolo, headless_yolo)),
        full_prompt,
    ]
    return _run_codex(
        argv=argv,
        output_path=output_path,
        timeout=timeout,
        expect_session=False,
        popen_cwd=cwd,
    )


# ---------------------------------------------------------------------------
# US4-lifecycle: reachability probe via codex's session index
# ---------------------------------------------------------------------------

import re as _re  # noqa: E402  (kept module-local to mirror the lazy-stdlib pattern above)

# Match the canonical codex session-id shape: lowercase 8-4-4-4-12 UUID.
# Anchoring to this shape lets us extract IDs from JSONL records regardless
# of which field name codex uses internally; if the schema changes, an
# integration smoke test will surface the drift well before the regex does.
_SESSION_ID_RE = _re.compile(
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
)


def default_session_index_path() -> Path:
    """Codex 0.130.0 writes its session index to ``~/.codex/session_index.jsonl``."""
    return Path.home() / ".codex" / "session_index.jsonl"


# Backwards-compatible alias for callers that imported the underscore-prefixed
# name before the public-API rename. New code should use the public name.
_default_session_index_path = default_session_index_path


def load_known_session_ids(
    *, session_index_path: Optional[Path] = None
) -> set[str]:
    """Return the set of session-ids known to codex on this machine.

    Reads ``~/.codex/session_index.jsonl`` (override via
    ``session_index_path`` for tests) and extracts every UUID-shaped
    string. Used by :func:`fno.agents.dispatch.reconcile_agents`
    to decide whether a registered codex agent's session still exists.

    Returns the empty set if the index file is missing (fresh codex
    install) or unreadable (permission denied, device error). The
    caller (reconcile) treats the empty-set outcome as a soft warning
    and skips the codex side of the sweep with a stderr WARN line.

    Robustness contract:

    - We do NOT pin to a specific JSON field name. Codex's index format
      may evolve; extracting UUIDs via regex from the raw file content
      survives schema renames as long as the IDs themselves stay UUID-
      shaped.
    - Empty / malformed lines are silently skipped (no JSON parsing).
    - The whole file is read once per call. Reconcile is read-mostly
      and called rarely; pessimization is acceptable in exchange for
      simplicity.
    """
    path = session_index_path or default_session_index_path()
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        # Fresh codex install / no sessions yet. Caller distinguishes via
        # ``session_index_exists`` returning False.
        return set()
    except OSError as exc:
        # Permission denied / device error: the file is there but we
        # cannot read it. Raising preserves the distinction "we don't
        # know what's in the index" vs "we know the index is empty",
        # so reconcile_agents can route codex agents to ``errors``
        # instead of mass-flipping them to orphaned. (Codex P1 finding
        # on PR #315.)
        raise ReachabilityProbeError(
            provider="codex",
            reason=f"cannot read codex session index at {path}: {exc}",
        ) from exc
    return set(_SESSION_ID_RE.findall(text))


def session_index_exists(*, session_index_path: Optional[Path] = None) -> bool:
    """Return True iff codex's session index file exists on disk.

    Reconcile uses this BEFORE :func:`load_known_session_ids` to
    distinguish "fresh codex install" (no index, treat codex agents as
    untouched) from "supervisor index lost the session" (index present
    but missing the id, mark the agent orphaned). Without this split,
    both states would collapse to ``orphaned`` and surprise the operator
    on a brand-new machine.
    """
    path = session_index_path or default_session_index_path()
    return path.exists()

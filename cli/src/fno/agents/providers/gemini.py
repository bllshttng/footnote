"""fno.agents.providers.gemini — gemini -p adapter for US4-gemini.

Surface:

- :func:`create` — invoke ``gemini --skip-trust -p ... --session-id <uuid>
  --output-format json``, parse the single-blob JSON output, return
  :class:`GeminiResult`.
- :func:`resume` — invoke ``gemini --skip-trust -p ... --resume <uuid>
  --output-format json`` from the registry-recorded cwd. Same parser;
  session_id is NOT re-captured (caller supplied it).
- :func:`inject_from_name` — bracket-prefix prompt injection.
- :func:`sandbox_flag` — argv tokens for the yolo bypass.
- :func:`gemini_session_reachable` — tri-state probe matching the
  ``ReachabilityProbeError`` contract lifted in Wave 1.1.

Pinned from a real gemini 0.42.0 capture (Wave 2.0,
`scripts/smoke/capture-gemini-json.sh`). The schema observed there is
encoded in ``_GEMINI_KEYS`` below; drift in any key fails the smoke
test in Wave 2.3 loudly so the parser code never silently degrades.

Cleavage from ``providers/codex.py``: gemini emits a SINGLE JSON
object at EOF (not a line-iterator stream), so ``_parse_response`` is
``json.load(proc.stdout)`` after ``proc.wait()``. Gemini also emits
structural warnings (``Ripgrep is not available``, MCP issues, skill
conflicts) to stderr that would corrupt the JSON parse if merged into
stdout — stderr is drained on a separate pipe to keep stdout pure
(divergence from codex's Locked Decision 12).

Locked Decision 11: every byte from gemini's stderr is teed alongside
the stdout JSON blob to ``output_path`` so ``fno agents logs <name>``
sees both for forensics.
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
from typing import Optional

from fno.agents.providers.base import ReachabilityProbeError


# Pinned JSON schema from Wave 2.0 smoke capture against gemini 0.42.0.
# A future gemini release that renames any of these keys will fail the
# Wave 2.3 smoke test before any silent parse-time drift can land.
_GEMINI_KEYS = {
    "session": "session_id",
    "reply": "response",
    "stats": "stats",
}

# Indirection so unit tests can monkeypatch the subprocess primitives
# without touching the real subprocess module. Lookup happens via module
# globals at call time so ``monkeypatch.setattr(gemini_mod,
# "_subprocess_popen", fake)`` works.
_subprocess_popen = subprocess.Popen

# Default cap on stderr captured during a single invocation. Gemini's
# startup noise is bounded but a runaway tool-loop could push large
# volumes; we cap to keep output.jsonl reasonable.
_DEFAULT_STDERR_CAP = 256 * 1024  # 256KB


@dataclass(frozen=True)
class GeminiResult:
    """Return shape for :func:`create` / :func:`resume`.

    Attributes:
        exit_code: gemini subprocess exit code (0 on happy path).
        session_id: UUID parsed from the JSON blob's ``session_id``
            field. None on resume only when gemini omits the field
            (drift signal; the smoke test pins this).
        last_msg: assistant text from the blob's ``response`` field;
            "" if the model emitted an empty reply.
        duration_ms: wall-clock elapsed since Popen returned.
    """

    exit_code: int
    session_id: Optional[str]
    last_msg: str
    duration_ms: int


class GeminiInvocationError(RuntimeError):
    """gemini exited non-zero and no assistant reply was captured.

    Carries the exit code so the dispatcher can propagate it to the
    shell with provider-tagged event emission.
    """

    def __init__(self, exit_code: int) -> None:
        super().__init__(f"gemini exited {exit_code} with no captured reply")
        self.exit_code = exit_code


class GeminiParseError(RuntimeError):
    """gemini emitted output that did not parse as the expected JSON shape.

    Carries the raw bytes (truncated to 200 chars) so the dispatcher's
    diagnostic surface includes enough context to triage. The tee
    preserves the FULL raw output for forensic inspection.
    """

    def __init__(self, raw_head: str) -> None:
        super().__init__(
            f"gemini output did not parse as JSON; first {len(raw_head)} "
            f"chars: {raw_head!r}"
        )
        self.raw_head = raw_head


class GeminiTimeoutError(RuntimeError):
    """gemini did not finish within the configured timeout."""

    def __init__(self, timeout_sec: float) -> None:
        super().__init__(f"gemini timed out after {timeout_sec}s")
        self.timeout_sec = timeout_sec


def inject_from_name(prompt: str, from_name: str) -> str:
    """Return ``"[from: <from_name>]\\n\\n<prompt>"`` (Locked Decision 8).

    Five-line port from ``codex.inject_from_name``. The smoke marker
    test in Wave 2.3 (``test_gemini_from_name_marker``, gated on
    ``GEMINI_SMOKE=1``) verifies the prefix reaches the model context.
    AC7-ERR pins the loud-failure path if it does not.
    """
    return f"[from: {from_name}]\n\n{prompt}"


def _gemini_sandbox_available() -> bool:
    """Best-effort: is a gemini ``--sandbox`` provider available? Never raises.

    Ladder (bounded-posture amendment LD4): macOS Seatbelt (``sandbox-exec``,
    built-in, no daemon) -> Docker/Podman when ``GEMINI_SANDBOX`` selects it AND
    the daemon is reachable -> no provider. A detection failure degrades to
    ``False`` (the caller then uses the unsandboxed-but-never-prompt fallback),
    never raising and never forcing a prompting mode.
    """
    try:
        import platform
        import shutil

        if platform.system() == "Darwin" and shutil.which("sandbox-exec"):
            return True
        sel = os.environ.get("GEMINI_SANDBOX", "").strip().lower()
        if sel in ("docker", "podman") and shutil.which(sel):
            proc = subprocess.run(
                [sel, "info"], capture_output=True, timeout=3, check=False
            )
            return proc.returncode == 0
        return False
    except Exception:
        return False


def sandbox_flag(yolo: bool, sandbox_available: Optional[bool] = None) -> list[str]:
    """Return argv tokens for gemini's create-path posture (bounded amendment).

    gemini exposes ``-s/--sandbox`` (boolean) INDEPENDENTLY of ``--approval-mode
    {default,auto_edit,yolo,plan}``. ``yolo`` auto-approves every tool (never
    prompts); ``auto_edit`` still prompts on shell/fetch (hang risk - FORBIDDEN
    headless). So a never-prompt-AND-sandboxed posture is ``--approval-mode yolo
    --sandbox``.

    - bounded (``yolo=False``, default): ``--approval-mode yolo --sandbox`` when
      a sandbox provider exists; else fall back to ``--approval-mode yolo``
      (never-prompt, unsandboxed) with a logged warning. NEVER ``default``/
      ``auto_edit``.
    - full yolo (``yolo=True``, explicit opt-in): bare ``--yolo`` (unsandboxed
      full-auto), unchanged from #498.

    ``sandbox_available`` is injectable for deterministic tests; ``None`` runs
    the best-effort detection ladder.
    """
    if yolo:
        return ["--yolo"]
    if sandbox_available is None:
        sandbox_available = _gemini_sandbox_available()
    if sandbox_available:
        return ["--approval-mode", "yolo", "--sandbox"]
    _stderr_warn(
        "warning: no gemini sandbox provider (sandbox-exec / docker); launching "
        "--approval-mode yolo UNSANDBOXED (still never-prompt, no hang)"
    )
    return ["--approval-mode", "yolo"]


def _effective_yolo(yolo: bool, headless_yolo: Optional[bool] = None) -> bool:
    """Resolve the effective sandbox-bypass for the autonomous exec lane (ab-994222ee).

    The create/resume path is the headless (MODE==exec) lane: a worker no
    operator is watching. Returns whether this launch is FULL yolo (bare
    ``--yolo``, unsandboxed) vs the BOUNDED default (``--approval-mode yolo
    --sandbox``, sandboxed + never-prompt). Both never prompt, so a headless
    gemini cannot wedge on an approval either way; the bounded default keeps the
    sandbox. ``config.agents.gemini.headless_yolo: true`` opts into full yolo.

    ``headless_yolo`` is the full-yolo selector, injectable for deterministic
    tests; ``None`` reads it from config. An explicit caller ``yolo=True``
    always wins. The interactive ``host``/``drive`` lane does NOT call this (a
    human is driving), so the posture is correctly scoped to autonomous workers.
    """
    if headless_yolo is None:
        # Best-effort: the config read pulls in fno.config (pydantic).
        # Degrade to the hang-safe BOUNDED default (False) on ANY failure -
        # including an ImportError when the provider runs in a minimal env
        # without the config deps (e.g. the bare-python3 Rust<->Python parity
        # harness) - so create()/resume() never crash on the config lookup.
        try:
            from fno.config import agents_headless_yolo

            headless_yolo = agents_headless_yolo("gemini")
        except Exception:
            headless_yolo = False
    return yolo or headless_yolo


def _open_tee(log_path: Path):
    """Open ``output.jsonl`` in append mode, line-buffered. Mirror of codex."""
    log_path.parent.mkdir(parents=True, exist_ok=True)
    return open(log_path, "a", buffering=1, encoding="utf-8")


def _stderr_warn(msg: str) -> None:
    """Emit a single-line stderr WARN. Used by the tee fallback."""
    print(msg, file=sys.stderr)


def _drain_pipe_into_list(
    stream,
    captured: list,
    cap: int,
    tee_fh=None,
    tee_lock=None,
) -> None:
    """Drain ``stream`` line-by-line into ``captured``, optionally tee'ing.

    Designed to run concurrently with another pipe drainer (e.g. on a
    background thread for stderr while the main thread reads stdout)
    to avoid the kernel pipe-buffer deadlock that arose pre-#317
    (codex P1 + gemini high-priority finding).

    Stops when the stream EOFs OR ``cap`` bytes have been read; on cap
    overflow, appends a ``[truncated at N bytes]`` marker. Tee writes
    happen under ``tee_lock`` so two concurrent drainers don't
    interleave bytes in the tee file. Tee write failures are non-fatal
    and warn ONCE per unique errno tuple.
    """
    if stream is None:
        return
    total = 0
    tee_warned_modes: set[tuple] = set()
    try:
        for line in iter(stream.readline, ""):
            if not line:
                break
            captured.append(line)
            total += len(line)
            if tee_fh is not None:
                try:
                    if tee_lock is not None:
                        with tee_lock:
                            tee_fh.write(line)
                            tee_fh.flush()
                    else:
                        tee_fh.write(line)
                        tee_fh.flush()
                except OSError as exc:
                    mode = (exc.errno, str(exc))
                    if mode not in tee_warned_modes:
                        tee_warned_modes.add(mode)
                        _stderr_warn(
                            f"gemini provider: tee write failed: {exc}"
                        )
            if total > cap:
                marker = f"\n[truncated at {cap} bytes]\n"
                captured.append(marker)
                if tee_fh is not None:
                    try:
                        if tee_lock is not None:
                            with tee_lock:
                                tee_fh.write(marker)
                                tee_fh.flush()
                        else:
                            tee_fh.write(marker)
                            tee_fh.flush()
                    except OSError:
                        pass
                break
    except (OSError, ValueError):
        # ValueError = closed file. Treat as EOF.
        pass


def _wait_with_grace(
    proc: subprocess.Popen, grace_sec: float = 5.0
) -> tuple[int, bool]:
    """Wait; SIGTERM after grace, SIGKILL after a further 5s.

    Mirror of codex's ``_wait_with_grace``. Returns ``(exit_code,
    sigkill_escalated)``. Signals target the process GROUP (Popen with
    ``start_new_session=True``).
    """
    def _killpg(sig) -> None:
        try:
            os.killpg(os.getpgid(proc.pid), sig)
        except (OSError, ProcessLookupError):
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
                return (-9, True)


def _parse_response(stdout_text: str) -> tuple[Optional[str], str]:
    """Parse the single-blob JSON output. Returns ``(session_id, reply)``.

    The structural cleavage from codex: gemini emits ONE JSON object at
    EOF, not a per-line stream. We read the full stdout into memory and
    pass it through ``json.loads`` once. Empty stdout (parse error
    upstream) and malformed JSON surface as :class:`GeminiParseError`
    with the first 200 chars of raw output attached.

    The ``response`` field may be:

    - non-empty string -> the assistant reply.
    - empty string -> the model returned no text; we surface "" so
      the caller writes a zero-length reply to stdout (forensic info
      via ``last_msg_len=0`` in the dispatcher's event payload).
    - ``null`` (gemini's "model errored" signal) -> we treat as ""
      and the caller relies on the non-zero exit code + stderr tee.

    A missing top-level field is a schema-drift signal -> raises
    :class:`GeminiParseError`.
    """
    if not stdout_text or not stdout_text.strip():
        raise GeminiParseError(stdout_text[:200])
    try:
        parsed = json.loads(stdout_text)
    except json.JSONDecodeError as exc:
        raise GeminiParseError(stdout_text[:200]) from exc
    if not isinstance(parsed, dict):
        raise GeminiParseError(stdout_text[:200])

    session_id = parsed.get(_GEMINI_KEYS["session"])
    if session_id is not None and not isinstance(session_id, str):
        # Defensive: a future gemini release that returns an integer or
        # null here would break downstream registry writes. Treat as
        # parse error so the schema drift fails the smoke test.
        raise GeminiParseError(stdout_text[:200])

    # Codex P2 (PR #317): require ``response`` and ``stats`` keys to be
    # PRESENT in the payload. The pre-fix behavior treated a missing
    # response field as a silent empty reply, which would let schema
    # drift land as a successful registry write with an empty assistant
    # message. Now: missing response OR missing stats -> GeminiParseError
    # so contract regressions surface loudly. A present-but-null response
    # remains acceptable (model declined to emit text — distinct from
    # schema drift).
    if _GEMINI_KEYS["reply"] not in parsed:
        raise GeminiParseError(stdout_text[:200])
    if _GEMINI_KEYS["stats"] not in parsed:
        raise GeminiParseError(stdout_text[:200])

    reply_raw = parsed[_GEMINI_KEYS["reply"]]
    if reply_raw is None:
        reply = ""
    elif isinstance(reply_raw, str):
        reply = reply_raw
    else:
        # Same schema-drift guard as session_id.
        raise GeminiParseError(stdout_text[:200])

    return session_id, reply


def _run_gemini(
    argv: list[str],
    output_path: Path,
    timeout: Optional[float],
    expect_session: bool,
    popen_cwd: Optional[Path] = None,
    agent_self: Optional[str] = None,
) -> GeminiResult:
    """Shared subprocess driver for :func:`create` and :func:`resume`.

    Wires up Popen with ``stdin=DEVNULL``, ``stdout=PIPE`` (captured to
    a buffer for json.load), ``stderr=PIPE`` (drained synchronously
    AFTER proc.wait() and tee'd to output_path). Watchdog timer +
    KeyboardInterrupt handling mirrors ``codex._run_codex``.
    """
    started = time.monotonic()
    timed_out: dict[str, bool] = {"flag": False}

    try:
        tee_fh = _open_tee(output_path)
    except (PermissionError, FileNotFoundError, OSError) as exc:
        _stderr_warn(
            f"gemini provider: cannot open output tee {output_path}: {exc}"
        )
        raise GeminiInvocationError(12) from exc

    timers: list[threading.Timer] = []
    proc: Optional[subprocess.Popen] = None
    exit_code: int = -1
    sigkill_escalated: bool = False
    stdout_text = ""

    # Inject FNO_AGENT_* env vars so nested `fno agents ask` calls
    # from inside this gemini session attribute back to the parent agent.
    if agent_self is not None:
        spawn_env: Optional[dict[str, str]] = dict(os.environ)
        spawn_env["FNO_AGENT_SELF"] = agent_self
        spawn_env["FNO_AGENT_PROVIDER"] = "gemini"
    else:
        spawn_env = None

    try:
        try:
            proc = _subprocess_popen(
                argv,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,  # Divergence from codex (LD12).
                text=True,
                bufsize=1,
                cwd=str(popen_cwd) if popen_cwd is not None else None,
                start_new_session=True,
                env=spawn_env,
            )
        except FileNotFoundError:
            raise GeminiInvocationError(127)
        except OSError as exc:
            _stderr_warn(f"gemini provider: OSError invoking gemini: {exc}")
            raise GeminiInvocationError(1) from exc

        def _killpg(sig) -> None:
            assert proc is not None
            try:
                os.killpg(os.getpgid(proc.pid), sig)
            except (OSError, ProcessLookupError):
                pass

        if timeout is not None and timeout > 0:
            def _on_timeout() -> None:
                if proc.poll() is not None:
                    return
                timed_out["flag"] = True
                _killpg(_signal.SIGTERM)
                followup = threading.Timer(
                    2.0, lambda: _killpg(_signal.SIGKILL)
                )
                followup.daemon = True
                timers.append(followup)
                followup.start()

            watchdog = threading.Timer(float(timeout), _on_timeout)
            watchdog.daemon = True
            timers.append(watchdog)
            watchdog.start()

        # Codex P1 + gemini high-priority finding (PR #317): stdout and
        # stderr MUST drain concurrently. Pre-fix, sequential reading
        # (stdout.read() then _drain_stderr) deadlocked when gemini's
        # stderr filled the kernel pipe buffer (~64KB) before closing
        # stdout — gemini blocked writing stderr, parent blocked reading
        # stdout, the watchdog timer was the only escape.
        #
        # The fix: drain stderr on a background thread while the main
        # thread reads stdout. Both threads share the tee via a lock so
        # interleaved writes don't corrupt the output file. The stderr
        # thread is a daemon so a wedged peer cannot prevent process
        # shutdown; we join with a generous timeout after stdout EOFs.
        tee_lock = threading.Lock()
        stderr_chunks: list[str] = []
        stderr_thread = threading.Thread(
            target=_drain_pipe_into_list,
            args=(proc.stderr, stderr_chunks, _DEFAULT_STDERR_CAP, tee_fh, tee_lock),
            daemon=True,
        )
        stderr_thread.start()

        try:
            assert proc.stdout is not None
            # stdout drain runs in-thread; tee under the shared lock so
            # the stderr thread's writes never interleave with the
            # single-blob stdout write below. We accumulate chunks first
            # and tee in one shot after EOF to preserve the JSON-as-a-
            # single-blob invariant for log readers.
            stdout_text = proc.stdout.read()
        except KeyboardInterrupt:
            # Forward SIGINT to the gemini process group; reap; re-raise.
            _killpg(_signal.SIGINT)
            try:
                proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                _killpg(_signal.SIGKILL)
                try:
                    proc.wait(timeout=1.0)
                except subprocess.TimeoutExpired:
                    pass
            # Wait for the stderr thread to finish before re-raising so
            # the tee file is consistent at the point KbInt propagates.
            stderr_thread.join(timeout=2.0)
            raise

        # Tee the stdout JSON blob under the shared lock so the stderr
        # thread's writes don't interleave. AC4-EDGE: malformed bytes
        # still land in output.jsonl verbatim for forensics.
        if stdout_text:
            try:
                with tee_lock:
                    tee_fh.write(stdout_text)
                    if not stdout_text.endswith("\n"):
                        tee_fh.write("\n")
                    tee_fh.flush()
            except OSError as exc:
                _stderr_warn(
                    f"gemini provider: tee write of stdout failed: {exc}"
                )

        exit_code, sigkill_escalated = _wait_with_grace(proc)

        # Wait for stderr drainer to finish (it sees EOF after the proc
        # exits). A generous 5s join handles the rare race where the
        # daemon thread hasn't reached EOF yet at proc.wait() return.
        stderr_thread.join(timeout=5.0)
        "".join(stderr_chunks)
    finally:
        for t in timers:
            t.cancel()
        if proc is not None:
            if proc.stdout is not None:
                try:
                    proc.stdout.close()
                except OSError:
                    pass
            if proc.stderr is not None:
                try:
                    proc.stderr.close()
                except OSError:
                    pass
        if proc is not None and proc.poll() is None:
            _wait_with_grace(proc, grace_sec=2.0)
        try:
            tee_fh.close()
        except OSError as close_exc:
            _stderr_warn(
                f"gemini provider: tee close failed on {output_path}: "
                f"{close_exc}"
            )

    duration_ms = int((time.monotonic() - started) * 1000)

    if timed_out["flag"]:
        raise GeminiTimeoutError(float(timeout))  # type: ignore[arg-type]

    if sigkill_escalated:
        raise GeminiInvocationError(exit_code if exit_code != 0 else 1)

    # Gemini medium (PR #317): parse stdout FIRST, then decide between
    # invocation error and parse error. Pre-fix the exit_code check ran
    # before the parse, which would surface a generic GeminiInvocationError
    # even when the JSON was structurally malformed; the tee preserved
    # the bytes but the dispatcher's exit-code mapping lost the parse
    # context. Post-fix: a present-but-malformed JSON on a non-zero exit
    # surfaces as both signals via the chained exception, and an
    # exit-zero with empty/missing JSON still raises GeminiParseError so
    # downstream callers can distinguish "model errored" from "schema
    # drift".
    try:
        session_id, reply = _parse_response(stdout_text)
    except GeminiParseError:
        if exit_code != 0:
            # Non-zero exit AND unparseable output: surface the exit code
            # (the dispatcher uses it to set the shell exit). The parse
            # error chains as __cause__ on the GeminiInvocationError so
            # forensic context is preserved.
            raise GeminiInvocationError(exit_code)
        # Exit-zero but malformed: schema drift. Re-raise the parse error.
        raise

    if exit_code != 0:
        # Gemini exited non-zero but did emit a parseable JSON. The
        # ``response`` field may carry the model's last partial output;
        # we propagate the exit code regardless because the operator
        # needs to know the invocation itself failed.
        raise GeminiInvocationError(exit_code)

    if expect_session and not session_id:
        # The session field was missing or empty AFTER a successful exit.
        # That's a hard contract violation — gemini emitted a JSON blob
        # but omitted the session id. Treat as parse error for the same
        # forensic surface as malformed JSON.
        raise GeminiParseError(stdout_text[:200])

    return GeminiResult(
        exit_code=exit_code,
        session_id=session_id,
        last_msg=reply,
        duration_ms=duration_ms,
    )


def create(
    *,
    cwd: Path,
    prompt: str,
    from_name: str,
    yolo: bool,
    output_path: Path,
    session_id: Optional[str] = None,
    timeout: Optional[float] = None,
    agent_self: Optional[str] = None,
    headless_yolo: Optional[bool] = None,
) -> GeminiResult:
    """Spawn ``gemini --skip-trust -p ... --session-id <uuid> --output-format json``.

    Captures the UUID from the JSON blob's ``session_id`` field (which
    matches the one we passed via ``--session-id``; Locked Decision 9
    asserted via Wave 2.0 smoke).

    Args:
        cwd: Working directory; gemini pins sessions to this cwd.
        prompt: User prompt; ``inject_from_name`` prepends the
            ``[from: <name>]`` annotation before gemini sees it.
        from_name: Already-validated bracket-annotation name (Wave 2.0
            OQ2 smoke marker pending in Wave 2.3).
        yolo: Map to gemini's ``--yolo`` flag (OQ5).
        output_path: Tee target for the JSON blob + stderr (Locked
            Decision 11).
        session_id: Optional pre-generated UUID. When provided, passed
            via ``--session-id``; gemini round-trips it in the
            response (LD9). When ``None``, gemini auto-generates a UUID
            and we capture it from the response.
        timeout: Wall-clock seconds. ``None`` means no timeout.

    Raises:
        :class:`GeminiInvocationError`: non-zero exit, FileNotFoundError,
            or sigkill escalation.
        :class:`GeminiParseError`: malformed JSON output or schema drift.
        :class:`GeminiTimeoutError`: wall-clock exceeded ``timeout``.
    """
    full_prompt = inject_from_name(prompt, from_name)
    argv: list[str] = [
        "gemini",
        "--skip-trust",
        "-p", full_prompt,
        "--output-format", "json",
        *sandbox_flag(_effective_yolo(yolo, headless_yolo)),
    ]
    if session_id:
        argv.extend(["--session-id", session_id])
    return _run_gemini(
        argv=argv,
        output_path=output_path,
        timeout=timeout,
        expect_session=True,
        popen_cwd=cwd,
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
) -> GeminiResult:
    """Spawn ``gemini --skip-trust -p ... --resume <uuid> ...`` from ``cwd``.

    Gemini's session lookup is cwd-pinned (Wave 2.0 OQ1) — running
    resume from a different cwd than where the session was created
    raises ``Invalid session identifier`` from gemini itself. The
    caller MUST pass the registry-recorded cwd, not the call-time cwd.

    Args:
        session_id: UUID from the registry's ``gemini_session_id`` field.
        cwd: registry-recorded cwd for the agent.
        yolo: Sandbox bypass; default False.

    Raises:
        :class:`GeminiInvocationError`: non-zero exit (e.g. session
            not found, model error). Exit code is propagated; stderr
            went into the tee.
        :class:`GeminiParseError`: malformed JSON output.
        :class:`GeminiTimeoutError`: wall-clock exceeded ``timeout``.
    """
    full_prompt = inject_from_name(prompt, from_name)
    argv = [
        "gemini",
        "--skip-trust",
        "-p", full_prompt,
        "--output-format", "json",
        "--resume", session_id,
        *sandbox_flag(_effective_yolo(yolo, headless_yolo)),
    ]
    return _run_gemini(
        argv=argv,
        output_path=output_path,
        timeout=timeout,
        expect_session=False,
        popen_cwd=cwd,
    )


# ---------------------------------------------------------------------------
# Reachability probe — tri-state per the lifted ReachabilityProbeError contract
# ---------------------------------------------------------------------------


def _gemini_chats_dir(cwd: Path) -> Path:
    """Return the per-cwd chats directory where gemini persists sessions.

    Layout observed in Wave 2.0 smoke discovery:

        ~/.gemini/tmp/<cwd-basename>/chats/

    The basename is the LAST path component of the registered cwd. A
    future gemini release that changes this layout will surface via the
    Wave 2.3 integration smoke test before the probe silently misbehaves.
    """
    return Path.home() / ".gemini" / "tmp" / cwd.name / "chats"


def gemini_session_reachable(session_id: str, cwd: Path) -> bool:
    """Tri-state reachability probe for a gemini agent.

    Tri-state contract (lifted in Wave 1.1):

    - Return ``True``: a session file exists at the cwd-pinned location
      whose name includes the session_id's short prefix.
    - Return ``False``: chats directory exists but no matching file.
      Reconcile MUST flip the agent's status to ``"orphaned"``.
    - Raise :class:`ReachabilityProbeError`: a transient error or
      permission issue prevents a definitive answer. Reconcile MUST
      preserve the entry's status unchanged and route to errors with
      a per-provider reason discriminator.

    PermissionError on stat / parent dir unreadable maps to
    ``ReachabilityProbeError(provider="gemini", reason=<errno-string>)``
    (AC8-FR). FileNotFoundError on the project dir itself maps to
    ``ReachabilityProbeError`` because we cannot distinguish "fresh
    gemini install, no sessions yet" (the operator-zero case) from
    "session was deleted on disk" (operator action) without scanning
    every cwd. Reconcile routes both to errors-with-status-preserved,
    which matches the codex "session index missing" precedent.
    """
    if not session_id:
        # Defensive: an empty UUID is a registry corruption signal.
        # Caller's responsibility, but we surface as inconclusive.
        raise ReachabilityProbeError(
            provider="gemini", reason="empty session_id in registry"
        )
    if len(session_id) < 8:
        raise ReachabilityProbeError(
            provider="gemini",
            reason=f"session_id too short to match short-prefix layout: "
                   f"{session_id!r}",
        )

    chats_dir = _gemini_chats_dir(cwd)
    short_id = session_id[:8]

    try:
        exists = chats_dir.exists()
    except PermissionError as exc:
        raise ReachabilityProbeError(
            provider="gemini", reason=f"permission denied on {chats_dir}: {exc}"
        ) from exc
    except OSError as exc:
        raise ReachabilityProbeError(
            provider="gemini", reason=f"stat {chats_dir}: {exc}"
        ) from exc

    if not exists:
        # The chats dir is missing — either a fresh gemini install (no
        # sessions yet for this cwd) or the operator nuked ~/.gemini/tmp/.
        # Without further evidence, treat as inconclusive (AC8-FR) so
        # reconcile preserves status instead of mass-orphaning every
        # gemini agent on a host that has not run gemini in this cwd.
        raise ReachabilityProbeError(
            provider="gemini",
            reason=f"chats dir does not exist: {chats_dir}",
        )

    try:
        matches = list(chats_dir.glob(f"session-*-{short_id}.jsonl"))
    except PermissionError as exc:
        raise ReachabilityProbeError(
            provider="gemini",
            reason=f"glob in {chats_dir}: {exc}",
        ) from exc
    except OSError as exc:
        raise ReachabilityProbeError(
            provider="gemini",
            reason=f"glob in {chats_dir}: {exc}",
        ) from exc

    if not matches:
        # Definitive miss — gemini has session files for THIS cwd but
        # none matches our short prefix. Reconcile flips to orphaned.
        return False

    # Short-prefix collisions are vanishingly unlikely (8 lowercase hex
    # chars = ~4B keyspace), but verify the full UUID by reading the
    # first line of any matching file. Gemini medium (PR #317):
    # read_text() pulls the entire chat log into memory; chat session
    # files can grow to multiple MB. Use ``open().readline()`` so we
    # only pay for the first newline-terminated chunk. Permission
    # errors on the individual file map back to inconclusive.
    for path in matches:
        try:
            with path.open(encoding="utf-8", errors="replace") as fh:
                first_line = fh.readline()
        except PermissionError as exc:
            raise ReachabilityProbeError(
                provider="gemini",
                reason=f"read {path}: {exc}",
            ) from exc
        except OSError as exc:
            # File races: another process deleted/rotated mid-glob.
            # Treat as inconclusive — the very next reconcile will see
            # the post-race state.
            raise ReachabilityProbeError(
                provider="gemini",
                reason=f"read {path}: {exc}",
            ) from exc
        if session_id in first_line:
            return True

    # Short prefix matched but full UUID didn't — collision case,
    # definitively NOT our session. Same operator outcome as no match.
    return False

"""sigma_dispatch: emitter scaffolding and dispatcher for per-agent sigma-review routing.

Task 1.2 (ab-978e93ed): This module exposes the event-emitter API that Wave 2
will call from the actual dispatch_sigma_subagent() function.

Task 1.3 (ab-978e93ed): Adds record_dispatch(), a best-effort observability
sidecar writer. See the function docstring for the policy distinction.

Task 2.1 (ab-978e93ed): Adds dispatch_sigma_subagent(), a context-manager-based
trust-boundary dispatcher for sigma-review subagents. The Claude path is
structurally ASYMMETRIC: the Task tool is a Claude-internal call so the
dispatcher cannot wrap a subprocess. Instead, _DispatchClaudeTask emits
subagent_spawn on __enter__, yields a dispatch object to the caller, and emits
subagent_complete on __exit__. The caller MUST invoke dispatch.record_complete()
inside the block. If the caller forgets, __exit__ still emits a complete event
with outcome='orchestrator_skipped' so the Wave 3 verifier can distinguish this
from a missing complete (outcome='subagent_complete_missing').

Task 2.2 (ab-978e93ed): Adds _DispatchSubprocess for non-Claude CLI paths
(gemini, codex, openclaw, hermes). spawn_with_provider_snapshot captures
the provider snapshot at __enter__ time; concurrent failover swaps after
that point cannot corrupt the spawn event's provider_id (invariant 9).
The subprocess blocks until completion in __exit__; the complete event is
on disk before the orchestrator regains control (invariant 10). Subprocess
crash (non-zero rc) still emits subagent_complete - the verifier soft-warns.
The hermes path raises NotImplementedError pending adapter wiring.

Task 2b.1 (ab-978e93ed): Adds _capture_stdout(), a best-effort per-agent sidecar
writer for post-mortem forensics. Files land at:
  .fno/sigma-review/{session_id}/{agent_name}.{out,err}
Session-scoped: same agent dispatched twice in one session appends with a
separator marker. Best-effort: write failures log WARNING and the dispatcher
continues unaffected. NOT gate-side: Wave 3.1's verifier reads events.jsonl
exclusively and MUST NOT consult these sidecar files as authoritative evidence
(the LLM has the Write tool and could fabricate a sidecar file; it cannot
fake events.jsonl writes which go through the shell helper + nonce).

Public API
----------
emit_subagent_spawn(...)       - emit a subagent_spawn event to events.jsonl
emit_subagent_complete(...)    - emit a subagent_complete event to events.jsonl
record_dispatch(...)           - append to .fno/subagent-dispatch.jsonl (sidecar)
dispatch_sigma_subagent(...)   - context manager: spawn -> Task call -> complete
EventEmitFailed                - raised when the shell helper exits non-zero

Both emitters delegate to emit-gate-transition.sh via subprocess so that
the events.jsonl line shape (JSON key order, ISO-8601 Z timestamp, trailing
newline) is byte-identical to existing phase_transition events. This matters
because the Wave 3 verifier uses grep -F on literal substrings.

FILE AUTHORITY DISTINCTION
--------------------------
.fno/events.jsonl         - GATE-SIDE authority. Failure raises EventEmitFailed.
                                  Wave 3 verify_provenance reads THIS file.
.fno/subagent-dispatch.jsonl - OBSERVABILITY sidecar. Failure logs WARNING and
                                  returns normally. Wave 3 MUST NOT consult this file
                                  as authoritative evidence.
.fno/sigma-review/{sid}/{agent}.{out,err}
                                - POST-MORTEM sidecar (Task 2b.1). Best-effort.
                                  Wave 3.1 verifier MUST NOT read these files.
"""
from __future__ import annotations

import contextlib
import dataclasses
import datetime
import fcntl
import hashlib
import json
import logging
import os
import re
import subprocess
import time
from pathlib import Path

logger = logging.getLogger(__name__)

# Imported at module level so tests can monkeypatch `fno.sigma_dispatch.spawn_with_provider_snapshot`
# rather than the original in adapters.providers.dispatch.
from fno.adapters.providers.dispatch import spawn_with_provider_snapshot  # noqa: E402

# ---------------------------------------------------------------------------
# Script path - module-level constant so tests can monkeypatch it.
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# Exception
# ---------------------------------------------------------------------------


class EventEmitFailed(RuntimeError):
    """Raised when emit-gate-transition.sh exits with a non-zero return code."""


# ---------------------------------------------------------------------------
# Internal helper
# ---------------------------------------------------------------------------


def _read_target_state(repo_root: Path) -> dict[str, str]:
    """Read session_id and provenance_nonce from .fno/target-state.md."""
    state_file = repo_root / ".fno" / "target-state.md"
    result: dict[str, str] = {}
    if not state_file.exists():
        return result
    text = state_file.read_text(encoding="utf-8")
    for key in ("session_id", "provenance_nonce"):
        m = re.search(rf"^\s*{key}:\s*(.+)$", text, re.MULTILINE)
        if m:
            result[key] = m.group(1).strip()
    return result


def _emit_event(
    event_type: str,
    fields: dict[str, str | int],
    repo_root: Path,
) -> None:
    """Append the event directly to .fno/events.jsonl.

    Historically this shelled out to scripts/lib/emit-gate-transition.sh;
    that script was deleted by the control-plane collapse wedge
    (ab-d0337fbc), so the writer is now native Python. The envelope keeps
    the shape the shell helper produced ({ts, type, source, data} with
    session_id + nonce merged into data) so downstream consumers (the
    fno-agents `verify-evidence` verb) read both eras identically.

    Raises EventEmitFailed when the append cannot be performed.
    """
    import datetime as _dt
    import json as _json

    state = _read_target_state(repo_root)
    data: dict[str, str | int] = {
        "session_id": state.get("session_id", ""),
        "nonce": state.get("provenance_nonce", ""),
        **fields,
    }
    event = {
        "ts": _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "type": event_type,
        "source": "subagent",
        "data": data,
    }
    events_path = repo_root / ".fno" / "events.jsonl"
    try:
        events_path.parent.mkdir(parents=True, exist_ok=True)
        with events_path.open("a", encoding="utf-8") as fh:
            fh.write(_json.dumps(event, ensure_ascii=True) + "\n")
    except OSError as exc:
        raise EventEmitFailed(
            f"events.jsonl append failed: {type(exc).__name__}: {exc}"
        ) from exc


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def emit_subagent_spawn(
    *,
    agent_name: str,
    provider_id: str,
    cli: str,
    session_id: str | None = None,
    provenance_nonce: str | None = None,
    repo_root: Path | None = None,
) -> None:
    """Emit a subagent_spawn event to .fno/events.jsonl.

    If session_id / provenance_nonce are not passed, they are read from
    .fno/target-state.md (the shell helper also does this, but the
    Python-side read improves test ergonomics when the shell cannot reach
    the right state file via git root discovery).
    """
    root = repo_root or Path.cwd()
    if session_id is None or provenance_nonce is None:
        state = _read_target_state(root)
        if session_id is None:
            session_id = state.get("session_id", "")
        if provenance_nonce is None:
            provenance_nonce = state.get("provenance_nonce", "")

    fields: dict[str, str | int] = {
        "agent_name": agent_name,
        "provider_id": provider_id,
        "cli": cli,
    }
    _emit_event("subagent_spawn", fields, root)


def emit_subagent_complete(
    *,
    agent_name: str,
    provider_id: str,
    cli: str,
    exit_code: int | None,
    stdout_sha256: str,
    stderr_sha256: str,
    duration_ms: int,
    session_id: str | None = None,
    provenance_nonce: str | None = None,
    repo_root: Path | None = None,
    outcome: str | None = None,
) -> None:
    """Emit a subagent_complete event to .fno/events.jsonl.

    stdout_sha256 and stderr_sha256 are pre-computed hex digest strings;
    this module does not hash subprocess output (that is Wave 2's job).
    duration_ms is elapsed wall-clock time in milliseconds.

    outcome (optional, Task 2.1): when set, the value is included as the
    'outcome' field in the event data. Recognized values:
      'ok'                   - caller called record_complete; real exit_code applies.
      'orchestrator_skipped' - caller never called record_complete; exit_code=null.
    When outcome is None (the default), the field is omitted from the JSON so
    Task 1.2's existing test surface is unaffected.

    exit_code is now typed as int | None to support the orchestrator_skipped path
    where no exit code is available. The shell helper serializes 'null' literally
    when the value is the string 'null'; callers should pass the sentinel string
    for that case.
    """
    root = repo_root or Path.cwd()
    if session_id is None or provenance_nonce is None:
        state = _read_target_state(root)
        if session_id is None:
            session_id = state.get("session_id", "")
        if provenance_nonce is None:
            provenance_nonce = state.get("provenance_nonce", "")

    fields: dict[str, str | int] = {
        "agent_name": agent_name,
        "provider_id": provider_id,
        "cli": cli,
        "exit_code": exit_code if exit_code is not None else "null",
        "stdout_sha256": stdout_sha256,
        "stderr_sha256": stderr_sha256,
        "duration_ms": duration_ms,
    }
    if outcome is not None:
        fields["outcome"] = outcome

    _emit_event("subagent_complete", fields, root)


# ---------------------------------------------------------------------------
# Task 1.3: Observability sidecar writer
# ---------------------------------------------------------------------------


def record_dispatch(
    *,
    sidecar_path: Path,
    turn_index: int,
    ts: str,
    agent_name: str,
    provider_id: str,
    cli: str,
    exit_code: int,
) -> None:
    """Append a single JSONL line to the per-session subagent-dispatch sidecar.

    Best-effort policy (distinct from events.jsonl):
      - Missing parent dir: auto-create.
      - Sidecar write fails (read-only fs, permissions): logs WARNING via
        the standard library `logging` module, returns normally.
      - Concurrent writers: serialized via fcntl.LOCK_EX on the file
        descriptor for the duration of the append. After 10x100 race tests
        the sidecar must contain exactly 1000 valid JSONL lines.

    THIS IS NOT THE GATE-SIDE EVENT STREAM. Wave 3's verify_provenance
    reads .fno/events.jsonl, not this sidecar. Do not consult this
    file as authoritative evidence.

    Mirrors turn_attribution.record_turn for the lock+append pattern.
    """
    sidecar_path = Path(sidecar_path)
    payload: dict = {
        "turn_index": int(turn_index),
        "ts": str(ts),
        "agent_name": str(agent_name),
        "provider_id": str(provider_id),
        "cli": str(cli),
        "exit_code": int(exit_code),
    }
    line = json.dumps(payload, separators=(",", ":")) + "\n"

    try:
        sidecar_path.parent.mkdir(parents=True, exist_ok=True)
        with open(sidecar_path, "a", encoding="utf-8") as f:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
            try:
                f.write(line)
                f.flush()
            finally:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    except OSError as exc:
        logger.warning("subagent-dispatch sidecar write failed: %s", exc)


# ---------------------------------------------------------------------------
# Task 2b.1: Per-agent stdout/stderr sidecar writer
# ---------------------------------------------------------------------------


def _capture_stdout(
    *,
    session_id: str,
    agent_name: str,
    stdout: str | bytes,
    stderr: str | bytes,
    repo_root: Path | None = None,
) -> None:
    """Best-effort write of subagent stdout/stderr to per-session sidecars.

    Files: .fno/sigma-review/{session_id}/{agent_name}.{out,err}

    Session-scoped: same agent dispatched twice in one session appends
    to the same file with a separator marker. Best-effort policy: any
    OSError is logged at WARNING level and the function returns
    normally. The dispatcher's caller proceeds with subagent_complete
    emit unaffected.

    NOT a gate-side authority. Wave 3.1's verify_provenance reads
    .fno/events.jsonl, not these sidecars. This file is for
    post-mortem and audit-loop iteration messages only. The LLM has
    the Write tool and could fabricate sidecar content; events.jsonl
    writes go through the shell helper + nonce and cannot be faked
    the same way.
    """
    root = repo_root or Path.cwd()
    base_dir = root / ".fno" / "sigma-review" / session_id

    # Decode bytes to text once, so the per-file logic is uniform.
    stdout_text: str = stdout.decode("utf-8", errors="replace") if isinstance(stdout, bytes) else stdout
    stderr_text: str = stderr.decode("utf-8", errors="replace") if isinstance(stderr, bytes) else stderr

    try:
        base_dir.mkdir(parents=True, exist_ok=True)

        ts = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        separator = f"\n\n--- dispatch at {ts} ---\n\n"

        for body, filename in ((stdout_text, f"{agent_name}.out"), (stderr_text, f"{agent_name}.err")):
            target = base_dir / filename
            with open(target, "a", encoding="utf-8") as f:
                fcntl.flock(f.fileno(), fcntl.LOCK_EX)
                try:
                    # Re-check size under lock to avoid TOCTOU on the separator check.
                    needs_separator = f.seek(0, 2) > 0  # seek to end, returns position
                    if needs_separator:
                        f.write(separator)
                    f.write(body)
                    f.flush()
                finally:
                    fcntl.flock(f.fileno(), fcntl.LOCK_UN)
    except OSError as exc:
        logger.warning("sigma-review stdout capture failed: %s", exc)


# ---------------------------------------------------------------------------
# Task 2.1: Trust-boundary dispatcher - Claude Task asymmetric path
# ---------------------------------------------------------------------------

_UNSET_SHA = "unset"


class _DispatchClaudeTask:
    """Asymmetric context manager for the Claude Task path (Task 2.1, ab-978e93ed).

    ASYMMETRY WARNING
    -----------------
    The Claude path cannot wrap a subprocess because the Task tool is a
    Claude-internal invocation. Instead, this class emits subagent_spawn on
    __enter__, yields control to the orchestrator (sigma-review SKILL.md) which
    calls Task() inside the block, and emits subagent_complete on __exit__.

    The orchestrator MUST call record_complete(stdout=..., exit_code=...) before
    leaving the `with` block. If it forgets, __exit__ still emits a complete
    event with outcome='orchestrator_skipped' and exit_code=null so the Wave 3.1
    verifier can reject with subagent_orchestrator_skipped (soft) rather than
    subagent_complete_missing (hard).

    Exception discipline
    --------------------
    __exit__ MUST NOT swallow exceptions raised inside the `with` block. If an
    exception is in flight and record_complete was never called, __exit__ emits
    outcome='orchestrator_skipped' and then returns False to let the exception
    propagate. If __exit__'s own emit call raises EventEmitFailed, Python chains
    it via __context__ automatically - no special handling needed here.
    """

    def __init__(
        self,
        *,
        agent_name: str,
        provider_id: str,
        cli: str,
        session_id: str | None,
        provenance_nonce: str | None,
        repo_root: Path,
    ) -> None:
        self.agent_name = agent_name
        self.provider_id = provider_id
        self.cli = cli
        self.repo_root = repo_root

        # Resolve session context from target-state.md if not provided.
        if session_id is None or provenance_nonce is None:
            state = _read_target_state(repo_root)
            if session_id is None:
                session_id = state.get("session_id") or ""
            if provenance_nonce is None:
                provenance_nonce = state.get("provenance_nonce") or ""

        self.session_id = session_id
        self.provenance_nonce = provenance_nonce

        # Result state - set by record_complete().
        self._stdout: str | None = None
        self._stderr: str = ""
        self._exit_code: int | None = None
        self._stdout_sha = _UNSET_SHA
        self._stderr_sha = _UNSET_SHA
        self._duration_ms: int = 0
        self._completed: bool = False
        self._start_ts: float | None = None

    def __enter__(self) -> "_DispatchClaudeTask":
        self._start_ts = time.monotonic()
        # Emit spawn FIRST. If this raises EventEmitFailed, we never entered
        # the block so __exit__ never runs - no orphaned complete event. Correct.
        emit_subagent_spawn(
            agent_name=self.agent_name,
            provider_id=self.provider_id,
            cli=self.cli,
            session_id=self.session_id or None,
            provenance_nonce=self.provenance_nonce or None,
            repo_root=self.repo_root,
        )
        return self

    def record_complete(self, *, stdout: str, exit_code: int, stderr: str = "") -> None:
        """Caller invokes after the Task tool returns to capture the result.

        Must be called inside the `with` block before exit. If not called,
        __exit__ emits outcome='orchestrator_skipped' instead of 'ok'.

        Capture policy: _capture_stdout is called here (not in __exit__) because
        __exit__ runs even when record_complete was never called (orchestrator_skipped
        path). There is nothing to capture on that path. The capture is best-effort
        and cannot block the complete event emitted by __exit__.

        Defensive coercion: stdout/stderr are coerced to str so that misbehaving
        Task tool returns (e.g. None) don't raise AttributeError before
        _completed=True is set. Without this, __exit__ would emit
        outcome='orchestrator_skipped' masking the real type error.
        """
        # Coerce non-str to str; None -> "".
        if not isinstance(stdout, str):
            stdout = "" if stdout is None else str(stdout)
        if not isinstance(stderr, str):
            stderr = "" if stderr is None else str(stderr)
        self._stdout = stdout
        self._stderr = stderr
        self._exit_code = exit_code
        self._stdout_sha = hashlib.sha256(stdout.encode("utf-8")).hexdigest()
        self._stderr_sha = hashlib.sha256(stderr.encode("utf-8")).hexdigest()
        self._completed = True
        # Best-effort: write sidecar files for post-mortem. Must not raise.
        _capture_stdout(
            session_id=self.session_id,
            agent_name=self.agent_name,
            stdout=stdout,
            stderr=stderr,
            repo_root=self.repo_root,
        )

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: object,
    ) -> bool:
        duration_ms = int((time.monotonic() - (self._start_ts or 0)) * 1000)
        outcome = "ok" if self._completed else "orchestrator_skipped"

        emit_subagent_complete(
            agent_name=self.agent_name,
            provider_id=self.provider_id,
            cli=self.cli,
            exit_code=self._exit_code,  # None -> "null" sentinel handled by emitter
            stdout_sha256=self._stdout_sha,
            stderr_sha256=self._stderr_sha,
            duration_ms=duration_ms,
            session_id=self.session_id or None,
            provenance_nonce=self.provenance_nonce or None,
            repo_root=self.repo_root,
            outcome=outcome,
        )
        return False  # never suppress an exception from inside the block


# ---------------------------------------------------------------------------
# Task 2.2: Hermes stub - synchronous delegate, adapter wiring deferred
# ---------------------------------------------------------------------------


def _delegate_via_hermes(prompt: str, provider_id: str, timeout_ms: int = 1_800_000) -> None:
    """Placeholder for the hermes delegate_task adapter.

    Hermes lives in docs/providers/provider-adapters.md and
    requires a separate wiring pass. When wired, this function will call
    hermes's synchronous delegate_task() and return (stdout_bytes, exit_code).
    On timeout it will return exit_code=124 (POSIX timeout sentinel).

    Until wired, this raises NotImplementedError so dispatch_sigma_subagent
    surfaces the gap immediately rather than silently emitting a dummy event.
    """
    raise NotImplementedError(
        "hermes adapter not yet wired in this repo. "
        "See docs/providers/provider-adapters.md for the wiring plan."
    )


# ---------------------------------------------------------------------------
# Task 2.2: _DispatchSubprocess - non-Claude CLI subprocess paths
# ---------------------------------------------------------------------------


class _DispatchSubprocess:
    """Context manager for non-Claude CLI subagent dispatch (Task 2.2, ab-978e93ed).

    Captures the provider snapshot at __enter__ time via spawn_with_provider_snapshot
    (which reads the active provider under a shared lock). Failover swaps AFTER
    snapshot do not affect this subagent's spawn event (invariant 9).

    The subprocess blocks until completion inside __exit__; the subagent_complete
    event is on disk before the orchestrator regains control (invariant 10).

    Subprocess crash (non-zero returncode) still emits subagent_complete with
    that exit_code -- the verifier soft-warns rather than the dispatcher raising.

    Supported CLIs and their spawn primitives:
      gemini    -> spawn_with_provider_snapshot(["gemini", "-p", prompt], ...)
      codex     -> spawn_with_provider_snapshot(["codex"], stdin=prompt.encode(), ...)
      openclaw  -> spawn_with_provider_snapshot(["openclaw", "-p", prompt], ...)
      hermes    -> _delegate_via_hermes(prompt) [synchronous; no subprocess - DEFERRED]
    """

    def __init__(
        self,
        *,
        agent_name: str,
        provider_id: str,
        cli: str,
        prompt: str,
        session_id: str | None,
        provenance_nonce: str | None,
        repo_root: Path,
    ) -> None:
        self.agent_name = agent_name
        self.provider_id = provider_id
        self.cli = cli
        self.prompt = prompt
        self.repo_root = repo_root

        # Resolve session context from target-state.md if not provided.
        if session_id is None or provenance_nonce is None:
            state = _read_target_state(repo_root)
            if session_id is None:
                session_id = state.get("session_id") or ""
            if provenance_nonce is None:
                provenance_nonce = state.get("provenance_nonce") or ""

        self.session_id = session_id
        self.provenance_nonce = provenance_nonce

        # Set during __enter__ / __exit__
        self._proc = None  # subprocess.Popen
        self._start_ts: float | None = None

    def _build_spawn_args(self) -> tuple[list[str], dict]:
        """Return (cmd, extra_popen_kwargs) for spawn_with_provider_snapshot."""
        if self.cli == "gemini":
            return ["gemini", "-p", self.prompt], {}
        elif self.cli == "codex":
            return ["codex"], {"stdin": self.prompt.encode("utf-8")}
        elif self.cli == "openclaw":
            return ["openclaw", "-p", self.prompt], {}
        elif self.cli == "hermes":
            # Hermes is synchronous; no subprocess. We raise here so the caller
            # sees the gap immediately rather than entering the block.
            _delegate_via_hermes(self.prompt, self.provider_id)
            # Unreachable, but satisfies type checker.
            return [], {}
        else:
            raise ValueError(f"unknown cli: {self.cli!r}")

    def __enter__(self) -> "_DispatchSubprocess":
        self._start_ts = time.monotonic()

        # Emit spawn BEFORE spawning the process. If this raises EventEmitFailed,
        # we bail out before touching the OS-level process (no zombie).
        emit_subagent_spawn(
            agent_name=self.agent_name,
            provider_id=self.provider_id,
            cli=self.cli,
            session_id=self.session_id or None,
            provenance_nonce=self.provenance_nonce or None,
            repo_root=self.repo_root,
        )

        # Snapshot captured here under shared lock (invariant 9).
        cmd, extra_kwargs = self._build_spawn_args()
        self._proc = spawn_with_provider_snapshot(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            **extra_kwargs,
        )
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc_val: BaseException | None,
        exc_tb: object,
    ) -> bool:
        if self._proc is None:
            # __enter__ didn't complete (e.g. spawn_with_provider_snapshot raised).
            # No complete event - correct: spawn event was not emitted.
            return False

        # Block until subprocess completes (invariant 10).
        # Default timeout: 30 minutes (1800s), matching the hermes delegate_task
        # convention. On TimeoutExpired: kill the process, capture whatever
        # partial output is available, emit complete with exit_code=124 (POSIX
        # timeout sentinel) and outcome='timeout'.
        _COMMUNICATE_TIMEOUT_S = 1800  # 30 minutes
        stdout_bytes: bytes = b""
        stderr_bytes: bytes = b""
        outcome: str
        returncode: int
        try:
            stdout_bytes, stderr_bytes = self._proc.communicate(timeout=_COMMUNICATE_TIMEOUT_S)
            returncode = self._proc.returncode
            outcome = "ok" if returncode == 0 else "error"
        except subprocess.TimeoutExpired:
            logger.warning(
                "sigma-review subagent '%s' timed out after %ds; killing",
                self.agent_name,
                _COMMUNICATE_TIMEOUT_S,
            )
            self._proc.kill()
            try:
                stdout_bytes, stderr_bytes = self._proc.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                pass  # give up on capturing output; still emit the complete event
            returncode = 124
            outcome = "timeout"

        duration_ms = int((time.monotonic() - (self._start_ts or 0)) * 1000)
        stdout_sha = hashlib.sha256(stdout_bytes).hexdigest()
        stderr_sha = hashlib.sha256(stderr_bytes).hexdigest()

        emit_subagent_complete(
            agent_name=self.agent_name,
            provider_id=self.provider_id,
            cli=self.cli,
            exit_code=returncode,
            stdout_sha256=stdout_sha,
            stderr_sha256=stderr_sha,
            duration_ms=duration_ms,
            session_id=self.session_id or None,
            provenance_nonce=self.provenance_nonce or None,
            repo_root=self.repo_root,
            outcome=outcome,
        )
        # Best-effort capture AFTER complete event is on disk. If capture fails,
        # the gate still passes because complete event already landed. Runs even
        # on subprocess crash (AC5-FR: partial stdout still gets captured).
        _capture_stdout(
            session_id=self.session_id,
            agent_name=self.agent_name,
            stdout=stdout_bytes,
            stderr=stderr_bytes,
            repo_root=self.repo_root,
        )
        return False  # never suppress exceptions from inside the block


@contextlib.contextmanager
def dispatch_sigma_subagent(
    *,
    agent_name: str,
    provider_id: str,
    cli: str,
    prompt: str = "",
    session_id: str | None = None,
    provenance_nonce: str | None = None,
    repo_root: Path | None = None,
):
    """Trust-boundary dispatch for sigma-review subagents.

    Usage (Claude path)::

        with dispatch_sigma_subagent(
            agent_name="code-reviewer",
            provider_id="claude-anthropic",
            cli="claude",
        ) as d:
            result = invoke_task_tool(...)        # orchestrator calls Task here
            d.record_complete(stdout=result.stdout, exit_code=0)
        # subagent_complete is on disk before this line executes.

    For cli='claude' (asymmetric path):
      - __enter__ emits subagent_spawn and yields a _DispatchClaudeTask.
      - Caller runs the Task tool inside the block and calls
        d.record_complete(stdout=..., exit_code=...) before exit.
      - __exit__ ALWAYS emits subagent_complete:
        - outcome='ok' when record_complete was called.
        - outcome='orchestrator_skipped' when record_complete was NOT called
          (exit_code=null). Wave 3.1's verifier rejects this with
          subagent_orchestrator_skipped (soft failure).
      - If emit_subagent_spawn raises EventEmitFailed on __enter__, the
        exception propagates and no complete event lands (correct: no spawn
        happened so there is nothing to complete).

    For cli in {'gemini', 'codex', 'openclaw', 'hermes'} (subprocess path, Task 2.2):
      - __enter__ captures the provider snapshot via spawn_with_provider_snapshot
        (shared-lock read; invariant 9) and spawns the subprocess.
      - __exit__ blocks until the subprocess completes (invariant 10), then
        emits subagent_complete with the sha256 hashes and duration.
      - Subprocess crash (non-zero rc) still emits complete - verifier soft-warns.
      - hermes raises NotImplementedError pending adapter wiring.

    For unknown CLIs: raises ValueError(f"unknown cli: {cli!r}").
    """
    root = repo_root or Path.cwd()

    if cli == "claude":
        ctx = _DispatchClaudeTask(
            agent_name=agent_name,
            provider_id=provider_id,
            cli=cli,
            session_id=session_id,
            provenance_nonce=provenance_nonce,
            repo_root=root,
        )
        with ctx as dispatch:
            yield dispatch
    elif cli in {"gemini", "codex", "openclaw", "hermes"}:
        ctx = _DispatchSubprocess(
            agent_name=agent_name,
            provider_id=provider_id,
            cli=cli,
            prompt=prompt,
            session_id=session_id,
            provenance_nonce=provenance_nonce,
            repo_root=root,
        )
        with ctx as dispatch:
            yield dispatch
    else:
        raise ValueError(f"unknown cli: {cli!r}")


# ---------------------------------------------------------------------------
# CG8 (Plan B, ab-0e5a921e): combo-aware target resolution.
#
# Precedence (highest priority first):
#   1. Per-agent pin: config.agents.<name>.provider (Spec 3) - single provider.
#   2. TARGET_COMBO env var: rotate via dispatch_with_combo(combo_name, fn).
#   3. settings active_combo: config.providers.active_combo.
#   4. settings active provider: config.providers.active.
#
# Per-agent pin wins over combo when both are set for the same agent
# (Spec 3 lock; combos compose with per-agent routing as additional
# fallback, not replacement).
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class DispatchTarget:
    """What the orchestrator should do for one subagent dispatch.

    Exactly one of ``provider_id`` or ``combo_name`` is set. ``source``
    names which precedence rule fired ('per_agent_pin', 'env_combo',
    'settings_combo', 'active_provider') for forensics + tests.
    """

    provider_id: str | None = None
    combo_name: str | None = None
    source: str = ""


def resolve_dispatch_target(
    agent_name: str,
    *,
    repo_root: Path | None = None,
    env: dict[str, str] | None = None,
) -> DispatchTarget:
    """Resolve the dispatch target for ``agent_name`` per CG8 precedence.

    Combo lookups use ``load_combos``; an unknown combo (TARGET_COMBO points
    to a deleted combo, or settings.yaml's active_combo names a missing
    combo) is logged at WARNING and falls through to the next rule rather
    than raising - this matches AC8.3's "fall through to active provider"
    contract.

    Note on the precedence chain (PR #230 review H2): the design doc lists
    five tiers (per-agent pin > skill modifier > env > settings active_combo
    > active provider), but skill modifier and env collapse to a single
    ``TARGET_COMBO`` slot here by design. Both surfaces (skill ``combo
    <name>`` modifier and ``--combo`` CLI flag) write to the same env var
    with last-writer-wins semantics; the conceptual five-tier chain is
    preserved by the SUM of write paths even though the read side sees
    one env tier. ``DispatchTarget.source = "env_combo"`` covers both.
    If a future spec needs the skill modifier to override an inherited
    env, introduce a distinct ``TARGET_COMBO_SKILL`` env var checked
    before ``TARGET_COMBO``.
    """
    import logging

    from fno.adapters.providers.loader import (
        load_combos,
        load_providers,
    )

    log = logging.getLogger(__name__)
    env_view = env if env is not None else os.environ
    root = repo_root or Path.cwd()

    # 1. Per-agent pin (Spec 3) wins over everything.
    try:
        config = load_providers(repo_root=root)
    except Exception as exc:  # ProviderConfigError or similar
        log.warning("resolve_dispatch_target: load_providers failed: %s", exc)
        config = None

    if config is not None:
        binding = config.agents.get(agent_name)
        if binding is not None and binding.provider in config.by_id:
            return DispatchTarget(
                provider_id=binding.provider, source="per_agent_pin"
            )

    # 2. TARGET_COMBO env var (set by --combo flag, skill modifier, or manifest).
    env_combo = env_view.get("TARGET_COMBO")
    try:
        combos = load_combos(repo_root=root)
    except Exception as exc:
        log.warning("resolve_dispatch_target: load_combos failed: %s", exc)
        combos = {}

    if env_combo:
        if env_combo in combos:
            return DispatchTarget(combo_name=env_combo, source="env_combo")
        log.warning(
            "resolve_dispatch_target: TARGET_COMBO=%r not found in combos %s; "
            "falling through to next precedence rule.",
            env_combo,
            sorted(combos),
        )

    # 3. settings active_combo - project-local-over-global, mirroring
    #    load_providers/load_combos. Walk both roots (config.toml, else legacy
    #    settings.yaml) so a global combo is reachable when no project-local
    #    override exists. (PR #230 Gemini MEDIUM #2: the previous
    #    implementation only inspected project-local.)
    from pathlib import Path as _Path

    from fno.config import config_read_candidates, read_config_flat

    active_combo: str | None = None
    for candidate in config_read_candidates([
        root / ".fno" / "settings.yaml",
        # Bootstrap: cannot use paths.config_file() here (settings loader self-reference)
        _Path.home() / ".fno" / "settings.yaml",
    ]):
        if not candidate.is_file():
            continue
        providers = read_config_flat(candidate).get("providers")
        ac = providers.get("active_combo") if isinstance(providers, dict) else None
        if ac:
            active_combo = ac
            break  # project-local wins; do not consult global

    if active_combo and active_combo in combos:
        return DispatchTarget(combo_name=active_combo, source="settings_combo")
    if active_combo and active_combo not in combos:
        # Configured but unknown - log distinctly so operators can tell this
        # apart from "no active_combo configured at all". Falls through to
        # the active-provider branch (matches the env_combo path's
        # log-and-fall-through behavior).
        log.warning(
            "resolve_dispatch_target: settings active_combo=%r not in "
            "loaded combos %s; falling through to active provider.",
            active_combo, sorted(combos),
        )

    # 4. Fall through to active provider.
    if config is not None and config.active and config.active in config.by_id:
        return DispatchTarget(provider_id=config.active, source="active_provider")

    return DispatchTarget(source="unresolved")

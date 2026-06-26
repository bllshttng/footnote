"""fno.agents.providers.claude — Claude --bg adapter for US1 dispatch.

Surface:

- :func:`bg_create` — invoke ``claude --bg --name <name> <message>``, parse
  the supervisor short-id from stdout, return :class:`ProviderResult`.
- :func:`parse_short_id` — pure regex extractor for the documented stdout
  shape, used both by ``bg_create`` and by callers that already have the
  raw stdout in hand.
- :class:`ProviderParseError` — raised when stdout does not match the
  ``^backgrounded · ([0-9a-f]{8}) · `` contract.
- :class:`ProviderSubprocessError` — raised when ``claude --bg`` exits
  non-zero, preserving the verbatim stderr and exit code for the caller
  to surface (AC1-FR subprocess non-zero).

Locked Decision 6: the short-id contract is the regex above. Any
``claude`` release that changes the format fails closed at this layer
with a diagnostic carrying the first 200 chars of stdout; the registry
write never runs.

Argv-overflow (AC1-EDGE 300KB → stdin): the implementation routes the
message via ``subprocess.run(input=msg)`` when the rendered argv would
exceed 200KB. The exact stdin-trigger argv shape is version-dependent
in real ``claude``; the integration tests use a fake script that reads
from stdin unconditionally, so the substrate is verified independent of
real CLI drift. When ``claude`` changes its stdin convention, only the
``_ARGV_OVERFLOW_THRESHOLD`` branch needs to follow.
"""
from __future__ import annotations

import html
import json
import os
import re
import socket
import subprocess
import sys
import time
from pathlib import Path
from typing import Literal, Optional

OrphanReason = Literal["not-found", "socket-null", "liveness-failed"]

from fno.agents.providers._claude_session_registry import (
    TERMINAL_STATES,
    SessionLocator,
    _jobs_dir_for,
    locate_session,
    read_state_json,
    read_timeline_tail,
    resolve_session_uuid,
)
from fno.agents.providers.base import ProviderResult, ReachabilityProbeError
from fno.claims import ClaimHeldByOther, acquire_claim, release_claim
from fno.claims.io import global_claims_root

# Locked Decision 6: 8 lowercase hex chars after "backgrounded · ".
_SHORT_ID_PATTERN = re.compile(r"^backgrounded · ([0-9a-f]{8}) · ")

# claude >= 2.1.191 colorizes the short-id in the --bg receipt even over a
# non-TTY pipe (`backgrounded · \x1b[36m<id>\x1b[39m · <name>`), which the
# anchored hex pattern above cannot match through. Strip CSI/SGR escapes
# before matching so the parser is robust to color drift across versions.
_ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")

# AC1-EDGE / Locked Decision 5: route messages above this size via stdin.
# Conservative threshold under both macOS argv limit (~256KB) and Linux
# (~128KB), so dispatch is portable.
_ARGV_OVERFLOW_THRESHOLD = 200 * 1024

# How many leading chars of stdout to include in parse-failure diagnostics.
_STDOUT_HEAD_LIMIT = 200

# Indirection so unit tests can monkeypatch the subprocess call without
# touching the real subprocess module's API surface. Lookup happens via
# module globals at call time so ``monkeypatch.setattr(claude_mod,
# "_subprocess_run", fake)`` works.
_subprocess_run = subprocess.run


class ProviderParseError(RuntimeError):
    """Raised when claude --bg stdout does not match the short-id contract.

    Carries the first :data:`_STDOUT_HEAD_LIMIT` chars of stdout so the
    caller can dump them to stderr for AC1-FR diagnostics.
    """

    def __init__(self, stdout_head: str) -> None:
        super().__init__(
            f"unable to parse short-id from claude --bg output: "
            f"first {len(stdout_head)} chars: {stdout_head!r}"
        )
        self.stdout_head = stdout_head


class ProviderSubprocessError(RuntimeError):
    """Raised when ``claude --bg`` exits non-zero, times out, or is missing.

    Preserves the verbatim stderr and exit code so the dispatcher can
    surface them unwrapped (AC1-FR subprocess non-zero / auth-quota).
    """

    def __init__(self, exit_code: int, stderr: str) -> None:
        super().__init__(
            f"claude --bg exited {exit_code}: {stderr!r}"
        )
        self.exit_code = exit_code
        self.stderr = stderr


def parse_short_id(stdout: str) -> str:
    """Extract the 8-hex short-id from claude --bg's stdout.

    Inspects only the first line. Raises :class:`ProviderParseError`
    with the first :data:`_STDOUT_HEAD_LIMIT` chars of stdout if the
    contract is not matched.
    """
    if not stdout:
        raise ProviderParseError(stdout_head="")

    first_line = _ANSI_ESCAPE.sub("", stdout.split("\n", 1)[0])
    match = _SHORT_ID_PATTERN.match(first_line)
    if match is None:
        raise ProviderParseError(stdout_head=stdout[:_STDOUT_HEAD_LIMIT])
    return match.group(1)


def _build_argv(name: str, message: str, use_stdin: bool) -> list[str]:
    """Render the argv list for ``claude --bg``.

    When ``use_stdin`` is True, the message is read from stdin by claude
    instead of being passed as an argv token (AC1-EDGE 300KB path). The
    placeholder marker is left to the implementation: real ``claude`` may
    accept ``-`` as a stdin sentinel, ``--message-from-stdin``, or just
    omit the message argv entirely. The fake-claude script in the test
    suite reads stdin unconditionally when ``FAKE_CLAUDE_STDIN_DUMP`` is
    set, so the substrate is verified portably. The smoke-marker test
    (Locked Decision 4) catches real-CLI drift.
    """
    argv = ["claude", "--bg", "--name", name]
    if not use_stdin:
        argv.append(message)
    return argv


def bg_create(
    name: str,
    message: str,
    cwd: Path,
    timeout: Optional[int] = None,
    role: Optional[str] = None,
) -> ProviderResult:
    """Invoke ``claude --bg`` for a brand-new supervisor session.

    Args:
        name: Agent name (already validated by the caller).
        message: First message to seed the supervisor session.
        cwd: Working directory passed to subprocess.run so claude inherits
            it for tool execution.
        timeout: Subprocess timeout in seconds. ``None`` means no timeout.
        role: Optional routing role (x-d2fe). An auxiliary role (coordinate /
            tidy / orient / consolidate) with a configured provider key routes
            the worker to a secondary provider via env overrides; ``None`` or a
            production role leaves the spawn env byte-for-byte as today.

    Returns:
        :class:`ProviderResult` with ``session_id_out`` set to the parsed
        short-id on success.

    Raises:
        ProviderSubprocessError: ``claude --bg`` exited non-zero.
        ProviderParseError: stdout did not match the short-id contract.
    """
    msg_bytes = message.encode("utf-8")
    use_stdin = len(msg_bytes) > _ARGV_OVERFLOW_THRESHOLD
    argv = _build_argv(name=name, message=message, use_stdin=use_stdin)

    # Inject FNO_AGENT_* env vars so nested `fno agents ask` calls
    # from inside the spawned agent attribute back to this parent.
    # FNO_AGENT_SESSION is intentionally omitted on create — the
    # session id is not known until claude --bg returns; env vars cannot
    # be set retroactively. The nested-attribution path handles missing
    # SESSION gracefully via caller_kind=nested_agent + from_session_id=None.
    spawn_env = dict(os.environ)
    spawn_env["FNO_AGENT_SELF"] = name
    spawn_env["FNO_AGENT_PROVIDER"] = "claude"

    # Role-based model routing (x-d2fe). An auxiliary role with a configured
    # provider key merges ANTHROPIC_BASE_URL/AUTH_TOKEN + the model env vars so
    # the worker runs on the secondary provider (z.ai GLM, ...); no role /
    # production role / missing key returns None and changes nothing
    # (fail-safe). Clear any stale ANTHROPIC_API_KEY so the routed auth token is
    # the credential that wins for the routed worker.
    from fno.agents.model_routing import resolve_route

    route = resolve_route(role, notice=lambda m: print(m, file=sys.stderr))
    if route:
        spawn_env.pop("ANTHROPIC_API_KEY", None)
        spawn_env.update(route)

    start = time.monotonic()
    try:
        result = _subprocess_run(
            argv,
            cwd=str(cwd),
            capture_output=True,
            text=True,
            timeout=timeout,
            input=message if use_stdin else None,
            env=spawn_env,
        )
    except subprocess.TimeoutExpired as exc:
        # Surface as a subprocess error so the dispatcher's event +
        # exit-code framing applies uniformly. The "claude --bg" process
        # was killed by subprocess.run on timeout; any half-created
        # supervisor session is on the caller to reconcile via US3 logs.
        raise ProviderSubprocessError(
            exit_code=124,
            stderr=f"claude --bg timed out after {exc.timeout}s",
        ) from exc
    except FileNotFoundError as exc:
        # claude went missing between the PATH check and exec. Rare race
        # (user uninstalled claude mid-flight) but still possible.
        raise ProviderSubprocessError(
            exit_code=127,
            stderr=f"claude CLI not found: {exc}",
        ) from exc
    duration_ms = int((time.monotonic() - start) * 1000)

    stdout = result.stdout or ""
    stderr = result.stderr or ""
    exit_code = result.returncode

    if exit_code != 0:
        raise ProviderSubprocessError(exit_code=exit_code, stderr=stderr)

    short_id = parse_short_id(stdout)

    return ProviderResult(
        exit_code=exit_code,
        stdout=stdout,
        stderr=stderr,
        duration_ms=duration_ms,
        session_id_out=short_id,
    )


# ===========================================================================
# Spawn-time full session-UUID capture (ab-f1b0ccd1, US1 / AC1-HP)
# ===========================================================================
#
# `claude --bg` registers a worker by its 8-hex jobId (`claude_short_id`), but
# the stream-json `--resume` lane that the live `/agents chat` escalation rides
# keys on the FULL session UUID -- the jobId is only a 32-bit prefix, not
# collision-safe as a resume key. So at spawn we best-effort resolve the full
# UUID and persist it alongside the short-id, making every spawned worker a
# first-class, adoptable citizen of the mesh.
#
# The resolution is best-effort and NEVER gates the short-id report: a miss
# leaves the field None (the live escalation then opens a fresh pipe with a
# visible note rather than adopting a guessed UUID). In the common case
# `claude` has already written the `~/.claude/sessions/<pid>.json` mapping by
# the time `claude --bg` returns the short-id, so the first probe succeeds; the
# bounded retry only covers the rare write-lag window. The retry count and
# backoff are read from module globals at call time so a test can patch
# `_SPAWN_UUID_RETRY_BACKOFF_SEC` to 0 and neither sleep nor read real state.
_SPAWN_UUID_RETRY_ATTEMPTS = 6
_SPAWN_UUID_RETRY_BACKOFF_SEC = 0.3


def resolve_session_uuid_at_spawn(
    short_id: str,
    *,
    _resolver=None,
    _sleep=time.sleep,
) -> Optional[str]:
    """Best-effort resolve the full session UUID for a freshly spawned worker.

    Returns the full ``sessionId`` matching ``short_id`` (the 8-hex jobId), or
    ``None`` when it cannot be resolved within the bounded retry window. This
    NEVER raises and NEVER blocks the short-id report past the bounded window:
    an unresolved UUID is a tolerated miss, not a failure (AC1-HP / the
    "full UUID unresolvable at spawn" multi-perspective row).

    ``_resolver`` / ``_sleep`` are injectable seams for direct unit tests; the
    retry count and backoff are read from module globals at call time so a test
    can patch them without re-binding a default.
    """
    if not short_id:
        return None
    resolver = _resolver if _resolver is not None else resolve_session_uuid
    attempts = max(1, _SPAWN_UUID_RETRY_ATTEMPTS)
    for i in range(attempts):
        try:
            uuid = resolver(short_id)
        except Exception:
            # Best-effort: any reader error (fs race, drift) degrades to a
            # retry/miss, never propagates to gate the launch.
            uuid = None
        if uuid:
            return uuid
        if i < attempts - 1 and _SPAWN_UUID_RETRY_BACKOFF_SEC > 0:
            try:
                _sleep(_SPAWN_UUID_RETRY_BACKOFF_SEC)
            except Exception:
                pass
    return None


# ===========================================================================
# US2 — follow-up via messaging socket
# ===========================================================================
#
# Reverse-engineered from claude 2.1.143 (functions BG8/CE7/Ag5/IE7); see
# the design doc and _claude_session_registry for surface-level docs. The
# adapter is intentionally a thin wrapper: a future MCP-channel-server
# replacement (US6) swaps this module and leaves dispatch.py untouched.

# Connect timeout for the 250 ms liveness probe. Short enough to fail fast
# when the messaging socket is dead, long enough to absorb the kernel's
# AF_UNIX setup. Tunable for tests but never exposed on the CLI.
_LIVENESS_PROBE_TIMEOUT_SEC = 0.25

# Socket timeout for the actual send. 5s mirrors claude's documented
# read deadline on the receive side (function CE7).
_SEND_SOCKET_TIMEOUT_SEC = 5.0


class ProviderOrphanError(RuntimeError):
    """Raised when the target bg session can't be reached for follow-up.

    ``reason`` is one of ``"not-found"`` (no sessions/<pid>.json entry
    matches the short-id), ``"socket-null"`` (matched entry has
    ``messagingSocketPath: null``; session is suspended), or
    ``"liveness-failed"`` (matched entry's socket exists but the 250 ms
    connect probe failed). The dispatch layer maps this to exit code 13.
    """

    def __init__(self, *, reason: OrphanReason, short_id: str,
                 detail: str = "") -> None:
        super().__init__(
            f"agent short-id {short_id!r} is not reachable (reason: {reason})"
            + (f": {detail}" if detail else "")
        )
        self.reason: OrphanReason = reason
        self.short_id = short_id
        self.detail = detail


class ProviderSocketError(RuntimeError):
    """Raised when the AF_UNIX send fails (connect/write/close error).

    The underlying ``OSError`` is preserved as ``__cause__`` so the
    dispatcher can include the verbatim error message in stderr.
    """

    def __init__(self, message: str) -> None:
        super().__init__(f"messaging socket error: {message}")


class ProviderTimeoutError(RuntimeError):
    """Raised when state.json fails to transition past baseline in time.

    Carries ``elapsed_sec`` so the dispatcher can format the AC-required
    `message sent but no reply within <N>s` stderr line.
    """

    def __init__(self, *, elapsed_sec: float, short_id: str = "") -> None:
        super().__init__(
            f"timed out waiting for reply after {elapsed_sec:.1f}s"
            + (f" (short_id={short_id})" if short_id else "")
        )
        self.elapsed_sec = elapsed_sec
        self.short_id = short_id


def _build_envelope(message: str, from_name: str) -> bytes:
    """Render the BG8 envelope as UTF-8 bytes including the trailing newline.

    Tag/attribute structure is fixed by claude 2.1.143's CE7 listener:

      <cross-session-message from-name="<escaped>">
      <message-text>
      </cross-session-message>

    XML-attribute escape is mandatory; the dispatch layer rejects
    XML-unsafe input before we get here, but a defensive escape keeps
    the envelope shape safe in every code path.
    """
    safe_from = html.escape(from_name, quote=True)
    wrapped = (
        f"<cross-session-message from-name=\"{safe_from}\">\n"
        f"{message}\n"
        f"</cross-session-message>"
    )
    envelope = {
        "type": "user",
        "message": {"role": "user", "content": wrapped},
        "priority": "next",
    }
    return (json.dumps(envelope, separators=(",", ":")) + "\n").encode("utf-8")


def send_to_session(sock_path: str, content: str, from_name: str) -> None:
    """Single-shot send of the BG8 envelope over the messaging socket.

    Opens an AF_UNIX SOCK_STREAM, writes the rendered envelope plus a
    newline, closes. Raises :class:`ProviderSocketError` on any
    connect/write/close failure, with the underlying ``OSError`` chained
    as ``__cause__``.

    On AF_UNIX SOCK_STREAM, post-``sendall`` close-time errors (EIO,
    ECONNRESET) are the only reliable "your bytes never made it across"
    signal — ``sendall`` only guarantees the bytes hit the kernel
    buffer, not that the peer accepted them. So a close OSError is
    propagated as a send failure rather than swallowed.
    """
    payload = _build_envelope(content, from_name)
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(_SEND_SOCKET_TIMEOUT_SEC)
    primary_exc: Optional[BaseException] = None
    try:
        sock.connect(sock_path)
        sock.sendall(payload)
    except OSError as exc:
        primary_exc = exc
    finally:
        try:
            sock.close()
        except OSError as close_exc:
            # If a primary error already exists, prefer it; close
            # failures after a send error are noise. Otherwise surface
            # the close error - on AF_UNIX it signals the recipient
            # didn't accept the bytes.
            if primary_exc is None:
                raise ProviderSocketError(
                    f"close after send failed: {close_exc}"
                ) from close_exc
    if primary_exc is not None:
        raise ProviderSocketError(str(primary_exc)) from primary_exc


def liveness_probe(sock_path: str) -> bool:
    """Return ``True`` iff a 250 ms connect to ``sock_path`` succeeds.

    Catches every ``OSError`` (ENOENT, ECONNREFUSED, timeout). The
    probe immediately closes the socket; it does not write or read.
    """
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(_LIVENESS_PROBE_TIMEOUT_SEC)
    try:
        try:
            sock.connect(sock_path)
        except OSError:
            return False
        return True
    finally:
        try:
            sock.close()
        except OSError:
            pass


def wait_for_reply(
    jobs_dir: Path,
    baseline_updated_at: Optional[str],
    timeline_offset: int,
    timeout: float,
    poll_interval: float = 0.5,
) -> str:
    """Poll ``jobs_dir/state.json`` until a fresh terminal state appears.

    Exit conditions per Locked Decision #9:

      ``state.updated_at`` is lexicographically greater than
      ``baseline_updated_at`` AND ``state`` is in
      ``{done, completed, failed, needs-input}``.

    On exit, prefer ``state.output.result`` as the reply text; fall
    back to :func:`read_timeline_tail` from the captured byte offset
    when ``output.result`` is empty or absent.

    Raises :class:`ProviderTimeoutError` when ``timeout`` seconds pass
    without satisfying the exit condition. ``baseline_updated_at`` may
    be ``None`` when the recipient never wrote state.json before the
    send; in that case any terminal state with a non-null ``updated_at``
    counts as a transition.
    """
    deadline = time.monotonic() + timeout
    final_snap = None
    while True:
        try:
            snap = read_state_json(jobs_dir)
        except (FileNotFoundError, json.JSONDecodeError):
            # FileNotFoundError: recipient hasn't created state.json yet.
            # JSONDecodeError: atomic-rename window (read_state_json
            # already retried once inside; if we still see this we treat
            # it as transient and poll again).
            snap = None
        # PermissionError, IsADirectoryError, EROFS, etc. are NOT
        # retryable - polling for 600 s would mask the cause as a
        # timeout. Let them propagate.

        if snap is not None and snap.state in TERMINAL_STATES:
            advanced = (
                baseline_updated_at is None
                or (snap.updated_at is not None
                    and snap.updated_at > baseline_updated_at)
            )
            if advanced:
                final_snap = snap
                break

        if time.monotonic() >= deadline:
            raise ProviderTimeoutError(elapsed_sec=timeout)

        time.sleep(poll_interval)

    if final_snap.output_result:
        return final_snap.output_result
    return read_timeline_tail(jobs_dir, timeline_offset)


def ask_followup(
    claude_short_id: str,
    message: str,
    cwd: Path,
    from_name: str,
    timeout: float,
    poll_interval: float = 0.5,
    jobs_dir: Optional[Path] = None,
) -> str:
    """Orchestrate locate -> probe -> send -> wait_for_reply for one follow-up.

    Returns the recipient's reply text (output.result preferred,
    timeline tail fallback). Raises :class:`ProviderOrphanError` when
    the session is not reachable, :class:`ProviderSocketError` on send
    failure, and :class:`ProviderTimeoutError` on poll timeout.

    ``cwd`` is currently unused — the messaging socket needs no cwd
    inheritance — but is part of the signature for parity with
    :func:`bg_create` and future use by codex/gemini follow-up. The
    parameter is intentionally retained.
    """
    del cwd  # parity placeholder

    locator: Optional[SessionLocator] = locate_session(claude_short_id)
    if locator is None:
        # Distinguish socket-null (suspended) from not-found by re-reading
        # the sessions dir: not-found is "no entry at all"; socket-null is
        # "entry exists but messagingSocketPath is null".
        reason = _classify_orphan_reason(claude_short_id)
        raise ProviderOrphanError(reason=reason, short_id=claude_short_id)

    if not liveness_probe(locator.messaging_socket_path):
        raise ProviderOrphanError(
            reason="liveness-failed", short_id=claude_short_id,
            detail=locator.messaging_socket_path,
        )

    # Capture baseline BEFORE send so a same-tick recipient transition
    # is not missed AND so pre-send output.result cannot impersonate the
    # reply (AC2-EDGE baseline invariant).
    target_jobs_dir = jobs_dir if jobs_dir is not None else locator.jobs_dir
    baseline_updated_at: Optional[str] = None
    timeline_offset = 0
    try:
        snap = read_state_json(target_jobs_dir)
        baseline_updated_at = snap.updated_at
    except (FileNotFoundError, OSError, json.JSONDecodeError):
        baseline_updated_at = None
    timeline_path = target_jobs_dir / "timeline.jsonl"
    if timeline_path.exists():
        try:
            timeline_offset = timeline_path.stat().st_size
        except OSError:
            timeline_offset = 0

    send_to_session(locator.messaging_socket_path, message, from_name)

    return wait_for_reply(
        jobs_dir=target_jobs_dir,
        baseline_updated_at=baseline_updated_at,
        timeline_offset=timeline_offset,
        timeout=timeout,
        poll_interval=poll_interval,
    )


def _classify_orphan_reason(short_id: str) -> OrphanReason:
    """Distinguish ``not-found`` from ``socket-null`` after locate returned None.

    ``locate_session`` returns ``None`` for both cases — we re-scan the
    sessions directory to figure out which one applies, so the dispatch
    layer can map to the AC-required stderr reason discriminator.
    """
    from fno.agents.providers._claude_session_registry import (
        _sessions_dir,
    )

    sessions = _sessions_dir()
    if not sessions.exists():
        return "not-found"
    for path in sessions.glob("*.json"):
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, OSError, UnicodeDecodeError):
            continue
        if not isinstance(raw, dict):
            continue
        if raw.get("jobId") != short_id:
            continue
        if raw.get("kind") != "bg":
            continue
        sock = raw.get("messagingSocketPath")
        if sock is None or sock == "":
            return "socket-null"
    return "not-found"


# ---------------------------------------------------------------------------
# US3 — read-path additions: claude_agents_json() + logs()
# ---------------------------------------------------------------------------

# Locked Decision 1 — 3-second shellout timeout for the live-status probe.
_AGENTS_JSON_TIMEOUT_DEFAULT = 3.0

# Locked Decision 3 — 500ms polling cadence for --follow without an
# inotify/fsevents dep. Matches US2's state.json polling pattern.
_FOLLOW_POLL_INTERVAL = 0.5


# Claude supervisor's documented live-status sentinel set. A value
# outside this set surfaces a forensic warning so vocabulary drift
# (e.g. claude renaming "Working" to "running") is loud, not silent.
# The value still passes through unchanged so consumers that already
# adapted aren't blocked on our config.
KNOWN_LIVE_STATUSES = frozenset({"Working", "Needs input", "Idle"})


def claude_agents_json(
    timeout: float = _AGENTS_JSON_TIMEOUT_DEFAULT,
) -> tuple[dict[str, dict], list[str]]:
    """Shell out to ``claude agents --json`` and return a short-id → fields map.

    Best-effort. Returns ``({}, warnings)`` on every failure mode so the
    caller can fall back to registry-only data (AC1-FR). Failure modes
    that map to a non-empty warning list:

    - ``FileNotFoundError`` — ``claude`` binary missing from PATH.
    - ``subprocess.TimeoutExpired`` — exceeded the per-call timeout.
    - non-zero exit code — claude itself reported an error.
    - ``json.JSONDecodeError`` — stdout was not parseable JSON.
    - structural drift — a record missing the documented ``short_id`` key
      is dropped with a forensic warning naming the missing field
      (AC3-FR).

    The success shape:

    ::

        {
            "<short_id>": {"live_status": "Working" | "Needs input" | "Idle"},
            ...
        }

    Per Locked Decision 1, the source of live-status truth is claude's
    supervisor view — replicating that state in fno would be duplicate
    truth. We translate claude's field name (``status`` in its --json
    output) to our orthogonal ``live_status`` axis (Locked Decision 6).
    """
    argv = ["claude", "agents", "--json"]

    try:
        result = _subprocess_run(
            argv,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return {}, [
            f"claude agents --json timed out after {timeout}s; "
            "live_status unavailable, falling back to registry-only view"
        ]
    except FileNotFoundError:
        return {}, [
            "claude agents --json: claude binary not found on PATH; "
            "live_status unavailable, falling back to registry-only view"
        ]
    except OSError as exc:
        return {}, [
            f"claude agents --json raised OSError: {exc}; "
            "live_status unavailable, falling back to registry-only view"
        ]

    if result.returncode != 0:
        head = (result.stderr or result.stdout or "").strip()[:200]
        return {}, [
            f"claude agents --json exited non-zero ({result.returncode}); "
            f"stderr: {head!r}; falling back to registry-only view"
        ]

    try:
        parsed = json.loads(result.stdout or "{}")
    except json.JSONDecodeError as exc:
        return {}, [
            f"claude agents --json parse failure: {exc}; "
            "falling back to registry-only view"
        ]

    # Claude's `agents --json` output shape isn't formally pinned: some
    # versions return ``{"agents": [...]}``, others return the bare
    # ``[...]`` array (per the CLI docs). Treat both as valid; the
    # legacy ``{"agents": ...}`` wrapper is unwrapped and a bare list
    # is used directly. Anything else degrades to the warned-fallback
    # path instead of crashing with ``AttributeError`` on ``.get()``.
    if isinstance(parsed, list):
        rows = parsed
    elif isinstance(parsed, dict):
        rows = parsed.get("agents")
        if not isinstance(rows, list):
            return {}, [
                "claude agents --json response missing 'agents' array; "
                "falling back to registry-only view"
            ]
    else:
        return {}, [
            f"claude agents --json response has unexpected shape "
            f"({type(parsed).__name__}); falling back to registry-only view"
        ]

    out_map: dict[str, dict] = {}
    warnings: list[str] = []
    for index, row in enumerate(rows):
        if not isinstance(row, dict):
            warnings.append(
                f"claude agents --json row {index} is not an object; skipped"
            )
            continue
        short_id = row.get("short_id")
        if not isinstance(short_id, str) or not short_id:
            warnings.append(
                f"claude agents --json row {index} missing short_id; skipped"
            )
            continue
        live_status = row.get("status")
        if live_status is not None and live_status not in KNOWN_LIVE_STATUSES:
            warnings.append(
                f"claude agents --json row {index} has unrecognized status="
                f"{live_status!r} (expected one of {sorted(KNOWN_LIVE_STATUSES)}); "
                "passing through unchanged"
            )
        out_map[short_id] = {"live_status": live_status}
    return out_map, warnings


# Indirection for the Popen-based follow path so tests can substitute a
# fake. Lookup at call time via module globals, same pattern as
# ``_subprocess_run``.
_subprocess_popen = subprocess.Popen


def logs(
    short_id: str,
    tail: Optional[int] = None,
    follow: bool = False,
    stdout=None,
    stderr=None,
) -> int:
    """Read or follow a Claude agent's log via ``claude logs <short_id>``.

    Behavior:

    - Without ``--follow``: invokes ``claude logs <short_id>`` via a
      capturing subprocess.run, slices ``--tail N`` in-process, writes
      stdout/stderr to the caller's streams, returns claude's exit code.
    - With ``--follow``: invokes ``claude logs --follow <short_id>`` via
      ``subprocess.Popen`` and forwards stdout line-by-line in real
      time so the operator sees output as claude emits it. SIGINT
      (``KeyboardInterrupt``) is intercepted, propagated to the child,
      and the function returns 0 (AC2-FR clean-exit contract).
    - With ``--tail`` AND ``--follow``: the tail is ignored on the
      claude path because we cannot retroactively buffer streamed
      lines; if the operator needs both, they can re-run with the same
      ``--tail`` value and without ``--follow``.

    Locked Decision 2 — codex/gemini logs return informative exit-13
    messages from the read.py layer; this function only handles the
    Claude path.

    Locked Decision 3 — when our own poll loop is needed (codex/gemini
    tee, _follow_jsonl in read.py), the cadence is 500ms. Claude's
    own ``--follow`` is delegated to the upstream supervisor.
    """
    import signal as _signal
    import sys

    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr

    argv = ["claude", "logs", short_id]
    if follow:
        argv.append("--follow")

    if follow:
        # Merge stderr into stdout via subprocess.STDOUT so there is
        # only one pipe to drain. If we kept stderr separate while only
        # draining stdout in the loop, a child that wrote >~64KB of
        # stderr during the follow would fill the kernel pipe buffer,
        # block on its next stderr write, stop emitting stdout, and the
        # stream would silently stall. STDOUT-merge moots that hazard
        # and matches operator expectations: `claude logs --follow` is
        # a passthrough surface where stream separation is not useful.
        try:
            proc = _subprocess_popen(
                argv,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,  # line-buffered so the operator sees output live
            )
        except FileNotFoundError:
            err.write(
                "claude logs: claude binary not found on PATH; install claude or check $PATH\n"
            )
            return 127
        except OSError as exc:
            err.write(
                f"claude logs {short_id!r}: OSError invoking claude: {exc}\n"
            )
            return 1

        try:
            for line in iter(proc.stdout.readline, ""):
                out.write(line)
                if hasattr(out, "flush"):
                    out.flush()
            return proc.wait()
        except KeyboardInterrupt:
            # AC2-FR: forward SIGINT to the child, then exit cleanly
            # with code 0 and no traceback on stderr.
            try:
                proc.send_signal(_signal.SIGINT)
                proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                # Child ignored SIGINT — escalate to SIGKILL. The
                # second wait has its own timeout so a wedged child
                # doesn't hang forever; SIGKILL is delivered by the
                # kernel and cannot be ignored, but reaping can race.
                proc.kill()
                try:
                    proc.wait(timeout=2.0)
                except subprocess.TimeoutExpired:
                    pass
            except (ProcessLookupError, OSError):
                # Child already exited (race between EOF and SIGINT
                # arrival) — nothing left to clean up.
                pass
            return 0
        finally:
            # Cleanup after a possibly-killed child: OSError on close
            # is suppressed intentionally because the underlying fd
            # may already have been reaped by proc.wait() in the
            # SIGINT escalation branch.
            if proc.stdout is not None:
                try:
                    proc.stdout.close()
                except OSError:
                    pass

    # Non-follow path — capture and emit, slicing tail in-process.
    try:
        result = _subprocess_run(argv, capture_output=True, text=True)
    except FileNotFoundError:
        err.write(
            "claude logs: claude binary not found on PATH; install claude or check $PATH\n"
        )
        return 127
    except OSError as exc:
        err.write(
            f"claude logs {short_id!r}: OSError invoking claude: {exc}\n"
        )
        return 1

    raw_stdout = result.stdout or ""
    raw_stderr = result.stderr or ""

    if tail is not None and tail > 0 and raw_stdout:
        lines = raw_stdout.splitlines(keepends=True)
        raw_stdout = "".join(lines[-tail:])
    elif tail is not None and tail == 0:
        raw_stdout = ""

    out.write(raw_stdout)
    if raw_stderr:
        err.write(raw_stderr)

    # If claude exited non-zero with no stderr to forward, surface a
    # diagnostic so the operator isn't stuck with a bare exit code.
    if result.returncode != 0 and not raw_stderr:
        err.write(
            f"claude logs {short_id!r} exited {result.returncode} with no stderr output\n"
        )

    return result.returncode


# ---------------------------------------------------------------------------
# US4-lifecycle shellouts: claude stop / rm / attach + reconcile reachability
# probe. Each helper is a thin wrapper around ``claude <verb> <short_id>``
# with a timeout. They live here so the fno ``dispatch`` layer can stay
# provider-agnostic and so the per-provider quirks (capture vs inherit-stdio
# for the interactive attach verb) sit alongside the existing ``logs``
# adapter.
# ---------------------------------------------------------------------------


def claude_stop(short_id: str, *, timeout: float = 30.0) -> tuple[int, str]:
    """Run ``claude stop <short_id>`` with a wall-clock timeout.

    Returns ``(exit_code, stderr_text)``. The caller decides whether to
    surface stderr verbatim and what exit code to translate to.

    Raises:
        FileNotFoundError: when ``claude`` is not on PATH. The dispatch
            layer maps this to exit 14, mirroring US1's invariant.
        subprocess.TimeoutExpired: when the wall-clock exceeds
            ``timeout``. The dispatch layer maps this to exit 15.
    """
    result = _subprocess_run(
        ["claude", "stop", short_id],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return (result.returncode, result.stderr or "")


def claude_rm(short_id: str, *, timeout: float = 30.0) -> tuple[int, str]:
    """Run ``claude rm <short_id>`` with a wall-clock timeout.

    Returns ``(exit_code, stderr_text)``. Non-zero exits do NOT raise; the
    caller (``dispatch.rm_agent``) inspects exit_code to decide whether
    ``--force`` should override the refusal.

    Raises:
        FileNotFoundError: claude not on PATH (caller maps to exit 14).
        subprocess.TimeoutExpired: wall-clock exceeded (caller maps to 15).
    """
    result = _subprocess_run(
        ["claude", "rm", short_id],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return (result.returncode, result.stderr or "")


def claude_attach(short_id: str) -> int:
    """Run ``claude attach <short_id>`` inheriting parent stdio.

    Returns claude's exit code. Output is NOT captured: the claude TUI
    takes over stdin/stdout/stderr until the operator detaches. No
    timeout is applied because attach is an interactive verb whose
    duration is operator-driven, not bounded by fno.

    Raises:
        FileNotFoundError: claude not on PATH (caller maps to exit 14).
    """
    result = _subprocess_run(["claude", "attach", short_id])
    return result.returncode


def claude_logs_reachable(short_id: str, *, timeout: float = 10.0) -> bool:
    """Cheap supervisor-reachability probe used by ``reconcile``.

    Invokes ``claude logs <short_id> --tail 1`` with output suppressed
    and decides three states:

    - Exit 0 → return ``True`` (supervisor reachable; flip to live).
    - Exit non-zero → return ``False`` (supervisor lost the session;
      flip to orphaned).
    - Timeout / OSError / FileNotFoundError → raise
      :class:`ReachabilityProbeError` (probe failed, preserve status;
      reconcile routes to errors with a reason discriminator).

    Locked Decision 9 caps the probe at 10s — longer than the
    supervisor's typical startup, short enough that a degenerate
    registry of 100+ entries does not freeze the operator's shell.

    Args:
        short_id: 8-hex claude short-id.
        timeout: wall-clock seconds (default 10).

    Raises:
        ReachabilityProbeError: probe could not produce a definitive
            answer; caller should preserve status. ``provider`` is
            ``"claude"`` by construction so reconcile routes the error
            with a per-provider reason discriminator.
    """
    try:
        result = _subprocess_run(
            ["claude", "logs", short_id, "--tail", "1"],
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired as exc:
        raise ReachabilityProbeError(
            provider="claude", reason=f"timeout after {timeout}s"
        ) from exc
    except FileNotFoundError as exc:
        # The caller's is_provider_available check should make this
        # unreachable, but defensive-catch keeps the contract explicit.
        raise ReachabilityProbeError(
            provider="claude", reason="claude vanished mid-probe"
        ) from exc
    except OSError as exc:
        raise ReachabilityProbeError(provider="claude", reason=str(exc)) from exc
    return result.returncode == 0


# =====================================================================
# Task 1.2 — single-writer session claim guard (stream-json host lane)
# =====================================================================
#
# Before the daemon respawns an idle claude session into the stream-json
# host lane (`claude -p --resume <uuid> ...`), fno must enforce
# single-writer: `claude --resume` does NOT itself guard against a live
# duplicate (only `--continue` skips live sessions), so two writers on one
# transcript would corrupt it. Two guards, in order:
#   1. Liveness: refuse if the bg session's supervisor is reachable (a human
#      interactive TUI / another writer is using it right now).
#   2. Atomic claim: acquire `fno claim session:<uuid>` (O_CREAT|O_EXCL) so two
#      concurrent adopts cannot both respawn the same transcript.
# Reuses the existing `fno claim` primitive (no new substrate). Session claims
# are host-global (a session is host-wide, not project-scoped), so they live
# under `global_claims_root()` (~/.fno/claims).


class SessionWriterClaimError(RuntimeError):
    """The single-writer claim for a session UUID could not be acquired.

    Either the bg session is currently held LIVE by another process (a human
    interactive TUI / another daemon thread), or another adopt already holds
    the atomic ``session:<uuid>`` claim. Adopt must refuse rather than respawn
    a live transcript.
    """

    def __init__(self, session_uuid: str, reason: str) -> None:
        super().__init__(f"cannot adopt session {session_uuid}: {reason}")
        self.session_uuid = session_uuid
        self.reason = reason


def session_is_live(claude_short_id: str) -> bool:
    """True iff the bg session's supervisor is reachable (a writer is live).

    Reuses :func:`locate_session` (the session-registry walk == "registry")
    plus :func:`liveness_probe` (the 250 ms socket connect == our
    ``isProcessRunning`` analog). A live session must NOT be adopted:
    respawning a live transcript is a double-writer.
    """
    locator = locate_session(claude_short_id)
    if locator is None:
        return False
    return liveness_probe(locator.messaging_socket_path)


def acquire_session_writer_claim(
    *,
    session_uuid: str,
    holder: str,
    claude_short_id: Optional[str] = None,
    pid: Optional[int] = None,
    root: Optional[Path] = None,
):
    """Acquire the atomic single-writer claim before respawning a session.

    ``claude_short_id``, when given, gates the liveness check (skip it for a
    session that was never live). ``pid`` lets the caller pin the claim's
    PID-liveness to a long-lived owner (the daemon) rather than the transient
    acquiring process. Returns the held :class:`~fno.claims.types.Claim`;
    the caller releases it via :func:`release_session_writer_claim` when the
    child orphans/exits.

    Raises :class:`SessionWriterClaimError` when the session is held live by
    another process (guard 1) or the claim is already held (guard 2).
    """
    if claude_short_id is not None and session_is_live(claude_short_id):
        raise SessionWriterClaimError(
            session_uuid,
            f"session is held live by another process (short-id "
            f"{claude_short_id}); refusing to respawn a live transcript",
        )
    claims_root = root if root is not None else global_claims_root()
    try:
        return acquire_claim(
            f"session:{session_uuid}",
            holder,
            reason="stream-json host lane single-writer",
            pid=pid,
            root=claims_root,
        )
    except ClaimHeldByOther as exc:
        raise SessionWriterClaimError(
            session_uuid,
            f"single-writer claim already held by {exc.holder} "
            f"(pid={exc.pid}, host={exc.host})",
        ) from exc


def release_session_writer_claim(
    *, session_uuid: str, holder: str, root: Optional[Path] = None
) -> None:
    """Release the single-writer claim when the adopted child orphans/exits.

    Idempotent (silent no-op if not held), so a worker that died before the
    claim was recorded does not error on cleanup.
    """
    claims_root = root if root is not None else global_claims_root()
    release_claim(f"session:{session_uuid}", holder, root=claims_root)


# =====================================================================
# Phase 5 (US6) — MCP channel backend for ask_followup
# =====================================================================
#
# The MCP backend is a SECOND send path alongside US2's
# messagingSocketPath socket. Both backends remain supported
# indefinitely (Locked Decision 6). The dispatcher (agents/dispatch.py)
# picks the MCP backend when the AgentEntry has ``mcp_channel_id`` set
# AND ``mcp_channel_reachable`` returns True; otherwise it falls back
# to the socket path. The user-visible behavior is identical regardless
# of backend — only the wire transport differs.
#
# Design note: Locked Decision 9 specifies ``mcp_channel_id`` as a
# server-generated UUIDv4. In this PR the value is populated from
# ``claude_short_id`` (1:1) so the sidecar can route by its native
# session id without an additional id-translation layer. The field
# type permits a UUIDv4 swap without a schema bump; we flag this in
# the PR body as a follow-up.


class MCPChannelSendError(RuntimeError):
    """Raised when the MCP send path fails after a successful reachability probe.

    Mirror of :class:`ProviderSocketError` for the MCP transport. The
    dispatcher catches this and falls back to ``ask_followup`` (US2
    socket path), emitting a ``mcp_channel_demoted_to_socket`` event
    with ``reason="send_failed_post_probe"`` (per spec AC1-ERR).

    Carries the underlying ``reason`` discriminator (from the sidecar's
    response payload) so the demotion event's reason field is
    machine-stable.
    """

    def __init__(self, reason: str) -> None:
        super().__init__(f"MCP channel send error: {reason}")
        self.reason = reason


def ask_followup_via_mcp(
    claude_short_id: str,
    message: str,
    cwd: Path,
    from_name: str,
    timeout: float,
    poll_interval: float = 0.5,
    jobs_dir: Optional[Path] = None,
    mcp_channel_id: Optional[str] = None,
) -> str:
    """MCP-backed follow-up send + state.json reply poll.

    Mirrors :func:`ask_followup` but routes the send through the
    fno MCP sidecar instead of the messagingSocketPath socket.
    The reply-collection half is identical (same ``wait_for_reply``
    over the session's state.json + timeline.jsonl) because the MCP
    transport only changes how the message arrives — it does NOT
    change how the reply is observed.

    Raises:
        ProviderOrphanError: session is no longer in
            ``~/.claude/sessions/`` (the jobs dir for reply polling
            could not be located).
        MCPChannelSendError: sidecar refused the send (channel not
            registered, channel write failed, or sidecar unreachable
            mid-call). The dispatcher should fall back to socket.
        ProviderTimeoutError: poll timeout exceeded.

    ``cwd`` is unused (parity with :func:`ask_followup`).
    ``mcp_channel_id``, when provided, is the AgentEntry's stored id;
    it currently equals ``claude_short_id`` (see module-level design
    note above). The parameter is accepted explicitly so callers
    surface the agent's persistent id at the call site even though
    the sidecar routes by session id today.
    """
    del cwd  # parity placeholder
    routing_key = mcp_channel_id or claude_short_id

    # Decouple from the messagingSocketPath socket (Task 1.1): the MCP transport
    # routes the send through the sidecar (by routing_key) and observes the reply
    # by polling the session's jobs-dir, neither of which needs a live socket.
    # locate_session() SKIPS socket-null/idle sessions and would falsely orphan
    # an idle-but-resumable session, so derive the jobs-dir directly from the
    # short-id instead. An absent jobs-dir means the session never ran (typo /
    # never-launched) -> genuine orphan, preserving fail-fast on a bad id.
    if jobs_dir is not None:
        target_jobs_dir = jobs_dir
    else:
        target_jobs_dir = _jobs_dir_for(claude_short_id)
        if not target_jobs_dir.exists():
            reason = _classify_orphan_reason(claude_short_id)
            raise ProviderOrphanError(reason=reason, short_id=claude_short_id)
    baseline_updated_at: Optional[str] = None
    timeline_offset = 0
    try:
        snap = read_state_json(target_jobs_dir)
        baseline_updated_at = snap.updated_at
    except (FileNotFoundError, OSError, json.JSONDecodeError):
        baseline_updated_at = None
    timeline_path = target_jobs_dir / "timeline.jsonl"
    if timeline_path.exists():
        try:
            timeline_offset = timeline_path.stat().st_size
        except OSError:
            timeline_offset = 0

    # Imported lazily to keep MCP optional at provider-module import
    # time — the MCP package is shipped alongside, but providers/claude.py
    # is also imported by code paths that never touch the channel surface.
    from fno.mcp import build_channel_notification
    from fno.mcp import client as _mcp_client

    envelope = build_channel_notification(
        content=message,
        meta={
            "source": "fno",
            "from_name": from_name,
            "session_id": routing_key,
        },
    )
    try:
        _mcp_client.send_to_channel(routing_key, envelope)
    except _mcp_client.MCPSidecarError as exc:
        raise MCPChannelSendError(exc.reason) from exc
    except _mcp_client.MCPSidecarUnreachable as exc:
        raise MCPChannelSendError(f"sidecar_unreachable:{exc.reason}") from exc

    return wait_for_reply(
        jobs_dir=target_jobs_dir,
        baseline_updated_at=baseline_updated_at,
        timeline_offset=timeline_offset,
        timeout=timeout,
        poll_interval=poll_interval,
    )


def mcp_channel_reachable(
    mcp_channel_id: str,
    *,
    timeout: float = 0.25,
) -> bool:
    """Tri-state MCP-channel reachability probe.

    - Returns ``True`` when the sidecar reports the channel as registered
      (reconcile flips to ``"live"``).
    - Returns ``False`` when the sidecar is alive AND reports no such
      channel id — the session is definitively orphaned from the MCP
      side (reconcile flips to ``"orphaned"``).
    - Raises :class:`ReachabilityProbeError` with ``provider="claude"``
      and ``reason="mcp_channel_disconnected"`` when the sidecar itself
      is unreachable (transient I/O, server not listening, parse
      failure). Reconcile MUST preserve status in this case.

    Default ``timeout`` is 250ms — fast enough to keep follow-up latency
    bounded; the sidecar is local, so any healthy response returns
    sub-50ms in practice.

    Spec: LD10 — ``ReachabilityProbeError`` is the lifted base class
    from US4-gemini Wave 1.1; ``reason="mcp_channel_disconnected"`` is
    the discriminator for this probe.
    """
    from fno.mcp import client as _mcp_client

    try:
        resp = _mcp_client.status(timeout=timeout)
    except _mcp_client.MCPSidecarUnreachable as exc:
        raise ReachabilityProbeError(
            provider="claude",
            reason="mcp_channel_disconnected",
        ) from exc

    channels = resp.get("channels") or []
    if not isinstance(channels, list):
        # Garbled sidecar response — treat as inconclusive, not orphaned.
        raise ReachabilityProbeError(
            provider="claude",
            reason="mcp_channel_disconnected",
        )

    for entry in channels:
        if not isinstance(entry, dict):
            continue
        if entry.get("session_id") == mcp_channel_id:
            return True
    return False

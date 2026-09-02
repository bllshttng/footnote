"""Cursor Agent pane adapter.

Cursor Agent has no local bidirectional transport. ``--print`` with
``stream-json`` is output-only, so fno drives the interactive process through
the mux pane. ``create-chat`` mints the chat UUID, prints it, and then stays
alive; this module reads one line, terminates that helper, and launches the
pane on the explicit id.

The chat store is remote. A second process recalling a nonce from the first
process is the identity proof; this module never looks for a local transcript
file and has no pi-style create claim because create and resume are separate
Cursor commands.
"""
from __future__ import annotations

import os
import re
import select
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterable, Sequence

from fno.agents.dispatch import DispatchAskError

CURSOR_AGENT_BINARY = "cursor-agent"
CREATE_CHAT_TIMEOUT_S = 15.0
CREATE_CHAT_REAP_TIMEOUT_S = 2.0
_UUID4_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-4[0-9a-fA-F]{3}-"
    r"[89aAbB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$"
)
_subprocess_popen = subprocess.Popen


@dataclass(frozen=True)
class CursorWorkerServerHandle:
    pid: int
    start_time: int


class CursorAgentSessionError(DispatchAskError):
    """Cursor returned or received no usable full chat UUID."""

    def __init__(self, message: str) -> None:
        super().__init__(message, exit_code=2)


def _chat_id_error(chat_id: Any) -> str | None:
    value = chat_id if isinstance(chat_id, str) else ""
    if value == "":
        return (
            "cursor-agent chat id is empty; pass the full UUID that "
            "`cursor-agent create-chat` returned."
        )
    if len(value) == 8 and all(char in "0123456789abcdefABCDEF" for char in value):
        return (
            f"cursor-agent chat id {value!r} is 8 hex characters, which is an "
            "fno session handle, not a chat id.\n"
            "Pass the full UUID that `cursor-agent create-chat` returned."
        )
    if _UUID4_RE.fullmatch(value) is None:
        return (
            f"cursor-agent chat id {value!r} is not a full UUIDv4 returned by "
            "`cursor-agent create-chat`."
        )
    return None


def _require_chat_id(chat_id: str) -> str:
    error = _chat_id_error(chat_id)
    if error is not None:
        raise CursorAgentSessionError(error)
    return chat_id


def create_chat(cwd: Path | str) -> str:
    """Read Cursor's callee-minted chat UUID and terminate ``create-chat``.

    Cursor prints the UUID before entering a long-lived state and does not exit
    on its own. A bounded fd read keeps a failed login or broken binary from
    wedging the spawn lane; the killed child is reaped only to avoid a zombie,
    never to inspect or trust its exit status.
    """
    try:
        process = _subprocess_popen(
            [CURSOR_AGENT_BINARY, "create-chat"],
            cwd=str(cwd),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as exc:
        raise CursorAgentSessionError(
            f"cursor-agent create-chat could not start: {exc}"
        ) from exc

    stdout = process.stdout
    stderr = getattr(process, "stderr", None)
    output = bytearray()
    stderr_output = bytearray()
    try:
        if stdout is None:
            raise CursorAgentSessionError(
                "cursor-agent create-chat returned no stdout; no chat id was captured"
            )
        deadline = time.monotonic() + CREATE_CHAT_TIMEOUT_S
        streams = [stream for stream in (stdout, stderr) if stream is not None]
        while time.monotonic() < deadline and b"\n" not in output:
            remaining = max(0.0, deadline - time.monotonic())
            ready, _, _ = select.select(streams, [], [], min(0.2, remaining))
            # EOF: select reports a closed pipe ready immediately, so once the
            # helper has exited (the fast-fail auth shape) a plain continue
            # would spin hot until the deadline. Drain this round, then stop.
            drained_and_exited = False
            for stream in ready:
                chunk = os.read(stream.fileno(), 4096)
                if not chunk:
                    if process.poll() is not None:
                        drained_and_exited = True
                    continue
                if stream is stdout:
                    output.extend(chunk)
                else:
                    stderr_output.extend(chunk)
            if drained_and_exited:
                break
    except (OSError, ValueError) as exc:
        raise CursorAgentSessionError(
            f"cursor-agent create-chat output could not be read: {exc}"
        ) from exc
    finally:
        try:
            process.kill()
        except (OSError, ProcessLookupError):
            pass
        try:
            process.wait(timeout=CREATE_CHAT_REAP_TIMEOUT_S)
        except subprocess.TimeoutExpired:
            try:
                process.kill()
            except (OSError, ProcessLookupError):
                pass
            try:
                process.wait(timeout=CREATE_CHAT_REAP_TIMEOUT_S)
            except (OSError, subprocess.TimeoutExpired):
                pass
        except (OSError, ProcessLookupError):
            pass
        if stdout is not None:
            try:
                stdout.close()
            except OSError:
                pass
        if stderr is not None:
            try:
                stderr.close()
            except OSError:
                pass

    first_line = bytes(output).decode("utf-8", "replace").splitlines()
    chat_id = first_line[0].strip() if first_line else ""
    error = _chat_id_error(chat_id)
    if error is not None:
        if not chat_id:
            error = (
                "cursor-agent create-chat returned no chat id within "
                f"{CREATE_CHAT_TIMEOUT_S:g}s; the helper was terminated."
            )
        else:
            error = f"cursor-agent create-chat returned invalid chat id {chat_id!r}: {error}"
        raise CursorAgentSessionError(error)
    return chat_id


def resume_argv(chat_id: str, *, model: str | None = None) -> list[str]:
    """Build the pane resume argv with an explicit chat id and trust flag."""
    _require_chat_id(chat_id)
    argv = [CURSOR_AGENT_BINARY, "--resume", chat_id, "--trust"]
    if model:
        argv.extend(["--model", model])
    return argv


def attach_argv(chat_id: str, *, model: str | None = None) -> list[str]:
    """Attach to the same remote chat; never open Cursor's id picker."""
    return resume_argv(chat_id, model=model)


def is_cursor_worker_server_command(command: str) -> bool:
    """Recognize Cursor's detached worker-server process, not generic Node."""
    lowered = command.lower()
    return "worker-server" in lowered and (
        "cursor-agent" in lowered or "/.cursor/" in lowered
    )


def select_owned_worker_server_pids(
    process_rows: Iterable[tuple[int, int, str]], owner_pid: int
) -> list[int]:
    """Select matching servers in the target pane's process tree only."""
    by_parent: dict[int, list[int]] = {}
    commands: dict[int, str] = {}
    for pid, parent_pid, command in process_rows:
        by_parent.setdefault(parent_pid, []).append(pid)
        commands[pid] = command

    descendants: set[int] = set()
    pending = [owner_pid]
    while pending:
        parent_pid = pending.pop()
        for child_pid in by_parent.get(parent_pid, []):
            if child_pid in descendants:
                continue
            descendants.add(child_pid)
            pending.append(child_pid)
    return sorted(
        pid
        for pid in descendants
        if is_cursor_worker_server_command(commands.get(pid, ""))
    )


def _process_start_token(pid: int, psutil_mod: Any) -> int | None:
    from fno.agents.spawn_gate import _process_start_time

    return _process_start_time(pid, psutil_mod)


def capture_detached_worker_servers(
    owner_pid: int | None, owner_pid_start_time: int | None
) -> tuple[CursorWorkerServerHandle, ...]:
    """Capture server children while the pane still proves their ownership."""
    try:
        import psutil
    except ImportError as exc:  # pragma: no cover - psutil is a CLI dependency
        raise RuntimeError(f"cannot inspect cursor-agent worker-server processes: {exc}") from exc

    if owner_pid is None or owner_pid_start_time is None:
        raise RuntimeError(
            "cursor-agent worker-server ownership requires the pane pid and start time"
        )
    if _process_start_token(owner_pid, psutil) != owner_pid_start_time:
        raise RuntimeError(
            f"cursor-agent pane pid {owner_pid} ownership could not be confirmed"
        )

    rows: list[tuple[int, int, str]] = []
    for process in psutil.process_iter(["pid", "ppid", "cmdline"]):
        try:
            info = process.info
            pid = int(info.get("pid") or process.pid)
            parent_pid = int(info.get("ppid") or 0)
            command = " ".join(info.get("cmdline") or [])
        except (psutil.AccessDenied, psutil.NoSuchProcess, psutil.ZombieProcess):
            continue
        rows.append((pid, parent_pid, command))

    handles: list[CursorWorkerServerHandle] = []
    for pid in select_owned_worker_server_pids(rows, owner_pid):
        start_time = _process_start_token(pid, psutil)
        if start_time is None:
            raise RuntimeError(
                f"cursor-agent worker-server pid {pid} identity could not be confirmed"
            )
        handles.append(CursorWorkerServerHandle(pid=pid, start_time=start_time))
    return tuple(handles)


def reap_detached_worker_servers(
    handles: Sequence[CursorWorkerServerHandle],
) -> int:
    """Terminate only previously captured, start-token-pinned servers."""
    try:
        import psutil
    except ImportError as exc:  # pragma: no cover - psutil is a CLI dependency
        raise RuntimeError(f"cannot inspect cursor-agent worker-server processes: {exc}") from exc

    candidates = []
    for handle in handles:
        try:
            process = psutil.Process(handle.pid)
            current_start = _process_start_token(handle.pid, psutil)
            if current_start is None:
                raise RuntimeError(
                    f"cursor-agent worker-server pid {handle.pid} identity could not be confirmed"
                )
            if current_start != handle.start_time:
                raise RuntimeError(
                    f"cursor-agent worker-server pid {handle.pid} identity changed"
                )
            if not is_cursor_worker_server_command(" ".join(process.cmdline())):
                raise RuntimeError(
                    f"cursor-agent worker-server pid {handle.pid} command changed"
                )
            process.terminate()
        except (psutil.AccessDenied, psutil.NoSuchProcess, psutil.ZombieProcess) as exc:
            if isinstance(exc, psutil.NoSuchProcess):
                continue
            raise RuntimeError(
                f"cursor-agent worker-server pid {handle.pid} could not be terminated: {exc}"
            ) from exc
        candidates.append(process)
    for process in candidates:
        try:
            process.wait(timeout=2)
        except psutil.TimeoutExpired:
            try:
                process.kill()
                process.wait(timeout=2)
            except (psutil.AccessDenied, psutil.NoSuchProcess, psutil.ZombieProcess, psutil.TimeoutExpired) as exc:
                raise RuntimeError(
                    f"cursor-agent worker-server pid {process.pid} survived teardown: {exc}"
                ) from exc
    return len(candidates)


def cursor_agent_provider() -> str:
    """Conceptual provider axis; Cursor's CLI folds it into its model service."""
    return os.environ.get("FNO_CURSOR_AGENT_PROVIDER") or "cursor"


def cursor_agent_model() -> str:
    """Configured Cursor model, or its native automatic selection."""
    return os.environ.get("FNO_CURSOR_AGENT_MODEL") or "auto"

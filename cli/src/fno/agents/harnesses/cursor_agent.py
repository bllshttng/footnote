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
from pathlib import Path
from typing import Any

from fno.agents.dispatch import DispatchAskError

CURSOR_AGENT_BINARY = "cursor-agent"
CREATE_CHAT_TIMEOUT_S = 15.0
CREATE_CHAT_REAP_TIMEOUT_S = 2.0
_UUID4_RE = re.compile(
    r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-4[0-9a-fA-F]{3}-"
    r"[89aAbB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$"
)
_subprocess_popen = subprocess.Popen


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
            stderr=subprocess.STDOUT,
        )
    except OSError as exc:
        raise CursorAgentSessionError(
            f"cursor-agent create-chat could not start: {exc}"
        ) from exc

    stdout = process.stdout
    output = bytearray()
    try:
        if stdout is None:
            raise CursorAgentSessionError(
                "cursor-agent create-chat returned no stdout; no chat id was captured"
            )
        deadline = time.monotonic() + CREATE_CHAT_TIMEOUT_S
        while time.monotonic() < deadline and b"\n" not in output:
            remaining = max(0.0, deadline - time.monotonic())
            ready, _, _ = select.select([stdout], [], [], min(0.2, remaining))
            if stdout in ready:
                chunk = os.read(stdout.fileno(), 4096)
                if not chunk:
                    break
                output.extend(chunk)
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


def reap_detached_worker_servers() -> int:
    """Terminate detached Cursor worker servers left after pane teardown."""
    try:
        import psutil
    except ImportError as exc:  # pragma: no cover - psutil is a CLI dependency
        raise RuntimeError(f"cannot inspect cursor-agent worker-server processes: {exc}") from exc

    candidates = []
    for process in psutil.process_iter(["cmdline"]):
        try:
            command = " ".join(process.info.get("cmdline") or [])
        except (psutil.AccessDenied, psutil.NoSuchProcess, psutil.ZombieProcess):
            continue
        if is_cursor_worker_server_command(command):
            candidates.append(process)
    for process in candidates:
        try:
            process.terminate()
        except (psutil.AccessDenied, psutil.NoSuchProcess, psutil.ZombieProcess) as exc:
            raise RuntimeError(
                f"cursor-agent worker-server pid {process.pid} could not be terminated: {exc}"
            ) from exc
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

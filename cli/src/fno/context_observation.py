#!/usr/bin/env python3
"""Capture exact hook-delivered directives and emit one canonical snapshot."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# This file is executed BY PATH from context-observe-hook.sh
# (`python3 <plugin>/cli/src/fno/context_observation.py`), which puts its own
# directory on sys.path but not the package root, so `import fno.*` fails under
# any interpreter that does not already have the package installed or on
# PYTHONPATH. The hook suppresses this helper's output and ends every call with
# `|| true`, so that failure is SILENT: the record step no-ops, the collect step
# then finds no directory to lock, and no snapshot is ever emitted. Add our own
# package root so the helper works under a bare `python3` the same way it does
# under `uv run --project cli`.
if __package__ in (None, ""):  # executed as a script, not imported
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))


ENTRY_STATES = {"startup", "resume", "clear", "post_compact"}


def _run_bounded(argv: list[str]) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--timeout", type=float, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if args.timeout <= 0 or not command:
        return 2
    process = subprocess.Popen(command, start_new_session=True)
    try:
        return process.wait(timeout=args.timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=0.2)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        return 124


def _identity(hook_input: bytes, explicit_entry: str | None) -> tuple[str, str, str, str]:
    try:
        payload = json.loads(hook_input.decode("utf-8")) if hook_input else {}
    except (UnicodeError, json.JSONDecodeError):
        payload = {}
    payload = payload if isinstance(payload, dict) else {}
    session_id = str(payload.get("session_id") or "").strip()
    harness = (os.environ.get("FNO_PLATFORM") or "").strip().lower()
    from fno.harness_identity import HARNESS_SESSION_MARKERS

    for marker, marker_harness in HARNESS_SESSION_MARKERS:
        value = (os.environ.get(marker) or "").strip()
        if not session_id and value:
            session_id = value
        if not harness and value:
            harness = marker_harness
    if not harness:
        if os.environ.get("CLAUDE_PLUGIN_ROOT"):
            harness = "claude"
        elif os.environ.get("GEMINI_PROJECT_DIR"):
            harness = "gemini"
        elif os.environ.get("CODEX_PLUGIN_ROOT"):
            harness = "codex"

    raw_entry = explicit_entry or payload.get("source") or payload.get("entry_state")
    entry_state = str(raw_entry or "startup").strip().lower().replace("-", "_")
    if entry_state in {"compact", "postcompact"}:
        entry_state = "post_compact"
    invocation_material = bytearray(hook_input or session_id.encode())
    transcript_path = payload.get("transcript_path")
    if isinstance(transcript_path, str) and transcript_path:
        try:
            stat = Path(transcript_path).stat()
        except OSError:
            pass
        else:
            invocation_material.extend(
                f"\0{stat.st_size}\0{stat.st_mtime_ns}".encode()
            )
    invocation_id = hashlib.sha256(invocation_material).hexdigest()
    return session_id, harness, entry_state, invocation_id


def _scratch_root() -> Path:
    override = os.environ.get("FNO_CONTEXT_OBSERVATION_DIR")
    if override:
        return Path(override)
    return Path(tempfile.gettempdir()) / "fno-context-observations"


def _group_dir(session_id: str, harness: str, entry_state: str, invocation_id: str) -> Path:
    identity = f"{session_id}\0{harness}\0{entry_state}\0{invocation_id}".encode()
    return _scratch_root() / hashlib.sha256(identity).hexdigest()


def _safe_source_id(source_id: str) -> str:
    return hashlib.sha256(source_id.encode()).hexdigest()


def _directive_bytes(
    output: bytes, *, output_is_directive: bool = False
) -> tuple[bytes, str | None]:
    if not output:
        return b"", None
    if output_is_directive:
        return output, None
    try:
        text = output.decode("utf-8")
    except UnicodeError as exc:
        return b"", f"{type(exc).__name__}: {exc}"
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return output, None
    if not isinstance(payload, dict):
        return output, None
    nested = payload.get("hookSpecificOutput")
    if isinstance(nested, dict) and isinstance(nested.get("additionalContext"), str):
        return nested["additionalContext"].encode(), None
    for key in ("additionalContext", "additional_context", "systemMessage"):
        if isinstance(payload.get(key), str):
            return payload[key].encode(), None
    return b"", None


def _write_record(
    *,
    source_id: str,
    expected: list[str],
    carrier: str,
    hook_input: bytes,
    output: bytes,
    hook_rc: int,
    explicit_entry: str | None,
    output_is_directive: bool = False,
) -> tuple[Path | None, dict | None]:
    session_id, harness, entry_state, invocation_id = _identity(
        hook_input, explicit_entry
    )
    if not session_id or harness not in {"claude", "codex", "gemini"}:
        return None, None
    if entry_state not in ENTRY_STATES:
        entry_state = "startup"
    directive, decode_error = _directive_bytes(
        output, output_is_directive=output_is_directive
    )
    error = decode_error
    if hook_rc != 0:
        error = f"hook exited {hook_rc}" + (f"; {error}" if error else "")
    status = "observed" if error is None else "unreadable"
    content_hash = hashlib.sha256(directive).hexdigest() if error is None else None
    record = {
        "source_id": source_id,
        "carrier": carrier,
        "status": status,
        "error": error,
        "bytes": len(directive),
        "estimated_tokens": (len(directive) + 3) // 4,
        "content_hash": content_hash,
        "expected_source_ids": expected,
        "session_id": session_id,
        "harness": harness,
        "entry_state": entry_state,
        "invocation_id": invocation_id,
        "observed_epoch": time.time(),
    }
    group = _group_dir(session_id, harness, entry_state, invocation_id)
    group.mkdir(parents=True, exist_ok=True)
    target = group / f"{_safe_source_id(source_id)}.json"
    fd, temporary = tempfile.mkstemp(prefix=".context-", suffix=".tmp", dir=group)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(record, handle, separators=(",", ":"))
        os.replace(temporary, target)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
    return group, record


def _collect(
    *,
    expected: list[str],
    hook_input: bytes,
    explicit_entry: str | None,
) -> bool:
    session_id, harness, entry_state, invocation_id = _identity(
        hook_input, explicit_entry
    )
    if (
        not session_id
        or harness not in {"claude", "codex", "gemini"}
        or entry_state not in ENTRY_STATES
    ):
        return False
    group = _group_dir(session_id, harness, entry_state, invocation_id)
    deadline = time.monotonic() + 2.0
    expected_paths = [
        group / f"{_safe_source_id(source_id)}.json" for source_id in expected
    ]
    while time.monotonic() < deadline and not all(path.exists() for path in expected_paths):
        time.sleep(0.02)
    lock_path = group / ".collect.lock"
    lock_path.touch(exist_ok=True)
    with lock_path.open("r+") as lock:
        fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        complete = all(path.exists() for path in expected_paths)
        marker = group / (".complete-emitted" if complete else ".incomplete-emitted")
        if marker.exists():
            return False

        manifest: list[dict] = []
        errors: list[str] = []
        from fno import paths
        from fno.context_audit import runtime_native_context_manifest

        native = runtime_native_context_manifest(
            paths.resolve_repo_root(),
            harness=harness,
            entry_state=entry_state,
        )
        manifest.extend(native)
        errors.extend(
            f"{record['source_id']}: {record.get('error') or 'unobserved'}"
            for record in native
            if record.get("status") != "observed"
        )
        for source_id in expected:
            path = group / f"{_safe_source_id(source_id)}.json"
            try:
                record = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, UnicodeError, json.JSONDecodeError) as exc:
                record = {
                    "source_id": source_id,
                    "carrier": None,
                    "status": "unreadable",
                    "error": f"{type(exc).__name__}: {exc}",
                    "bytes": 0,
                    "estimated_tokens": 0,
                    "content_hash": None,
                }
            if (
                record.get("session_id") != session_id
                or record.get("harness") != harness
                or record.get("entry_state") != entry_state
                or record.get("invocation_id") != invocation_id
            ):
                record["status"] = "unreadable"
                record["error"] = "observation identity mismatch"
            public = {
                key: record.get(key)
                for key in (
                    "source_id",
                    "carrier",
                    "status",
                    "error",
                    "bytes",
                    "estimated_tokens",
                    "content_hash",
                )
            }
            manifest.append(public)
            if public["status"] != "observed":
                errors.append(f"{source_id}: {public['error'] or 'unobserved'}")

        source_hashes = [
            str(record["content_hash"])
            for record in manifest
            if record.get("status") == "observed" and record.get("content_hash")
        ]
        context_hash = (
            hashlib.sha256("\n".join(source_hashes).encode()).hexdigest()
            if source_hashes
            else None
        )
        context_bytes = sum(
            int(record.get("bytes") or 0)
            for record in manifest
            if record.get("status") == "observed"
        )

        from fno import paths
        from fno.events import append_event, context_snapshot

        event = context_snapshot(
            session_id=session_id,
            harness=harness,
            entry_state=entry_state,
            context_bytes=context_bytes,
            estimated_tokens=(context_bytes + 3) // 4,
            context_hash=context_hash,
            source_hashes=source_hashes,
            source_manifest=manifest,
            measurement_complete=complete and not errors,
            measurement_errors=errors,
        )
        append_event(
            event,
            events_path=paths.project_log("events.jsonl"),
            lock_timeout_seconds=0.5,
        )
        marker.write_text(event["ts"], encoding="utf-8")
        return True


def main() -> int:
    if len(sys.argv) > 1 and sys.argv[1] == "run-bounded":
        return _run_bounded(sys.argv[2:])
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("record", "collect", "direct"))
    parser.add_argument("--source-id")
    parser.add_argument("--expected", default="")
    parser.add_argument("--carrier", default="")
    parser.add_argument("--input-file", required=True)
    parser.add_argument("--output-file")
    parser.add_argument("--hook-rc", type=int, default=0)
    parser.add_argument("--entry")
    parser.add_argument("--output-is-directive", action="store_true")
    args = parser.parse_args()

    expected = [item for item in args.expected.split(",") if item]
    hook_input = Path(args.input_file).read_bytes()
    if args.mode in {"record", "direct"}:
        if not args.source_id or not args.output_file:
            parser.error("record/direct require --source-id and --output-file")
        _write_record(
            source_id=args.source_id,
            expected=expected,
            carrier=args.carrier,
            hook_input=hook_input,
            output=Path(args.output_file).read_bytes(),
            hook_rc=args.hook_rc,
            explicit_entry=args.entry,
            output_is_directive=args.output_is_directive,
        )
    if args.mode in {"collect", "direct"}:
        _collect(
            expected=expected,
            hook_input=hook_input,
            explicit_entry=args.entry,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

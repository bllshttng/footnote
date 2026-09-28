"""fno.agents.read — pure-read entry point for `fno agents logs`.

Logs never mutate the registry, never flip ``status`` based on inferred live
state, and never emit events.
"""
from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from fno.agents.registry import (
    AgentResolutionError,
    RegistryVersionError,
    load_registry,
    resolve_agent_across_sources,
)

# ---------------------------------------------------------------------------
# `fno agents logs <name>` entry point
# ---------------------------------------------------------------------------

# Exit codes are part of the CLI contract — matched in tests.
EXIT_OK = 0
EXIT_NOT_FOUND = 13  # AC2-ERR / AC2-EDGE


@dataclass
class LogsResult:
    """Outcome of a ``read_logs`` call (the non-streaming branches).

    For the Claude raw-passthrough and the codex/gemini tee-read paths
    that complete eagerly, the caller receives an exit code plus any
    warning lines. For ``follow=True`` streaming, the producer writes
    directly to stdout / stderr and returns when the stream terminates.
    """

    exit_code: int = 0
    warnings: list[str] = field(default_factory=list)


def read_logs(
    name: str,
    tail: Optional[int] = None,
    follow: bool = False,
    json_out: bool = False,
    stdout=None,
    stderr=None,
) -> LogsResult:
    """Tail or follow an agent's log output.

    Behavior per provider:

    - **Claude** — shell out to ``claude logs <short_id>`` via
      :func:`fno.agents.harnesses.claude.logs`. Raw passthrough
      to ``stdout``; exit code mirrors ``claude``'s.
    - **Codex / Gemini** — read the JSONL tee file at the entry's
      ``log_path``. If the file does not exist, emit a precise
      "provider not yet shipped (US4)" WARN to ``stderr`` and exit 13
      (AC2-EDGE).

    For unknown names, exits 13 with ``"agent not found: <name>"`` on
    ``stderr`` (AC2-ERR).

    ``stdout`` / ``stderr`` default to ``sys.stdout`` / ``sys.stderr``;
    the CLI layer passes them in so test fixtures can capture both.
    """
    import sys

    out = stdout if stdout is not None else sys.stdout
    err = stderr if stderr is not None else sys.stderr

    try:
        entries = load_registry()
    except RegistryVersionError as exc:
        return LogsResult(exit_code=1, warnings=[str(exc)])

    try:
        entry = resolve_agent_across_sources(entries, name).entry
    except AgentResolutionError as exc:
        err.write(f"{exc}\n" if exc.ambiguous else f"agent not found: {name}\n")
        return LogsResult(exit_code=EXIT_NOT_FOUND)

    if entry.harness == "claude":
        warnings: list[str] = []
        if json_out:
            # JSON for claude logs is a future concern (would require
            # parsing claude's log format). Surface the gap via the
            # result's warning list so the CLI applies its "WARN: "
            # prefix uniformly with other diagnostics.
            warnings.append(
                "JSON output for Claude logs not implemented in US3; "
                "falling back to raw passthrough"
            )
        if not entry.short_id:
            err.write(
                f"claude agent {name} (created {entry.created_at}) has no "
                "short id on file; cannot read logs. This entry may "
                "predate US1's short-id capture; try re-dispatching with "
                "`fno agents ask`.\n"
            )
            # Missing short_id is a data-integrity error, not a
            # name-resolution miss; reserve exit 13 for the latter.
            return LogsResult(exit_code=1, warnings=warnings)
        from fno.agents.harnesses import claude as claude_mod

        exit_code = claude_mod.logs(
            short_id=entry.short_id,
            tail=tail,
            follow=follow,
            stdout=out,
            stderr=err,
        )
        return LogsResult(exit_code=exit_code, warnings=warnings)

    # Codex / Gemini path — read the JSONL tee file if it exists. Retrieval is
    # implemented (see test_logs_codex_oneshot_parity); the only failure here is
    # a genuinely-absent log file, so report that honestly rather than the stale
    # "ships in Phase 3 US4" stub that made codex look unsupported.
    # Byte-parity with client_verbs.rs's matching branch. Check emptiness first:
    # Path("") is Path("."), which exists, so the old code mis-read an empty
    # log_path row as the cwd directory.
    log_path_str = entry.log_path or ""
    log_path = Path(log_path_str) if log_path_str else None
    if log_path is None or not log_path.exists():
        where = log_path_str if log_path_str else "(no log_path recorded)"
        err.write(
            f"no logs for {entry.harness} agent {name}: no log file at {where}\n"
        )
        return LogsResult(exit_code=EXIT_NOT_FOUND)

    # Codex/gemini logs are JSON-Lines; emit raw text by default.
    # `--tail N` slices the last N records, `--follow` polls.
    # The KeyboardInterrupt guard opens BEFORE the tail READ, not after it and
    # not after the write. Two earlier placements each closed only part of the
    # window: at the follow branch it missed the write, and above the write it
    # still missed the read, which is the slowest step of the three on a large
    # log and so the likeliest place for a Ctrl-C to land. The Rust twin arms
    # SIGINT before its own read, so this is also what keeps the two paths
    # describing the same protected region.
    try:
        try:
            records = _read_jsonl_tail(log_path, tail=tail)
        except OSError as exc:
            err.write(f"failed to read {log_path}: {exc}\n")
            return LogsResult(exit_code=1)

        for line in records:
            out.write(line)
            if not line.endswith("\n"):
                out.write("\n")

        if follow:
            # Best-effort 500ms polling loop for codex/gemini. Claude logs
            # delegate follow to harnesses.claude.logs which has its own
            # signal-safe implementation. OSError covers the open-time race
            # (log deleted/rotated between the tail read above and the
            # _follow_jsonl open below) — without it the operator sees a
            # traceback for what is a normal rotation event. These two arms
            # stay scoped to the open so their messages keep naming the open.
            try:
                _follow_jsonl(log_path, stdout=out, stderr=err)
            except FileNotFoundError as exc:
                # Open-time race: log file was removed between the tail read
                # above and the _follow_jsonl open. Treat as the same shape
                # as the mid-stream disappearance the inner loop detects.
                err.write(f"log file disappeared before follow could attach: {exc}\n")
                return LogsResult(exit_code=EXIT_NOT_FOUND)
            except OSError as exc:
                # Other open-time failures (e.g. PermissionError, EIO) are
                # genuine infrastructure problems — surface them with a
                # distinct message + generic exit so callers can tell them
                # apart from the "disappeared" case.
                err.write(f"failed to open log file for follow: {exc}\n")
                return LogsResult(exit_code=1)
    except KeyboardInterrupt:
        if not follow:
            # Branch on the ARGUMENT, not on whether the loop was entered.
            #
            # Keying this on loop entry was tried and reverted. It reopens the
            # race this guard exists to close: the tail write is the readiness
            # marker a follower waits on, so a SIGINT arriving between that
            # write and the loop would exit 130 again, which is the exact
            # intermittent failure the guard was written for. It also
            # contradicts AC2-FR, whose contract is that `--follow` interrupted
            # exits clean, full stop. For a stream the operator stopped on
            # purpose the tail is a preamble, so a short preamble is not a
            # truncated deliverable.
            #
            # A one-shot dump is the opposite case and keeps the strict rule: it
            # has no clean-exit contract, and swallowing there made a truncated
            # tail exit 0 and read as a complete one.
            #
            # SystemExit(130) rather than a bare re-raise: `cmd_logs` traps no
            # KeyboardInterrupt, so re-raising printed a Python traceback for an
            # ordinary Ctrl-C. 130 is the same distinguishable code without it.
            raise SystemExit(130)
        # AC2-FR clean exit — no traceback on stderr.
        return LogsResult(exit_code=EXIT_OK)

    return LogsResult(exit_code=EXIT_OK)


def _read_jsonl_tail(path: Path, tail: Optional[int]) -> list[str]:
    """Read the last ``tail`` lines from a JSON-Lines file.

    Returns ``[]`` when ``tail`` is zero or negative (CLI contract:
    ``--tail 0`` emits no output). Returns all lines when ``tail`` is
    ``None``. For positive ``tail``, uses a bounded ``collections.deque``
    over the file iterator so memory stays O(tail) rather than O(file).
    """
    if tail is not None and tail <= 0:
        return []
    import collections

    with path.open("r", encoding="utf-8") as fh:
        if tail is None:
            return fh.readlines()
        return list(collections.deque(fh, maxlen=tail))


def _follow_jsonl(path: Path, stdout, stderr, poll_interval: float = 0.5) -> None:
    """Tail-follow a JSON-Lines file. Locked Decision 3: 500ms polling.

    Trapped by ``read_logs`` for ``KeyboardInterrupt``. SIGTERM is
    handled by Python's default signal disposition (exits cleanly).

    Rotation / truncation detection runs once per poll cycle:

    - inode change → atomic-rename rotation (logrotate-style).
    - ``st_size < fh.tell()`` → truncate-in-place rotation where the
      writer has not yet refilled past our read offset.
    - ``st_size < last_size`` → truncate-in-place rotation that the
      writer refilled past our offset before our next poll. Without
      this check, ``fh.tell() <= st_size`` even though the underlying
      content was replaced, and the next ``readline`` would emit
      mid-record garbage to the operator.

    Either condition terminates the loop with a structured stderr
    note so the operator gets a real diagnostic rather than a silent
    hang or torn-record output.
    """
    import time

    try:
        initial_stat = path.stat()
        initial_ino = initial_stat.st_ino
        last_size = initial_stat.st_size
    except OSError:
        initial_ino = None
        last_size = 0

    with path.open("r", encoding="utf-8") as fh:
        fh.seek(0, os.SEEK_END)
        while True:
            line = fh.readline()
            if line:
                stdout.write(line)
                if hasattr(stdout, "flush"):
                    stdout.flush()
                continue

            try:
                st = path.stat()
            except OSError:
                stderr.write(f"log file disappeared: {path}\n")
                return

            if initial_ino is not None and st.st_ino != initial_ino:
                stderr.write(
                    f"log file rotated (inode changed): {path}\n"
                )
                return

            if st.st_size < fh.tell():
                stderr.write(f"log file truncated: {path}\n")
                return

            # Window-spanning truncate-then-refill: between the previous
            # iteration and this one the file shrank then regrew. The
            # offset comparison above does not catch this if the regrew
            # size exceeds our offset; the cross-iteration size delta
            # does.
            if st.st_size < last_size:
                stderr.write(
                    f"log file truncated (size shrank across poll): {path}\n"
                )
                return
            last_size = st.st_size

            time.sleep(poll_interval)

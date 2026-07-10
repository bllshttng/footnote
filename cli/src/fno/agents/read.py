"""fno.agents.read — pure-read entry points for `fno agents list / logs`.

Locked Decision 5 — list and logs never mutate the registry, never flip
``status`` based on inferred live state, never emit events. Status
mutations belong to dedicated write verbs (stop, rm, future reconcile).

Locked Decision 6 — registry ``status`` (fno's view, ``live | orphaned``)
and ``live_status`` (claude's supervisor view, ``Working | Needs input |
Idle | null``) are separate axes. Both appear in the JSON shape.
"""
from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from fno.agents import format as fmt
from fno.agents import truth_status
from fno.agents.registry import (
    AgentEntry,
    RegistryVersionError,
    load_registry,
)


@dataclass
class ListResult:
    """Outcome of a ``list_agents`` call.

    ``output`` is the rendered string destined for stdout (empty on
    failure). ``warnings`` is the list of WARN messages destined for
    stderr (one per line). ``exit_code`` is the CLI exit code.
    """

    output: str = ""
    warnings: list[str] = field(default_factory=list)
    exit_code: int = 0


def list_agents(
    cwd: Optional[str] = None,
    provider: Optional[str] = None,
    status: Optional[str] = None,
    json_out: bool = False,
    tty: bool = True,
    discover: bool = True,
) -> ListResult:
    """List registered agents, optionally filtered, rendered for stdout.

    Format selection (Locked Decision 4):

    - ``json_out=True`` → JSON regardless of TTY.
    - ``json_out=False`` and ``tty=False`` → JSON (orchestrator default).
    - ``json_out=False`` and ``tty=True`` → human table.

    On registry load failure (corrupt JSON, schema mismatch), returns a
    :class:`ListResult` with ``exit_code=1`` and the parser error in
    ``warnings`` (AC1-ERR). The Claude-shellout fallback (AC1-FR) does
    NOT change the exit code — it only annotates ``live_status: None``
    for affected entries and pushes WARN lines into ``warnings``.
    """
    # AC3-EDGE: resolve cwd filter to absolute BEFORE comparing.
    resolved_cwd: Optional[str] = None
    if cwd is not None:
        resolved_cwd = str(Path(cwd).resolve())

    filters_applied = {
        "cwd": resolved_cwd,
        "provider": provider,
        "status": status,
    }

    try:
        entries = load_registry()
    except RegistryVersionError as exc:
        # AC1-ERR — corrupt or schema-mismatched registry. Exit 1, stdout
        # empty, stderr carries the parser context (the exception message
        # already names the file path).
        return ListResult(output="", warnings=[str(exc)], exit_code=1)

    # Apply filters in order: cwd → provider → status. Empty registry
    # short-circuits — no shellout needed when there's nothing to augment.
    filtered: list[AgentEntry] = []
    for entry in entries:
        if resolved_cwd is not None:
            try:
                entry_cwd = str(Path(entry.cwd).resolve())
            except OSError:
                entry_cwd = entry.cwd
            if entry_cwd != resolved_cwd:
                continue
        if provider is not None and entry.provider != provider:
            continue
        if status is not None and entry.status != status:
            continue
        filtered.append(entry)

    warnings: list[str] = []

    # Best-effort augmentation: shell out once per call to ``claude
    # agents --json`` (Locked Decision 1) only if at least one Claude
    # entry survives filtering. Failures (timeout / non-zero / parse)
    # are caught inside ``claude_agents_json`` and surface as warnings;
    # this layer trusts the contract and does not add a broad catch
    # that would also swallow programmer errors (AttributeError /
    # TypeError / ImportError).
    live_map: dict[str, dict] = {}
    if any(e.provider == "claude" for e in filtered):
        from fno.agents.providers import claude as claude_mod

        live_map, augment_warnings = claude_mod.claude_agents_json()
        warnings.extend(augment_warnings)

    # fno-truth status (x-4a48): a bg /target worker between turns reads Idle
    # even while CI/preflight run externally. Fill only the ambiguous Idle /
    # missing gap from the node claim + loop_check recency; never override a
    # harness Working / Needs input (Locked Decision 1). One tail read of the
    # events log, shared across rows (not O(rows)).
    loop_check_ages = truth_status.build_loop_check_index()

    rows: list[dict] = []
    for entry in filtered:
        live_status: Optional[str] = None
        if entry.provider == "claude" and entry.claude_short_id:
            live_status = (live_map.get(entry.claude_short_id) or {}).get(
                "live_status"
            )
        if live_status in (None, "Idle"):
            node_id = truth_status.parse_node_id(entry.name)
            if node_id is not None:
                truth = truth_status.resolve_truth_status(
                    node_id,
                    manifest_cwd=entry.cwd,
                    loop_check_ages=loop_check_ages,
                )
                rendered = truth_status.render_truth_status(truth)
                if rendered is not None:
                    live_status = rendered
        rows.append(fmt.serialize_entry(entry, live_status=live_status))

    # P1 (ab-098967b4): the discovered-live-sessions lane. Best-effort
    # augmentation over Claude Code's on-disk session registry; it must never
    # crash `agents list` (US5/AC5-FR), so a broad catch here is intentional
    # (unlike the registered-agents path) — but the error surfaces as a WARN,
    # never silently. Excludes sessions already in the fno registry so the
    # lane means "live but un-adopted" (no double-listing).
    discovered_rows: list[dict] = []
    if discover and provider in (None, "claude"):
        try:
            from fno.agents import discover as discover_mod

            registered_short_ids = {
                e.claude_short_id for e in entries if e.claude_short_id
            }
            # Projects-store rows key on full session_id (their short_id is the
            # uuid prefix, not the registry's hex handle), so exclude adopted
            # sessions by cc_session_id too (x-a1d5: no double-listing).
            registered_session_ids = {
                e.cc_session_id for e in entries if e.cc_session_id
            }
            sessions = discover_mod.discover_live_sessions(
                exclude_short_ids=registered_short_ids,
                exclude_session_ids=registered_session_ids,
            )
            for sess in sessions:
                if resolved_cwd is not None:
                    # An empty cwd must NOT resolve to the process cwd and then
                    # spuriously match a --cwd filter (gemini review); drop it.
                    if not sess.cwd:
                        continue
                    try:
                        sess_cwd = str(Path(sess.cwd).resolve())
                    except OSError:
                        sess_cwd = sess.cwd
                    if sess_cwd != resolved_cwd:
                        continue
                discovered_rows.append(sess.to_row())
        except Exception as exc:  # noqa: BLE001 — robustness over precision here
            warnings.append(f"live-session discovery skipped: {exc}")

    if json_out or not tty:
        output = fmt.render_json(
            rows, filters_applied=filters_applied, discovered=discovered_rows
        )
    else:
        output = fmt.render_table(rows, discovered=discovered_rows)

    return ListResult(output=output, warnings=warnings, exit_code=0)


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
      :func:`fno.agents.providers.claude.logs`. Raw passthrough
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

    entry: Optional[AgentEntry] = None
    for e in entries:
        if e.name == name:
            entry = e
            break

    if entry is None:
        err.write(f"agent not found: {name}\n")
        return LogsResult(exit_code=EXIT_NOT_FOUND)

    if entry.provider == "claude":
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
        if not entry.claude_short_id:
            err.write(
                f"claude agent {name} (created {entry.created_at}) has no "
                "claude_short_id on file; cannot read logs. This entry may "
                "predate US1's short-id capture; try re-dispatching with "
                "`fno agents ask`.\n"
            )
            # Missing short_id is a data-integrity error, not a
            # name-resolution miss; reserve exit 13 for the latter.
            return LogsResult(exit_code=1, warnings=warnings)
        from fno.agents.providers import claude as claude_mod

        exit_code = claude_mod.logs(
            short_id=entry.claude_short_id,
            tail=tail,
            follow=follow,
            stdout=out,
            stderr=err,
        )
        return LogsResult(exit_code=exit_code, warnings=warnings)

    # Codex / Gemini path — read the JSONL tee file if it exists. Retrieval is
    # implemented (see test_logs_codex_oneshot_parity); the only failure here is
    # a genuinely-absent log file, so report that honestly rather than the stale
    # "ships in Phase 3 US4" stub that made codex look unsupported (ab-65c3e60d).
    # Byte-parity with client_verbs.rs's matching branch. Check emptiness first:
    # Path("") is Path("."), which exists, so the old code mis-read an empty
    # log_path row as the cwd directory.
    log_path_str = entry.log_path or ""
    log_path = Path(log_path_str) if log_path_str else None
    if log_path is None or not log_path.exists():
        where = log_path_str if log_path_str else "(no log_path recorded)"
        err.write(
            f"no logs for {entry.provider} agent {name}: no log file at {where}\n"
        )
        return LogsResult(exit_code=EXIT_NOT_FOUND)

    # Codex/gemini logs are JSON-Lines; emit raw text by default.
    # `--tail N` slices the last N records, `--follow` polls.
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
        # delegate follow to providers.claude.logs which has its own
        # signal-safe implementation. OSError covers the open-time race
        # (log deleted/rotated between the tail read above and the
        # _follow_jsonl open below) — without it the operator sees a
        # traceback for what is a normal rotation event.
        try:
            _follow_jsonl(log_path, stdout=out, stderr=err)
        except KeyboardInterrupt:
            # AC2-FR clean exit — no traceback on stderr.
            return LogsResult(exit_code=EXIT_OK)
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

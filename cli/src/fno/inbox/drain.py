"""Headless inbox drain: thread-per-file dispatcher.

Iterates ``read_unread_threads(recipient)`` and dispatches each thread to
one of three handlers based on ``kind``:

  heads-up -> ``_handle_heads_up``  - LLM triage; create_node + mark_read
                                       on success; mark_read on ignore;
                                       leave unread on request_clarification.
  question -> ``_handle_question``  - drop wake-signal; intentionally leave
                                       the thread unread so a human still sees it.
  fyi      -> ``_handle_fyi``        - if frontmatter ``persist_to_memory: true``,
                                       write a memory file for the recipient;
                                       otherwise dismiss (mark read, no persist).
                                       Mark read in either branch.

Drain runs ONCE per call and processes up to ``max_threads`` unread threads
(default 10, matches the megawalk Step 0 cap).
"""
from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from fno import _subprocess_util
from fno.graph._constants import extract_node_ids
from fno.paths import resolve_canonical_worktree
from fno.inbox.store import (
    Kind,
    ThreadHandle,
    mark_thread_read,
    read_unread_threads,
)
from fno.inbox.triage import (
    TriageFailedError,
    read_triage_settings,
    triage_thread,
)
from fno.wake.signal import WakeSignal, drop_signal


@dataclass
class DrainResult:
    thread_id: str
    kind: str
    action: str
    thread_path: Optional[str] = None
    node_id: Optional[str] = None
    memory_path: Optional[str] = None
    error: Optional[str] = None


def drain_inbox(
    repo_root: Path,
    project: str,
    max_threads: int = 10,
) -> list[DrainResult]:
    """Process up to ``max_threads`` unread threads. Returns per-thread results.

    A failure in one handler must not abort the batch: each ``drain_thread``
    call is wrapped so a bug or transient I/O error in one handler is
    surfaced as a ``handler_failed`` DrainResult and the rest of the batch
    keeps moving.
    """
    threads = read_unread_threads(project)[:max_threads]
    out: list[DrainResult] = []
    for h in threads:
        try:
            out.append(drain_thread(repo_root, project, h))
        except Exception as exc:  # noqa: BLE001 - intentional defensive guard
            out.append(
                DrainResult(
                    thread_id=h.thread_id,
                    kind=h.kind,
                    action="handler_failed",
                    thread_path=str(h.path),
                    error=f"{type(exc).__name__}: {exc}",
                )
            )
    return out


def drain_thread(repo_root: Path, project: str, h: ThreadHandle) -> DrainResult:
    """Dispatch one thread to its handler, keyed by ``h.kind``."""
    if h.kind == Kind.HEADS_UP.value:
        return _handle_heads_up(repo_root, project, h)
    if h.kind == Kind.QUESTION.value:
        return _handle_question(repo_root, h)
    if h.kind == Kind.FYI.value:
        return _handle_fyi(repo_root, h)
    if h.kind == Kind.SEND.value:
        # Agent-to-agent envelope from `fno mail send` (bus Group 2):
        # fyi semantics - surface the body and mark read (consumed). The
        # Group 3 bus log replaces this path with cursor-based drains.
        return _handle_fyi(repo_root, h)

    # Unknown kind: log and leave unread so it surfaces in lint.
    return DrainResult(
        thread_id=h.thread_id,
        kind=h.kind,
        action="unknown_kind",
        thread_path=str(h.path),
        error=f"unknown kind: {h.kind!r}",
    )


# ---------------------------------------------------------------------------
# Handlers
# ---------------------------------------------------------------------------

def _handle_question(repo_root: Path, h: ThreadHandle) -> DrainResult:
    """Drop a wake-signal; do NOT mark read (human escape hatch)."""
    summary = ""
    if h.messages:
        summary = h.messages[-1].body.split("\n", 1)[0][:160]

    sig = WakeSignal(
        source="inbox-drain",
        kind="question",
        msg_id=h.thread_id,
        from_project=h.from_project,
        summary=summary,
        ts=datetime.now(tz=timezone.utc),
    )
    drop_signal(repo_root, sig)
    return DrainResult(
        thread_id=h.thread_id,
        kind="question",
        action="wake_signal_dropped",
        thread_path=str(h.path),
    )


def _handle_heads_up(repo_root: Path, project: str, h: ThreadHandle) -> DrainResult:
    """LLM triage; create graph node on success."""
    settings = read_triage_settings()
    try:
        plan = triage_thread(h, settings=settings, project_override=project)
    except TriageFailedError as exc:
        return DrainResult(
            thread_id=h.thread_id,
            kind="heads-up",
            action="triage_failed",
            thread_path=str(h.path),
            error=str(exc),
        )

    if plan.action == "ignore":
        mark_thread_read(h.path)
        return DrainResult(
            thread_id=h.thread_id,
            kind="heads-up",
            action="ignored",
            thread_path=str(h.path),
        )
    if plan.action == "request_clarification":
        # Leave unread; human will respond via reply.
        return DrainResult(
            thread_id=h.thread_id,
            kind="heads-up",
            action="clarification_pending",
            thread_path=str(h.path),
        )

    node_id = _create_graph_node_from_plan(repo_root, h, plan)
    mark_thread_read(h.path)
    return DrainResult(
        thread_id=h.thread_id,
        kind="heads-up",
        action="created_node",
        thread_path=str(h.path),
        node_id=node_id,
    )


def _handle_fyi(repo_root: Path, h: ThreadHandle) -> DrainResult:
    """Persist to memory (when persist_to_memory) or dismiss."""
    if h.persist_to_memory:
        return _fyi_persist_memory(repo_root, h)
    return _fyi_dismiss(repo_root, h)


def _fyi_persist_memory(repo_root: Path, h: ThreadHandle) -> DrainResult:
    memory_dir = _resolve_memory_dir(repo_root)
    memory_file = memory_dir / f"auto_inbox_lesson_{h.thread_id}.md"

    body_parts: list[str] = []
    for m in h.messages:
        body_parts.append(f"## {m.msg_id} from {m.from_project} at {m.timestamp.isoformat()}")
        body_parts.append("")
        body_parts.append(m.body)
        body_parts.append("")

    text = (
        f"---\n"
        f"name: inbox lesson from {h.from_project}\n"
        f"description: lesson received via inbox; auto-written by drain\n"
        f"type: feedback\n"
        f"auto_generated: true\n"
        f"source_inbox_thread: {h.path}\n"
        f"source_project: {h.from_project}\n"
        f"---\n\n"
        + "\n".join(body_parts).rstrip("\n")
        + "\n"
    )
    try:
        memory_dir.mkdir(parents=True, exist_ok=True)
        memory_file.write_text(text, encoding="utf-8")
    except OSError as exc:
        return DrainResult(
            thread_id=h.thread_id,
            kind="fyi",
            action="memory_write_failed",
            thread_path=str(h.path),
            error=f"{type(exc).__name__}: {exc}",
        )
    try:
        mark_thread_read(h.path)
    except (OSError, ValueError, TimeoutError) as exc:
        return DrainResult(
            thread_id=h.thread_id,
            kind="fyi",
            action="mark_read_failed",
            thread_path=str(h.path),
            memory_path=str(memory_file),
            error=f"{type(exc).__name__}: {exc}",
        )
    return DrainResult(
        thread_id=h.thread_id,
        kind="fyi",
        action="memory_written",
        thread_path=str(h.path),
        memory_path=str(memory_file),
    )


def _fyi_dismiss(repo_root: Path, h: ThreadHandle) -> DrainResult:
    """Mark a non-persisted fyi thread read without writing it anywhere.

    The convo-signals capture this used to feed had zero readers; resurrect
    only with a concrete reader in hand.
    """
    try:
        mark_thread_read(h.path)
    except (OSError, ValueError, TimeoutError) as exc:
        return DrainResult(
            thread_id=h.thread_id,
            kind="fyi",
            action="mark_read_failed",
            thread_path=str(h.path),
            error=f"{type(exc).__name__}: {exc}",
        )
    return DrainResult(
        thread_id=h.thread_id,
        kind="fyi",
        action="dismissed",
        thread_path=str(h.path),
    )


# ---------------------------------------------------------------------------
# Graph-node creation
# ---------------------------------------------------------------------------


def _create_graph_node_from_plan(repo_root: Path, h: ThreadHandle, plan) -> str:
    """Run ``fno new`` with thread-aware provenance flags. Returns the new node id.

    We pass both ``--source-inbox-msg`` (root msg-id) and ``--source-inbox-thread``
    (thread file path) when supported, so legacy graph queries that look up by
    msg-id keep working while new queries can resolve back to the full thread.
    """
    args = [
        *_subprocess_util.fno_py_cmd(), "new",
        plan.title,
        "--priority", plan.priority,
        "--source-kind", "from_inbox",
        "--source-project", h.from_project,
        "--source-inbox-msg", h.root_msg_id,
        "--force-domain",
    ]
    # Best-effort: append thread path when the graph CLI supports the flag.
    # `fno new --help` is cheap; failing the probe only loses provenance breadth.
    try:
        help_out = subprocess.run(
            [*_subprocess_util.fno_py_cmd(), "new", "--help"],
            capture_output=True, text=True, check=False, cwd=repo_root,
            timeout=5,
        )
        if "--source-inbox-thread" in (help_out.stdout or ""):
            args.extend(["--source-inbox-thread", str(h.path)])
    except (subprocess.TimeoutExpired, FileNotFoundError, OSError):
        pass

    result = subprocess.run(args, capture_output=True, text=True, check=True, cwd=repo_root)
    # Liberal node-id extraction (legacy ab- or any configured prefix/width)
    # over the trusted `fno new` stdout, which prints the new id last.
    candidates = extract_node_ids(result.stdout)
    if not candidates:
        raise RuntimeError(
            f"fno new did not return a node-id token: stdout={result.stdout!r}"
        )
    return candidates[-1]


def _resolve_memory_dir(repo_root: Path) -> Path:
    """Resolve the auto-memory directory for inbox-lesson writes.

    FNO_AUTO_MEMORY_DIR wins when set (tests, ephemeral setups). Otherwise
    resolve repo_root to the canonical (main) WORKING TREE via the shared
    `paths.resolve_canonical_worktree`, which skips bare repos and
    separate-git-dir gitdir mis-reports (a `git clone --bare` source is listed
    FIRST, so taking record [0] keyed the memory dir to the bare path and
    linked-worktree sessions never saw those entries). Falls back to repo_root
    when the helper finds no usable working tree (non-git / bare-only).
    """
    override = os.environ.get("FNO_AUTO_MEMORY_DIR")
    if override:
        return Path(override)

    canonical = repo_root.resolve()
    worktree = resolve_canonical_worktree(canonical, timeout=2)
    if worktree is not None:
        canonical = worktree.resolve()

    # Match Claude Code's project-dir encoding exactly: leading dash from the
    # POSIX root slash is preserved (~/.claude/projects/-Users-foo-... format).
    # Earlier .lstrip("-") stripped the leading dash, so production writes
    # landed in a sibling dir that Claude Code never reads from.
    encoded = canonical.as_posix().replace(":", "").replace("/", "-")
    return Path.home() / ".claude" / "projects" / encoded / "memory"

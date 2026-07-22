"""Transcript resolver for backlog node provenance (Task 2.3, x-30f6).

Core primitive: resolve_transcript(harness, session_id, cwd) -> ResolvedTranscript

Claude layout:
    ~/.claude/projects/<slug(cwd)>/<session_id>.jsonl
where slug(cwd) replaces BOTH '/' and '.' with '-'.
Example: /Users/bb16/code/me/fno -> -Users-bb16-code-me-fno

Codex resolves to the rollout jsonl embedding the session id (kind="jsonl");
opencode resolves to the SQLite store, with the session id as the lookup key
(kind="opencode-db", the "path" names the store, not a per-session file). Both
reuse fno.agents.discover's shipped, read-only store readers.

For all remaining harnesses (gemini, antigravity, ...) the function returns
resolved=False with reason="harness-not-supported" and NEVER raises.

All degenerate inputs (None/empty session_id, None cwd) also return
resolved=False without raising.

The projects_root parameter is injectable (default: Path.home()/".claude"/"projects")
so tests never touch the real ~/.claude tree.
"""
from __future__ import annotations

import dataclasses
import glob as _glob
import json
from pathlib import Path
from typing import Optional

# Default used in production; injected in tests via monkeypatch.
_DEFAULT_PROJECTS_ROOT: Path = Path.home() / ".claude" / "projects"


@dataclasses.dataclass
class ResolvedTranscript:
    """Result of a transcript resolution attempt.

    All fields are JSON-serializable (str / bool / None) so callers can
    pass dataclasses.asdict(result) directly to json.dumps().
    """

    harness: Optional[str]
    session_id: Optional[str]
    cwd: Optional[str]
    resolved: bool
    transcript_path: Optional[str] = None
    reason: Optional[str] = None
    ambiguous: bool = False
    # Store shape of transcript_path. "jsonl" (claude/codex rollout, a per-session
    # file) or "opencode-db" (transcript_path names the SQLite store; session_id
    # is the lookup key). Default keeps every existing caller/test unchanged.
    kind: str = "jsonl"


def _slug(cwd: str) -> str:
    """Convert an absolute cwd path to a Claude projects-directory slug.

    Both '/' and '.' are replaced with '-'.  A leading '/' becomes a
    leading '-', so '/Users/bb16/code/me/fno' maps to
    '-Users-bb16-code-me-fno' (confirmed from real ~/.claude layout).
    """
    return cwd.replace("/", "-").replace(".", "-")


def _claude_has_conversation(path: Path) -> bool:
    """True iff the transcript holds at least one user/assistant turn.

    Distinguishes a real transcript from a metadata-only stub (the header
    records -- ``last-prompt``, ``custom-title``, ``agent-name`` -- carry no
    conversation). Short-circuits on the first conversational record, so a real
    multi-MB transcript costs a few KB and a stub costs its whole (tiny) size.
    Any read error -> False (a file we cannot read is not proof of conversation).
    """
    try:
        with path.open("r", encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except (ValueError, UnicodeDecodeError):
                    continue
                if isinstance(rec, dict) and rec.get("type") in ("user", "assistant"):
                    return True
    except OSError:
        return False
    return False


def _newest_mtime(paths: list[Path]) -> Optional[Path]:
    """The newest-mtime path, statting defensively so a file vanishing mid-scan
    (a duplicate being migrated) never sinks the whole resolution -- another
    valid transcript still wins. None only if every path vanished."""
    best: Optional[Path] = None
    best_mt = float("-inf")
    for p in paths:
        try:
            mt = p.stat().st_mtime
        except OSError:
            continue
        if mt > best_mt:
            best_mt, best = mt, p
    return best


def resolve_transcript(
    harness: Optional[str],
    session_id: Optional[str],
    cwd: Optional[str],
    *,
    projects_root: Optional[Path] = None,
    codex_sessions_dir: Optional[Path] = None,
    opencode_db_path: Optional[Path] = None,
) -> ResolvedTranscript:
    """Resolve a provenance pointer to its on-disk transcript path.

    Parameters
    ----------
    harness:
        Harness identifier, e.g. "claude", "codex", "gemini".  Only "claude"
        is actively resolved; everything else returns resolved=False.
    session_id:
        Full UUID-style session id OR an 8-hex prefix for a glob match.
        None/empty -> resolved=False immediately.
    cwd:
        Working directory of the session that produced the node.
        None -> resolved=False immediately.
    projects_root:
        Override the default ~/.claude/projects root.  Required in tests.
    codex_sessions_dir / opencode_db_path:
        Override the codex rollout store / opencode SQLite store. Required in
        tests so no read ever touches the developer's real stores.

    Returns
    -------
    ResolvedTranscript
        Never raises.  resolved=True only when an actual store entry was found.
    """
    root = projects_root if projects_root is not None else _DEFAULT_PROJECTS_ROOT

    if not session_id:
        return ResolvedTranscript(
            harness=harness,
            session_id=session_id,
            cwd=cwd,
            resolved=False,
            reason="missing-input",
        )

    # codex keys on the session id in the rollout; opencode keys on the session
    # id in the store. Neither needs cwd (only claude does, for its slug).
    if harness == "codex":
        return _resolve_codex(harness, session_id, cwd, codex_sessions_dir)
    if harness == "opencode":
        return _resolve_opencode(harness, session_id, cwd, opencode_db_path)

    # Guard: unsupported harnesses (gemini, antigravity, ...)
    if harness != "claude":
        return ResolvedTranscript(
            harness=harness,
            session_id=session_id,
            cwd=cwd,
            resolved=False,
            reason="harness-not-supported",
        )

    # claude needs cwd for the projects slug (the guard above narrows it to str).
    if not cwd:
        return ResolvedTranscript(
            harness=harness,
            session_id=session_id,
            cwd=cwd,
            resolved=False,
            reason="missing-input",
        )

    # Claude resolution. Transcript-truth (x-a472): a session's transcript can
    # exist in more than one project dir -- EnterWorktree re-keys it from the
    # canonical cwd's slug to the worktree cwd's slug, and CC leaves a stub in
    # the other dir. Trusting the passed cwd's slug goes blind exactly when a bg
    # worker enters its worktree (peek/discovery then read the stub and report
    # "no activity" on a live worker). So we search EVERY project dir for this
    # session id. cwd stays required (a claude pointer without it is
    # missing-input, guarded above) but no longer scopes the search.
    try:
        # Escape the id so a stray glob metachar ('*', '?', '[') in an 8-hex
        # prefix can't widen the match. `*/` scopes to one-level project dirs
        # (the CC layout), never the UUID tool-results subdirs. Filter sibling
        # artifacts: only `<uuid>.jsonl` is a real transcript, so a stem carrying
        # a dot (`<uuid>.orphaned-...`, `<uuid>.sync-conflict-...`) is dropped --
        # otherwise a full uuid would match its own artifacts and read as
        # ambiguous, regressing exact-match-first.
        esc = _glob.escape(session_id)
        matches = sorted(
            p for p in root.glob(f"*/{esc}*.jsonl") if "." not in p.name[: -len(".jsonl")]
        )
        if not matches:
            return ResolvedTranscript(
                harness=harness,
                session_id=session_id,
                cwd=cwd,
                resolved=False,
                reason="not-found",
            )

        stems = {m.name for m in matches}
        if len(stems) > 1:
            # A short (8-hex) prefix matched two DISTINCT session uuids across
            # the store: genuinely ambiguous. Preserve the first-sorted +
            # ambiguous contract rather than guessing across two sessions.
            chosen: Optional[Path] = matches[0]
            ambiguous = True
        elif len(matches) == 1:
            # The overwhelming common case: one transcript, no content read.
            chosen = matches[0]
            ambiguous = False
        else:
            # Same session copied across canonical + worktree dirs. mtime alone
            # is a TRAP here: CC writes a metadata-only stub in the other dir,
            # and that stub's creation can POST-DATE the real transcript's last
            # turn -- so newest-mtime picks the empty stub, recent_records()
            # returns zero, and the blind-peek failure survives. Prefer the
            # copies that actually carry conversation, and only among those take
            # the newest write. When none carry conversation (all stubs), fall
            # back to newest-of-all (best effort).
            with_convo = [m for m in matches if _claude_has_conversation(m)]
            chosen = _newest_mtime(with_convo or matches)
            ambiguous = False

        if chosen is None:  # every candidate vanished mid-scan (stat race)
            return ResolvedTranscript(
                harness=harness,
                session_id=session_id,
                cwd=cwd,
                resolved=False,
                reason="not-found",
            )
        return ResolvedTranscript(
            harness=harness,
            session_id=session_id,
            cwd=cwd,
            resolved=True,
            transcript_path=str(chosen),
            ambiguous=ambiguous,
        )

    except Exception:
        # Never raise (defensive: permissions, unexpected OS errors, etc.)
        return ResolvedTranscript(
            harness=harness,
            session_id=session_id,
            cwd=cwd,
            resolved=False,
            reason="error",
        )


def _resolve_codex(
    harness: str,
    session_id: str,
    cwd: Optional[str],
    sessions_dir: Optional[Path],
) -> ResolvedTranscript:
    """Resolve a codex session to its rollout jsonl (kind stays "jsonl")."""
    try:
        from fno.agents import discover

        path = discover.codex_rollout_for_session(
            session_id, sessions_dir=sessions_dir
        )
    except Exception:
        path = None
    if path is None:
        return ResolvedTranscript(
            harness=harness, session_id=session_id, cwd=cwd,
            resolved=False, reason="not-found",
        )
    return ResolvedTranscript(
        harness=harness, session_id=session_id, cwd=cwd,
        resolved=True, transcript_path=str(path),
    )


def _resolve_opencode(
    harness: str,
    session_id: str,
    cwd: Optional[str],
    db_path: Optional[Path],
) -> ResolvedTranscript:
    """Resolve an opencode session in the SQLite store (kind="opencode-db").

    The store is a single database; transcript_path names it and session_id is
    the lookup key. A read-only existence probe confirms the session exists
    without mutating (WAL + ON DELETE CASCADE make a stray write destructive).
    ``ses_...`` ids are never case-folded.
    """
    try:
        from fno.agents import discover

        store = db_path if db_path is not None else discover.default_opencode_db_path()
        if not store.exists():
            return ResolvedTranscript(
                harness=harness, session_id=session_id, cwd=cwd,
                resolved=False, reason="not-found",
            )
        rows = discover.opencode_query(
            store, "SELECT 1 FROM session WHERE id = ? LIMIT 1", (session_id,)
        )
    except Exception:
        return ResolvedTranscript(
            harness=harness, session_id=session_id, cwd=cwd,
            resolved=False, reason="error",
        )
    if not rows:
        return ResolvedTranscript(
            harness=harness, session_id=session_id, cwd=cwd,
            resolved=False, reason="not-found",
        )
    return ResolvedTranscript(
        harness=harness, session_id=session_id, cwd=cwd,
        resolved=True, transcript_path=str(store), kind="opencode-db",
    )

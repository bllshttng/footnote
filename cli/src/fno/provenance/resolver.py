"""Transcript resolver for backlog node provenance (Task 2.3, x-30f6).

Core primitive: resolve_transcript(harness, session_id, cwd) -> ResolvedTranscript.
Layouts, degenerate-input rules, and the never-raises contract live on the
function itself; store roots are injectable so tests never touch the real
stores. transcript_listing() shares one store-wide listing across a batch.
"""
from __future__ import annotations

import dataclasses
import glob as _glob
import json
from contextlib import contextmanager
from contextvars import ContextVar
from pathlib import Path
from typing import Optional

# Default used in production; injected in tests via monkeypatch.
_DEFAULT_PROJECTS_ROOT: Path = Path.home() / ".claude" / "projects"

_ACTIVE_LISTING: ContextVar = ContextVar("claude_transcript_listing", default=None)


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


@contextmanager
def transcript_listing(projects_root: Optional[Path] = None):
    """Share one listing of the claude transcript store across the scope.

    Same stem filter as the per-session glob (a dotted stem is a sibling
    artifact). A miss re-globs, so a transcript written after entry is still
    found, and a different root ignores the scope. Scoped, never process-cached:
    a worktree entry can copy a transcript into a second project dir (x-a472),
    and a long-lived cache would keep serving the stale first-dir copy.
    """
    root = projects_root or _DEFAULT_PROJECTS_ROOT
    listing = [p for p in root.glob("*/*.jsonl") if "." not in p.name[: -len(".jsonl")]]
    token = _ACTIVE_LISTING.set((root, listing))
    try:
        yield
    finally:
        _ACTIVE_LISTING.reset(token)


def _unresolved(
    harness: Optional[str], session_id: Optional[str], cwd: Optional[str], reason: str
) -> ResolvedTranscript:
    return ResolvedTranscript(
        harness=harness, session_id=session_id, cwd=cwd, resolved=False, reason=reason
    )


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

    ``harness``: "claude", "codex" and "opencode" are actively resolved; every
    other harness returns resolved=False, reason="harness-not-supported".
    ``session_id``: a full uuid or an 8-hex prefix; None/empty -> resolved=False.
    ``cwd``: the producing session's working directory; None -> resolved=False
    (claude needs it even though the search spans every project dir).
    ``projects_root`` / ``codex_sessions_dir`` / ``opencode_db_path``: store
    root overrides, required in tests so no read touches the real stores.

    Never raises.  resolved=True only when an actual store entry was found.
    """
    root = projects_root if projects_root is not None else _DEFAULT_PROJECTS_ROOT

    if not session_id:
        return _unresolved(harness, session_id, cwd, "missing-input")

    # codex keys on the session id in the rollout; opencode keys on the session
    # id in the store. Neither needs cwd (only claude does, for its slug).
    if harness == "codex":
        return _resolve_codex(harness, session_id, cwd, codex_sessions_dir)
    if harness == "opencode":
        return _resolve_opencode(harness, session_id, cwd, opencode_db_path)

    # Guard: unsupported harnesses (gemini, antigravity, ...)
    if harness != "claude":
        return _unresolved(harness, session_id, cwd, "harness-not-supported")

    # claude needs cwd for the projects slug (the guard above narrows it to str).
    if not cwd:
        return _unresolved(harness, session_id, cwd, "missing-input")

    # Claude resolution. Transcript-truth (x-a472): a session's transcript can
    # exist in more than one project dir -- EnterWorktree re-keys it from the
    # canonical cwd's slug to the worktree cwd's slug, and CC leaves a stub in
    # the other dir. Trusting the passed cwd's slug goes blind exactly when a bg
    # worker enters its worktree (peek/discovery then read the stub and report
    # "no activity" on a live worker). So we search EVERY project dir for this
    # session id. cwd stays required (a claude pointer without it is
    # missing-input, guarded above) but no longer scopes the search.
    try:
        scope = _ACTIVE_LISTING.get()
        matches: list[Path] = []
        if scope is not None and scope[0] == root:
            matches = sorted(p for p in scope[1] if p.name.startswith(session_id))
        if not matches:
            # No listing scope (or a miss inside one: the listing predates a
            # transcript written after entry). Escape a stray glob metachar
            # ('*', '?', '[') in an 8-hex prefix. A stem carrying a dot is a
            # sibling artifact (`<uuid>.orphaned-...`), never a transcript:
            # dropping it keeps a full uuid from matching its own artifacts
            # and reading as ambiguous.
            esc = _glob.escape(session_id)
            matches = sorted(
                p for p in root.glob(f"*/{esc}*.jsonl") if "." not in p.name[: -len(".jsonl")]
            )
        if not matches:
            return _unresolved(harness, session_id, cwd, "not-found")

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
            return _unresolved(harness, session_id, cwd, "not-found")
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
        return _unresolved(harness, session_id, cwd, "error")


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

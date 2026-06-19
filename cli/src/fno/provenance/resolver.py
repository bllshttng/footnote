"""Transcript resolver for backlog node provenance (Task 2.3, x-30f6).

Core primitive: resolve_transcript(harness, session_id, cwd) -> ResolvedTranscript

Claude layout (the only supported harness):
    ~/.claude/projects/<slug(cwd)>/<session_id>.jsonl
where slug(cwd) replaces BOTH '/' and '.' with '-'.
Example: /Users/bb16/code/me/abilities -> -Users-bb16-code-me-abilities

For all other harnesses (codex, gemini, antigravity, ...) the function
returns resolved=False with reason="harness-not-supported" and NEVER raises.

All degenerate inputs (None/empty session_id, None cwd) also return
resolved=False without raising.

The projects_root parameter is injectable (default: Path.home()/".claude"/"projects")
so tests never touch the real ~/.claude tree.
"""
from __future__ import annotations

import dataclasses
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


def _slug(cwd: str) -> str:
    """Convert an absolute cwd path to a Claude projects-directory slug.

    Both '/' and '.' are replaced with '-'.  A leading '/' becomes a
    leading '-', so '/Users/bb16/code/me/abilities' maps to
    '-Users-bb16-code-me-abilities' (confirmed from real ~/.claude layout).
    """
    return cwd.replace("/", "-").replace(".", "-")


def resolve_transcript(
    harness: Optional[str],
    session_id: Optional[str],
    cwd: Optional[str],
    *,
    projects_root: Optional[Path] = None,
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

    Returns
    -------
    ResolvedTranscript
        Never raises.  resolved=True only when an actual .jsonl file was found.
    """
    root = projects_root if projects_root is not None else _DEFAULT_PROJECTS_ROOT

    # Guard: missing/empty inputs
    if not session_id or not cwd:
        return ResolvedTranscript(
            harness=harness,
            session_id=session_id,
            cwd=cwd,
            resolved=False,
            reason="missing-input",
        )

    # Guard: unsupported harnesses
    if harness != "claude":
        return ResolvedTranscript(
            harness=harness,
            session_id=session_id,
            cwd=cwd,
            resolved=False,
            reason="harness-not-supported",
        )

    # Claude resolution
    try:
        proj_dir = root / _slug(cwd)

        # Try exact match first
        exact = proj_dir / f"{session_id}.jsonl"
        if exact.exists():
            return ResolvedTranscript(
                harness=harness,
                session_id=session_id,
                cwd=cwd,
                resolved=True,
                transcript_path=str(exact),
            )

        # Glob for prefix match (session_id may be an 8-hex prefix)
        matches = sorted(proj_dir.glob(f"{session_id}*.jsonl"))
        if not matches:
            return ResolvedTranscript(
                harness=harness,
                session_id=session_id,
                cwd=cwd,
                resolved=False,
                reason="not-found",
            )

        # One or more matches: return first (sorted deterministically), note ambiguity
        first = matches[0]
        return ResolvedTranscript(
            harness=harness,
            session_id=session_id,
            cwd=cwd,
            resolved=True,
            transcript_path=str(first),
            ambiguous=len(matches) > 1,
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

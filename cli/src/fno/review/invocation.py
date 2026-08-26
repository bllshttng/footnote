"""Build and join review-invocation observations.

The sender and reviewer run in different processes, so the invocation id is
kept in a best-effort per-session sidecar while the canonical event remains in
the worktree's ``.fno/events.jsonl`` journal.
"""
from __future__ import annotations

import json
import os
import secrets
import shlex
from pathlib import Path
from typing import Any


REVIEW_VERBS = frozenset({"code-review", "review", "review-changes", "sigma-review"})
REVIEW_LEVELS = frozenset({"low", "medium", "high", "xhigh", "max"})


def mint_invocation_id() -> str:
    """Return a unique, human-identifiable review invocation id."""
    return f"ri-{secrets.token_hex(16)}"


def _home(home: Path | None) -> Path:
    if home is not None:
        return Path(home)
    return Path(os.environ.get("FNO_HOME") or (Path.home() / ".fno"))


def pending_invocation_path(target_session_id: str, *, home: Path | None = None) -> Path:
    """Return the sidecar path for one target session."""
    if not target_session_id or Path(target_session_id).name != target_session_id:
        raise ValueError("target_session_id must be a non-empty path component")
    return _home(home) / "review-invocations" / f"{target_session_id}.json"


def write_pending_invocation(
    *,
    target_session_id: str,
    invocation_id: str,
    home: Path | None = None,
) -> bool:
    """Write one pending id without replacing a concurrent sender's id.

    A sidecar failure is deliberately best-effort. The sender event remains
    useful by itself, and the reviewer can mint an unjoined id if adoption is
    unavailable.
    """
    try:
        path = pending_invocation_path(target_session_id, home=home)
        path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as stream:
                json.dump(
                    {
                        "invocation_id": invocation_id,
                        "target_session_id": target_session_id,
                    },
                    stream,
                    separators=(",", ":"),
                )
        except Exception:
            try:
                path.unlink()
            except OSError:
                pass
            raise
        return True
    except (OSError, TypeError, ValueError):
        return False


def adopt_pending_invocation(
    target_session_id: str,
    *,
    home: Path | None = None,
) -> str | None:
    """Read and consume a pending id, returning ``None`` on any miss."""
    try:
        path = pending_invocation_path(target_session_id, home=home)
        raw = json.loads(path.read_text(encoding="utf-8"))
        invocation_id = raw.get("invocation_id") if isinstance(raw, dict) else None
        if not isinstance(invocation_id, str) or not invocation_id:
            return None
        path.unlink()
        return invocation_id
    except (OSError, TypeError, ValueError, json.JSONDecodeError):
        return None


def build_review_invocation_data(
    *,
    invocation_id: str,
    stage: str,
    verb: str,
    **observations: Any,
) -> dict[str, Any]:
    """Build payload data while omitting only unmeasured ``None`` values."""
    data: dict[str, Any] = {
        "invocation_id": invocation_id,
        "stage": stage,
        "verb": verb,
    }
    data.update({key: value for key, value in observations.items() if value is not None})
    return data


def build_review_invocation_event(
    *,
    source: str,
    invocation_id: str,
    stage: str,
    verb: str,
    **observations: Any,
) -> dict[str, Any]:
    """Build and schema-validate one canonical review invocation event."""
    from fno.events import _build

    return _build(
        "review_invocation",
        source,
        build_review_invocation_data(
            invocation_id=invocation_id,
            stage=stage,
            verb=verb,
            **observations,
        ),
    )


def emit_review_invocation(
    *,
    source: str,
    invocation_id: str,
    stage: str,
    verb: str,
    events_path: Path | None = None,
    **observations: Any,
) -> dict[str, Any] | None:
    """Append one event, returning it; event failures never block a review."""
    from fno.events import append_event

    try:
        event = build_review_invocation_event(
            source=source,
            invocation_id=invocation_id,
            stage=stage,
            verb=verb,
            **observations,
        )
        append_event(event, events_path=events_path)
        return event
    except Exception:
        return None


def parse_review_invocation(raw: str) -> dict[str, Any] | None:
    """Parse a review command without altering its raw argument string."""
    candidate = raw.strip()
    if not candidate:
        return None
    parts = candidate.split(None, 1)
    token = parts[0].lstrip("/")
    name = token.rsplit(":", 1)[-1]
    if name not in REVIEW_VERBS:
        return None
    args_raw = parts[1] if len(parts) == 2 else ""
    try:
        args = shlex.split(args_raw)
    except ValueError:
        args = args_raw.split()
    level = "unset"
    level_source = "fallback"
    if args and args[0] == "ultra":
        level = "ultra"
        level_source = "ultra_forced"
    elif args and args[0] in REVIEW_LEVELS:
        level = args[0]
        level_source = "explicit"
    return {
        "verb": f"/{name}",
        "args_raw": args_raw,
        "level": level,
        "level_source": level_source,
        "flags": [arg for arg in args if arg.startswith("--")],
    }


"""Inbox archive rotation for the thread-per-file inbox layout.

Read thread files (frontmatter ``read_at:`` set) older than a retention
window are moved out of ``{recipient}/inbox/`` into a monthly archive at
``{recipient}/inbox/archive/{YYYY-MM}/{filename}.md``.

Unread threads (``read_at`` absent) are NEVER archived; rotation only
moves files where the recipient is done with them.

Trigger: callers explicitly invoke ``archive_old_threads(recipient)``.
The auto-rotate hook from the old flat-file layout was a coupling between
``store.append_message`` and rotation; with one file per thread the cost
of running rotation on every send is wasted. Operators run it via the
periodic launchd task or ``fno mail archive run`` (followup).
"""
from __future__ import annotations

import os
import shutil
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

from fno.inbox.store import (
    ThreadHandle,
    inbox_dir_for,
    read_thread,
)


@dataclass
class InboxSettings:
    auto_rotate: bool = True
    keep_recent_read: int = 50
    max_size_bytes: int = 1_048_576  # retained for back-compat; not used post-2026-05


@dataclass
class ArchiveResult:
    archived_count: int
    archive_root: Path
    kept_unread: int
    kept_recent_read: int


def _archive_root_for(recipient: str) -> Path:
    """Return ``{recipient}/inbox/archive/`` under the inbox root."""
    return inbox_dir_for(recipient) / "archive"


def archive_old_threads(
    recipient: str,
    settings: Optional[InboxSettings] = None,
) -> ArchiveResult:
    """Move stale read threads under a monthly archive directory.

    Stale = thread frontmatter has ``read_at`` AND that thread is older
    than the most-recent ``settings.keep_recent_read`` read threads. Sort
    is by ``read_at`` (most recent first) so rotation never strands a
    thread the recipient just acked.

    Returns an ArchiveResult with the move counts. Idempotent: re-running
    on a freshly-archived recipient is a no-op.
    """
    if settings is None:
        settings = read_inbox_settings()

    inbox = inbox_dir_for(recipient)
    if not inbox.exists():
        return ArchiveResult(
            archived_count=0,
            archive_root=_archive_root_for(recipient),
            kept_unread=0,
            kept_recent_read=0,
        )

    archive_root = _archive_root_for(recipient)
    threads: list[ThreadHandle] = []
    for p in sorted(inbox.glob("*.md")):
        h = read_thread(p)
        if h is not None:
            threads.append(h)

    unread = [h for h in threads if h.read_at is None]
    read_threads = [h for h in threads if h.read_at is not None]

    # Most-recent read threads stay in the live folder.
    read_threads.sort(key=lambda h: h.read_at or datetime.min, reverse=True)
    keep = read_threads[: settings.keep_recent_read]
    to_move = read_threads[settings.keep_recent_read :]

    archived = 0
    for h in to_move:
        ts = h.read_at or h.created
        ym = ts.astimezone(timezone.utc).strftime("%Y-%m")
        target_dir = archive_root / ym
        target_dir.mkdir(parents=True, exist_ok=True)
        target_path = target_dir / h.path.name
        # Collision-safe: append numeric suffix if name already exists in archive
        suffix = 0
        while target_path.exists():
            suffix += 1
            target_path = target_dir / f"{h.path.stem}-{suffix}{h.path.suffix}"
        try:
            shutil.move(str(h.path), str(target_path))
            archived += 1
        except OSError as exc:
            print(
                f"warning: failed to archive {h.path}: {type(exc).__name__}: {exc}",
                file=sys.stderr,
            )

    return ArchiveResult(
        archived_count=archived,
        archive_root=archive_root,
        kept_unread=len(unread),
        kept_recent_read=len(keep),
    )


# ---------------------------------------------------------------------------
# Settings reader (preserved API)
# ---------------------------------------------------------------------------

def read_inbox_settings(cwd: Path | None = None) -> InboxSettings:
    """Read .fno/settings.yaml under config.inbox.* with defaults.

    Walks up from cwd to find .fno/settings.yaml. Returns
    InboxSettings() with defaults when no file found or config.inbox absent.

    Test override: if env var FNO_INBOX_SETTINGS_CWD is set, walk up from
    that path instead.
    """
    import yaml

    env_cwd = os.environ.get("FNO_INBOX_SETTINGS_CWD")
    if env_cwd:
        start = Path(env_cwd)
    else:
        start = cwd if cwd is not None else Path.cwd()
    current = start.resolve()

    for candidate in [current, *current.parents]:
        settings_file = candidate / ".fno" / "settings.yaml"
        if settings_file.exists():
            try:
                raw = settings_file.read_text(encoding="utf-8")
                data = yaml.safe_load(raw)
            except Exception:
                return InboxSettings()

            if not isinstance(data, dict):
                return InboxSettings()

            config = data.get("config", {})
            if not isinstance(config, dict):
                return InboxSettings()

            inbox_cfg = config.get("inbox", {})
            if not isinstance(inbox_cfg, dict):
                return InboxSettings()

            def _int(key: str, default: int) -> int:
                val = inbox_cfg.get(key, default)
                try:
                    return int(val)
                except (TypeError, ValueError):
                    return default

            def _bool(key: str, default: bool) -> bool:
                val = inbox_cfg.get(key, default)
                if isinstance(val, bool):
                    return val
                if isinstance(val, str):
                    return val.lower() in ("true", "1", "yes")
                return bool(val)

            return InboxSettings(
                auto_rotate=_bool("auto_rotate", True),
                max_size_bytes=_int("max_size_bytes", 1_048_576),
                keep_recent_read=_int("keep_recent_read", 50),
            )

    return InboxSettings()


# ---------------------------------------------------------------------------
# Back-compat shims (legacy flat-file callers should migrate to archive_old_threads)
# ---------------------------------------------------------------------------

def needs_rotation(*_args, **_kwargs) -> bool:
    """Deprecated. Returns False unconditionally on the thread-per-file layout."""
    return False


def rotate(*_args, **_kwargs):  # type: ignore[no-untyped-def]
    """Deprecated. The thread-per-file layout uses ``archive_old_threads``."""
    raise NotImplementedError(
        "rotate() was the flat-file rotation entrypoint. "
        "Use archive_old_threads(recipient, settings) on the new layout."
    )

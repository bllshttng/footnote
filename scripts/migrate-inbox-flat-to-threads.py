#!/usr/bin/env python3
"""One-shot migration: pre-2026-05 flat ``inbox.md`` → thread-per-file layout.

Walks every ``<inbox-agents-root>/*/inbox.md`` (override with
``$FNO_INBOX_ROOT``; the default is vault-derived via
``paths.inbox_agents_root``) and splits each file into one markdown file per thread under
``{recipient}/inbox/{YYYY-MM-DD}-{slug}.md``. The original ``inbox.md`` is
moved to ``inbox-pre-migration.md`` (NOT deleted) as a safety net.

Threading collapses ``reply_to:`` chains: a top-level message with no
``reply_to:`` becomes a new thread; replies are appended to their root.

Idempotent. Skips a project when ``inbox/`` already exists or
``inbox-pre-migration.md`` is present. Re-running on a partially
migrated tree never overwrites or duplicates messages.

Usage:
    python3 scripts/migrate-inbox-flat-to-threads.py --dry-run   # preview, no writes
    python3 scripts/migrate-inbox-flat-to-threads.py             # apply

Exit codes:
    0  success
    1  one or more projects errored (other projects still migrated)
"""
from __future__ import annotations

import argparse
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

# Make the fno CLI package importable when run from the repo root.
_REPO_ROOT = Path(__file__).resolve().parents[1]
_CLI_SRC = _REPO_ROOT / "cli" / "src"
if str(_CLI_SRC) not in sys.path:
    sys.path.insert(0, str(_CLI_SRC))

from fno.inbox.legacy import LegacyMessage, parse_legacy_inbox  # noqa: E402
from fno.inbox.store import (  # noqa: E402
    DEPRECATED_KINDS,
    Kind,
    ThreadHandle,
    ThreadMessage,
    VALID_KINDS,
    _format_thread,
    _slug_for,
    inbox_dir_for,
)


# Map deprecated kinds -> new kind for the migrated thread file.
# notification + lesson + answer + complete all roll up to fyi; lesson sets
# persist_to_memory: true to preserve the cross-project memory write.
_KIND_MIGRATION: dict[str, tuple[str, bool]] = {
    "heads-up": ("heads-up", False),
    "question": ("question", False),
    "fyi": ("fyi", False),
    "notification": ("fyi", False),
    "lesson": ("fyi", True),
    "answer": ("fyi", False),
    "complete": ("fyi", False),
}


def _inbox_root() -> Path:
    override = os.environ.get("FNO_INBOX_ROOT")
    if override:
        return Path(override)
    from fno.paths import inbox_agents_root
    return inbox_agents_root()


def _legacy_status_to_read_at(msg: LegacyMessage) -> Optional[datetime]:
    """Translate the legacy `status:` field to a `read_at:` datetime.

    'read' / 'answered' use the msg-block timestamp as the read_at value
    (best information we have). 'unread' returns None, leaving the new
    thread unread. Any other status defaults to None too.
    """
    if msg.status in ("read", "answered"):
        return msg.timestamp
    return None


def _build_threads_for_project(messages: list[LegacyMessage]) -> list[ThreadHandle]:
    """Group flat messages into threads by reply_to chains."""
    by_id = {m.msg_id: m for m in messages}
    children: dict[str, list[LegacyMessage]] = {}
    roots: list[LegacyMessage] = []

    for m in messages:
        if m.reply_to and m.reply_to in by_id:
            children.setdefault(m.reply_to, []).append(m)
        else:
            roots.append(m)

    roots.sort(key=lambda m: m.timestamp)

    threads: list[ThreadHandle] = []
    for root in roots:
        new_kind, persist = _KIND_MIGRATION.get(root.kind, ("fyi", False))
        if new_kind not in VALID_KINDS:
            new_kind = "fyi"

        # Walk the reply chain in timestamp order.
        chain: list[LegacyMessage] = [root]
        queue = list(children.get(root.msg_id, []))
        while queue:
            queue.sort(key=lambda m: m.timestamp)
            current = queue.pop(0)
            chain.append(current)
            queue.extend(children.get(current.msg_id, []))

        thread_messages = [
            ThreadMessage(
                msg_id=m.msg_id,
                timestamp=m.timestamp,
                from_project=m.from_project,
                body=m.body,
            )
            for m in chain
        ]

        # read_at is set on the thread iff every message in the chain was read.
        read_at: Optional[datetime] = None
        per_msg_read_at = [_legacy_status_to_read_at(m) for m in chain]
        if all(r is not None for r in per_msg_read_at):
            # Use the latest read_at as the thread's read_at.
            read_at = max(r for r in per_msg_read_at if r is not None)

        # Refs: union of all messages' refs (root takes precedence on collision).
        refs: dict[str, str] = {}
        for m in reversed(chain):
            refs.update(m.refs)

        # Date prefix uses the root timestamp.
        date_part = root.timestamp.astimezone(timezone.utc).strftime("%Y-%m-%d")
        slug = _slug_for(root.body, fallback_msg_id=root.msg_id)
        # We delay actual filename collision resolution until write-time, so
        # use a placeholder Path - the migration's own dedup re-resolves it
        # against the on-disk inbox/ directory.
        placeholder_path = Path(f"{date_part}-{slug}.md")

        threads.append(
            ThreadHandle(
                thread_id=root.msg_id,
                path=placeholder_path,
                from_project=root.from_project,
                to_project="",  # filled in at write time
                kind=new_kind,
                created=root.timestamp,
                read_at=read_at,
                replies_to=None,
                persist_to_memory=persist,
                refs=refs,
                messages=thread_messages,
            )
        )
    return threads


def _resolve_target_path(inbox_dir: Path, placeholder: Path) -> Path:
    """Pick an unused filename in inbox_dir based on the placeholder name."""
    base = placeholder.stem
    suffix_n = 0
    while True:
        candidate = inbox_dir / f"{base}.md" if suffix_n == 0 else inbox_dir / f"{base}-{suffix_n}.md"
        if not candidate.exists():
            return candidate
        suffix_n += 1


def migrate_project(
    project: str,
    legacy_path: Path,
    *,
    dry_run: bool = False,
) -> dict:
    """Migrate a single ``inbox.md`` to the thread-per-file layout."""
    result = {
        "project": project,
        "skipped": False,
        "reason": "",
        "threads_written": 0,
        "messages_migrated": 0,
        "errors": [],
    }

    inbox_dir = inbox_dir_for(project)
    pre_migration_path = legacy_path.with_name("inbox-pre-migration.md")

    # Idempotency guards
    if pre_migration_path.exists():
        result["skipped"] = True
        result["reason"] = f"already migrated (inbox-pre-migration.md present)"
        return result

    if inbox_dir.exists() and any(inbox_dir.glob("*.md")):
        result["skipped"] = True
        result["reason"] = f"inbox/ already populated"
        return result

    if not legacy_path.exists():
        result["skipped"] = True
        result["reason"] = "no inbox.md to migrate"
        return result

    messages = parse_legacy_inbox(legacy_path)
    if not messages:
        result["skipped"] = True
        result["reason"] = "inbox.md has no parseable messages"
        return result

    threads = _build_threads_for_project(messages)
    result["messages_migrated"] = sum(len(h.messages) for h in threads)

    if not dry_run:
        inbox_dir.mkdir(parents=True, exist_ok=True)

    written: list[Path] = []
    try:
        for h in threads:
            h.to_project = project
            target_path = _resolve_target_path(inbox_dir, h.path)
            h.path = target_path
            content = _format_thread(h)
            if dry_run:
                continue
            target_path.write_text(content, encoding="utf-8")
            written.append(target_path)
        result["threads_written"] = len(threads)

        if not dry_run:
            # Move the original inbox.md to inbox-pre-migration.md as safety net.
            legacy_path.rename(pre_migration_path)
    except Exception as exc:
        # On any failure, leave the half-written state visible: do NOT delete
        # already-written thread files. Operators can see the partial state.
        result["errors"].append(f"{type(exc).__name__}: {exc}")
        return result

    return result


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true", help="Preview without writes")
    parser.add_argument(
        "--project",
        action="append",
        default=None,
        help="Migrate only this project (may be passed multiple times)",
    )
    args = parser.parse_args(argv)

    root = _inbox_root()
    if not root.exists():
        print(f"no inbox root at {root}; nothing to migrate")
        return 0

    if args.project:
        projects = list(args.project)
    else:
        projects = sorted(p.parent.name for p in root.glob("*/inbox.md") if p.is_file())

    overall_rc = 0
    for project in projects:
        legacy_path = root / project / "inbox.md"
        result = migrate_project(project, legacy_path, dry_run=args.dry_run)
        if result["skipped"]:
            print(f"[skip] {project}: {result['reason']}")
            continue
        if result["errors"]:
            overall_rc = 1
            print(f"[ERROR] {project}: {'; '.join(result['errors'])}")
            continue
        verb = "would write" if args.dry_run else "wrote"
        print(
            f"[ok] {project}: {verb} {result['threads_written']} thread file(s) "
            f"covering {result['messages_migrated']} message(s)"
        )

    return overall_rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

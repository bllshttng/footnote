"""Helpers for scanning unread thread files from shell hooks.

The wake hooks and target stop hook need to answer "are there unread inbox
threads for the local project?" without parsing YAML. This module exposes
a tiny CLI:

    python -m fno.inbox.unread_scan count
    python -m fno.inbox.unread_scan list-json

Both resolve the local project from .fno/settings.yaml. ``count``
prints an integer to stdout. ``list-json`` prints a JSON array of
``{thread_id, kind, from, path, summary}`` objects.

Used by ``hooks/inbox-wake-*.sh`` and ``hooks/target-stop-hook.sh``.
"""
from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Optional

from fno.inbox.store import (
    ProjectIdentificationError,
    read_unread_threads,
    resolve_project,
)


def _scan() -> list[dict]:
    try:
        project = resolve_project()
    except ProjectIdentificationError:
        return []
    threads = read_unread_threads(project)
    out: list[dict] = []
    for h in threads:
        summary = ""
        if h.messages:
            summary = h.messages[-1].body.split("\n", 1)[0][:160]
        out.append(
            {
                "thread_id": h.thread_id,
                "kind": h.kind,
                "from": h.from_project,
                "path": str(h.path),
                "summary": summary,
            }
        )
    return out


def _block_complete_on_unread() -> bool:
    """Read config.inbox.block_complete_on_unread; default false."""
    try:
        from fno.inbox.settings import _load_inbox_config
        cfg = _load_inbox_config(None) or {}
        val = cfg.get("block_complete_on_unread")
        if isinstance(val, bool):
            return val
        if isinstance(val, str):
            return val.lower() in ("true", "1", "yes")
        if isinstance(val, int):
            return val != 0
    except Exception:
        pass
    return False


def _wake_render(repo_root_str: Optional[str], header: str) -> str:
    """Drain question wake-signals + scan unread threads + render a
    single ``<system-reminder>`` block. Returns "" when nothing to surface.

    Combines the data-gather + render path used by both wake hooks into
    one Python entrypoint so the shell script invokes one ``uv run``
    instead of six. Repo root for wake-signal drain is passed via
    ``repo_root_str`` (the hook resolves $CLAUDE_PROJECT_DIR or the git
    toplevel); it falls back to cwd when unset.
    """
    repo_root = Path(repo_root_str) if repo_root_str else Path.cwd()
    try:
        from fno.wake.signal import drain_signals
        wake = drain_signals(repo_root, kind="question")
    except Exception:
        wake = []
    threads = _scan()
    if not wake and not threads:
        return ""

    lines: list[str] = ["", "<system-reminder>"]
    if wake:
        lines.append(f"## Inbox: {len(wake)} {header}")
        lines.append("")
        for sig in wake:
            lines.append(
                f"- **From {sig.get('from_project', '?')}** "
                f"(msg {sig.get('msg_id', '?')}): {sig.get('summary', '')}"
            )
        lines.append("")
    if threads:
        lines.append(f"## Inbox: {len(threads)} unread thread(s)")
        lines.append("")
        for t in threads:
            lines.append(
                f"- **{t['kind']}** from {t['from']} "
                f"(thread {t['thread_id']}): {t['summary']}"
            )
            lines.append(f"  {t['path']}")
        lines.append("")
    lines.append(
        "Drain via `fno mail drain` or read the full body with "
        "`fno mail unread --json`."
    )
    lines.append("</system-reminder>")
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    if not argv:
        print(
            "usage: unread_scan "
            "{count|list-json|should-block|combined|wake-render}",
            file=sys.stderr,
        )
        return 2
    cmd = argv[0]
    if cmd == "should-block":
        print("true" if _block_complete_on_unread() else "false")
        return 0
    if cmd == "combined":
        # Single JSON response with everything the hook needs in one call.
        # Saves a second `uv run` startup when both pieces of state are
        # required by the same caller (eg. target-stop-hook deciding
        # whether to block COMPLETE on unread mail).
        threads = _scan()
        payload = {
            "count": len(threads),
            "threads": threads,
            "should_block": _block_complete_on_unread(),
        }
        print(json.dumps(payload))
        return 0
    if cmd == "wake-render":
        # All-in-one render path for inbox-wake-{prompt-submit,session-start}.sh.
        # Args: wake-render <repo_root> <header_phrase>
        # Header is e.g. "new question(s) since your last turn" or "question(s) waiting".
        repo_root = argv[1] if len(argv) > 1 else None
        header = argv[2] if len(argv) > 2 else "question(s) waiting"
        out = _wake_render(repo_root, header)
        if out:
            print(out)
        return 0
    threads = _scan()
    if cmd == "count":
        print(len(threads))
        return 0
    if cmd == "list-json":
        print(json.dumps(threads))
        return 0
    print(f"unknown subcommand: {cmd!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

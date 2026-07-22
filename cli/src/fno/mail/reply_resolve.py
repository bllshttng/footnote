"""Recover the sender of a live-injected ``<fno_mail id="...">`` from the invoking
session's OWN transcript, for ``fno mail reply --to <id>`` when the id has no
durable bus thread.

A live-confirmed delivery writes no durable record BY DESIGN (the recipient's
transcript IS the record). So a reply to a live-injected message cannot resolve
its sender off the bus -- the only place the ``id -> from`` binding exists is the
envelope the recipient already has in its transcript. This module reads it back.
"""
from __future__ import annotations

import re
from pathlib import Path
from typing import Optional

from fno.harness_identity import resolve_harness_identity

# Match one <fno_mail ...> open tag; attribute order is NOT assumed (id and from
# are pulled independently within the tag).
_OPEN_TAG_RE = re.compile(r"<fno_mail\b[^>]*>")
_FROM_RE = re.compile(r'from="([^"]+)"')


def sender_from_transcript_text(text: str, msg_id: str) -> Optional[str]:
    """Return the ``from`` handle of the ``<fno_mail ... id="<msg_id>" ...>`` open
    tag in ``text``, or ``None`` if no such envelope is present.

    The envelope lives inside JSONL transcript records, so its quotes arrive
    escaped (``from=\\"X\\"``); normalize ``\\"`` to ``"`` before matching so a
    raw or a JSON-escaped transcript both resolve.
    """
    normalized = text.replace('\\"', '"')
    needle = f'id="{msg_id}"'
    for tag in _OPEN_TAG_RE.finditer(normalized):
        s = tag.group(0)
        if needle not in s:
            continue
        m = _FROM_RE.search(s)
        if m:
            return m.group(1)
    return None


def resolve_live_sender(msg_id: str) -> Optional[str]:
    """Find ``msg_id``'s sender handle by scanning the invoking session's own
    transcript. ``None`` on any miss (no ambient identity, unreadable store, id
    absent) so the caller falls through to its existing not-on-bus error path."""
    ident = resolve_harness_identity()
    if not ident.session_id or not ident.harness:
        return None
    path = _transcript_path(ident.harness, ident.session_id)
    if path is None:
        return None
    try:
        # ponytail: reads the whole transcript. A received message is near the
        # tail, but it can be older; whole-file is the simple correct read. Bound
        # to a tail window only if a profiler ever says transcript size hurts.
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    return sender_from_transcript_text(text, msg_id)


def _transcript_path(harness: str, session_id: str) -> Optional[Path]:
    """Locate the invoking session's transcript, mirroring self_stamp's resolver
    (claude: ``<projects>/*/<id>.jsonl``; codex: rollout embedding the id)."""
    if harness == "claude":
        from fno.agents.discover import default_projects_dir

        return next(default_projects_dir().glob(f"*/{session_id}.jsonl"), None)
    if harness == "codex":
        from fno.agents.discover import default_codex_sessions_dir

        return next(default_codex_sessions_dir().rglob(f"*{session_id}*.jsonl"), None)
    return None

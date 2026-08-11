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
# Capture the id attribute of a <fno_mail ...> open tag (W2 dedup-at-drain).
_ID_RE = re.compile(r'<fno_mail\b[^>]*\bid="([^"]+)"')


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


def _read_own_transcript_text() -> Optional[str]:
    """The invoking session's own transcript text, or ``None`` when it cannot be
    resolved or read (no ambient identity, no transcript path, unreadable store).

    ponytail: reads the whole transcript. A received message is near the tail,
    but it can be older; whole-file is the simple correct read. Bound to a tail
    window only if a profiler ever says transcript size hurts.
    """
    ident = resolve_harness_identity()
    if not ident.session_id or not ident.harness:
        return None
    path = _transcript_path(ident.harness, ident.session_id)
    if path is None:
        return None
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None


def resolve_live_sender(msg_id: str) -> Optional[str]:
    """Find ``msg_id``'s sender handle by scanning the invoking session's own
    transcript. ``None`` on any miss (no ambient identity, unreadable store, id
    absent) so the caller falls through to its existing not-on-bus error path."""
    text = _read_own_transcript_text()
    if text is None:
        return None
    return sender_from_transcript_text(text, msg_id)


def present_mail_ids() -> Optional[set[str]]:
    """Every ``<fno_mail id="...">`` id already in the invoking session's OWN
    transcript, or ``None`` when the transcript cannot be resolved or read.

    W2 dedup-at-drain: a live-injected message lands verbatim in the recipient
    transcript, so the transcript is the ledger for "did this already arrive" --
    no new state. The set is built in ONE read per drain, not one per message.

    ``None`` (not an empty set) is the AC5-ERR signal: a read failure is not
    evidence of absence, so the caller must print everything rather than risk a
    drop. An empty set means "read it; nothing matched," which is a safe
    print-everything because the transcript genuinely carries none of these ids.
    """
    text = _read_own_transcript_text()
    if text is None:
        return None
    # JSONL-escaped envelopes arrive with \\"; normalize so the regex matches the
    # raw form too (mirrors sender_from_transcript_text).
    return set(_ID_RE.findall(text.replace('\\"', '"')))


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

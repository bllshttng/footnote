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

# Match one <fno_mail ...> open tag; attribute order is NOT assumed (id and from
# are pulled independently within the tag).
_OPEN_TAG_RE = re.compile(r"<fno_mail\b[^>]*>")
_FROM_RE = re.compile(r'from="([^"]+)"')
_TO_RE = re.compile(r'to="([^"]+)"')
# Capture the id attribute of a <fno_mail ...> open tag (W2 dedup-at-drain).
_ID_RE = re.compile(r'<fno_mail\b[^>]*\bid="([^"]+)"')


def sender_from_transcript_text(
    text: str, msg_id: str, *, to: Optional[str] = None
) -> Optional[str]:
    """Return the ``from`` handle of the ``<fno_mail ... id="<msg_id>" ...>`` open
    tag in ``text``, or ``None`` if no such envelope is present.

    The envelope lives inside JSONL transcript records, so its quotes arrive
    escaped (``from=\\"X\\"``); normalize ``\\"`` to ``"`` before matching so a
    raw or a JSON-escaped transcript both resolve.

    ``to`` turns the match from a MENTION into a RECEIPT: only an envelope
    addressed to that handle counts. An envelope quoted verbatim in a forward
    carries the original ``to=``, so without this a forwarded copy resolves in
    whichever session happens to hold the quote. Callers searching more than one
    candidate store must pass it; the default keeps a single-store search
    unchanged.
    """
    normalized = text.replace('\\"', '"')
    needle = f'id="{msg_id}"'
    for tag in _OPEN_TAG_RE.finditer(normalized):
        s = tag.group(0)
        if needle not in s:
            continue
        if to is not None:
            m_to = _TO_RE.search(s)
            if m_to is None or m_to.group(1) != to:
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

    Import kept local (not module-level): this module is imported lazily from
    inside ``cmd_drain_self``, so a module-level ``from fno.harness_identity
    import resolve_harness_identity`` binds whatever that name pointed to at
    the moment of THAT first import - permanently, since the module is cached
    in ``sys.modules`` thereafter. A test that monkeypatches
    ``harness_identity.resolve_harness_identity`` and happens to trigger this
    module's first import while the patch is active poisons every later caller
    in the same process; monkeypatch's teardown only reverts the attribute on
    ``fno.harness_identity``, not this module's separate name binding.
    """
    from fno.harness_identity import resolve_harness_identity

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


def _candidate_stores() -> list[tuple[str, str]]:
    """Every ``(harness, session_id)`` store this process could own, deduped.

    A process can carry markers for more than one harness family: a claude
    session launched under codex inherits ``CODEX_SESSION_ID`` alongside its own
    ``CLAUDE_CODE_SESSION_ID``. ``resolve_harness_identity`` picks ONE by
    precedence, and picking wrong reads a stranger's rollout as this session's
    transcript. So enumerate instead of picking, and let the receipt decide.
    """
    from fno.harness_identity import present_harness_markers

    seen: set[tuple[str, str]] = set()
    out: list[tuple[str, str]] = []
    for _marker, harness, value in present_harness_markers():
        key = (harness, value)
        if key in seen:
            continue
        seen.add(key)
        out.append(key)
    return out


def resolve_live_sender(msg_id: str) -> Optional[str]:
    """Find ``msg_id``'s sender handle by scanning this session's own transcript.

    Searches every candidate store and accepts the one holding a RECEIPT: an
    envelope carrying both ``id="<msg_id>"`` and a ``to=`` equal to that store's
    own handle. That is a record rather than an inference - a store the message
    was never addressed to cannot produce one, so a wrong candidate is excluded
    by evidence and not by precedence.

    ``None`` on any miss (no marker, unreadable store, id absent) so the caller
    falls through to its existing not-on-bus error path.
    """
    from fno.harness_identity import canonical_handle

    for harness, session_id in _candidate_stores():
        path = _transcript_path(harness, session_id)
        if path is None:
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        sender = sender_from_transcript_text(
            text, msg_id, to=canonical_handle(session_id)
        )
        if sender is not None:
            return sender
    return None


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

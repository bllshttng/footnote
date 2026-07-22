"""Auto-brief resolution for backlog dispatch (x-d1f4).

``resolve_dispatch_brief(node)`` is the single entry point the advance-layer
dispatch call sites use in place of a bare ``node.get("dispatch_brief")``. It is
a first-non-empty-rung priority chain:

  1. ``node.dispatch_brief``  - operator-authored, verbatim. Over the 8 KB env
     budget it stays fail-closed DOWNSTREAM (harness_map raises), so it is
     returned unclamped here.
  2. sidecar brief            - ``has_brief`` + ``briefs_dir()/{id}.md``, the
     carrier that megawalk/spawn writes but that never rode into the env until
     now. Auto-sourced, so clamped into budget with a marker, never refused.
  3. mechanical synthesis     - an ``<fno_spawn>`` envelope + node details +,
     when details are thin, the tail of the source conversation near the node's
     creation. Assembled, never summarized: no LLM at dispatch.
  4. nothing                  - ``(None, "none")``; dispatch proceeds brief-less
     exactly as before, with the tag making the contextless spawn visible.

The whole feature is best-effort: every failure degrades DOWN the chain and no
exception escapes ``resolve_dispatch_brief``. No graph writes happen here.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Optional

from fno.provenance.resolver import ResolvedTranscript, resolve_transcript

# The env budget a brief must fit (mirrors harness_map._BRIEF_MAX_BYTES). Auto
# rungs clamp to it on a UTF-8 byte boundary; the explicit rung is left to the
# downstream fail-closed gate so operator input is never silently truncated.
_BRIEF_MAX_BYTES = 8192
# Details get first claim on the budget; the transcript tail fills what remains.
_DETAILS_MAX_BYTES = 6144
# Rough envelope-header reservation when estimating tail headroom; the final
# clamp guarantees the whole brief fits regardless of this estimate.
_HEADER_ALLOWANCE_BYTES = 512
# Below this headroom a tail fragment is not worth shipping.
_MIN_TAIL_HEADROOM = 512
# Details shorter than this pull the transcript tail; longer details are already
# curated and sufficient on their own.
_THIN_DETAILS_BYTES = 400
# The conversation that birthed the node lives at-or-before created_at + this.
_CREATED_AT_WINDOW_SECONDS = 120
# Records read off the tail of a jsonl transcript / rows off the opencode store.
_TAIL_RECORDS_READ = 200
# Conversational turns kept after windowing.
_TAIL_PAIRS_KEEP = 12
# Per-turn clamp so one huge message cannot swallow the whole tail budget.
_PER_LINE_MAX_BYTES = 600
_TRUNC_MARKER = "\n[truncated]"


def resolve_dispatch_brief(
    node: dict,
    *,
    max_bytes: int = _BRIEF_MAX_BYTES,
    briefs_dir: Optional[Path] = None,
    projects_root: Optional[Path] = None,
    codex_sessions_dir: Optional[Path] = None,
    opencode_db_path: Optional[Path] = None,
) -> tuple[Optional[str], str]:
    """Resolve the dispatch brief for ``node`` -> ``(brief, source_tag)``.

    ``source_tag`` is one of ``explicit | sidecar | synth-details |
    synth-details+tail | synth-tail | none``. Never raises; every store path is
    injectable so tests never touch the real ~/.claude, ~/.codex, or opencode.db.
    """
    try:
        # Rung 1: explicit dispatch_brief, verbatim (unclamped: the >8 KB error
        # is the downstream harness_map gate's, not ours).
        explicit = node.get("dispatch_brief")
        if isinstance(explicit, str) and explicit:
            return explicit, "explicit"

        # Rung 2: sidecar brief.
        try:
            sidecar = _read_sidecar(node, briefs_dir)
        except Exception:
            sidecar = None
        if sidecar:
            return _clamp_bytes(sidecar, max_bytes), "sidecar"

        # Rung 3: mechanical synthesis.
        try:
            brief, tag = _synthesize(
                node, max_bytes, projects_root, codex_sessions_dir, opencode_db_path
            )
        except Exception:
            brief, tag = None, "none"
        if brief:
            return _clamp_bytes(brief, max_bytes), tag

        # Rung 4: nothing to synthesize from.
        return None, "none"
    except Exception:
        # Absolute backstop: dispatch is never blocked by brief resolution.
        return None, "none"


# --------------------------------------------------------------------------- #
# Rung 2 - sidecar
# --------------------------------------------------------------------------- #

def _read_sidecar(node: dict, briefs_dir: Optional[Path]) -> Optional[str]:
    """Content of ``briefs_dir()/{id}.md`` when ``has_brief``, else None."""
    if not node.get("has_brief"):
        return None
    nid = node.get("id")
    if not nid:
        return None
    if briefs_dir is not None:
        base = briefs_dir
    else:
        from fno.paths import briefs_dir as _default_briefs_dir

        base = _default_briefs_dir()
    path = base / f"{nid}.md"
    if not path.exists():
        return None
    text = path.read_text(encoding="utf-8", errors="replace").strip()
    return text or None


# --------------------------------------------------------------------------- #
# Rung 3 - mechanical synthesis
# --------------------------------------------------------------------------- #

def _synthesize(
    node: dict,
    max_bytes: int,
    projects_root: Optional[Path],
    codex_sessions_dir: Optional[Path],
    opencode_db_path: Optional[Path],
) -> tuple[Optional[str], str]:
    details = _details_text(node)
    details_block = _clamp_bytes(details, _DETAILS_MAX_BYTES) if details else ""

    tail_block = ""
    if len(details.encode("utf-8")) < _THIN_DETAILS_BYTES:
        used = len(details_block.encode("utf-8")) + _HEADER_ALLOWANCE_BYTES
        headroom = max_bytes - used
        if headroom >= _MIN_TAIL_HEADROOM:
            tail_block = _transcript_tail_text(
                node, headroom, projects_root, codex_sessions_dir, opencode_db_path
            )

    have_details = bool(details_block)
    have_tail = bool(tail_block)
    if have_details and have_tail:
        tag = "synth-details+tail"
    elif have_details:
        tag = "synth-details"
    elif have_tail:
        tag = "synth-tail"
    else:
        return None, "none"

    return _envelope(node, tag, details_block, tail_block), tag


def _details_text(node: dict) -> str:
    for key in ("details", "description"):
        v = node.get(key)
        if isinstance(v, str) and v.strip():
            return v.strip()
    return ""


def _envelope(node: dict, tag: str, details_block: str, tail_block: str) -> str:
    """The <fno_spawn> envelope header + body. Attributes degrade by omission
    when provenance is missing; they are never fabricated."""
    handle = _initiator_handle(node)
    attrs = []
    if handle:
        attrs.append(f'from="{_attr(handle)}"')
    harness = node.get("source_harness")
    if isinstance(harness, str) and harness.strip():
        attrs.append(f'harness="{_attr(harness)}"')
    label = _node_label(node)
    if label:
        attrs.append(f'node="{_attr(label)}"')
    attrs.append(f'source="{tag}"')

    body = [f"<fno_spawn {' '.join(attrs)}>"]
    if handle:
        body.append(f"reply: fno mail send {handle}")
    title = node.get("title")
    if isinstance(title, str) and title.strip():
        body.append(f"title: {title.strip()}")
    if details_block:
        body.append("details:")
        body.append(details_block)
    if tail_block:
        body.append(tail_block)
    body.append("</fno_spawn>")
    return "\n".join(body)


def _initiator_handle(node: dict) -> Optional[str]:
    """The reply-to mail handle: the live dispatching session's ambient identity
    (a court king dispatching its wave outranks the historical filer), else the
    node's ``source_session_id`` provenance, else None."""
    live = (os.environ.get("FNO_AGENT_SELF") or "").strip()
    if live:
        return live
    sid = node.get("source_session_id")
    if isinstance(sid, str) and sid.strip():
        return sid.strip()
    return None


def _node_label(node: dict) -> str:
    nid = node.get("id")
    if not isinstance(nid, str) or not nid:
        return ""
    slug = node.get("slug")
    if isinstance(slug, str) and slug:
        return f"{nid} {slug}"
    return nid


def _attr(v: str) -> str:
    """Keep an attribute value on one line and inside its quotes so the envelope
    stays machine-greppable."""
    return v.replace('"', "'").replace("\n", " ").replace("\r", " ").strip()


# --------------------------------------------------------------------------- #
# Transcript tail extraction (mechanical, per harness)
# --------------------------------------------------------------------------- #

def _transcript_tail_text(
    node: dict,
    headroom: int,
    projects_root: Optional[Path],
    codex_sessions_dir: Optional[Path],
    opencode_db_path: Optional[Path],
) -> str:
    harness = node.get("source_harness")
    sid = node.get("source_session_id")
    cwd = node.get("source_cwd")
    if not harness or not sid:
        return ""
    rt = resolve_transcript(
        harness,
        sid,
        cwd,
        projects_root=projects_root,
        codex_sessions_dir=codex_sessions_dir,
        opencode_db_path=opencode_db_path,
    )
    # Ambiguous resolution is a miss for the tail rung: wrong-session context is
    # worse than none (AC8).
    if not rt.resolved or rt.ambiguous:
        return ""
    pairs = _extract_pairs(rt, sid)
    if not pairs:
        return ""
    pairs = _apply_window(pairs, node.get("created_at"))
    pairs = pairs[-_TAIL_PAIRS_KEEP:]
    if not pairs:
        return ""
    return _clamp_bytes(_format_pairs(pairs, harness, sid), headroom)


def _extract_pairs(
    rt: ResolvedTranscript, sid: str
) -> list[tuple[str, str, Optional[float]]]:
    try:
        path = Path(rt.transcript_path) if rt.transcript_path else None
        if path is None:
            return []
        if rt.kind == "opencode-db":
            return _opencode_pairs(path, sid)
        if rt.harness == "codex":
            return _codex_pairs(path)
        return _claude_pairs(path)
    except Exception:
        return []


def _claude_pairs(path: Path) -> list[tuple[str, str, Optional[float]]]:
    from fno.agents import read

    pairs: list[tuple[str, str, Optional[float]]] = []
    for line in read._read_jsonl_tail(path, _TAIL_RECORDS_READ):
        rec = _loads(line)
        if not isinstance(rec, dict) or rec.get("type") not in ("user", "assistant"):
            continue
        msg = rec.get("message")
        if not isinstance(msg, dict) or msg.get("role") not in ("user", "assistant"):
            continue
        text = _text_from_content(msg.get("content"))
        if text:
            pairs.append((msg["role"], text, _parse_ts(rec.get("timestamp"))))
    return pairs


def _codex_pairs(path: Path) -> list[tuple[str, str, Optional[float]]]:
    from fno.agents import read

    pairs: list[tuple[str, str, Optional[float]]] = []
    for line in read._read_jsonl_tail(path, _TAIL_RECORDS_READ):
        rec = _loads(line)
        if not isinstance(rec, dict) or rec.get("type") != "response_item":
            continue
        payload = rec.get("payload")
        if not isinstance(payload, dict) or payload.get("type") != "message":
            continue
        if payload.get("role") not in ("user", "assistant"):
            continue
        text = _text_from_content(payload.get("content"))
        if text:
            pairs.append((payload["role"], text, _parse_ts(rec.get("timestamp"))))
    return pairs


def _opencode_pairs(
    db_path: Path, session_id: str
) -> list[tuple[str, str, Optional[float]]]:
    from fno.agents import discover

    rows = discover.opencode_query(
        db_path,
        "SELECT m.time_created, json_extract(m.data, '$.role'), "
        "json_extract(p.data, '$.text') "
        "FROM message m JOIN part p ON p.message_id = m.id "
        "WHERE m.session_id = ? AND json_extract(p.data, '$.type') = 'text' "
        "ORDER BY m.time_created DESC, p.time_created DESC LIMIT ?",
        (session_id, _TAIL_RECORDS_READ),
    )
    pairs: list[tuple[str, str, Optional[float]]] = []
    for ts_ms, role, text in rows:
        if role not in ("user", "assistant") or not isinstance(text, str):
            continue
        text = text.strip()
        if not text:
            continue
        ts = (ts_ms / 1000.0) if isinstance(ts_ms, (int, float)) else None
        pairs.append((role, text, ts))
    pairs.reverse()  # store returns newest-first; restore chronological order
    return pairs


def _text_from_content(content) -> str:
    """Text from a message's content: keep text-ish blocks, drop tool_use /
    tool_result / reasoning / thinking and other non-conversational payloads."""
    if isinstance(content, str):
        return content.strip()
    if not isinstance(content, list):
        return ""
    parts = []
    for block in content:
        if not isinstance(block, dict):
            continue
        btype = block.get("type") or ""
        if not btype.endswith("text"):  # text / input_text / output_text
            continue
        t = block.get("text")
        if isinstance(t, str) and t.strip():
            parts.append(t.strip())
    return "\n".join(parts).strip()


def _apply_window(
    pairs: list[tuple[str, str, Optional[float]]], created_at
) -> list[tuple[str, str, Optional[float]]]:
    """Prefer records at-or-before created_at + window (the conversation that
    birthed the node). The plain-tail fallback applies ONLY when timestamps are
    unusable - an unparseable created_at, or no record carrying a timestamp. When
    records ARE timestamped but every one falls after the cutoff (e.g. the node's
    session kept going long after filing and the bounded tail holds only later
    turns), the tail is OMITTED rather than injecting that unrelated later
    conversation into the brief."""
    cutoff = _parse_ts(created_at)
    if cutoff is None:
        return pairs
    if not any(p[2] is not None for p in pairs):
        return pairs
    cutoff += _CREATED_AT_WINDOW_SECONDS
    return [p for p in pairs if p[2] is not None and p[2] <= cutoff]


def _format_pairs(
    pairs: list[tuple[str, str, Optional[float]]], harness, sid
) -> str:
    short = sid[:8] if isinstance(sid, str) else ""
    lines = [f"--- source-transcript tail ({harness} {short}, near created_at) ---"]
    for role, text, _ts in pairs:
        one = " ".join(text.split())
        lines.append(f"{role}: {_clamp_bytes(one, _PER_LINE_MAX_BYTES)}")
    return "\n".join(lines)


# --------------------------------------------------------------------------- #
# Byte budget
# --------------------------------------------------------------------------- #

def _clamp_bytes(s: str, limit: int) -> str:
    """Clamp ``s`` to ``limit`` UTF-8 bytes on a codepoint boundary, appending a
    truncation marker. A multibyte char is never split."""
    b = s.encode("utf-8")
    if len(b) <= limit:
        return s
    marker_bytes = len(_TRUNC_MARKER.encode("utf-8"))
    keep = max(0, limit - marker_bytes)
    head = b[:keep].decode("utf-8", "ignore")
    return head + _TRUNC_MARKER


def _loads(line: str):
    try:
        return json.loads(line)
    except (ValueError, TypeError):
        return None


def _parse_ts(value) -> Optional[float]:
    """Epoch seconds from an ISO-8601 string (claude/codex ``timestamp``) or a
    numeric epoch. None on anything unparseable."""
    if isinstance(value, (int, float)):
        return float(value)
    if not isinstance(value, str) or not value.strip():
        return None
    s = value.strip()
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    try:
        from datetime import datetime

        return datetime.fromisoformat(s).timestamp()
    except (ValueError, TypeError):
        return None

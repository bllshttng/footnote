"""Event log - atomic JSONL append + audit helpers.

Format: one JSON object per line in .fno/events.jsonl
Schema: {type, campaign_id, session_id, nonce, ts, payload}

Writes commit through the native event store; no file lock is needed.
"""
from __future__ import annotations

import json
import secrets
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, TypedDict

import yaml


# -- Legacy event envelope (write-path only) --

class LegacyEvent(TypedDict):
    """TypedDict for the legacy {type, campaign_id, session_id, nonce, ts, payload} envelope.

    Used exclusively at the emit_event construction site so type-checkers catch
    missing or mis-typed keys on the WRITE path. The read path keeps
    List[Dict[str, Any]] for backward compat with old-shape events on disk.
    """

    type: str
    campaign_id: Optional[str]
    session_id: str
    nonce: str
    ts: str
    payload: Dict[str, Any]


# -- Nonce --

def mint_nonce() -> str:
    """Generate a 32-char lowercase hex nonce via secrets.token_hex(16)."""
    return secrets.token_hex(16)


# -- State helpers --

def _read_state_fields(state_path: Path) -> Dict[str, Any]:
    """Read session_id and campaign_id from a target-state.md frontmatter."""
    text = Path(state_path).read_text(encoding="utf-8")
    if not text.startswith("---"):
        return {}
    rest = text[3:]
    if rest.startswith("\n"):
        rest = rest[1:]
    end_marker = "\n---"
    idx = rest.find(end_marker)
    if idx == -1:
        return {}
    yaml_block = rest[:idx]
    data = yaml.safe_load(yaml_block) or {}
    return data


# -- Envelope normalization --


def normalize_event(event: Dict[str, Any]) -> Dict[str, Any]:
    """Project canonical and legacy envelopes into one read-only join shape."""
    canonical = event.get("data")
    legacy = event.get("payload")
    data = canonical if isinstance(canonical, dict) else (legacy if isinstance(legacy, dict) else {})
    session_id = event.get("session_id") or data.get("session_id")
    holder = data.get("holder")
    holder_session_id = None
    if isinstance(holder, str) and holder.startswith("target-session:"):
        holder_session_id = holder.split(":", 1)[1]
    node_id = (
        event.get("node_id")
        or event.get("graph_node_id")
        or data.get("node_id")
        or data.get("graph_node_id")
        or data.get("node")
    )
    key = data.get("key")
    if node_id is None and isinstance(key, str) and key.startswith("node:"):
        node_id = key.split(":", 1)[1]
    pr_number = event.get("pr_number") or data.get("pr_number")
    if isinstance(pr_number, str) and pr_number.isdigit():
        pr_number = int(pr_number)
    return {
        "type": event.get("type") or event.get("kind"),
        "ts": event.get("ts") or event.get("timestamp"),
        "source": event.get("source"),
        "session_id": session_id,
        "holder_session_id": holder_session_id,
        "node_id": node_id,
        "pr_number": pr_number,
        "short_id": event.get("short_id") or data.get("short_id"),
        "head_sha": event.get("head_sha") or data.get("head_sha"),
        "data": data,
        "raw": event,
    }


# -- Append --

def emit_event(
    event_type: str,
    payload: Dict[str, Any],
    *,
    state_path: Optional[Path] = None,
    events_path: Optional[Path] = None,
) -> str:
    """Append one event to the legacy ``{type, campaign_id, session_id, nonce, ts, payload}`` log.

    LEGACY ENVELOPE - new callers should use ``fno.events._build`` plus
    ``fno.events.append_event`` (canonical ``{ts, type, source, data}``
    envelope) instead. The legacy envelope fails ``fno.events.validate``
    because it lacks ``source`` and uses ``payload`` where the schema expects
    ``data``. Direct callers remain for backwards compatibility (gates,
    megawalk) and are out of scope for a single cleanup; a
    separate spec will drain them.

    Args:
        event_type: Event type string (e.g. "phase_init", "gate_written").
        payload: Arbitrary JSON-serializable dict.
        state_path: Path to target-state.md. Defaults to .fno/target-state.md.
        events_path: Path to events.jsonl. Defaults to .fno/events.jsonl.

    Returns:
        The nonce generated for this event (32 hex chars).
    """
    if state_path is None:
        state_path = Path(".fno/target-state.md")
    if events_path is None:
        # NOT a cwd-relative literal. This is the defect spawn_think and
        # backlog/advance both carried: a hand-built path consults neither
        # FNO_EVENTS_PATH nor FNO_REPO_ROOT, so a test emitting through this
        # writer lands a production-shaped row in the developer's journal. This
        # module wrote under its own filelock rather than through
        # append_event; resolving through fno.paths is still what keeps a
        # test emit out of the developer's production journal.
        from fno.paths import project_events_json

        events_path = project_events_json()

    state_path = Path(state_path)
    events_path = Path(events_path)

    # Read session metadata from state. Log on failure so events with
    # missing session_id (which break gate verification filtering) are
    # attributable to a specific cause rather than appearing as "orphan" rows.
    try:
        state = _read_state_fields(state_path)
    except (FileNotFoundError, OSError) as exc:
        import sys
        print(
            f"events.log.emit: could not read state at {state_path}: "
            f"{type(exc).__name__}: {exc}. Event will be written with empty "
            f"session_id; gate verification may fail to correlate it.",
            file=sys.stderr,
        )
        state = {}

    session_id = state.get("session_id") or ""
    campaign_id = state.get("campaign_id") or None

    nonce = mint_nonce()
    ts = datetime.now(timezone.utc).isoformat()

    # The legacy envelope becomes canonical at the storage boundary: the
    # store only commits {ts, type, source, data}, so the legacy fields land
    # under data and the row stays joinable by every canonical reader
    # (normalize_event projects either shape on the read side).
    data: Dict[str, Any] = dict(payload)
    if session_id:
        data.setdefault("session_id", session_id)
    if campaign_id:
        data.setdefault("campaign_id", campaign_id)
    data.setdefault("nonce", nonce)
    envelope = {"ts": ts, "type": event_type, "source": "legacy", "data": data}
    from fno.events.store_client import emit_envelope

    emit_envelope(envelope, events_path, timeout=10)
    return nonce


# -- Read / filter --

def _filter_by_session(
    rows: List[Dict[str, Any]], session_id: Optional[str]
) -> List[Dict[str, Any]]:
    if session_id is None:
        return rows
    out: List[Dict[str, Any]] = []
    for event in rows:
        normalized = normalize_event(event)
        if session_id in {normalized["session_id"], normalized["holder_session_id"]}:
            out.append(event)
    return out


def read_events(
    events_path: Optional[Path] = None,
    *,
    session_id: Optional[str] = None,
) -> List[Dict[str, Any]]:
    """Read events from the JSONL file, optionally filtered by session_id.

    Args:
        events_path: Path to events.jsonl. Defaults to .fno/events.jsonl.
        session_id: If provided, only return events for this session.

    Returns:
        List of event dicts in append order.

    Raises:
        ValueError: If a line is not valid JSON (log corruption).
    """
    if events_path is None:
        # Read what the writer above writes, or a reader run from a different
        # cwd answers about a different file.
        from fno.paths import project_events_json

        events_path = project_events_json()

    events_path = Path(events_path)

    # SQL authority: committed rows in commit order; a missing store falls
    # back to the raw journal (a fixture or pre-cutover bytes nothing has
    # imported yet), and an unreadable store raises rather than reading empty.
    from fno.events.store_client import import_journal, store_db_path

    if events_path.exists() and events_path.stat().st_size > 0:
        import_journal(events_path)
    if not store_db_path(events_path).exists():
        raw_rows: List[Dict[str, Any]] = []
        if events_path.exists():
            for raw in events_path.read_text(encoding="utf-8").splitlines():
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    raw_rows.append(json.loads(raw))
                except json.JSONDecodeError:
                    continue
        return _filter_by_session(raw_rows, session_id)

    from fno.events.store_client import query_rows

    return _filter_by_session(query_rows(events_path), session_id)


# -- Audit --

def audit_session(
    events_path: Optional[Path] = None,
    *,
    session_id: str,
    strict: bool = False,
) -> Dict[str, Any]:
    """Audit events for a session, optionally checking for required sequences.

    In strict mode, verifies that every phase_init event for a phase also has
    a corresponding gate_written event for the same phase.

    Args:
        events_path: Path to events.jsonl. Defaults to .fno/events.jsonl.
        session_id: Session to audit.
        strict: If True, check for required event sequence gaps.

    Returns:
        {ok: bool, events: [...], gaps: [...] if strict and gaps found}
    """
    events = read_events(events_path, session_id=session_id)

    if not strict:
        return {"ok": True, "events": events}

    # Store rows nest the legacy payload under data; raw pre-cutover lines
    # carry it as payload. Accept either so the audit reads both shapes.
    phases_initiated: set[str] = set()
    for event in events:
        if event["type"] == "phase_init":
            body = event.get("payload") or event.get("data") or {}
            phase = body.get("phase")
            if phase:
                phases_initiated.add(phase)

    phases_gate_written: set[str] = set()
    for event in events:
        if event["type"] == "gate_written":
            body = event.get("payload") or event.get("data") or {}
            phase = body.get("phase")
            if phase:
                phases_gate_written.add(phase)

    gaps: List[str] = []
    for phase in sorted(phases_initiated):
        if phase not in phases_gate_written:
            gaps.append(f"{phase}: gate_written missing")

    if gaps:
        return {"ok": False, "events": events, "gaps": gaps}

    return {"ok": True, "events": events}

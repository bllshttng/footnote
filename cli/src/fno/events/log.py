"""Event log - atomic JSONL append + audit helpers.

Format: one JSON object per line in .fno/events.jsonl
Schema: {type, campaign_id, session_id, nonce, ts, payload}

Writes are atomic via filelock so concurrent processes can't interleave bytes.
"""
from __future__ import annotations

import json
import secrets
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, TypedDict

import filelock
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
    sigma_dispatch, megawalk) and are out of scope for a single cleanup; a
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
        events_path = Path(".fno/events.jsonl")

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

    event: LegacyEvent = {
        "type": event_type,
        "campaign_id": campaign_id,
        "session_id": session_id,
        "nonce": nonce,
        "ts": ts,
        "payload": payload,
    }
    line = json.dumps(event, ensure_ascii=False) + "\n"

    # Ensure parent directory exists
    events_path.parent.mkdir(parents=True, exist_ok=True)

    lock_path = str(events_path) + ".lock"
    with filelock.FileLock(lock_path, timeout=10):
        with events_path.open("a", encoding="utf-8") as fh:
            fh.write(line)

    return nonce


# -- Read / filter --

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
        events_path = Path(".fno/events.jsonl")

    events_path = Path(events_path)

    if not events_path.exists():
        return []

    results: List[Dict[str, Any]] = []
    for lineno, raw in enumerate(events_path.read_text(encoding="utf-8").splitlines(), start=1):
        raw = raw.strip()
        if not raw:
            continue
        try:
            event = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise ValueError(
                f"Corrupted events.jsonl at line {lineno}: {exc}. "
                "Repair or truncate the file before continuing."
            ) from exc
        if session_id is None or event.get("session_id") == session_id:
            results.append(event)

    return results


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

    # Find all phases that had a phase_init
    phases_initiated: set[str] = set()
    for event in events:
        if event["type"] == "phase_init":
            phase = event.get("payload", {}).get("phase")
            if phase:
                phases_initiated.add(phase)

    # Find all phases that had a gate_written
    phases_gate_written: set[str] = set()
    for event in events:
        if event["type"] == "gate_written":
            phase = event.get("payload", {}).get("phase")
            if phase:
                phases_gate_written.add(phase)

    gaps: List[str] = []
    for phase in sorted(phases_initiated):
        if phase not in phases_gate_written:
            gaps.append(f"{phase}: gate_written missing")

    if gaps:
        return {"ok": False, "events": events, "gaps": gaps}

    return {"ok": True, "events": events}

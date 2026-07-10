"""fno-truth worker status: derive Working-while-waiting from claim liveness.

A bg ``/target`` worker reads as "Idle" in fleet surfaces whenever it is
between turns, even while its real work (CI, smoke tests, preflight builds)
runs externally - the harness classifier sees only turn boundaries. fno holds
the truth it cannot: the ``node:<id>`` claim liveness (the sole durable
liveness signal) and per-session ``loop_check`` event recency.

This module is a read-only derivation over those two existing signals. It
never writes claims, events, or registry state, and it fails quiet: an
unresolvable node join returns ``unknown`` so the caller renders exactly as
today (Locked Decision 5).

State table (Locked Decision 2 - ``working`` requires BOTH claim-live AND a
recent fire; claim-live alone is ``waiting``):

    claim live  + recent loop_check   -> working
    claim live  + no recent fire      -> waiting
    claim suspect (in-TTL, dead pid)  -> suspect
    claim stale                       -> stalled
    claim free / corrupted / no node  -> unknown

``done`` is intentionally not derived: a finished worker has RELEASED its
claim (state ``free``), so there is no holder left to recover its session id,
and the harness row already shows the terminal state. ``unknown`` hands it off
(AC7). See the node's Open Questions.
"""

from __future__ import annotations

import json
import re
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

from fno.claims.core import claim_status
from fno.claims.io import claims_root_for

# Claim-live + a fire within this window reads as working; older reads waiting.
# Surfaced in the rendered age so a mis-tuned window misleads less (the age
# being visible means a stale "2h ago" reads wrong at a glance).
RECENCY_WINDOW_S = 1800  # 30 min

# Bounded tail read of events.jsonl so a multi-MB log stays cheap. The newest
# fire per session is always near the end (append-ordered).
_TAIL_BYTES = 256 * 1024

# Worker names are ``target-<node-id>-<slug>`` (dispatch-node.sh). node ids are
# ``<prefix>-<hex>``. A loose match is safe: a mis-parse yields a key that does
# not resolve -> unknown -> no change (fail-quiet, AC7).
_NAME_NODE_RE = re.compile(r"^target-([a-z][a-z0-9]*-[0-9a-f]+)(?:-|$)")

_HOLDER_PREFIX = "target-session:"


def parse_node_id(name: Optional[str]) -> Optional[str]:
    """Extract a node id from a ``target-<node>-<slug>`` worker name, or None."""
    if not name:
        return None
    m = _NAME_NODE_RE.match(name)
    return m.group(1) if m else None


def _session_from_holder(holder: Optional[str]) -> Optional[str]:
    if holder and holder.startswith(_HOLDER_PREFIX):
        sid = holder[len(_HOLDER_PREFIX) :]
        return sid or None
    return None


def _manifest_session_for_holder(
    cwd: Optional[str], holder: Optional[str], claim_key: str
) -> Optional[str]:
    """Join a claim owner to its per-run target session through the manifest.

    Codex claims are owned by the durable thread id while loop events are keyed
    by the unique target-run id. Trust the manifest join only when it records
    the exact claim holder; legacy manifests without ``target_claim_holder``
    remain valid when their session id is itself the holder.
    """
    if not cwd or not holder:
        return None
    try:
        raw = (Path(cwd) / ".fno" / "target-state.md").read_text(
            encoding="utf-8", errors="replace"
        )
    except OSError:
        return None

    def field(name: str) -> Optional[str]:
        match = re.search(rf"^{re.escape(name)}\s*:\s*(.*)$", raw, re.MULTILINE)
        if match is None:
            return None
        value = match.group(1).strip().strip("\"'")
        return value if value and value != "null" else None

    sid = field("session_id")
    if sid is None:
        return None
    recorded_holder = field("target_claim_holder")
    if recorded_holder is not None:
        recorded_key = field("target_claim_key")
        return sid if recorded_holder == holder and recorded_key == claim_key else None
    return sid if holder == f"{_HOLDER_PREFIX}{sid}" else None


def _events_path(events_path: Optional[Path]) -> Path:
    if events_path is not None:
        return events_path
    from fno import paths

    return paths.state_dir() / "events.jsonl"


def _tail_lines(path: Path) -> list[str]:
    """Read the last ``_TAIL_BYTES`` of ``path`` as whole lines (drops a partial
    leading line and a mid-append partial trailing line is skipped by the JSON
    parse). Missing/unreadable file -> empty (AC5-ERR)."""
    try:
        with path.open("rb") as fh:
            fh.seek(0, 2)
            size = fh.tell()
            start = max(0, size - _TAIL_BYTES)
            fh.seek(start)
            chunk = fh.read()
    except OSError:
        return []
    text = chunk.decode("utf-8", errors="replace")
    lines = text.split("\n")
    if start > 0 and lines:
        lines = lines[1:]  # drop the partial first line
    return lines


def _now_s(now_s: Optional[float]) -> float:
    if now_s is not None:
        return now_s
    return datetime.now(timezone.utc).timestamp()


def _ts_to_epoch(ts: str) -> Optional[float]:
    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp()
    except (ValueError, AttributeError):
        return None


def build_loop_check_index(
    *, events_path: Optional[Path] = None, now_s: Optional[float] = None
) -> dict[str, float]:
    """One tail pass over events.jsonl -> ``{session_id: age_seconds}`` for the
    newest ``loop_check`` fire per session. Built once per ``fno agents list``
    invocation and shared across rows (avoids an O(rows) file read)."""
    now = _now_s(now_s)
    newest: dict[str, float] = {}  # session_id -> newest epoch seconds
    for line in _tail_lines(_events_path(events_path)):
        line = line.strip()
        if not line or '"loop_check"' not in line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("type") != "loop_check":
            continue
        data = rec.get("data")
        if not isinstance(data, dict):
            continue
        sid = data.get("session_id")
        ts = rec.get("ts")
        if not isinstance(sid, str) or not isinstance(ts, str):
            continue
        epoch = _ts_to_epoch(ts)
        if epoch is None:
            continue
        if sid not in newest or epoch > newest[sid]:
            newest[sid] = epoch
    return {sid: max(0.0, now - epoch) for sid, epoch in newest.items()}


def resolve_truth_status(
    node_id: Optional[str],
    *,
    manifest_cwd: Optional[str] = None,
    claims_root: Optional[Path] = None,
    events_path: Optional[Path] = None,
    loop_check_ages: Optional[dict[str, float]] = None,
    now_s: Optional[float] = None,
    recency_window_s: float = RECENCY_WINDOW_S,
) -> dict[str, Any]:
    """Derive a worker's fno-truth status from its ``node:<id>`` claim + the
    newest ``loop_check`` fire for the session that holds the claim.

    Returns ``{state, claim_state, last_loop_check_age_s, session_id}`` where
    ``state`` is one of working | waiting | suspect | stalled | unknown.
    Never raises (``claim_status`` never raises; every read degrades to
    unknown). ``loop_check_ages`` is an optional pre-built index (see
    :func:`build_loop_check_index`); when absent the single session is scanned.
    """
    unknown = {
        "state": "unknown",
        "claim_state": None,
        "last_loop_check_age_s": None,
        "session_id": None,
    }
    if not node_id:
        return unknown

    # node:<id> claims are GLOBAL (like ~/.fno/graph.json): they live under the
    # global root ($FNO_CLAIMS_ROOT / $HOME), NOT the cwd/canonical .fno/claims
    # that claim_status's default resolution uses. Route through claims_root_for
    # so `fno agents list` reads the same dir the claim was written to; without
    # this the list always reads `free` and the fill never appears (codex P2).
    key = f"node:{node_id}"
    root = claims_root if claims_root is not None else claims_root_for(key)
    claim = claim_status(key, root=root)
    cs = claim.get("state")
    holder = claim.get("holder")
    sid = _manifest_session_for_holder(manifest_cwd, holder, key)
    if sid is None:
        sid = _session_from_holder(holder)

    if cs == "stale":
        return {
            "state": "stalled",
            "claim_state": cs,
            "last_loop_check_age_s": None,
            "session_id": sid,
        }
    if cs == "suspect":
        return {
            "state": "suspect",
            "claim_state": cs,
            "last_loop_check_age_s": None,
            "session_id": sid,
        }
    if cs != "live":
        # free / corrupted / anything unexpected: hands off unchanged (AC7).
        return {**unknown, "claim_state": cs}

    # Claim live: working iff a recent fire exists for its session.
    age: Optional[float] = None
    if sid is not None:
        if loop_check_ages is not None:
            age = loop_check_ages.get(sid)
        else:
            age = build_loop_check_index(
                events_path=events_path, now_s=now_s
            ).get(sid)
    state = "working" if age is not None and age <= recency_window_s else "waiting"
    return {
        "state": state,
        "claim_state": cs,
        "last_loop_check_age_s": None if age is None else int(age),
        "session_id": sid,
    }


def _humanize_age(seconds: Optional[int]) -> str:
    if seconds is None:
        return "?"
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        return f"{seconds // 60}m"
    return f"{seconds // 3600}h"


def render_truth_status(result: dict[str, Any]) -> Optional[str]:
    """Compact display string with evidence, e.g. ``Working (loop 2m ago)`` /
    ``Stalled (claim stale)``. None for unknown (caller renders as today)."""
    state = result.get("state")
    if state == "working":
        return f"Working (loop {_humanize_age(result.get('last_loop_check_age_s'))} ago)"
    if state == "waiting":
        return "Waiting (claim live)"
    if state == "suspect":
        return "Suspect (claim suspect)"
    if state == "stalled":
        return "Stalled (claim stale)"
    return None

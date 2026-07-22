"""Failure-streak derivation + stranded-dependent surfacing for the
failed-node cascade redesign (ab-5b7cf63a, #34).

Pure, IO-light helpers shared by ``fno backlog maintain`` (the auto-defer
apply-leg) and ``fno backlog triage health`` (the stranded-dependent section).
All policy lives here and in the Python CLI verbs, never in the Rust walker
(Locked Decision #3); the streak is DERIVED from the walker's existing
``node_failed`` / ``node_closed`` events (Locked Decision #4), not a new walker
write or a persistent counter field.

Event envelope
==============

The walker journals each loop event as
``{"ts","type","source":"loop","data":{"unit_id",...}}``
(crates/fno-agents/src/loop_runtime.rs). For the megawalk / target drivers the
``unit_id`` IS the backlog node id, so the streak keys on ``data.unit_id`` (the
design assumed ``graph_node_id``; the real field is ``unit_id`` - the design's
Domain Pitfall flagged exactly this). The flat agents-emitter envelope
(``{...,"kind":...}``) is also accepted so the ``node_undeferred`` reset
boundary can be emitted via ``fno.agents.events.emit``.

Failure / reset classification:

* failure  -> ``node_failed``, or ``node_closed`` with ``close == "parked"``.
* reset    -> ``node_closed`` with ``close == "closed"`` (a success ship), or
              ``node_undeferred`` (emitted by ``fno backlog undefer``).
* ignore   -> everything else, including ``node_closed{close=refused}`` (a
              dispatch refusal, not a work failure): never counts, never resets.
"""
from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Optional

# Reason-prefix sentinel that marks an AUTO defer (vs a human/manual defer), so
# health can always distinguish the two (Failure Modes / Invariants). Claude's
# Discretion #2: a reason-prefix sentinel avoids the schema churn of a dedicated
# field while staying greppable.
AUTO_FAILURE_SENTINEL = "auto-failure:"

_FAIL_TYPE = "node_failed"
_CLOSE_TYPE = "node_closed"
_UNDEFER_TYPE = "node_undeferred"


def events_path() -> Path:
    """The global events log the walker mirrors loop ``node_*`` events into.

    Resolved via ``paths.state_dir()`` (the canonical, config-aware resolver the
    whole Python side uses for ``events.jsonl`` - including
    ``agents.events.emit``; a literal ``~/.fno`` is rejected by the
    no-hardcoded-paths CI guard). The default ``state_dir`` is ``~/.fno``,
    which is exactly where the Rust walker mirrors its loop events, so reader and
    producer coincide on every default install. NOTE: the Rust walker *hardcodes*
    its global mirror to ``$HOME/.fno`` (loop_target.rs) and does not honor
    a customized ``config.state_dir``; under that non-default config the two
    diverge. Closing that gap is a Rust-side follow-up (make the walker honor
    ``state_dir``), out of scope here per Locked Decision #3 (walker untouched).
    Resolved at call time so the conftest ``$HOME`` redirect is honored in tests.
    """
    from fno import paths

    return paths.state_dir() / "events.jsonl"


def read_events(path: Optional[Path] = None) -> list[dict]:
    """Read raw event envelopes from the JSONL log, skipping malformed lines.

    Streams the file line by line so peak memory stays constant as the
    append-only log grows. A truncated / non-JSON line is skipped and never
    raises (AC2-ERR); an absent file yields an empty list (Boundaries).
    """
    target = path if path is not None else events_path()
    out: list[dict] = []
    try:
        with target.open(encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    rec = json.loads(line)
                except (json.JSONDecodeError, ValueError):
                    continue  # truncated / non-JSON line: skip, never abort
                if isinstance(rec, dict):
                    out.append(rec)
    except OSError:
        return out
    return out


@dataclass(frozen=True)
class _Ev:
    node_id: str
    kind: str  # "fail" | "reset"


def _classify(raw: object) -> Optional[_Ev]:
    """Map one raw envelope to a node-scoped (node_id, kind), or None to skip.

    Handles both the walker envelope ``{"type","data":{"unit_id"...}}`` and the
    flat agents envelope ``{"kind","unit_id"...}``. Anything that is not a
    node-scoped failure / reset signal returns None.
    """
    if not isinstance(raw, dict):
        return None
    etype = raw.get("type") or raw.get("kind")
    _data = raw.get("data")
    data = _data if isinstance(_data, dict) else raw
    node_id = data.get("unit_id") or data.get("node_id") or data.get("graph_node_id")
    if not isinstance(node_id, str) or not node_id:
        return None
    if etype == _FAIL_TYPE:
        return _Ev(node_id, "fail")
    if etype == _UNDEFER_TYPE:
        return _Ev(node_id, "reset")
    if etype == _CLOSE_TYPE:
        close = data.get("close")
        if close == "parked":
            return _Ev(node_id, "fail")
        if close == "closed":
            return _Ev(node_id, "reset")
        # "refused" (dispatch refusal) and any other close: neither fail nor reset.
        return None
    return None


def consecutive_failures(node_id: str, events: Iterable[object]) -> int:
    """Count consecutive failure events for ``node_id`` since the most recent
    reset boundary, scanning newest -> oldest.

    Reset boundaries are a success close (``node_closed{close=closed}``) or an
    undefer (``node_undeferred``); node creation is the implicit floor because
    no failure event can precede a node's existence. ``events`` is taken in file
    order (the journal appends chronologically); only events for ``node_id``
    are considered, and dispatch-refusals / unrelated events are ignored so they
    neither inflate nor reset the streak.

    A node with zero failure events yields 0 (Boundaries).
    """
    # Scan newest -> oldest, classifying on demand and stopping at the first
    # reset boundary, so a node with thousands of older events is not fully
    # classified just to read a short recent streak.
    streak = 0
    for raw in reversed(list(events)):
        ev = _classify(raw)
        if ev is None or ev.node_id != node_id:
            continue
        if ev.kind == "fail":
            streak += 1
        else:  # "reset"
            break
    return streak


# NOTE: the failure-defer CANDIDATE detector lives in maintain.py
# (``detect_failure_defers``), alongside the other pure maintain detectors it
# mirrors (``detect_temp_leaks`` / ``detect_rescope_fixes``). This module owns
# only the streak primitive (``consecutive_failures``) it builds on.


def is_auto_failure_deferred(e: object) -> bool:
    """True iff ``e`` is deferred with the ``auto-failure`` sentinel reason.

    Distinguishes an auto-defer from a manual one so a hand-deferred node never
    strand-reports its dependents (Invariants).
    """
    if not isinstance(e, dict) or not e.get("deferred_at"):
        return False
    reason = e.get("deferred_reason")
    return isinstance(reason, str) and reason.startswith(AUTO_FAILURE_SENTINEL)


def stranded_dependents(entries: Iterable[dict]) -> dict[str, list[str]]:
    """Map each auto-failure-deferred node to its dependents (``blocked_by`` it).

    Read-only surfacing (Locked Decision #2): dependents are NEVER mutated. Only
    nodes deferred with the ``auto-failure`` sentinel are considered. A blocker
    with no dependents is omitted (an absent entry means "nothing stranded").
    """
    ents = [e for e in entries if isinstance(e, dict)]
    # Collect the auto-failure-deferred blocker ids once, then a SINGLE pass over
    # entries maps each dependent to its blocker(s) - O(N) instead of a nested
    # scan per blocker.
    blocker_ids = {
        e.get("id")
        for e in ents
        if isinstance(e.get("id"), str) and is_auto_failure_deferred(e)
    }
    out: dict[str, list[str]] = {}
    for e in ents:
        eid = e.get("id")
        if not isinstance(eid, str):
            continue
        # Guard against a malformed non-list blocked_by (a bare string would
        # otherwise do a substring `in` match).
        blocked_by = e.get("blocked_by")
        if not isinstance(blocked_by, list):
            continue
        for bid in blocked_by:
            if bid in blocker_ids:
                out.setdefault(bid, []).append(eid)
    return out

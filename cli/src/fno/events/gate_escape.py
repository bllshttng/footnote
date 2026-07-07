"""Shared gate_escape telemetry (x-f894 Tier-1 + x-91b5 Tier-2).

One emit path for the autonomy-debt counter: reconcile's auto ``dead-bot``
emit, the ``spawn-cap`` auto emit from both spawn gates, and the manual
``fno event gate-escape <reason>`` verb all land here. Fails OPEN (an emit
never aborts the host op), dedups on ``(dedup_key, reason)``, and writes to the
CANONICAL events log so a closed node's telemetry outlives its worktree and
retro aggregates one coherent log.

The layering is deliberate: this sits in the ``events`` package (the low layer
``graph._reconcile`` already imports up from), so reconcile reuses these
primitives without an events -> graph cycle.
"""
from __future__ import annotations

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Mapping, Optional

#: Env vars that mark a test context. ``FNO_SPAWN_GATE=0`` is dual-purpose
#: (test disable + operator escape); these are the ONLY thing separating the
#: two, so the spawn-cap auto-emit fires only when none of these are set
#: (Locked Decision 2). Empty string counts as unset (matches shell ``-n``).
_TEST_CONTEXT_ENV = ("PYTEST_CURRENT_TEST", "CI", "FNO_E2E")


def _env(env: Optional[Mapping[str, str]]) -> Mapping[str, str]:
    return os.environ if env is None else env


def in_test_context(env: Optional[Mapping[str, str]] = None) -> bool:
    """True when any test-context marker is set to a non-empty value."""
    e = _env(env)
    return any(e.get(k) for k in _TEST_CONTEXT_ENV)


def should_emit_spawn_cap(env: Optional[Mapping[str, str]] = None) -> bool:
    """Parity-fixture core (AC2-FR): would a bypass in THIS env emit spawn-cap?

    True iff the gate is bypassed (``FNO_SPAWN_GATE=0``) AND this is not a test
    context. The Rust guard (``spawn_gate.rs``) mirrors this exactly; a shared
    fixture fails the build on any divergence (Locked Decision 5).
    """
    e = _env(env)
    return e.get("FNO_SPAWN_GATE") == "0" and not in_test_context(e)


def _utc_day(env: Optional[Mapping[str, str]] = None) -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%d")


def default_dedup_key(reason: str, env: Optional[Mapping[str, str]] = None) -> str:
    """The ``(reason, session, day)`` bucket for a PR-less escape.

    Collapses a burst-loop of bypassed spawns in one session/day to one count
    (AC2-INV). When no shared session id is available the bucket degrades to
    ``reason:day`` (all same-day same-reason escapes collapse) - deliberately
    coarse, because under-reporting is the safe direction and over-reporting
    poisons the exact roadmap signal this metric feeds (Locked Decision 1). A
    shared session id is preferred over pid so sibling ``fno agents spawn``
    processes from one operator burst collapse rather than triple-count.
    """
    e = _env(env)
    session = (e.get("FNO_SESSION") or e.get("FNO_SESSION_PID") or "").strip()
    return f"{reason}:{session}:{_utc_day(e)}"


def canonical_events_path(cwd: Optional[str] = None) -> Path:
    """The single events log gate_escape telemetry lands in (retro reads it).

    A closed node outlives its worktree and per-worktree events.jsonl are not
    shared, so the metric must aggregate from the canonical root. Resolve from
    ``cwd`` when given (a full-graph reconcile in repo A can close a node whose
    worktree is repo B), else the process-cwd canonical root.
    """
    from fno.paths import resolve_canonical_repo_root, resolve_canonical_worktree

    if cwd:
        canon = resolve_canonical_worktree(Path(cwd))
        if canon is not None:
            return canon.resolve() / ".fno" / "events.jsonl"
    return resolve_canonical_repo_root() / ".fno" / "events.jsonl"


def failure_log_path(events_path: Path) -> Path:
    """The durable emit-failure counter, beside its events log (retro AC1-FR)."""
    return Path(events_path).parent / "gate_escape_emit_failures.jsonl"


def record_emit_failure(
    log_path: Optional[Path], node_id: str, reason: str, exc: Exception
) -> None:
    """Append one line to a durable emit-failure log so retro can surface a
    broken counter (AC1-FR): a fail-open emit that silently dropped would make
    the metric under-report and OVERSTATE autonomy. Best-effort - a failure to
    log the failure is swallowed; never raise from the telemetry path."""
    print(
        f"gate_escape emit failed (reason={reason}): {type(exc).__name__}: {exc}; "
        f"host op unaffected, telemetry fails open",
        file=sys.stderr,
    )
    if log_path is None:
        return
    try:
        Path(log_path).parent.mkdir(parents=True, exist_ok=True)
        line = json.dumps(
            {
                "ts": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
                "node": node_id or "",
                "reason": reason,
                "error": f"{type(exc).__name__}: {exc}",
            },
            separators=(",", ":"),
        )
        with Path(log_path).open("a", encoding="utf-8") as fh:
            fh.write(line + "\n")
    except Exception:
        pass  # ponytail: the failure-log write is itself best-effort; never raise


def already_emitted(
    events_path: Path,
    reason: str,
    *,
    pr: Optional[int] = None,
    dedup_key: Optional[str] = None,
) -> bool:
    """True if events.jsonl already holds a matching gate_escape.

    Dedup on ``(reason, pr)`` for a PR-bearing escape (Tier-1 dead-bot) or
    ``(reason, dedup_key)`` for a PR-less one (Tier-2). With neither key there
    is nothing to dedup on -> not a duplicate. A missing/unreadable log reads
    as 'not emitted' (fail-open toward emitting).

    This read is NOT atomic with the append that follows in ``emit_gate_escape``
    (they take the events lock separately). Two same-bucket emits racing (e.g.
    the Rust and Python bypass paths in one session/day) can both read
    'not emitted' and both append - a rare, bounded double-count the design
    explicitly accepts, mirroring Tier-1's stance. The tested AC2-INV contract
    (a sequential burst in one process) holds because each read follows the
    prior append."""
    if pr is None and dedup_key is None:
        return False
    try:
        # Stream line-by-line: the canonical events log grows unboundedly, so
        # never slurp it whole just to scan for a dup (gemini review on #241).
        with Path(events_path).open("r", encoding="utf-8") as fh:
            for line in fh:
                if '"gate_escape"' not in line:
                    continue
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(ev, dict) or ev.get("type") != "gate_escape":
                    continue
                data = ev.get("data")
                if not isinstance(data, dict) or data.get("reason") != reason:
                    continue
                if pr is not None and data.get("pr") == pr:
                    return True
                if dedup_key is not None and data.get("dedup_key") == dedup_key:
                    return True
    except (OSError, UnicodeDecodeError):
        return False
    return False


def emit_gate_escape(
    reason: str,
    *,
    pr: Optional[int] = None,
    node_id: Optional[str] = None,
    detail: Optional[str] = None,
    dedup_key: Optional[str] = None,
    source: str = "backlog",
    events_path: Optional[Path] = None,
    cwd: Optional[str] = None,
) -> Optional[Path]:
    """Emit one gate_escape, deduped and fail-open. Returns the events path on
    a successful append, or None on a dedup-skip / swallowed failure.

    A bad-reason :class:`ValidationError` PROPAGATES (fail-closed enum): the
    manual verb turns it into a loud non-zero exit and emits nothing (AC1-ERR).
    Auto callers pass a hardcoded-valid reason and wrap their own try/except, so
    a spawn is never blocked by telemetry (AC1-FR). Every OTHER runtime failure
    (an unwritable log, a resolve error) fails OPEN here and is recorded to a
    durable counter so retro can surface an under-reporting metric (AC1-FR).
    """
    # Normalize a placeholder/unassigned PR to "no PR" HERE (one place), so the
    # (pr, reason) dedup and the emitted payload agree. Then enforce the dedup
    # contract loudly: an escape dedups on (reason, pr) XOR (reason, dedup_key),
    # never both - passing both would let already_emitted OR-match and suppress a
    # genuinely-new escape that merely shares one field, silently miscounting the
    # trust-core metric. No current caller passes both; the guard protects future
    # ones (sigma type-design review).
    if pr is not None and pr <= 0:
        pr = None
    if pr is not None and dedup_key is not None:
        raise ValueError("emit_gate_escape: pass pr XOR dedup_key, not both")

    resolved: Optional[Path] = Path(events_path) if events_path is not None else None
    from fno.events import ValidationError, _build, append_event

    try:
        event = _build("gate_escape", source, _escape_data(reason, pr, node_id, detail, dedup_key))
    except ValidationError:
        raise  # fail-closed enum; caller decides loud-exit vs swallow

    try:
        if resolved is None:
            resolved = canonical_events_path(cwd)
        if already_emitted(resolved, reason, pr=pr, dedup_key=dedup_key):
            return None
        append_event(event, events_path=resolved)
        return resolved
    except Exception as exc:  # AC1-FR: any runtime failure fails OPEN
        record_emit_failure(
            failure_log_path(resolved) if resolved else None, node_id or "", reason, exc
        )
        return None


def _escape_data(
    reason: str,
    pr: Optional[int],
    node_id: Optional[str],
    detail: Optional[str],
    dedup_key: Optional[str],
) -> dict[str, Any]:
    data: dict[str, Any] = {"reason": reason}
    if pr is not None and pr > 0:
        data["pr"] = pr
    if node_id:
        data["graph_node_id"] = node_id
    if detail:
        data["detail"] = detail
    if dedup_key:
        data["dedup_key"] = dedup_key
    return data

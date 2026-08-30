"""The pr-watch tick's king wake phase.

The one component that owns the refill edge: a king that correctly exited
``NoWork`` on an empty board and whose board then refilled (or whose mail
arrived) is woken from here. Nothing inside the king loop can observe that - a
terminated loop is not running - and the fleet watchdog implements a different
predicate (``stalled``), which a cleanly-exited king never is.

Triggers, in the order the operator fixed: mail first (the strongest - the
signal already exists, is durable, and its arrival for an absent holder is a
complete positive wake marker), board change second, and a timer backstop
last, kept only so a missed event cannot strand a scope forever.

The wake ledger (``fno.king.wake``, rolling 24h window on the manifest) is the
rate bound; ``should_wake`` gates every trigger through it, and the bill lands
BEFORE the dispatch: a crash between the two costs one slot of a 32-wide
window, while the reverse costs an unbounded respawn storm.
"""
from __future__ import annotations

import hashlib
import json
import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Optional

#: Marker for the operator question a ceiling refusal raises, deduped through
#: the shared already-asked fold so one stranded scope asks once, not per tick.
_CEILING_MARKER = "king-wake-ceiling"


@dataclass(frozen=True)
class CrownTarget:
    """One crowned scope the phase may wake."""

    holder: str
    scope: str
    root: Path
    manifest: Path
    #: The holder's session short id - the REPLY handle mail carries. Measured
    #: on the live bus (2026-08-29): of 2699 rows, 394 sit at ``to ==
    #: <short_id>`` for the busiest king and ZERO at its registry name, so a
    #: name-only scan reads a permanent zero while mail piles up. Senders
    #: address a row by name, repliers by the footer handle; both spellings
    #: arrive, so both are scanned.
    short_id: str = ""


def _crowned(court_fn: Callable, rows_fn: Optional[Callable] = None) -> tuple[list[CrownTarget], str]:
    """Every crowned scope with its holder, root, and manifest path.

    Registry rows that merely lack a manifest (a crown armed while the king
    loop was off writes no file) are skipped: the walk refuses a missing
    manifest, and there is no ledger to bill. A scope held by more than one
    live row is a conflict the court's own lane owns; waking into it would
    spawn a second king over a disputed territory, so it is skipped too.
    """
    if rows_fn is None:
        from fno.agents.registry import load_registry

        rows_fn = load_registry
    rows = rows_fn()
    by_holder = {row.name: row for row in rows}
    court = court_fn(rows)
    crowns = court.get("crowns")
    if crowns is None:
        return [], "registry unreadable - no scope enumerated, nothing woken"
    by_scope: dict[str, list[dict]] = {}
    for entry in crowns:
        by_scope.setdefault(entry.get("scope") or "", []).append(entry)
    out: list[CrownTarget] = []
    skipped_conflicts = 0
    for scope, entries in by_scope.items():
        if not scope:
            continue
        if len(entries) > 1:
            skipped_conflicts += 1
            continue
        holder = entries[0].get("holder") or ""
        row = by_holder.get(holder)
        cwd = getattr(row, "cwd", "") if row is not None else ""
        short_id = (getattr(row, "short_id", "") or "") if row is not None else ""
        if not holder or not cwd:
            continue
        root = Path(cwd)
        # The validating helper, never a hand join: a corrupted crown_scope
        # ("../x", an absolute path) must refuse here, not escape .fno/kings
        # into a file the ledger would rewrite.
        try:
            from fno.king.state import king_manifest_path

            manifest = king_manifest_path(scope, state_root=root / ".fno")
        except ValueError:
            continue
        if not manifest.is_file():
            continue
        out.append(
            CrownTarget(
                holder=holder,
                scope=scope,
                root=root,
                manifest=manifest,
                short_id=short_id,
            )
        )
    note = f"{skipped_conflicts} conflicting scope(s) skipped" if skipped_conflicts else ""
    return out, note


def _holder_absent(truth: dict) -> "str | None":
    """The refusal word for a holder that is NOT absent, else None.

    Liveness is transcript truth via ``resolve_session_truth``, never a
    registry state word. Only ``done`` and ``unknown``/``not-found`` are
    absence. ``working``/``watching``/``your-move`` mean a live king;
    ``stalled`` belongs to the fleet watchdog, which already wakes it; and
    ``unknown`` with ``no-records`` or ``resolver-error`` is an instrument
    failure, which is not an absence - failing closed here inverts the king
    board's own quiet-holder posture, because the action differs: the board's
    action on an unresolved holder is one harmless wake message, while this
    phase's action spawns a whole king process against a scope that may
    already have a live one.
    """
    state = truth.get("state")
    if state == "done":
        return None
    if state == "unknown":
        reason = truth.get("reason")
        return None if reason == "not-found" else f"unknown/{reason}"
    return str(state)


def _mail_trigger(target: CrownTarget, unread_fn: Callable) -> Optional[str]:
    """The matched address with undrained mail, else ``None``.

    A non-empty ``scan_unread`` is a complete positive signal: the cursor has
    not advanced past those rows by construction, so there is no "since last
    tick" bookkeeping to add and none should exist. The address set covers
    both spellings a sender produces (registry name, reply-handle short id -
    see :class:`CrownTarget.short_id`) plus every project in the scope, whose
    broadcasts carry ``to == <project>``.

    The match is RETURNED, not just reported: the woken king is a fresh
    session whose whoami names its own ids, and ``scan_unread`` matches
    ``to ==`` exactly, so the wake prompt must carry the address the trigger
    matched or the row that woke the scope can never be drained.
    """
    from fno.agents.crown import split_scope

    addresses = {target.holder, target.short_id, *split_scope(target.scope)}
    for address in sorted(a for a in addresses if a):
        if unread_fn(address):
            return address
    return None


def _raise_ceiling_question(target: CrownTarget, count: int, ceiling: int) -> str:
    """One durable operator question per scope at the ceiling.

    A ceiling refusal against a live trigger is exactly the silent stranding
    this phase exists to end, so it escalates once (marker-deduped) rather
    than staying an event nobody reads. The dedupe IS the "once per window":
    while the scope sits at the ceiling the question stands, and clearing it
    while the scope is still stranded re-asks - correctly.
    """
    import secrets

    from fno.agents.stale_escalate import already_asked
    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    existing = already_asked(target.root, target.scope, marker=_CEILING_MARKER)
    if existing:
        return existing
    qid = f"q-{secrets.token_hex(4)}"
    append_question_event(
        operator_question(
            question_id=qid,
            question=(
                f"[{_CEILING_MARKER}:{target.scope}] Scope {target.scope} is at its "
                f"king wake ceiling ({count}/{ceiling} in the rolling 24h) while a "
                f"wake trigger is live. The king cannot be woken again until "
                f"stamps age out of the window; if this is wrong, raise "
                f"king.wake_ceiling."
            ),
            cwd=str(target.root),
            ask=f"raise king.wake_ceiling or clear the trigger for {target.scope}",
            source="daemon",
        ),
        target.root,
    )
    return qid


def _sidecar_path(target: CrownTarget) -> Path:
    """``.fno/kings/<scope>.wake.json`` - the tick-local board-hash cache.

    Not the manifest: this is a cache with no reign meaning, and the
    manifest's write-once contract should not absorb a value that changes
    every ten minutes. Declared in docs/state-root-inventory.md.
    """
    return target.manifest.parent / f"{target.scope}.wake.json"


def _board_hash(scope: str, entries: list, resolver: Optional[Callable] = None) -> str:
    """sha256 over the scope's ``(id, status, column, priority)`` rows.

    Reuses :func:`fno.king.board.compile_scope_ids` for scope resolution -
    never a reimplementation. An empty string means "no signal" (uncompilable
    scope), which the caller must treat as unchanged, never as a change: an
    epic that left the graph is not a board refill.
    """
    from fno.king.board import compile_scope_ids

    kwargs = {"resolve": resolver} if resolver is not None else {}
    try:
        ids = compile_scope_ids(scope, entries, **kwargs)
    except Exception:  # noqa: BLE001 - an uncompilable scope is not a trigger
        return ""
    rows = sorted(
        (
            str(row.get("id") or ""),
            str(row.get("status") or ""),
            str(row.get("_kanban_column") or ""),
            str(row.get("priority") or ""),
        )
        for row in entries
        if isinstance(row, dict) and str(row.get("id") or "") in ids
    )
    joined = "\n".join("|".join(row) for row in rows)
    return hashlib.sha256(joined.encode("utf-8")).hexdigest()


def _board_trigger(
    target: CrownTarget,
    entries: list,
    resolver: Optional[Callable] = None,
) -> tuple[bool, Optional[str]]:
    """``(wake?, hash_to_store_after_a_dispatch)``.

    An ABSENT stored hash is a first observation, not a change: it is stored
    here and wakes nothing, or every new crown wakes on its first tick. A
    CHANGED hash is returned for the caller to store only after a dispatch
    actually fired - a refused board change must stay a trigger, exactly like
    the refused mail a cursor has not advanced past. An unreadable graph
    (empty entries) is not a board emptied: no signal.
    """
    if not entries:
        return False, None
    fresh = _board_hash(target.scope, entries, resolver)
    if not fresh:
        return False, None
    sidecar = _sidecar_path(target)
    stored = ""
    try:
        stored = str(
            json.loads(sidecar.read_text(encoding="utf-8")).get("board_hash") or ""
        )
    except (OSError, ValueError):
        stored = ""  # a corrupt sidecar reads as first observation
    if not stored:
        _store_board_hash(target, fresh)
        return False, None
    if stored == fresh:
        return False, None
    return True, fresh


def _store_board_hash(target: CrownTarget, digest: str) -> None:
    sidecar = _sidecar_path(target)
    sidecar.parent.mkdir(parents=True, exist_ok=True)
    tmp = sidecar.with_suffix(".json.tmp")
    tmp.write_text(
        json.dumps({"board_hash": digest}), encoding="utf-8"
    )
    tmp.replace(sidecar)


#: The undispatched columns a backstop counts as actionable. Not full parity
#: with the king board's actionable count (which also reads claims and PRs):
#: this only decides whether a periodic re-check is WORTH a wake, and a wrong
#: positive costs one debounced wake that terminates NoWork.
_ACTIONABLE_COLUMNS = frozenset({"ready", "next"})


def _scope_actionable(scope: str, entries: list, resolver: Optional[Callable] = None) -> int:
    """Scope rows in an undispatched column at a king-worked priority."""
    from fno.king.board import KING_PRIORITIES, compile_scope_ids

    kwargs = {"resolve": resolver} if resolver is not None else {}
    try:
        ids = compile_scope_ids(scope, entries, **kwargs)
    except Exception:  # noqa: BLE001 - an uncompilable scope has nothing to re-check
        return 0
    return sum(
        1
        for row in entries
        if isinstance(row, dict)
        and str(row.get("id") or "") in ids
        and str(row.get("_kanban_column") or "") in _ACTIONABLE_COLUMNS
        and str(row.get("priority") or "") in KING_PRIORITIES
    )


def _backstop_due(
    target: CrownTarget,
    entries: list,
    *,
    now: datetime,
    backstop_s: int,
    resolver: Optional[Callable] = None,
) -> bool:
    """Whether the timer backstop should fire for this scope.

    The backstop is an APPROXIMATION of the mail and board-change triggers,
    kept so a missed event cannot strand a scope forever: it fires mostly on
    unchanged boards, and the interval is a policy choice, not a measurement -
    do not read 1800 as derived. No mail and no board change are established
    by the caller (this only runs when no other trigger fired). The last leg
    of the condition is the ledger itself: no billed wake inside the backstop
    window, so a woken-and-working king is never re-fired by its own backstop.

    A king terminal inside the window also suppresses it, reading the journal
    rather than trusting the proxy below. The proxy counts ready rows the
    real board would not count actionable (rows an active worker holds), so a
    dead-king scope with live workers would otherwise fire a NoWork walk
    every window and burn the wake ceiling in under a day. A walk that ran
    and answered is the positive record that it need not run again yet.
    """
    from fno.king.state import last_run_is_fresh
    from fno.king.wake import read_wakes
    from fno.outstanding.core import events_path

    now_iso = now.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    if last_run_is_fresh(events_path(target.root), since_s=backstop_s, now_iso=now_iso):
        return False
    if _scope_actionable(target.scope, entries, resolver) <= 0:
        return False
    stamps = read_wakes(target.manifest, now=now)
    if stamps and (now - stamps[-1]).total_seconds() < backstop_s:
        return False
    return True


def _dispatch_walk(
    target: CrownTarget, reason: str, binary: str, address: Optional[str] = None
) -> None:
    """Spawn the wake-mode walk, detached.

    The walk blocks for the whole reign it starts, and the tick has later legs
    and a hard deadline, so the walk runs in its own session. Its stdout goes
    to a per-scope log beside the manifest; its terminations go to the events
    journal, which is the receipt the next reader trusts. A mail wake carries
    the matched address: the woken session is fresh and can derive neither the
    dead holder's name nor its reply-handle short id from any whoami of its
    own, so without the address on the command line the row that woke the
    scope is undrainable and the wake refires on it.
    """
    argv = [
        binary,
        "loop",
        "run",
        "--driver",
        "king",
        "--scope",
        target.scope,
        "--wake",
        "--wake-reason",
        reason,
    ]
    if address:
        argv += ["--wake-address", address]
    log = target.manifest.with_suffix(".md.wake.log")
    with log.open("ab") as sink:
        subprocess.Popen(  # noqa: S603 - fixed argv, no shell
            argv,
            cwd=str(target.root),
            stdout=sink,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )


def run_king_wake(
    settings,
    *,
    emit: Callable[[str, dict], Any],
    now: Optional[datetime] = None,
    court_fn: Optional[Callable] = None,
    rows_fn: Optional[Callable] = None,
    truth_fn: Optional[Callable] = None,
    unread_fn: Optional[Callable] = None,
    entries_fn: Optional[Callable] = None,
    scope_resolver: Optional[Callable] = None,
    admit_fn: Optional[Callable] = None,
    dispatch_fn: Optional[Callable] = None,
    ask_fn: Optional[Callable] = None,
) -> dict[str, Any]:
    """One pass over every crowned scope. Never raises into the tick.

    Returns a summary the tick echoes: how many scopes were considered, which
    woke and why, and which refusals named what.
    """
    cfg = getattr(settings, "king", None)
    if not getattr(cfg, "wake_enabled", False):
        return {"armed": False}
    now = now or datetime.now(timezone.utc)
    if court_fn is None:
        from fno.agents.court import gather_court

        court_fn = gather_court
    if truth_fn is None:
        from fno.agents.session_truth import resolve_session_truth

        truth_fn = resolve_session_truth
    if unread_fn is None:
        from fno.bus.cursor import scan_unread

        unread_fn = scan_unread
    if entries_fn is None:
        def entries_fn() -> list:  # noqa: F811 - lazy default, loaded once below
            # Backend-switched, mirroring the tick's own board read: under an
            # external tracker backend there is no graph to hash, and a stale
            # graph.json must not drive a board-change trigger.
            from fno.graph.store import read_graph
            from fno.paths import graph_json
            from fno.tracker import active_backend_name

            try:
                if active_backend_name() != "graph":
                    return []
                return read_graph(graph_json())
            except Exception:  # noqa: BLE001 - an unreadable graph is no signal
                return []

    entries: Optional[list] = None
    if admit_fn is None:
        from fno.king.wake import admit_wake

        admit_fn = admit_wake

    # None-vs-zero matters on all three: a configured 0 is the unbounded or
    # disabled spelling (mirroring at_respawn_ceiling's convention), and `or`
    # would coerce it back to the default, silently refusing an operator's
    # explicit choice.
    def _cfg_int(key: str, default: int) -> int:
        raw = getattr(cfg, key, None)
        return int(raw) if raw is not None else default

    ceiling = _cfg_int("wake_ceiling", 32)
    debounce_s = _cfg_int("wake_debounce_seconds", 900)
    backstop_s = _cfg_int("wake_backstop_seconds", 1800)

    targets, note = _crowned(court_fn, rows_fn)
    summary: dict[str, Any] = {
        "armed": True,
        "crowns": len(targets),
        "woke": [],
        "refused": [],
        "note": note,
    }
    for target in targets:
        refusal = _holder_absent(truth_fn(target.holder))
        if refusal is not None:
            # Liveness refusals are routine (a live king reads "working"):
            # they ride the summary, not the event stream - one event per
            # crown per 600s tick is noise that buries the real refusals.
            summary["refused"].append({"scope": target.scope, "refusal": refusal})
            continue
        reason: Optional[str] = None
        wake_address: Optional[str] = None
        matched = _mail_trigger(target, unread_fn)
        if matched is not None:
            reason = "mail"
            wake_address = matched
        fresh_board_hash: Optional[str] = None
        if reason is None:
            if entries is None:
                entries = entries_fn()
            changed, fresh_board_hash = _board_trigger(
                target, entries, scope_resolver
            )
            if changed:
                reason = "board"
            elif _backstop_due(
                target,
                entries,
                now=now,
                backstop_s=backstop_s,
                resolver=scope_resolver,
            ):
                reason = "backstop"
        if reason is None:
            continue
        # Admit-and-bill in ONE lock: `allowed` means the bill already landed,
        # so two overlapping ticks cannot both dispatch - the loser sees the
        # winner's stamp inside the critical section and takes the refusal.
        verdict = admit_fn(
            target.manifest, now=now, ceiling=ceiling, debounce_s=debounce_s
        )
        if not verdict.allowed:
            emit(
                "king_wake_refused",
                {
                    "scope": target.scope,
                    "refusal": verdict.refusal,
                    "reason": reason,
                    "window_count": verdict.count,
                    "ceiling": ceiling,
                },
            )
            if verdict.refusal == "ceiling":
                ask = ask_fn or (lambda t, c, k: _raise_ceiling_question(t, c, k))
                try:
                    ask(target, verdict.count, ceiling)
                except Exception:  # noqa: BLE001 - a failed ask never blocks the wake lane
                    summary["note"] = "ceiling question could not be raised"
            summary["refused"].append(
                {"scope": target.scope, "refusal": verdict.refusal}
            )
            continue
        window_count = verdict.count
        if dispatch_fn is not None:
            dispatch_fn(target, reason, wake_address)
        else:
            import shutil

            _dispatch_walk(
                target,
                reason,
                shutil.which("fno-agents") or "fno-agents",
                wake_address,
            )
        if fresh_board_hash:
            _store_board_hash(target, fresh_board_hash)
        emit(
            "king_woken",
            {
                "scope": target.scope,
                "reason": reason,
                "address": wake_address,
                "window_count": window_count,
                "ceiling": ceiling,
            },
        )
        summary["woke"].append(
            {"scope": target.scope, "reason": reason, "address": wake_address}
        )
    return summary

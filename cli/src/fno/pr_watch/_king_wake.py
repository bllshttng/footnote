"""The pr-watch tick's king wake phase: a king that exited ``NoWork`` and
whose board then refilled is woken from here, because nothing inside a
terminated loop can observe that. Triggers, best first: an answer to the
king's own escalation, mail, board change, timer backstop. The manifest's
wake ledger is the rate bound, billed before dispatch."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterable, Optional, cast

#: Ceiling-refusal question markers, deduped through the shared already-asked
#: fold so one stranded scope asks once. The successor marker's remedy is the
#: manifest's respawn ceiling, not ``king.wake_ceiling``.
_CEILING_MARKER = "king-wake-ceiling"
_SUCCESSOR_CEILING_MARKER = "king-wake-respawn-ceiling"


def _ask_wake_ceiling(target: "CrownTarget", count: int, ceiling: int) -> str:
    return _raise_marker_question(
        target,
        _CEILING_MARKER,
        f"Scope {target.scope} is at its king wake ceiling ({count}/{ceiling} in "
        f"the rolling 24h) while a wake trigger is live.",
        f"raise king.wake_ceiling or clear the trigger for {target.scope}",
    )


def _ask_respawn_ceiling(target: "CrownTarget", count: int, ceiling: int) -> str:
    return _raise_marker_question(
        target,
        _SUCCESSOR_CEILING_MARKER,
        f"Holder {target.holder} of scope {target.scope} is gone, a trigger is "
        f"live, and the respawn budget is spent ({count}/{ceiling}).",
        f"crown a fresh king for {target.scope} or raise its respawn ceiling",
    )


@dataclass(frozen=True)
class CrownTarget:
    """One crowned scope the phase may wake."""

    holder: str
    scope: str
    root: Path
    manifest: Path
    #: The REPLY handle mail carries. Measured 2026-08-29: of 2699 rows, 394 sit
    #: at ``to == <short_id>`` for the busiest king, ZERO at its name.
    short_id: str = ""


def _crowned(
    court_fn: Callable, rows_fn: Optional[Callable] = None
) -> tuple[list[CrownTarget], str]:
    """Crowned scopes with holder, root, manifest; no-manifest and disputed
    rows are skipped."""
    if rows_fn is None:
        from fno.agents.registry import load_registry

        rows_fn = load_registry
    from fno.king.state import king_manifest_path, king_state_root

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
        # must refuse, not escape .fno/kings.
        try:
            manifest = king_manifest_path(scope, state_root=king_state_root(root))
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
    """The refusal word for a holder that is NOT absent: only ``done`` and
    ``unknown/not-found`` are absence; instrument failures fail closed."""
    state = truth.get("state")
    if state == "done":
        return None
    if state == "unknown":
        reason = truth.get("reason")
        return None if reason == "not-found" else f"unknown/{reason}"
    return str(state)


def _mail_trigger(target: CrownTarget, unread_fn: Callable) -> Optional[str]:
    """The matched address with undrained mail, returned for the prompt."""
    from fno.agents.crown import split_scope

    addresses = {target.holder, target.short_id, *split_scope(target.scope)}
    for address in sorted(a for a in addresses if a):
        if unread_fn(address):
            return address
    return None


def _escalation_answer_trigger(
    target: CrownTarget,
    records: list,
    cursor: str,
) -> tuple[Optional[str], str]:
    """``(prompt, matched_close_ts)`` for the OLDEST unprocessed answer to the
    holder's own question: one per tick, so a pile of answers delivers in
    order. Delivery mail addresses the full session id, which no mailbox scan
    covers. The ts is stored only after a dispatch - refusals re-fire."""
    addresses = {target.holder, target.short_id}
    from fno.harness_identity import canonical_handle
    from fno.king.state import parse_manifest

    full_id = parse_manifest(target.manifest).get("harness_session_id") or ""
    if full_id:
        addresses.update({full_id, canonical_handle(full_id)})
    prompt: Optional[str] = None
    matched_ts = ""
    for record in records:
        closed_ts = str(record.get("closed_ts") or "")
        if not closed_ts or closed_ts <= cursor:
            continue
        if record.get("asker") in addresses:
            prompt = (
                f"Answer to your question {record['id']} "
                f'"{record["question"]}": {record["answer"]}.'
            )
            matched_ts = closed_ts
            break
    return prompt, matched_ts


def _raise_marker_question(target: CrownTarget, marker: str, question: str, ask: str) -> str:
    """One durable question per scope per marker; clearing it while the
    scope is still stranded re-asks - correctly."""
    import secrets

    from fno.agents.stale_escalate import already_asked
    from fno.events import operator_question
    from fno.outstanding.core import append_question_event

    existing = already_asked(target.root, target.scope, marker=marker)
    if existing:
        return existing
    qid = f"q-{secrets.token_hex(4)}"
    append_question_event(
        operator_question(
            question_id=qid,
            question=f"[{marker}:{target.scope}] {question}",
            cwd=str(target.root),
            ask=ask,
            source="daemon",
        ),
        target.root,
    )
    return qid


def _sidecar_path(target: CrownTarget) -> Path:
    """``.fno/kings/<scope>.wake.json`` - the tick-local cache, never the manifest."""
    return target.manifest.parent / f"{target.scope}.wake.json"


def _read_sidecar(target: CrownTarget) -> dict:
    """The whole sidecar payload; unreadable reads as empty."""
    try:
        payload = json.loads(_sidecar_path(target).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return payload if isinstance(payload, dict) else {}


def _write_sidecar(target: CrownTarget, payload: dict) -> None:
    sidecar = _sidecar_path(target)
    sidecar.parent.mkdir(parents=True, exist_ok=True)
    tmp = sidecar.with_suffix(f".json.{os.getpid()}.tmp")
    tmp.write_text(json.dumps(payload), encoding="utf-8")
    tmp.replace(sidecar)


def _update_sidecar(target: CrownTarget, **fields: object) -> None:
    """Read-modify-write under the manifest-lock helper, like the ledger:
    two overlapping ticks must not lose each other's key."""
    from fno.king.state import _manifest_lock

    with _manifest_lock(_sidecar_path(target)):
        payload = _read_sidecar(target)
        payload.update(fields)
        _write_sidecar(target, payload)


def _board_rows(
    scope: str, entries: list, resolver: Optional[Callable] = None
) -> "list[tuple[str, str, str, str]] | None":
    """The scope's ``(id, status, column, priority)`` rows, sorted, via
    ``compile_scope_ids``; None is no signal, not an empty board."""
    from fno.king.scope import compile_scope_ids

    kwargs = {"resolve": resolver} if resolver is not None else {}
    try:
        ids = compile_scope_ids(scope, entries, **kwargs)
    except Exception:  # noqa: BLE001 - an uncompilable scope is not a trigger
        return None
    fields = ("id", "status", "_kanban_column", "priority")
    rows = [
        tuple(str(row.get(f) or "") for f in fields)
        for row in entries
        if isinstance(row, dict) and str(row.get("id") or "") in ids
    ]
    return sorted(cast("list[tuple[str, str, str, str]]", rows))


def _hash_rows(rows: list) -> str:
    joined = "\n".join("|".join(row) for row in rows)
    return hashlib.sha256(joined.encode("utf-8")).hexdigest()


def _board_digest(scope: str, entries: list, resolver: Optional[Callable] = None) -> str:
    """The digest the sidecar stores: sha256 over the scope's rows; no
    observation digests as the empty string."""
    rows = _board_rows(scope, entries, resolver)
    return "" if rows is None else _hash_rows(rows)


def _birth_cursor(manifest: Path) -> str:
    """The manifest's created_at in the journal's ts shape: seeding the
    answer cursor at the reign's birth keeps a just-closed answer live."""
    from fno.king.state import parse_manifest

    try:
        stamp = datetime.strptime(
            parse_manifest(manifest).get("created_at") or "", "%Y-%m-%dT%H:%M:%SZ"
        )
    except ValueError:
        return ""
    return stamp.replace(tzinfo=timezone.utc).isoformat()


def _read_board_sidecar(target: CrownTarget) -> "tuple[str, list[tuple[str, ...]] | None]":
    """``(stored_hash, stored_rows)``; corrupt or row-less reads as a first
    observation."""
    payload = _read_sidecar(target)
    stored_hash = str(payload.get("board_hash") or "")
    raw_rows = payload.get("board_rows")
    rows = None
    if isinstance(raw_rows, list) and raw_rows:
        # A corrupt element reads as no observation, never raises out of the
        # tick: every later scope would be stranded with it.
        rows = [
            tuple(str(f) for f in row)
            for row in raw_rows
            if isinstance(row, (list, tuple)) and len(row) == 4
        ] or None
    return stored_hash, rows


def _board_trigger(
    target: CrownTarget, rows
) -> tuple[bool, Optional[str], Optional[list], Optional[str]]:
    """``(wake?, hash+rows_to_store_after_a_dispatch, diff)``. An absent hash
    or row-less sidecar is a first observation; a changed hash stores only
    after a dispatch; no rows is no signal."""
    if rows is None:
        return False, None, None, None
    fresh = _hash_rows(rows)
    stored, stored_rows = _read_board_sidecar(target)
    if not stored or stored_rows is None:
        _store_board_hash(target, fresh, rows)
        return False, None, None, None
    if stored == fresh:
        return False, None, None, None
    return True, fresh, rows, render_board_diff(stored_rows, rows)


def _store_board_hash(target: CrownTarget, digest: str, rows: Iterable) -> None:
    _update_sidecar(target, board_hash=digest, board_rows=[list(row) for row in rows])


def _scope_undelivered(scope: str, entries: list, resolver: Optional[Callable] = None) -> int:
    """Crown nodes not closed for good; 0 when the scope cannot compile."""
    from fno.king.scope import scope_undelivered

    try:
        return scope_undelivered(scope, entries, resolver)
    except Exception:  # noqa: BLE001 - an uncompilable scope has nothing to re-check
        return 0


def _backstop_due(
    target: CrownTarget,
    entries: list,
    *,
    now: datetime,
    backstop_s: int,
    resolver: Optional[Callable] = None,
) -> bool:
    """Whether the timer backstop fires: an approximation of the event
    triggers so a missed event cannot strand a scope; a billed wake or king
    terminal inside the window suppresses it. The count below is the goal's
    own predicate, done or superseded, so a driven-but-unshipped row keeps
    the scope wakeable."""
    from fno.king.state import last_run_is_fresh
    from fno.king.wake import read_wakes
    from fno.outstanding.core import events_path

    now_iso = now.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    if last_run_is_fresh(events_path(target.root), since_s=backstop_s, now_iso=now_iso):
        return False
    if _scope_undelivered(target.scope, entries, resolver) <= 0:
        return False
    stamps = read_wakes(target.manifest, now=now)
    if stamps and (now - stamps[-1]).total_seconds() < backstop_s:
        return False
    return True


#: Byte bound on one rendered diff: it rides the walk's argv as
#: ``--wake-detail``, so a capped render names what it elided.
MAX_DETAIL_CHARS = 2000


def render_board_diff(old_rows, new_rows, *, cap: int = MAX_DETAIL_CHARS) -> str:
    """The added/changed/removed rows between two wake observations, as
    prompt text; a removed row is named, not hidden."""
    old = {str(row[0]): tuple(str(f) for f in row) for row in old_rows or ()}
    new = {str(row[0]): tuple(str(f) for f in row) for row in new_rows or ()}
    lines: list[str] = []
    for row_id in sorted(set(old) - set(new)):
        lines.append(f"removed: {row_id} ({_row_label(old[row_id])})")
    for row_id in sorted(set(new) - set(old)):
        lines.append(f"added: {row_id} ({_row_label(new[row_id])})")
    for row_id in sorted(set(new) & set(old)):
        if new[row_id] != old[row_id]:
            lines.append(
                f"changed: {row_id} {_row_label(old[row_id])} -> {_row_label(new[row_id])}"
            )
    if not lines:
        return ""
    text = "\n".join(lines)
    if len(text) <= cap:
        return text
    shown: list[str] = []
    used = 0
    for line in lines:
        if used + len(line) + 1 > cap - 32:
            break
        shown.append(line)
        used += len(line) + 1
    return "\n".join(shown) + f"\n...(+{len(lines) - len(shown)} more rows elided)"


def _row_label(row: tuple) -> str:
    return "/".join(part for part in row[1:] if part)


def _dispatch_walk(
    target: CrownTarget,
    reason: str,
    binary: str,
    address: Optional[str] = None,
    detail: Optional[str] = None,
    successor: bool = False,
) -> None:
    """Spawn the wake-mode walk, detached. The address and the diff travel on
    the command line: the fresh session cannot derive either itself."""
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
        "--wake-holder",
        target.holder,
    ]
    if address:
        argv += ["--wake-address", address]
    if detail:
        argv += ["--wake-detail", detail]
    if successor:
        argv += ["--wake-successor"]
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
    answered_fn: Optional[Callable] = None,
    entries_fn: Optional[Callable] = None,
    scope_resolver: Optional[Callable] = None,
    admit_fn: Optional[Callable] = None,
    dispatch_fn: Optional[Callable] = None,
    ask_fn: Optional[Callable] = None,
) -> dict[str, Any]:
    """One pass over every crowned scope; never raises into the tick. Returns
    the summary the tick echoes: scopes considered, wakes, refusals."""
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
    if answered_fn is None:
        from fno.outstanding.core import read_answered_questions

        answered_fn = read_answered_questions
    if entries_fn is None:

        def entries_fn() -> list:  # noqa: F811 - lazy default, loaded once below
            # Backend-switched like the tick's board read: a stale graph under
            # an external tracker backend must not drive a trigger.
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

    # None-vs-zero matters: 0 is the unbounded spelling, and `or` would
    # coerce it back to the default, refusing an operator's explicit choice.
    def _cfg_int(key: str, default: int) -> int:
        raw = getattr(cfg, key, None)
        return int(raw) if raw is not None else default

    ceiling = _cfg_int("wake_ceiling", 32)
    debounce_s = _cfg_int("wake_debounce_seconds", 900)
    backstop_s = _cfg_int("wake_backstop_seconds", 1800)

    # One question-journal read per tick, shared by every scope like `entries`.
    try:
        answered_records: list = answered_fn()
    except Exception:  # noqa: BLE001 - an unreadable journal is not a trigger
        answered_records = []

    targets, note = _crowned(court_fn, rows_fn)
    summary: dict[str, Any] = {
        "armed": True,
        "crowns": len(targets),
        "woke": [],
        "refused": [],
        "note": note,
    }
    for target in targets:
        truth = truth_fn(target.holder)
        refusal = _holder_absent(truth)
        if refusal is not None:
            # Liveness refusals ride the summary, not the event stream.
            summary["refused"].append({"scope": target.scope, "refusal": refusal})
            continue
        # A GONE holder is replaced, not woken: the dispatch bills the
        # respawn budget, not only the wake ledger.
        holder_gone = truth.get("state") == "unknown" and truth.get("reason") == "not-found"
        reason: Optional[str] = None
        wake_address: Optional[str] = None
        wake_detail: Optional[str] = None
        answered_cursor_to_store = ""
        sidecar = _read_sidecar(target)
        if "answered_cursor" not in sidecar:
            # Seed at birth, never the journal max: a max seed swallows an
            # answer closed before the first armed tick saw it.
            _update_sidecar(target, answered_cursor=_birth_cursor(target.manifest))
        else:
            answer_prompt, matched_ts = _escalation_answer_trigger(
                target, answered_records, str(sidecar.get("answered_cursor") or "")
            )
            if answer_prompt is not None:
                reason = "escalation_answered"
                # Delivery mail addressed the full session id: name it or the
                # row lingers unread (no scan covers that address).
                from fno.king.state import parse_manifest

                full_id = parse_manifest(target.manifest).get("harness_session_id") or ""
                if full_id:
                    answer_prompt += (
                        f" The answer was also delivered as mail addressed to "
                        f"{full_id}: run `fno agents mail unread --name {full_id}` "
                        f"and drain it, then advance the cursor with "
                        f"`fno agents mail ack <id> --name {full_id}`."
                    )
                if len(answer_prompt) > MAX_DETAIL_CHARS:
                    # One argv element: a pasted log must not abort the pass.
                    answer_prompt = answer_prompt[:MAX_DETAIL_CHARS] + " ...(truncated)"
                wake_detail = answer_prompt
                answered_cursor_to_store = matched_ts
        matched = None if reason is not None else _mail_trigger(target, unread_fn)
        if matched is not None:
            reason = "mail"
            wake_address = matched
        fresh_board_hash: Optional[str] = None
        fresh_board_rows: Optional[list] = None
        if reason is None:
            if entries is None:
                entries = entries_fn()
            # One compile feeds both lanes; None rows (empty or uncompilable
            # scope) is no signal for either.
            rows = _board_rows(target.scope, entries, scope_resolver) if entries else None
            changed, fresh_board_hash, fresh_board_rows, wake_detail = _board_trigger(target, rows)
            if changed:
                reason = "board"
            elif rows is not None and _backstop_due(
                target, entries, now=now, backstop_s=backstop_s, resolver=scope_resolver
            ):
                reason = "backstop"
        if reason is None:
            continue
        if holder_gone:
            from fno.king.state import at_respawn_ceiling, parse_manifest, respawn_ceiling

            if at_respawn_ceiling(target.manifest):
                fields = parse_manifest(target.manifest)
                emit(
                    "king_wake_refused",
                    {
                        "scope": target.scope,
                        "refusal": "respawn-ceiling",
                        "reason": reason,
                        "holder": target.holder,
                    },
                )
                try:
                    (ask_fn or _ask_respawn_ceiling)(
                        target,
                        int(fields.get("respawn_count") or 0),
                        respawn_ceiling(target.manifest),
                    )
                except Exception:  # noqa: BLE001 - a failed ask never blocks the lane
                    summary["note"] = "successor ceiling question could not be raised"
                summary["refused"].append({"scope": target.scope, "refusal": "respawn-ceiling"})
                continue
        # Admit-and-bill in ONE lock: an answered escalation skips only the
        # debounce, never the ceiling.
        verdict = admit_fn(
            target.manifest,
            now=now,
            ceiling=ceiling,
            debounce_s=0 if reason == "escalation_answered" else debounce_s,
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
                try:
                    (ask_fn or _ask_wake_ceiling)(target, verdict.count, ceiling)
                except Exception:  # noqa: BLE001 - a failed ask never blocks the wake lane
                    summary["note"] = "ceiling question could not be raised"
            summary["refused"].append({"scope": target.scope, "refusal": verdict.refusal})
            continue
        window_count = verdict.count
        if dispatch_fn is not None:
            dispatch_fn(target, reason, wake_address, wake_detail, holder_gone)
        else:
            import shutil

            _dispatch_walk(
                target,
                reason,
                shutil.which("fno-agents") or "fno-agents",
                wake_address,
                wake_detail,
                holder_gone,
            )
        if holder_gone:
            # The new session does not exist yet; its trail is the walk's journal.
            from fno.king.state import parse_manifest as _pm

            emit(
                "king_spawned_successor",
                {
                    "scope": target.scope,
                    "holder": target.holder,
                    "old_session_id": _pm(target.manifest).get("harness_session_id") or "",
                    "trigger": reason,
                },
            )
        if fresh_board_hash:
            _store_board_hash(target, fresh_board_hash, fresh_board_rows or ())
        if answered_cursor_to_store:
            _update_sidecar(target, answered_cursor=answered_cursor_to_store)
        receipt = {
            "scope": target.scope,
            "reason": reason,
            "address": wake_address,
            "successor": holder_gone,
        }
        emit("king_woken", {**receipt, "window_count": window_count, "ceiling": ceiling})
        summary["woke"].append(receipt)
    return summary

"""The pr-watch tick's lead wake phase: a lead that exited ``NoWork`` and
whose board then refilled is woken from here, because nothing inside a
terminated loop can observe that. Triggers, best first: an answer to the
lead's own escalation, mail, board change, timer backstop. The manifest's
wake ledger is the rate bound, billed before dispatch."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from datetime import datetime, timezone
from functools import partial
from pathlib import Path
from typing import Any, Callable, Iterable, Optional, cast

#: Ceiling-refusal question markers, deduped through the shared already-asked
#: fold so one stranded scope asks once. The successor marker's remedy is the
#: manifest's respawn ceiling, not ``lead.wake_ceiling``.
_CEILING_MARKER = "lead-wake-ceiling"
_SUCCESSOR_CEILING_MARKER = "lead-wake-respawn-ceiling"


def _ask_wake_ceiling(target: "RoleTarget", count: int, ceiling: int) -> str:
    return _raise_marker_question(
        target,
        _CEILING_MARKER,
        f"Scope {target.scope} is at its lead wake ceiling ({count}/{ceiling} in "
        f"the rolling 24h) while a wake trigger is live.",
        f"raise lead.wake_ceiling or clear the trigger for {target.scope}",
    )


def _ask_respawn_ceiling(target: "RoleTarget", count: int, ceiling: int) -> str:
    return _raise_marker_question(
        target,
        _SUCCESSOR_CEILING_MARKER,
        f"Holder {target.holder} of scope {target.scope} is gone, a trigger is "
        f"live, and the respawn budget is spent ({count}/{ceiling}).",
        f"role a fresh lead for {target.scope} or raise its respawn ceiling",
    )


@dataclass(frozen=True)
class RoleTarget:
    """One promoted scope the phase may wake."""

    holder: str
    scope: str
    root: Path
    manifest: Path
    #: The REPLY handle mail carries. Measured 2026-08-29: of 2699 rows, 394 sit
    #: at ``to == <short_id>`` for the busiest lead, ZERO at its name.
    short_id: str = ""


def _promoted(
    team_fn: Callable, rows_fn: Optional[Callable] = None
) -> tuple[list[RoleTarget], str]:
    """Promoted scopes use registry rows for roots; drops are named by scope."""
    if rows_fn is None:
        from fno.agents.registry import load_registry

        rows_fn = load_registry
    from fno.lead.state import lead_manifest_path, lead_state_root

    rows = rows_fn()
    by_holder = {row.name: row for row in rows}
    team = team_fn(rows)
    roles = team.get("roles")
    if roles is None:
        return [], "registry unreadable - no scope enumerated, nothing woken"
    by_scope: dict[str, list[dict]] = {}
    for entry in roles:
        by_scope.setdefault(entry.get("scope") or "", []).append(entry)
    out: list[RoleTarget] = []
    dropped: list[str] = []
    for scope, entries in by_scope.items():
        if not scope:
            dropped.append("(no scope): empty scope")
            continue
        if len(entries) > 1:
            holders = ", ".join(str(e.get("holder") or "?") for e in entries)
            dropped.append(f"{scope}: conflicting holders {holders}")
            continue
        holder = entries[0].get("holder") or ""
        if not holder:
            dropped.append(f"{scope}: holderless role")
            continue
        row = by_holder.get(holder)
        cwd = getattr(row, "cwd", "") if row is not None else ""
        short_id = (getattr(row, "short_id", "") or "") if row is not None else ""
        if not cwd:
            dropped.append(f"{scope}: unregistered holder")
            continue
        root = Path(cwd)
        # The validating helper, never a hand join: a corrupted role_scope
        # must refuse, not escape .fno/leads.
        try:
            manifest = lead_manifest_path(scope, state_root=lead_state_root(root))
        except ValueError as exc:
            dropped.append(f"{scope}: {exc}")
            continue
        if not manifest.is_file():
            dropped.append(f"{scope}: manifest missing at {manifest}")
            continue
        out.append(
            RoleTarget(
                holder=holder,
                scope=scope,
                root=root,
                manifest=manifest,
                short_id=short_id,
            )
        )
    return out, "; ".join(dropped)


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


def _mail_trigger(target: RoleTarget, unread_fn: Callable) -> Optional[str]:
    """The matched address with undrained mail, returned for the prompt."""
    from fno.agents.role import split_scope

    addresses = {target.holder, target.short_id, *split_scope(target.scope)}
    for address in sorted(a for a in addresses if a):
        if unread_fn(address):
            return address
    return None


def _escalation_answer_trigger(
    target: RoleTarget,
    records: list,
    cursor: str,
) -> tuple[Optional[str], str]:
    """``(prompt, matched_close_ts)`` for the OLDEST unprocessed answer to the
    holder's own question: one per tick, so a pile of answers delivers in
    order. Delivery mail addresses the full session id, which no mailbox scan
    covers. The ts is stored only after a dispatch - refusals re-fire."""
    addresses = {target.holder, target.short_id}
    from fno.harness_identity import canonical_handle
    from fno.lead.state import parse_manifest

    full_id = parse_manifest(target.manifest).get("harness_session_id") or ""
    if full_id:
        addresses.update({full_id, canonical_handle(full_id)})
    from fno.events.store_client import read_projection
    from fno.outstanding.core import questions_path

    result = read_projection(questions_path(), "--next-answer", {
        "addresses": sorted(addresses), "records": records, "cursor": cursor,
    })
    return result["prompt"], result["cursor"]


def _raise_marker_question(target: RoleTarget, marker: str, question: str, ask: str) -> str:
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


def _sidecar_path(target: RoleTarget) -> Path:
    """``.fno/leads/<scope>.wake.json`` - the tick-local cache, never the manifest."""
    return target.manifest.parent / f"{target.scope}.wake.json"


def _read_sidecar(target: RoleTarget) -> dict:
    """The whole sidecar payload; unreadable reads as empty."""
    try:
        payload = json.loads(_sidecar_path(target).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return payload if isinstance(payload, dict) else {}


def _write_sidecar(target: RoleTarget, payload: dict) -> None:
    sidecar = _sidecar_path(target)
    sidecar.parent.mkdir(parents=True, exist_ok=True)
    tmp = sidecar.with_suffix(f".json.{os.getpid()}.tmp")
    tmp.write_text(json.dumps(payload), encoding="utf-8")
    tmp.replace(sidecar)


def _update_sidecar(target: RoleTarget, **fields: object) -> None:
    """Read-modify-write under the manifest-lock helper, like the ledger:
    two overlapping ticks must not lose each other's key."""
    from fno.lead.state import _manifest_lock

    with _manifest_lock(_sidecar_path(target)):
        payload = _read_sidecar(target)
        payload.update(fields)
        _write_sidecar(target, payload)


def _board_rows(
    scope: str, entries: list, resolver: Optional[Callable] = None
) -> "list[tuple[str, str, str, str]] | None":
    """The scope's ``(id, status, column, priority)`` rows, sorted, via
    ``compile_scope_ids``; None is no signal, not an empty board."""
    from fno.lead.scope import compile_scope_ids

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
    answer cursor at the term's birth keeps a just-closed answer live."""
    from fno.lead.state import parse_manifest

    try:
        stamp = datetime.strptime(
            parse_manifest(manifest).get("created_at") or "", "%Y-%m-%dT%H:%M:%SZ"
        )
    except ValueError:
        return ""
    return stamp.replace(tzinfo=timezone.utc).isoformat()


def _read_board_sidecar(target: RoleTarget) -> "tuple[str, list[tuple[str, ...]] | None]":
    """``(stored_hash, stored_rows)``; corrupt reads as no observation."""
    payload = _read_sidecar(target)
    stored_hash = str(payload.get("board_hash") or "")
    raw_rows = payload.get("board_rows")
    rows = None
    if isinstance(raw_rows, list):
        # A corrupt element reads as no observation, never raises out of the
        # tick: every later scope would be stranded with it.
        rows = [
            tuple(str(f) for f in row)
            for row in raw_rows
            if isinstance(row, (list, tuple)) and len(row) == 4
        ]
        if len(rows) != len(raw_rows):
            rows = None
    return stored_hash, rows


def _board_trigger(
    target: RoleTarget, rows
) -> tuple[bool, Optional[str], Optional[list], Optional[str], bool]:
    """``(wake?, hash+rows_to_store_after_a_dispatch, diff, first_observation)``.
    Pure: it never writes. An absent-hash sidecar is a first
    observation - the caller stores it only after the holder reads present; a
    changed hash stores only after a dispatch; no rows is no signal."""
    if rows is None:
        return False, None, None, None, False
    fresh = _hash_rows(rows)
    stored, stored_rows = _read_board_sidecar(target)
    if not stored or stored_rows is None:
        return False, fresh, rows, None, True
    if stored == fresh:
        return False, None, None, None, False
    return True, fresh, rows, render_board_diff(stored_rows, rows), False


def _store_board_hash(target: RoleTarget, digest: str, rows: Iterable) -> None:
    _update_sidecar(target, board_hash=digest, board_rows=[list(row) for row in rows])


def _scope_undelivered(scope: str, entries: list, resolver: Optional[Callable] = None) -> int:
    """Role nodes not closed for good; 0 when the scope cannot compile."""
    from fno.lead.scope import scope_undelivered

    try:
        return scope_undelivered(scope, entries, resolver)
    except Exception:  # noqa: BLE001 - an uncompilable scope has nothing to re-check
        return 0


def _backstop_due(
    target: RoleTarget,
    entries: list,
    *,
    now: datetime,
    backstop_s: int,
    resolver: Optional[Callable] = None,
) -> bool:
    """Whether the timer backstop fires: an approximation of the event
    triggers so a missed event cannot strand a scope; a billed wake or lead
    terminal inside the window suppresses it. The count below is the goal's
    own predicate, done or superseded, so a driven-but-unshipped row keeps
    the scope wakeable."""
    from fno.lead.state import last_run_is_fresh
    from fno.lead.wake import read_wakes
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
    target: RoleTarget,
    reason: str,
    binary: str,
    address: Optional[str] = None,
    detail: Optional[str] = None,
    successor: bool = False,
) -> bool:
    """Spawn the wake-mode walk, detached. The address and the diff travel on
    the command line: the fresh session cannot derive either itself.

    Neither can it derive a model: the argv carries the role manifest's pin,
    and a manifest without one refuses the walk - an unpinned lead respawn
    bills the account default model."""
    from fno.lead.state import parse_manifest

    model = (parse_manifest(target.manifest).get("model") or "").strip()
    log = target.manifest.with_suffix(".md.wake.log")
    if not model:
        with log.open("ab") as sink:
            sink.write(
                f"lead-wake refused {target.scope}: the role manifest carries "
                "no model pin; re-role pinned, never respawn on the account "
                "default.\n".encode("utf-8")
            )
        return False
    argv = [
        binary,
        "loop",
        "run",
        "--driver",
        "lead",
        "--scope",
        target.scope,
        "--model",
        model,
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
    with log.open("ab") as sink:
        subprocess.Popen(  # noqa: S603 - fixed argv, no shell
            argv,
            cwd=str(target.root),
            stdout=sink,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    return True


#: A pass stops under 15s left rather than be cut mid-step and lose every role after it.
_LEAD_STEP_FLOOR_S = 15.0
#: Wait bound on one truth read: a slower read yields its role to the rest of the pass.
_LEAD_TRUTH_WAIT_S = 10.0

_GRAPH_ENTRIES_MEMO: dict = {"ident": None, "entries": None}


def graph_entries(path: Optional[Path] = None) -> list:
    """The tick's graph read, one real read per graph identity: an unchanged
    graph is byte-identical, so the memo is not staleness. Serves every
    phase that needs entries - sweep discovery and the wake alike - so the
    15 MB store is read once per tick, not once per phase."""
    from fno.graph.api import wire_rows
    from fno.lead import drain_cache
    from fno.paths import graph_json
    from fno.tracker import active_backend_name

    try:
        if active_backend_name() != "graph":
            return []
        gpath = path or graph_json()
        ident = drain_cache.graph_ident(gpath)
        if ident is not None and _GRAPH_ENTRIES_MEMO["ident"] == ident:
            return _GRAPH_ENTRIES_MEMO["entries"]
        entries = wire_rows(path=gpath)
        if ident is not None:
            _GRAPH_ENTRIES_MEMO.update(ident=ident, entries=entries)
        return entries
    except Exception:  # noqa: BLE001 - an unreadable graph is no signal
        return []


def run_lead_wake(
    settings,
    *,
    emit: Callable[[str, dict], Any],
    now: Optional[datetime] = None,
    team_fn: Optional[Callable] = None,
    rows_fn: Optional[Callable] = None,
    truth_fn: Optional[Callable] = None,
    unread_fn: Optional[Callable] = None,
    answered_fn: Optional[Callable] = None,
    entries_fn: Optional[Callable] = None,
    scope_resolver: Optional[Callable] = None,
    admit_fn: Optional[Callable] = None,
    dispatch_fn: Optional[Callable] = None,
    ask_fn: Optional[Callable] = None,
    seconds_left_fn: Optional[Callable[[], Optional[float]]] = None,
    on_step: Optional[Callable[[str], None]] = None,
) -> dict[str, Any]:
    """One pass over every promoted scope; never raises into the tick. Returns
    the summary the tick echoes: scopes considered, wakes, refusals, plus
    ``truth_reads``/``evaluated`` (the pass's real cost) and ``budget_spent``
    when it stopped under its step floor. Triggers are evaluated read-only,
    cheapest first; the truth read runs only for a role that can wake."""
    cfg = getattr(settings, "lead", None)
    if not getattr(cfg, "wake_enabled", False):
        return {"armed": False}
    now = now or datetime.now(timezone.utc)
    if team_fn is None:
        from fno.agents.team import gather_team

        # The wake reads holder and scope, never agreement: skip that graph parse.
        team_fn = partial(gather_team, agree=False)
    if truth_fn is None:
        from fno.agents.cli import _batch_resolver
        from fno.agents.session_truth import resolve_session_truth
        batch: list = []  # one discovery scan per pass, built inside the first bounded read

        def _resolve(handle: str):
            if not batch:
                batch.append(_batch_resolver())
            return batch[0](handle)
        truth_fn = partial(resolve_session_truth, resolve=_resolve)
    if answered_fn is None:
        from fno.outstanding.core import read_answered_questions

        answered_fn = read_answered_questions
    if entries_fn is None:
        entries_fn = graph_entries

    # The graph read starts with the pass and overlaps the setup reads. Later roles poll it.
    graph_pool = ThreadPoolExecutor(max_workers=1)
    graph_future, graph_cut = graph_pool.submit(entries_fn), False

    entries: Optional[list] = None
    if admit_fn is None:
        from fno.lead.wake import admit_wake

        admit_fn = admit_wake

    # None-vs-zero matters: 0 is the unbounded spelling, and `or` would
    # coerce it back to the default, refusing an operator's explicit choice.
    def _cfg_int(key: str, default: int) -> int:
        raw = getattr(cfg, key, None)
        return int(raw) if raw is not None else default

    ceiling = _cfg_int("wake_ceiling", 32)
    debounce_s = _cfg_int("wake_debounce_seconds", 900)
    backstop_s = _cfg_int("wake_backstop_seconds", 1800)

    _step = on_step or (lambda _s: None)

    def _wait_cap(left, cap=_LEAD_TRUTH_WAIT_S):
        return cap if left is None else min(cap, max(0.0, left - _LEAD_STEP_FLOOR_S))

    def _bounded(fn, *args, wait_s: float):
        # Returns (value, timed_out); the reader runs on past a timeout: a join would spend the bound.
        pool = ThreadPoolExecutor(max_workers=1)
        try:
            future = pool.submit(fn, *args)
            return future.result(timeout=wait_s), False
        except TimeoutError:  # the reads never raise it themselves
            return None, True
        finally:
            pool.shutdown(wait=False)

    def _setup_bounded(step, fn, *args, cap: Optional[float] = None):
        left = seconds_left_fn() if seconds_left_fn is not None else None
        if left is not None and left < _LEAD_STEP_FLOOR_S:
            return None, True
        if step:
            _step(step)
        return _bounded(fn, *args,
                        wait_s=_wait_cap(left, cap if cap is not None else _LEAD_TRUTH_WAIT_S))

    if unread_fn is None:
        from fno.bus.cursor import scan_unread
        from fno.bus.log import iter_messages

        # One bus read per pass, not one per address: a role has up to nine.
        scanned, _bus_cut = _setup_bounded(None, lambda: list(iter_messages()))
        unread_fn = partial(scan_unread, messages=scanned or [])

    def _note(msg: str) -> None:
        prior = str(summary.get("note") or "")
        summary["note"] = f"{prior}; {msg}" if prior else msg

    def _budget_stop(step: str) -> dict[str, Any]:
        # Appends, never clobbers: a stop keeps the note naming why.
        _note(f"budget spent after {summary['evaluated']} of {len(targets)} roles, before {step}")
        summary["budget_spent"] = True
        return summary


    # gather_team misses the 10s truth bound even unloaded; scale by load.
    try:
        load = os.getloadavg()[0] / (os.cpu_count() or 1)
        team_wait_s = min(45.0, _LEAD_TRUTH_WAIT_S * max(1.0, load / 2.0))
    except (AttributeError, OSError):
        team_wait_s = _LEAD_TRUTH_WAIT_S
    outcome, team_cut = _setup_bounded("team", _promoted, team_fn, rows_fn, cap=team_wait_s)
    targets, note = outcome or ([], "team read did not complete in its slice bound")
    summary: dict[str, Any] = {
        "armed": True,
        "roles": len(targets),
        "woke": [],
        "refused": [],
        "truth_reads": 0,
        "evaluated": 0,
        "note": note,
        "team_incomplete": team_cut,
    }
    # One question-journal read per tick, shared by every scope like `entries`.
    try:
        answered, _answers_cut = _setup_bounded("answers", answered_fn)
        answered_records: list = answered or []
    except Exception:  # noqa: BLE001 - an unreadable journal is not a trigger
        answered_records = []

    # Clock-keyed rotation, no progress kept: the offset advances one role per window.
    offset = int(now.timestamp() // max(1, debounce_s)) % len(targets) if targets else 0
    for target in targets[offset:] + targets[:offset]:
        sidecar = _read_sidecar(target)
        reason: Optional[str] = None
        wake_address: Optional[str] = None
        wake_detail: Optional[str] = None
        answered_cursor_to_store = ""
        # Triggers are evaluated read-only, cheapest first: the truth
        # read costs seconds per role and a quiet role can never wake, so it
        # runs only for a trigger or a pending first-observation seed.
        pending_answer_seed = "answered_cursor" not in sidecar
        if not pending_answer_seed:
            answer_prompt, matched_ts = _escalation_answer_trigger(
                target, answered_records, str(sidecar.get("answered_cursor") or "")
            )
            if answer_prompt is not None:
                reason = "escalation_answered"
                # Delivery mail addressed the full session id: name it or the
                # row lingers unread (no scan covers that address).
                from fno.lead.state import parse_manifest

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
        if reason is None:
            _step("mail")
            matched = _mail_trigger(target, unread_fn)
            if matched is not None:
                reason = "mail"
                wake_address = matched
        fresh_board_hash: Optional[str] = None
        fresh_board_rows: Optional[list] = None
        first_observation = False
        if reason is None:
            if entries is None:
                left = seconds_left_fn() if seconds_left_fn is not None else None
                if left is not None and left < _LEAD_STEP_FLOOR_S and not graph_cut:
                    return _budget_stop("graph")
                _step("graph")
                try:
                    entries = graph_future.result(timeout=0 if graph_cut else _wait_cap(left))
                except TimeoutError:
                    if not graph_cut:
                        graph_cut = True
                        _note("graph read timed out; later roles poll the same read")
            # One compile feeds both lanes; None rows (empty or uncompilable
            # scope) is no signal for either.
            _step("board")
            rows = _board_rows(target.scope, entries, scope_resolver) if entries else None
            changed, fresh_board_hash, fresh_board_rows, wake_detail, first_observation = (
                _board_trigger(target, rows)
            )
            if changed:
                reason = "board"
            elif rows is not None and _backstop_due(
                target, entries or [], now=now, backstop_s=backstop_s, resolver=scope_resolver
            ):
                reason = "backstop"
        if reason is None:
            # Seeds record what this pass observed, so a role that cannot
            # wake never pays the read that was meant to gate them.
            if pending_answer_seed:
                _update_sidecar(target, answered_cursor=_birth_cursor(target.manifest))
            if first_observation and fresh_board_hash is not None:
                _store_board_hash(target, fresh_board_hash, fresh_board_rows or ())
            summary["evaluated"] += 1
            continue
        left = seconds_left_fn() if seconds_left_fn is not None else None
        if left is not None and left < _LEAD_STEP_FLOOR_S:
            return _budget_stop(f"truth:{target.scope}")
        _step(f"truth:{target.scope}")
        truth, truth_timed_out = _bounded(truth_fn, target.holder, wait_s=_wait_cap(left))
        if truth_timed_out:
            # Yields to the rest; triggers stay armed, so the next rotation retries it first.
            summary["refused"].append({"scope": target.scope, "refusal": "truth-timeout"})
            continue
        summary["truth_reads"] += 1
        refusal = _holder_absent(truth)
        if refusal is not None:
            # Liveness refusals ride the summary, not the event stream.
            summary["refused"].append({"scope": target.scope, "refusal": refusal})
            summary["evaluated"] += 1
            continue
        # A GONE holder is replaced, not woken: the dispatch bills the
        # respawn budget, not only the wake ledger.
        holder_gone = truth.get("state") == "unknown" and truth.get("reason") == "not-found"
        # Seeds land only for a holder that is present, exactly as when the
        # truth read came first: the write set is unchanged.
        if pending_answer_seed:
            # Seed at birth, never the journal max: a max seed swallows an
            # answer closed before the first armed tick saw it.
            _update_sidecar(target, answered_cursor=_birth_cursor(target.manifest))
        if first_observation and fresh_board_hash is not None:
            _store_board_hash(target, fresh_board_hash, fresh_board_rows or ())
        if holder_gone:
            from fno.lead.state import at_respawn_ceiling, parse_manifest, respawn_ceiling

            if at_respawn_ceiling(target.manifest):
                fields = parse_manifest(target.manifest)
                emit(
                    "lead_wake_refused",
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
                summary["evaluated"] += 1
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
                "lead_wake_refused",
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
            summary["evaluated"] += 1
            continue
        window_count = verdict.count
        if dispatch_fn is not None:
            dispatch_fn(target, reason, wake_address, wake_detail, holder_gone)
            spawned = True
        else:
            import shutil

            spawned = _dispatch_walk(
                target,
                reason,
                shutil.which("fno-agents") or "fno-agents",
                wake_address,
                wake_detail,
                holder_gone,
            )
            if not spawned:
                # A refused spawn still spent a wake bill; the feed must say
                # why nothing launched, not leave it in the wake log alone.
                refusal = "manifest-carries-no-model-pin"
                emit(
                    "lead_wake_refused",
                    {
                        "scope": target.scope,
                        "refusal": refusal,
                        "reason": reason,
                        "window_count": window_count,
                        "ceiling": ceiling,
                    },
                )
                summary["refused"].append({"scope": target.scope, "refusal": refusal})
        if holder_gone and spawned:
            # The new session does not exist yet; its trail is the walk's journal.
            from fno.lead.state import parse_manifest as _pm

            emit(
                "lead_spawned_successor",
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
        emit("lead_woken", {**receipt, "window_count": window_count, "ceiling": ceiling})
        summary["woke"].append(receipt)
        summary["evaluated"] += 1
    graph_pool.shutdown(wait=False)
    return summary

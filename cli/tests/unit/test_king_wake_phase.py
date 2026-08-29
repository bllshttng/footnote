"""The pr-watch tick's king wake phase: triggers, liveness, gate, receipts.

Every assertion names a positive marker: a dispatched walk with its reason, an
emitted event naming its word, a billed ledger line. "Nothing spawned" is only
ever asserted beside a same-run positive that proves the phase actually ran.
"""
from __future__ import annotations

from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import SimpleNamespace

from fno.king.state import write_manifest
from fno.king.wake import bill_wake
from fno.pr_watch._king_wake import (
    CrownTarget,
    _holder_absent,
    _raise_ceiling_question,
    run_king_wake,
)

NOW = datetime(2026, 8, 29, 12, 0, 0, tzinfo=timezone.utc)


def _settings(*, armed: bool = True) -> SimpleNamespace:
    return SimpleNamespace(
        king=SimpleNamespace(
            wake_enabled=armed,
            wake_ceiling=32,
            wake_debounce_seconds=900,
            wake_backstop_seconds=1800,
        )
    )


class _Recorder:
    def __init__(self) -> None:
        self.events: list[tuple[str, dict]] = []
        self.dispatches: list[tuple[str, str]] = []
        self.asks: list[tuple[str, int, int]] = []
        self.unread_calls: list[str] = []

    def emit(self, event_type: str, data: dict) -> None:
        self.events.append((event_type, data))

    def dispatch(self, target: CrownTarget, reason: str) -> None:
        self.dispatches.append((target.scope, reason))


def _court(crowns):
    return lambda rows: {"crowns": crowns, "conflicts": []}


def _rows(root, holder="king-x", status="live", short_id="aa11bb22"):
    return lambda: [
        SimpleNamespace(
            name=holder, cwd=str(root), status=status, short_id=short_id
        )
    ]


def _run(
    tmp_path,
    *,
    truth=None,
    unread=None,
    armed=True,
    pre=None,
    extra=None,
):
    rec = _Recorder()
    root = tmp_path / "proj"
    root.mkdir(exist_ok=True)
    manifest = root / ".fno" / "kings" / "epic-x.md"
    write_manifest(
        manifest,
        scope="epic-x",
        harness_session_id="11111111-2222-3333-4444-555555555555",
        force=True,
    )
    if pre is not None:
        pre(manifest)
    crowns = [{"holder": "king-x", "scope": "epic-x", "status": "live"}]

    def unread_fn(address):
        rec.unread_calls.append(address)
        return unread(address) if unread else []

    kwargs = dict(
        emit=rec.emit,
        now=NOW,
        court_fn=_court(crowns),
        rows_fn=_rows(root),
        truth_fn=truth or (lambda holder: {"state": "done"}),
        unread_fn=unread_fn,
        dispatch_fn=rec.dispatch,
        ask_fn=lambda t, c, k: rec.asks.append((t.scope, c, k)),
    )
    if extra:
        kwargs.update(extra)
    summary = run_king_wake(_settings(armed=armed), **kwargs)
    return rec, summary, manifest


def test_absent_holder_with_undrained_mail_wakes_and_bills(tmp_path):
    rec, summary, manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda address: [object()] if address == "king-x" else [],
    )

    assert rec.dispatches == [("epic-x", "mail")], f"woke: {rec.dispatches}"
    woken = [e for e in rec.events if e[0] == "king_woken"]
    assert woken and woken[0][1]["reason"] == "mail"
    assert woken[0][1]["scope"] == "epic-x"
    # The bill landed BEFORE the dispatch: the ledger names the wake.
    text = manifest.read_text(encoding="utf-8")
    assert "wake_times: " in text and NOW.strftime("%Y-%m-%dT%H:%M:%SZ") in text


def test_mail_addressed_to_the_reply_handle_short_id_wakes(tmp_path):
    # Measured on the live bus: replies carry the session short id, not the
    # registry name. A name-only scan reads a permanent zero for them.
    rec, _summary, _manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda address: [object()] if address == "aa11bb22" else [],
    )

    assert rec.dispatches == [("epic-x", "mail")], f"addresses: {rec.unread_calls}"


def test_project_broadcast_address_wakes_an_absent_holder(tmp_path):
    # A to_kind=project broadcast carries to == <project>; the scope's project
    # member must reach the king through that address too, not only the name.
    rec, _summary, _manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda address: [object()] if address == "epic-x" else [],
    )

    assert rec.dispatches == [("epic-x", "mail")]


def test_working_stalled_and_broken_instrument_holders_never_wake(tmp_path):
    for n, truth in enumerate(
        (
            {"state": "working"},
            {"state": "watching"},
            {"state": "your-move"},
            {"state": "stalled"},
            {"state": "unknown", "reason": "no-records"},
            {"state": "unknown", "reason": "resolver-error"},
        )
    ):
        case = tmp_path / f"case-{n}"
        case.mkdir()
        rec, summary, _ = _run(
            case,
            truth=lambda h, t=truth: t,
            unread=lambda address: [object()],
        )
        assert rec.dispatches == [], f"{truth} spawned a king"
        assert summary["refused"] == [
            {"scope": "epic-x", "refusal": _holder_absent(truth)}
        ], f"{truth} must name its state in the refusal"


def test_unknown_not_found_is_absence_and_wakes(tmp_path):
    rec, _summary, _ = _run(
        tmp_path,
        truth=lambda h: {"state": "unknown", "reason": "not-found"},
        unread=lambda address: [object()],
    )

    assert rec.dispatches == [("epic-x", "mail")]


def test_unarmed_phase_reads_no_bus_and_emits_nothing(tmp_path):
    rec, summary, _ = _run(tmp_path, armed=False, unread=lambda a: [object()])

    assert summary == {"armed": False}
    assert rec.unread_calls == [], "an unarmed tick must not read the bus"
    assert rec.events == []
    assert rec.dispatches == []


def test_a_fresh_wake_is_debounced_and_bills_nothing(tmp_path):
    rec, _summary, manifest = _run(
        tmp_path,
        unread=lambda address: [object()],
        # Pre-bill a wake 5 minutes ago: inside the 900s debounce.
        pre=lambda m: bill_wake(m, now=NOW - timedelta(minutes=5)),
    )

    assert rec.dispatches == [], "a debounced trigger must not spawn"
    refused = [e for e in rec.events if e[0] == "king_wake_refused"]
    assert refused and refused[0][1]["refusal"] == "debounce"
    assert rec.asks == [], "a debounce refusal is routine and stays an event"
    # The refused wake was not billed: no stamp at NOW, only the 11:55 one.
    text = manifest.read_text(encoding="utf-8")
    assert "2026-08-29T12:00:00Z" not in text, "a refusal must not bill"
    assert "2026-08-29T11:55:00Z" in text, "the prior bill survives"
    # Nothing advanced any cursor: the waking mail is still undrained, so the
    # next allowed wake still carries it.


def test_the_33rd_wake_is_refused_naming_ceiling_and_asks_once(tmp_path):
    def fill(manifest):
        # 32 real bills, ages 30m..1270m at 40m spacing: all inside 24h, all
        # past the 15m debounce.
        for i in range(32):
            bill_wake(manifest, now=NOW - timedelta(minutes=30 + 40 * i))

    rec, _summary, _manifest = _run(
        tmp_path,
        unread=lambda address: [object()],
        pre=fill,
    )

    assert rec.dispatches == [], "wake 33 in the same window must not spawn"
    refused = [e for e in rec.events if e[0] == "king_wake_refused"]
    assert refused and refused[0][1]["refusal"] == "ceiling"
    assert refused[0][1]["ceiling"] == 32
    assert refused[0][1]["window_count"] == 32
    assert len(rec.asks) == 1, "the ceiling question is raised once per window"


def test_the_ceiling_question_dedupes_on_its_marker(tmp_path):
    target = CrownTarget(
        holder="king-x",
        scope="epic-x",
        root=tmp_path,
        manifest=tmp_path / ".fno" / "kings" / "epic-x.md",
    )

    first = _raise_ceiling_question(target, 32, 32)
    second = _raise_ceiling_question(target, 32, 32)

    assert first == second, "an open question must not be re-asked each tick"


def test_no_mail_and_no_trigger_spawns_nothing_but_ran(tmp_path):
    rec, summary, _ = _run(
        tmp_path,
        unread=lambda address: [],
        extra={"entries_fn": lambda: []},
    )

    assert rec.dispatches == []
    assert rec.events == []
    assert summary["crowns"] == 1, "the phase ran and considered the crown"


def test_a_conflicted_scope_is_skipped(tmp_path):
    root = tmp_path / "proj"
    root.mkdir()
    write_manifest(
        root / ".fno" / "kings" / "epic-x.md",
        scope="epic-x",
        harness_session_id="11111111-2222-3333-4444-555555555555",
    )
    rec = _Recorder()
    crowns = [
        {"holder": "king-a", "scope": "epic-x", "status": "live"},
        {"holder": "king-b", "scope": "epic-x", "status": "live"},
    ]

    summary = run_king_wake(
        _settings(),
        emit=rec.emit,
        now=NOW,
        court_fn=_court(crowns),
        rows_fn=lambda: [
            SimpleNamespace(name="king-a", cwd=str(root), status="live"),
            SimpleNamespace(name="king-b", cwd=str(root), status="live"),
        ],
        truth_fn=lambda h: {"state": "done"},
        unread_fn=lambda a: [object()],
        dispatch_fn=rec.dispatch,
        ask_fn=lambda *a: None,
    )

    assert rec.dispatches == [], "never wake into a disputed territory"
    assert "conflicting" in (summary["note"] or "")


def test_an_unreadable_registry_wakes_nothing(tmp_path):
    rec = _Recorder()
    summary = run_king_wake(
        _settings(),
        emit=rec.emit,
        now=NOW,
        court_fn=lambda rows: {"crowns": None, "summary": {"reason": "registry unreadable"}},
        rows_fn=lambda: [],
        truth_fn=lambda h: {"state": "done"},
        unread_fn=lambda a: [object()],
        dispatch_fn=rec.dispatch,
        ask_fn=lambda *a: None,
    )

    assert rec.dispatches == []
    assert rec.events == []
    assert summary["crowns"] == 0


# ── the board-change trigger ──────────────────────────────────────────────

from fno.pr_watch._king_wake import _board_hash, _store_board_hash  # noqa: E402

#: Fixture rung: a level-1 project crown over "proj", so no machine config
#: or graph shape is load-bearing in these tests.
_PROJECT_RESOLVER = lambda parts: (1, "proj")  # noqa: E731

_BOARD_A = [
    {
        "id": "x-1",
        "project": "proj",
        "status": "ready",
        "_kanban_column": "ready",
        "priority": "p1",
    }
]
#: Same row at p2: hash-visible but below the king's priorities, so these
#: fixtures isolate the board lane without the timer backstop firing on them.
_BOARD_A_QUIET = [dict(_BOARD_A[0], priority="p2")]
#: Same row, priority moved: a change the hash must see.
_BOARD_A_REPRIORITIZED = [dict(_BOARD_A[0], priority="p0")]
#: One row added: the refill case this trigger exists for.
_BOARD_B = _BOARD_A + [
    {
        "id": "x-2",
        "project": "proj",
        "status": "ready",
        "_kanban_column": "ready",
        "priority": "p1",
    }
]


class _SidecarTarget:
    """The narrow slice of CrownTarget the sidecar writer needs."""

    def __init__(self, manifest, scope="epic-x"):
        self.manifest = manifest
        self.scope = scope


def _sidecar(manifest):
    return manifest.parent / "epic-x.wake.json"


def _stored(manifest) -> str:
    import json

    return str(json.loads(_sidecar(manifest).read_text())["board_hash"])


def test_first_observation_stores_the_hash_and_wakes_nothing(tmp_path):
    rec, _summary, manifest = _run(
        tmp_path,
        unread=lambda a: [],
        extra={
            "entries_fn": lambda: _BOARD_A_QUIET,
            "scope_resolver": _PROJECT_RESOLVER,
        },
    )

    assert rec.dispatches == [], "a first observation is not a change"
    assert _stored(manifest) == _board_hash("epic-x", _BOARD_A_QUIET, _PROJECT_RESOLVER)


def test_a_changed_board_wakes_with_reason_board_and_stores_the_hash(tmp_path):
    # First pass: first observation only stores. The sidecar persists beside
    # the manifest across passes in the same root, exactly as across ticks.
    _run(
        tmp_path,
        unread=lambda a: [],
        extra={"entries_fn": lambda: _BOARD_A, "scope_resolver": _PROJECT_RESOLVER},
    )

    rec, _summary, manifest = _run(
        tmp_path,
        unread=lambda a: [],
        extra={"entries_fn": lambda: _BOARD_B, "scope_resolver": _PROJECT_RESOLVER},
    )

    assert rec.dispatches == [("epic-x", "board")], "the refill wakes"
    woken = [e for e in rec.events if e[0] == "king_woken"]
    assert woken and woken[0][1]["reason"] == "board"
    assert _stored(manifest) == _board_hash("epic-x", _BOARD_B, _PROJECT_RESOLVER)


def test_an_unchanged_board_wakes_nothing_and_keeps_the_stored_hash(tmp_path):
    def prime(manifest):
        _store_board_hash(
            _SidecarTarget(manifest),
            _board_hash("epic-x", _BOARD_A_QUIET, _PROJECT_RESOLVER),
        )

    rec, _summary, manifest = _run(
        tmp_path,
        unread=lambda a: [],
        pre=prime,
        extra={
            "entries_fn": lambda: _BOARD_A_QUIET,
            "scope_resolver": _PROJECT_RESOLVER,
        },
    )

    assert rec.dispatches == [], "no mail and no change means no wake"
    assert rec.events == []
    assert _stored(manifest) == _board_hash("epic-x", _BOARD_A_QUIET, _PROJECT_RESOLVER)


def test_a_refused_board_change_keeps_the_old_hash_so_it_stays_a_trigger(tmp_path):
    def prime(manifest):
        _store_board_hash(
            _SidecarTarget(manifest),
            _board_hash("epic-x", _BOARD_A, _PROJECT_RESOLVER),
        )
        bill_wake(manifest, now=NOW - timedelta(minutes=2))  # inside debounce

    rec, _summary, manifest = _run(
        tmp_path,
        unread=lambda a: [],
        pre=prime,
        extra={"entries_fn": lambda: _BOARD_B, "scope_resolver": _PROJECT_RESOLVER},
    )

    assert rec.dispatches == [], "the debounce refuses the spawn"
    refused = [e for e in rec.events if e[0] == "king_wake_refused"]
    assert refused and refused[0][1]["reason"] == "board"
    assert _stored(manifest) == _board_hash("epic-x", _BOARD_A, _PROJECT_RESOLVER), (
        "a refused change must not consume the trigger"
    )


def test_a_priority_move_alone_counts_as_a_board_change(tmp_path):
    def prime(manifest):
        _store_board_hash(
            _SidecarTarget(manifest),
            _board_hash("epic-x", _BOARD_A, _PROJECT_RESOLVER),
        )

    rec, _summary, _manifest = _run(
        tmp_path,
        unread=lambda a: [],
        pre=prime,
        extra={
            "entries_fn": lambda: _BOARD_A_REPRIORITIZED,
            "scope_resolver": _PROJECT_RESOLVER,
        },
    )

    assert rec.dispatches == [("epic-x", "board")]


def test_an_empty_graph_read_is_not_a_board_emptied(tmp_path):
    rec, _summary, _manifest = _run(
        tmp_path,
        unread=lambda a: [],
        extra={"entries_fn": lambda: [], "scope_resolver": _PROJECT_RESOLVER},
    )

    assert rec.dispatches == []
    assert rec.events == []


# ── the timer backstop ────────────────────────────────────────────────────

_BOARD_QUIET = [
    {
        "id": "x-1",
        "project": "proj",
        "status": "done",
        "_kanban_column": "done",
        "priority": "p1",
    }
]


def _primed_unchanged(manifest):
    """Store the current board's hash: mail empty, board unchanged."""
    _store_board_hash(
        _SidecarTarget(manifest), _board_hash("epic-x", _BOARD_A, _PROJECT_RESOLVER)
    )


def test_the_backstop_fires_when_no_event_did(tmp_path):
    rec, _summary, _manifest = _run(
        tmp_path,
        unread=lambda a: [],
        pre=_primed_unchanged,
        extra={"entries_fn": lambda: _BOARD_A, "scope_resolver": _PROJECT_RESOLVER},
    )

    assert rec.dispatches == [("epic-x", "backstop")], (
        "actionable work, no event, no recent wake: the re-check fires"
    )
    woken = [e for e in rec.events if e[0] == "king_woken"]
    assert woken and woken[0][1]["reason"] == "backstop"


def test_the_backstop_waits_out_its_window(tmp_path):
    def prime(manifest):
        _primed_unchanged(manifest)
        bill_wake(manifest, now=NOW - timedelta(minutes=10))

    rec, _summary, _manifest = _run(
        tmp_path,
        unread=lambda a: [],
        pre=prime,
        extra={"entries_fn": lambda: _BOARD_A, "scope_resolver": _PROJECT_RESOLVER},
    )

    assert rec.dispatches == [], "a wake 10m ago is inside the 1800s window"
    assert rec.events == []


def test_the_backstop_skips_a_scope_with_nothing_actionable(tmp_path):
    def prime(manifest):
        _store_board_hash(
            _SidecarTarget(manifest), _board_hash("epic-x", _BOARD_QUIET, _PROJECT_RESOLVER)
        )

    rec, _summary, _manifest = _run(
        tmp_path,
        unread=lambda a: [],
        pre=prime,
        extra={"entries_fn": lambda: _BOARD_QUIET, "scope_resolver": _PROJECT_RESOLVER},
    )

    assert rec.dispatches == [], "a quiet board is a NoWork king, not a wake"
    assert rec.events == []

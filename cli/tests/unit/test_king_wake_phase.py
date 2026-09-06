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
    _board_rows,
    _holder_absent,
    _raise_ceiling_question,
    _store_board_hash,
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
        self.dispatches: list[tuple[str, str, str | None, str | None]] = []
        self.asks: list[tuple[str, int, int]] = []
        self.unread_calls: list[str] = []

    def emit(self, event_type: str, data: dict) -> None:
        self.events.append((event_type, data))

    def dispatch(
        self,
        target: CrownTarget,
        reason: str,
        address: str | None,
        detail: str | None = None,
    ) -> None:
        self.dispatches.append((target.scope, reason, address, detail))

def _king_manifest(root):
    """The manifest path as the wake phase resolves it: the repo's space,
    canonical-keyed (the seed must land where the reader looks)."""
    from fno.king.state import king_manifest_path, king_state_root

    return king_manifest_path("epic-x", state_root=king_state_root(root))



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
    manifest = _king_manifest(root)
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

    assert rec.dispatches == [("epic-x", "mail", "king-x", None)], f"woke: {rec.dispatches}"
    woken = [e for e in rec.events if e[0] == "king_woken"]
    assert woken and woken[0][1]["reason"] == "mail"
    assert woken[0][1]["address"] == "king-x"
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

    assert rec.dispatches == [("epic-x", "mail", "aa11bb22", None)], (
        f"addresses: {rec.unread_calls}"
    )


def test_project_broadcast_address_wakes_an_absent_holder(tmp_path):
    # A to_kind=project broadcast carries to == <project>; the scope's project
    # member must reach the king through that address too, not only the name.
    rec, _summary, _manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda address: [object()] if address == "epic-x" else [],
    )

    assert rec.dispatches == [("epic-x", "mail", "epic-x", None)]


def test_the_spawned_walk_argv_carries_the_matched_address(monkeypatch, tmp_path):
    # Every dispatch_fn-injected test stops one call short of the real spawn;
    # this pins the argv the walk process actually receives. The address must
    # travel on the command line: the woken session is fresh and can derive
    # neither the dead holder's name nor its reply-handle short id from any
    # whoami of its own.
    import subprocess as subprocess_mod

    from fno.pr_watch import _king_wake as phase_mod

    argv: list[str] = []

    class _FakePopen:
        def __init__(self, args, **kwargs):
            argv.extend(args)

    monkeypatch.setattr(subprocess_mod, "Popen", _FakePopen)
    target = CrownTarget(
        holder="king-x",
        scope="epic-x",
        root=tmp_path,
        manifest=_king_manifest(tmp_path),
        short_id="aa11bb22",
    )
    target.manifest.parent.mkdir(parents=True, exist_ok=True)
    target.manifest.write_text(
        "---\nfno_id: k-1\nscope: epic-x\n---\n", encoding="utf-8"
    )

    phase_mod._dispatch_walk(target, "mail", "fno-agents", "aa11bb22")

    assert argv[argv.index("--wake-address") + 1] == "aa11bb22"
    assert argv[argv.index("--wake-reason") + 1] == "mail"
    assert argv[argv.index("--wake-holder") + 1] == "king-x"


def test_king_wake_permission_mode_the_woken_session_argv_carries_bypass():
    # The operator's 2026-08-23 report (wakes landing on an approve prompt)
    # named a resume that repeated no permission mode. Measured 2026-09-05:
    # the wake path holds no resume to fix - it dispatches the walk, and the
    # session the walk launches runs THIS driver's argv, which hardcodes full
    # bypass (a mode every spawn recording subsumes). The pin is the one edit
    # that could bring the report back: dropping the flag from driver_invoke.
    repo = Path(__file__).resolve().parents[3]
    driver = repo / "scripts" / "lib" / "driver-claude-code.sh"
    invoke = driver.read_text(encoding="utf-8").split("driver_invoke()", 1)[1]
    assert "--dangerously-skip-permissions" in invoke


# ── the escalation-answer trigger ──────────────────────────────────────────


def _answered(asker, answer="ship it", closed_ts="2026-08-29T11:00:00Z", qid="q-ab12cd34"):
    from types import SimpleNamespace as _NS

    return _NS(
        id=qid,
        asker=asker,
        question="what does the operator want for epic-x?",
        answer=answer,
        closed_ts=closed_ts,
    )


def _seed_cursor(manifest, cursor="2026-08-29T10:00:00Z"):
    import json as _json

    _sidecar(manifest).write_text(
        _json.dumps({"answered_cursor": cursor}), encoding="utf-8"
    )


def test_an_answered_king_escalation_wakes_with_the_answer_as_the_prompt(tmp_path):
    # The acceptance: a parked crowned holder that asked q-X, cleared with an
    # answer, wakes on the next tick with the answer as the prompt body.
    rec, _summary, manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda a: [],
        pre=_seed_cursor,
        extra={
            "answered_fn": lambda root: [
                _answered(asker=None, qid="q-old"),  # unattributable: never a trigger
                _answered(asker="aa11bb22"),
            ]
        },
    )

    assert rec.dispatches and rec.dispatches[0][:3] == ("epic-x", "escalation_answered", None)
    detail = rec.dispatches[0][3]
    assert "q-ab12cd34" in detail and "ship it" in detail, detail
    woken = [e for e in rec.events if e[0] == "king_woken"]
    assert woken and woken[0][1]["reason"] == "escalation_answered"
    import json as _json

    payload = _json.loads(_sidecar(manifest).read_text(encoding="utf-8"))
    assert payload["answered_cursor"] == "2026-08-29T11:00:00Z"


def test_the_answer_delivery_address_is_invisible_to_the_mail_trigger(tmp_path):
    # Why this trigger exists: the answer's mail delivery addresses the
    # holder's FULL session id (outstanding/deliver.py), and the mail scan
    # covers name, short id, and scope projects only - so the mail trigger
    # reads a permanent zero on an answer while the question journal fires.
    full_id = "11111111-2222-3333-4444-555555555555"
    rec, _summary, _manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda address: [object()] if address == full_id else [],
        pre=_seed_cursor,
        extra={"answered_fn": lambda root: []},
    )

    assert full_id not in rec.unread_calls, "the full id is not a scanned address"
    assert rec.dispatches == [], "no mail spelling matched, nothing wakes"


def test_the_answer_trigger_fires_despite_the_debounce(tmp_path):
    # "Ahead of the debounce, since the king asked for it": a wake billed two
    # minutes ago still refuses mail, board, and backstop, but not an answer.
    def prime(manifest):
        _seed_cursor(manifest)
        bill_wake(manifest, now=NOW - timedelta(minutes=2))

    rec, _summary, _manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda a: [object()],
        pre=prime,
        extra={"answered_fn": lambda root: [_answered(asker="king-x")]},
    )

    assert rec.dispatches and rec.dispatches[0][1] == "escalation_answered"
    assert rec.dispatches[0][1] != "mail", "mail must stay debounced"


def test_a_fresh_arm_seeds_the_answer_cursor_without_waking(tmp_path):
    rec, _summary, manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda a: [],
        extra={"answered_fn": lambda root: [_answered(asker="king-x")]},
    )

    assert rec.dispatches == [], "a first observation is not a trigger"
    import json as _json

    payload = _json.loads(_sidecar(manifest).read_text(encoding="utf-8"))
    assert payload["answered_cursor"] == "2026-08-29T11:00:00Z"


def test_an_answer_to_another_asker_wakes_nothing(tmp_path):
    rec, _summary, _manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda a: [],
        pre=_seed_cursor,
        extra={"answered_fn": lambda root: [_answered(asker="somebody-else")]},
    )

    assert rec.dispatches == []
    assert [e for e in rec.events if e[0] == "king_woken"] == []


def test_a_ceiling_refused_answer_keeps_the_cursor_so_it_stays_a_trigger(tmp_path):
    # The debounce cannot refuse an answer (the king asked for it), so the
    # one refusal left is the ceiling - and it must not consume the answer.
    def prime(manifest):
        _seed_cursor(manifest)
        for _ in range(32):
            bill_wake(manifest, now=NOW - timedelta(hours=1))

    rec, _summary, manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda a: [],
        pre=prime,
        extra={"answered_fn": lambda root: [_answered(asker="king-x")]},
    )

    assert rec.dispatches == [], "the ceiling refuses the 33rd wake"
    refused = [e for e in rec.events if e[0] == "king_wake_refused"]
    assert refused and refused[0][1]["reason"] == "escalation_answered"
    import json as _json

    payload = _json.loads(_sidecar(manifest).read_text(encoding="utf-8"))
    assert payload["answered_cursor"] == "2026-08-29T10:00:00Z", (
        "a refused answer must stay a trigger"
    )

    rec, _summary, _manifest = _run(
        tmp_path,
        truth=lambda h: {"state": "done"},
        unread=lambda a: [],
        extra={
            "answered_fn": lambda root: [_answered(asker="king-x")],
            "now": NOW + timedelta(hours=25),  # the window rolled; the answer waits
        },
    )

    assert rec.dispatches and rec.dispatches[0][1] == "escalation_answered"


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

    assert rec.dispatches == [("epic-x", "mail", "aa11bb22", None)]


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
        manifest=_king_manifest(tmp_path),
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
        _king_manifest(root),
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
#: Two rows added: the acceptance case for the diff-as-prompt change.
_BOARD_C = _BOARD_A + [
    {
        "id": "x-2",
        "project": "proj",
        "status": "ready",
        "_kanban_column": "ready",
        "priority": "p1",
    },
    {
        "id": "x-3",
        "project": "proj",
        "status": "ready",
        "_kanban_column": "ready",
        "priority": "p1",
    },
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

    assert ("epic-x", "board", None) == rec.dispatches[0][:3], "the refill wakes"
    woken = [e for e in rec.events if e[0] == "king_woken"]
    assert woken and woken[0][1]["reason"] == "board"
    assert _stored(manifest) == _board_hash("epic-x", _BOARD_B, _PROJECT_RESOLVER)


def test_the_board_diff_names_the_two_added_rows_and_nothing_unchanged(tmp_path):
    # Acceptance 5.2: a board that gained two rows wakes with a prompt that
    # names those two and omits the unchanged row. The woken session is
    # fresh; without the diff on the command line it re-reads the whole board.
    _run(
        tmp_path,
        unread=lambda a: [],
        extra={"entries_fn": lambda: _BOARD_A, "scope_resolver": _PROJECT_RESOLVER},
    )

    rec, _summary, _manifest = _run(
        tmp_path,
        unread=lambda a: [],
        extra={"entries_fn": lambda: _BOARD_C, "scope_resolver": _PROJECT_RESOLVER},
    )

    assert rec.dispatches and rec.dispatches[0][3], "the diff is the wake payload"
    detail = rec.dispatches[0][3]
    assert "added: x-2" in detail and "added: x-3" in detail, detail
    assert "x-1" not in detail, f"an unchanged row is noise: {detail}"


def test_a_refused_board_change_keeps_the_old_rows_so_the_diff_survives(tmp_path):
    def prime(manifest):
        _store_board_hash(
            _SidecarTarget(manifest),
            _board_hash("epic-x", _BOARD_A, _PROJECT_RESOLVER),
            _board_rows("epic-x", _BOARD_A, _PROJECT_RESOLVER),
        )
        bill_wake(manifest, now=NOW - timedelta(minutes=2))  # inside debounce

    _run(
        tmp_path,
        unread=lambda a: [],
        pre=prime,
        extra={"entries_fn": lambda: _BOARD_B, "scope_resolver": _PROJECT_RESOLVER},
    )

    rec, _summary, _manifest = _run(
        tmp_path,
        unread=lambda a: [],
        # outside the debounce now, so the retry dispatches
        extra={
            "entries_fn": lambda: _BOARD_B,
            "scope_resolver": _PROJECT_RESOLVER,
            "now": NOW + timedelta(minutes=20),
        },
    )

    assert rec.dispatches, "the retry wakes once the debounce lapses"
    assert "added: x-2" in (rec.dispatches[0][3] or ""), rec.dispatches[0][3]


def test_a_sidecar_from_before_rows_were_stored_is_a_first_observation(tmp_path):
    # A legacy sidecar carries a hash with no rows beside it. An honest diff
    # needs the prior rows, so that one transition records them and wakes
    # nothing rather than naming every row "added". The rows sit at p2 so the
    # backstop lane stays out of the case (quiet priority, no fresh terminal).
    quiet_b = _BOARD_A_QUIET + [
        {"id": "x-2", "project": "proj", "status": "ready", "_kanban_column": "ready", "priority": "p2"}
    ]

    def prime(manifest):
        _store_board_hash(
            _SidecarTarget(manifest),
            _board_hash("epic-x", _BOARD_A_QUIET, _PROJECT_RESOLVER),
        )

    rec, _summary, manifest = _run(
        tmp_path,
        unread=lambda a: [],
        pre=prime,
        extra={"entries_fn": lambda: quiet_b, "scope_resolver": _PROJECT_RESOLVER},
    )

    assert rec.dispatches == [], "no honest diff, no board wake"
    import json as json_mod

    payload = json_mod.loads(_sidecar(manifest).read_text(encoding="utf-8"))
    assert payload["board_rows"], "the pass records the rows it could not diff"


def test_the_spawned_walk_argv_carries_the_board_diff(monkeypatch, tmp_path):
    import subprocess as subprocess_mod

    from fno.pr_watch import _king_wake as phase_mod

    argv: list[str] = []

    class _FakePopen:
        def __init__(self, args, **kwargs):
            argv.extend(args)

    monkeypatch.setattr(subprocess_mod, "Popen", _FakePopen)
    target = CrownTarget(
        holder="king-x",
        scope="epic-x",
        root=tmp_path,
        manifest=_king_manifest(tmp_path),
        short_id="aa11bb22",
    )
    target.manifest.parent.mkdir(parents=True, exist_ok=True)
    target.manifest.write_text("---\nfno_id: k-1\nscope: epic-x\n---\n", encoding="utf-8")

    phase_mod._dispatch_walk(
        target, "board", "fno-agents", None, "added: x-2 (ready/p1)"
    )

    assert argv[argv.index("--wake-detail") + 1] == "added: x-2 (ready/p1)"


def test_render_board_diff_covers_removed_changed_and_caps():
    from fno.king.wake import MAX_DETAIL_CHARS, render_board_diff

    old = [("x-1", "ready", "ready", "p1")]
    new = [("x-1", "next", "next", "p0"), ("x-2", "ready", "ready", "p1")]
    text = render_board_diff(old, new)
    assert "changed: x-1" in text and "ready/ready/p1 -> next/next/p0" in text, text
    assert "added: x-2" in text, text

    removed = render_board_diff(old + [("x-9", "done", "done", "p2")], old)
    assert "removed: x-9" in removed, removed
    assert render_board_diff(old, old) == ""

    wide = render_board_diff([], [(f"x-{n}", "ready", "ready", "p1") for n in range(500)])
    assert len(wide) <= MAX_DETAIL_CHARS + 32, len(wide)
    assert "more rows elided" in wide, "a capped diff names what it cut"


def test_an_unchanged_board_wakes_nothing_and_keeps_the_stored_hash(tmp_path):
    def prime(manifest):
        _store_board_hash(
            _SidecarTarget(manifest),
            _board_hash("epic-x", _BOARD_A_QUIET, _PROJECT_RESOLVER),
            _board_rows("epic-x", _BOARD_A_QUIET, _PROJECT_RESOLVER),
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
            _board_rows("epic-x", _BOARD_A, _PROJECT_RESOLVER),
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
            _board_rows("epic-x", _BOARD_A, _PROJECT_RESOLVER),
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

    assert [d[:3] for d in rec.dispatches] == [("epic-x", "board", None)]


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

    assert rec.dispatches == [("epic-x", "backstop", None, None)], (
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


def test_an_unsafe_crown_scope_is_skipped_not_joined(tmp_path):
    # A corrupted registry row can carry a traversal or an absolute path as
    # its crown_scope. The phase must skip the scope, not build a manifest
    # path outside .fno/kings that the ledger would rewrite.
    from fno.pr_watch._king_wake import _crowned

    for bad_scope in ("../evil", "/tmp/absolute", "a/b"):
        crowns = [{"holder": "king-x", "scope": bad_scope, "status": "live"}]
        targets, _note = _crowned(
            _court(crowns),
            lambda: [SimpleNamespace(name="king-x", cwd=str(tmp_path), status="live")],
        )
        assert targets == [], f"{bad_scope!r} must not become a target"


def test_a_configured_ceiling_of_zero_resolves_unbounded(tmp_path):
    # `or 32` would coerce the operator's explicit 0 back to the default and
    # silently refuse the unbounded spelling. 33 stamps - past the default
    # ceiling - must still wake.
    def fill(manifest):
        for i in range(33):
            bill_wake(manifest, now=NOW - timedelta(minutes=20 + 40 * i))

    settings = SimpleNamespace(
        king=SimpleNamespace(
            wake_enabled=True,
            wake_ceiling=0,
            wake_debounce_seconds=900,
            wake_backstop_seconds=1800,
        )
    )
    rec = _Recorder()
    root = tmp_path / "proj"
    root.mkdir()
    write_manifest(
        _king_manifest(root),
        scope="epic-x",
        harness_session_id="11111111-2222-3333-4444-555555555555",
    )
    fill(_king_manifest(root))

    summary = run_king_wake(
        settings,
        emit=rec.emit,
        now=NOW,
        court_fn=_court([{"holder": "king-x", "scope": "epic-x", "status": "live"}]),
        rows_fn=_rows(root),
        truth_fn=lambda h: {"state": "done"},
        unread_fn=lambda a: [object()],
        entries_fn=lambda: [],
        dispatch_fn=rec.dispatch,
        ask_fn=lambda *a: None,
    )

    assert rec.dispatches == [("epic-x", "mail", "aa11bb22", None)], "ceiling 0 is unbounded"


def test_a_recent_king_terminal_suppresses_the_backstop(tmp_path):
    import json as _json

    def prime(manifest):
        _primed_unchanged(manifest)
        # A king walk terminated 10 minutes ago: the journal answers "a walk
        # ran recently", so the proxy's over-count must not re-fire it.
        events = manifest.parent.parent / "events.jsonl"
        events.write_text(
            _json.dumps(
                {
                    "ts": "2026-08-29T11:50:00Z",
                    "type": "loop_terminated",
                    "source": "walk",
                    "data": {"driver": "king", "reason": "NoWork", "scope": "epic-x"},
                }
            )
            + "\n",
            encoding="utf-8",
        )

    rec, _summary, _manifest = _run(
        tmp_path,
        unread=lambda a: [],
        pre=prime,
        extra={"entries_fn": lambda: _BOARD_A, "scope_resolver": _PROJECT_RESOLVER},
    )

    assert rec.dispatches == [], "a walk that answered inside the window suffices"
    assert rec.events == []


# ── the drain loop on the real bus machinery ──────────────────────────────
#
# The seams above prove the phase logic; this proves the loop on the real
# bus, cursor, and ledger: a message lands for an absent king, the phase
# wakes, the respawned session's drain verb advances the cursor, and the
# next phase pass finds no mail and wakes nothing. No fake readers.


def test_the_wake_fires_on_a_real_bus_row_and_drains_by_cursor(tmp_path, monkeypatch):
    from fno.bus.cursor import advance_cursor, read_cursor, scan_unread
    from fno.bus.log import Envelope, append

    monkeypatch.setenv("FNO_BUS_DIR", str(tmp_path / "bus"))
    rec = _Recorder()
    root = tmp_path / "proj"
    root.mkdir()
    manifest = _king_manifest(root)
    write_manifest(
        manifest,
        scope="epic-x",
        harness_session_id="11111111-2222-3333-4444-555555555555",
        force=True,
    )
    crowns = [{"holder": "king-x", "scope": "epic-x", "status": "live"}]

    # A worker mails the absent king: addressed to its reply handle.
    waking = Envelope.new(
        from_="worker-a",
        to="aa11bb22",
        kind="note",
        body="merge landed for x-1, board refilled",
        to_kind="name",
    )
    append(waking)

    # Positive control inside the same run: the reader sees the row.
    assert len(scan_unread("aa11bb22")) == 1, "the reader must see the waking row"

    summary = run_king_wake(
        _settings(),
        emit=rec.emit,
        now=datetime(2026, 8, 29, 12, 0, 0, tzinfo=timezone.utc),
        court_fn=_court(crowns),
        rows_fn=_rows(root),
        truth_fn=lambda h: {"state": "done"},
        unread_fn=scan_unread,
        entries_fn=lambda: [],
        dispatch_fn=rec.dispatch,
        ask_fn=lambda *a: None,
    )

    assert rec.dispatches == [("epic-x", "mail", "aa11bb22", None)], f"woke: {rec.dispatches}"

    # The respawned session drains: the ack verb advances the cursor.
    assert advance_cursor("aa11bb22", waking.id) is True
    assert read_cursor("aa11bb22") == waking.id, "the cursor names the waking id"

    # Next tick: no undrained mail, no board, nothing actionable -> no wake.
    rec2 = _Recorder()
    run_king_wake(
        _settings(),
        emit=rec2.emit,
        now=datetime(2026, 8, 29, 12, 30, 0, tzinfo=timezone.utc),
        court_fn=_court(crowns),
        rows_fn=_rows(root),
        truth_fn=lambda h: {"state": "done"},
        unread_fn=scan_unread,
        entries_fn=lambda: [],
        dispatch_fn=rec2.dispatch,
        ask_fn=lambda *a: None,
    )

    assert rec2.dispatches == [], "a drained inbox must not wake again"
    assert rec2.events == []

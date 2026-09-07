"""x-c624: the SILENCE verdict and its drive-then-end apply lane.

A silent worker on an OPEN node is driven (a bounded resume, same mechanism
as WAKE), then - only past `recovery.max_nudges` drives and only with
`recovery.watchdog.end_after_drives` armed - ended and handed back to
`fno backlog advance` so the grid picks a fresh thread or harness. Never a
liveness claim (d-10a72d88): the verdict reads a quiet transcript and an
open node, not whether the session is "alive".
"""
from __future__ import annotations

from datetime import datetime, timezone
from types import SimpleNamespace

from fno.agents import watchdog
from fno.agents.watchdog import (
    Row,
    SILENCE,
    TailFacts,
    Verdict,
    verdicts,
)

NOW_1840 = datetime(2026, 8, 16, 18, 40, 0, tzinfo=timezone.utc).timestamp()


def _iso(epoch: float) -> str:
    return datetime.fromtimestamp(epoch, tz=timezone.utc).isoformat().replace(
        "+00:00", "Z"
    )


def _facts(age_min: float) -> TailFacts:
    epoch = NOW_1840 - age_min * 60
    return TailFacts((), epoch, "", None, None, ())


class _Proc:
    def __init__(self, returncode=0, stderr="", stdout=""):
        self.returncode = returncode
        self.stderr = stderr
        self.stdout = stdout


# ---------------------------------------------------------------------------
# AC1: the SILENCE verdict
# ---------------------------------------------------------------------------

def test_silence_verdict_open_node_quiet_transcript_drives():
    """AC1-HP: an open node, a transcript 20m quiet, silence_after_s=900 (15m)
    -> SILENCE, action drive."""
    row = Row("sess-1", "worker-1", "working", "x-1", "/repo")
    [v] = verdicts(
        [row],
        transcript_for=lambda sid: _facts(20),
        claim_for=lambda node: {},
        node_state_for=lambda node: {"status": "ready"},
        now_s=NOW_1840,
        silence_after_s=900,
    )
    assert v.verdict == SILENCE
    assert v.action == "drive"
    assert "open node x-1" in v.basis
    assert "20m" in v.basis


def test_silence_verdict_node_done_is_never_silenced():
    """AC1-EDGE (node half): a done node never silences, however quiet the
    transcript - retirement is a different sweep's question (x-c672)."""
    row = Row("sess-1", "worker-1", "working", "x-1", "/repo")
    [v] = verdicts(
        [row],
        transcript_for=lambda sid: _facts(20),
        claim_for=lambda node: {},
        node_state_for=lambda node: {"status": "done"},
        now_s=NOW_1840,
        silence_after_s=900,
    )
    assert v.verdict != SILENCE


def test_silence_verdict_scope_excludes_crown_operator_and_outside_root(
    monkeypatch, tmp_path,
):
    """AC1-EDGE (scope half) + AC3-EDGE: a crowned row, an operator-origin
    row, and a row whose project_root is outside every root never reach the
    silence table - a claude session from another project is absent."""
    from fno.agents import registry as registry_mod
    from fno.agents.registry import AgentEntry

    in_root = str(tmp_path)
    outside = str(tmp_path.parent / "some-other-repo")
    rows = [
        AgentEntry(
            name="ok-row", harness="claude", harness_session_id="sid-ok",
            cwd=in_root, log_path="", status="live", origin="spawn",
            crown_level=None, node="x-1", project_root=in_root,
        ),
        AgentEntry(
            name="crowned-row", harness="claude", harness_session_id="sid-crown",
            cwd=in_root, log_path="", status="live", origin="spawn",
            crown_level=1, node="x-2", project_root=in_root,
        ),
        AgentEntry(
            name="operator-row", harness="claude", harness_session_id="sid-op",
            cwd=in_root, log_path="", status="live", origin="operator",
            crown_level=None, node="x-3", project_root=in_root,
        ),
        AgentEntry(
            name="outside-row", harness="claude", harness_session_id="sid-out",
            cwd=outside, log_path="", status="live", origin="spawn",
            crown_level=None, node="x-4", project_root=outside,
        ),
    ]
    monkeypatch.setattr(registry_mod, "load_registry", lambda: rows)

    got, _warnings = watchdog.silence_rows([tmp_path])

    assert [r.name for r in got] == ["ok-row"]


def test_silence_verdict_classifies_a_codex_row(monkeypatch, tmp_path):
    """AC1-CODEX: a codex row meeting the silence conditions classifies.
    fleet_rows never sees this population; the silence lane reads the
    registry directly."""
    from fno.agents import registry as registry_mod
    from fno.agents.registry import AgentEntry

    row = AgentEntry(
        name="codex-worker", harness="codex", harness_session_id="thread-9",
        cwd=str(tmp_path), log_path="", status="live", origin="spawn",
        crown_level=None, node="x-9", project_root=str(tmp_path),
    )
    monkeypatch.setattr(registry_mod, "load_registry", lambda: [row])

    rows, _warnings = watchdog.silence_rows([tmp_path])
    [v] = verdicts(
        rows,
        transcript_for=lambda sid: _facts(20),
        claim_for=lambda node: {},
        node_state_for=lambda node: {"status": "ready"},
        now_s=NOW_1840,
        silence_after_s=900,
    )
    assert v.verdict == SILENCE
    assert v.name == "codex-worker"


# ---------------------------------------------------------------------------
# AC2: drive, then end and hand back
# ---------------------------------------------------------------------------

def _settings(*, max_nudges=3, end_after_drives=False):
    return SimpleNamespace(
        recovery=SimpleNamespace(
            max_nudges=max_nudges,
            watchdog=SimpleNamespace(end_after_drives=end_after_drives),
        )
    )


def test_apply_silence_zero_drives_wakes_once(monkeypatch):
    """AC2-HP: zero prior drives -> one resume, one watchdog_applied
    {action: drive, attempt: 1}; no stop or rm."""
    v = Verdict("sess-1", "worker-1", "working", SILENCE,
                "open node x-1, transcript quiet 20m", "drive")
    wake_calls = []

    def fake_wake(vv, *, cwd, runner):
        wake_calls.append(1)
        return "applied", "woke worker-1; message confirmed in transcript"

    monkeypatch.setattr(watchdog, "_apply_wake", fake_wake)
    monkeypatch.setattr(watchdog, "_fno", lambda: ["fno"])
    events = []
    monkeypatch.setattr(
        watchdog, "emit_event", lambda kind, data: events.append((kind, data))
    )
    run_calls = []

    def runner(argv, **kw):
        run_calls.append(argv)
        return _Proc(0)

    outcome, detail = watchdog._apply_silence(
        v, cwd="/repo", node="x-1", runner=runner,
        settings=_settings(max_nudges=3, end_after_drives=False),
        now_s=NOW_1840,
        events_reader=lambda row_id: [],
        truth_for=lambda name: {"last_activity_age_s": 20 * 60},
    )

    assert outcome == "applied", detail
    assert len(wake_calls) == 1
    assert run_calls == []  # the drive step never shells out itself
    kinds = [k for k, _ in events]
    assert kinds == ["watchdog_applied"]
    data = events[0][1]
    assert data["action"] == "drive"
    assert data["attempt"] == 1


def test_apply_silence_ends_after_drives_exhausted(monkeypatch):
    """AC2-END: max_nudges drives recorded after the row's last transcript
    write, end_after_drives true -> stop, claim release, rm, then
    watchdog_applied {action: end}, in that order."""
    v = Verdict("sess-1", "worker-1", "working", SILENCE,
                "open node x-1, transcript quiet 3h", "drive")
    monkeypatch.setattr(watchdog, "_fno", lambda: ["fno"])
    events = []
    monkeypatch.setattr(
        watchdog, "emit_event", lambda kind, data: events.append((kind, data))
    )
    run_calls = []

    def runner(argv, **kw):
        run_calls.append(argv)
        if "advance" in argv:
            return _Proc(0, stdout="advance: dispatched x-1 to a fresh thread\n")
        return _Proc(0)

    last_event_epoch = NOW_1840 - 3 * 3600
    drive_events = [{"ts": _iso(last_event_epoch + 600 * i)} for i in range(1, 4)]

    outcome, detail = watchdog._apply_silence(
        v, cwd="/repo", node="x-1", runner=runner,
        settings=_settings(max_nudges=3, end_after_drives=True),
        now_s=NOW_1840,
        events_reader=lambda row_id: drive_events,
        truth_for=lambda name: {"last_activity_age_s": 3 * 3600},
        node_state_for=lambda node: None,
    )

    assert outcome == "applied", detail
    kinds = [k for k, _ in events]
    assert kinds == ["agent_stopped", "agent_removed", "watchdog_applied"]
    end_event = events[-1][1]
    assert end_event["action"] == "end"
    assert "advance: dispatched x-1" in end_event["redispatch"]
    subcommands = [tuple(c[1:3]) for c in run_calls]
    assert subcommands == [
        ("agents", "stop"),
        ("agents", "claim"),
        ("agents", "rm"),
        ("backlog", "note"),
        ("backlog", "rank"),
        ("backlog", "advance"),
    ]


def test_apply_silence_fresh_write_resets_drive_count(monkeypatch):
    """AC2-RESET: the same three drives, but the row wrote AFTER the last
    one - the count reads zero and the row is driven, not ended."""
    v = Verdict("sess-1", "worker-1", "working", SILENCE,
                "open node x-1, transcript quiet 5m", "drive")
    monkeypatch.setattr(watchdog, "_fno", lambda: ["fno"])
    monkeypatch.setattr(
        watchdog, "_apply_wake",
        lambda vv, *, cwd, runner: ("applied", "woke worker-1; confirmed"),
    )
    events = []
    monkeypatch.setattr(
        watchdog, "emit_event", lambda kind, data: events.append((kind, data))
    )
    run_calls = []

    def runner(argv, **kw):
        run_calls.append(argv)
        return _Proc(0)

    old_epoch = NOW_1840 - 5 * 3600
    drive_events = [{"ts": _iso(old_epoch - 600 * i)} for i in range(1, 4)]

    outcome, detail = watchdog._apply_silence(
        v, cwd="/repo", node="x-1", runner=runner,
        settings=_settings(max_nudges=3, end_after_drives=True),
        now_s=NOW_1840,
        events_reader=lambda row_id: drive_events,
        # A fresh write: the row spoke 5 minutes ago, after all 3 drives.
        truth_for=lambda name: {"last_activity_age_s": 5 * 60},
    )

    assert outcome == "applied", detail
    assert run_calls == []  # driven, not stopped
    kinds = [k for k, _ in events]
    assert kinds == ["watchdog_applied"]
    assert events[0][1]["action"] == "drive"
    assert events[0][1]["attempt"] == 1


def test_apply_silence_disarmed_end_reports_only(monkeypatch):
    """AC2-EDGE: drives exhausted, end_after_drives false -> reported,
    nothing stopped."""
    v = Verdict("sess-1", "worker-1", "working", SILENCE,
                "open node x-1, transcript quiet 3h", "drive")
    monkeypatch.setattr(watchdog, "_fno", lambda: ["fno"])
    events = []
    monkeypatch.setattr(
        watchdog, "emit_event", lambda kind, data: events.append((kind, data))
    )
    run_calls = []

    def runner(argv, **kw):
        run_calls.append(argv)
        return _Proc(0)

    last_event_epoch = NOW_1840 - 3 * 3600
    drive_events = [{"ts": _iso(last_event_epoch + 600 * i)} for i in range(1, 4)]

    outcome, detail = watchdog._apply_silence(
        v, cwd="/repo", node="x-1", runner=runner,
        settings=_settings(max_nudges=3, end_after_drives=False),
        now_s=NOW_1840,
        events_reader=lambda row_id: drive_events,
        truth_for=lambda name: {"last_activity_age_s": 3 * 3600},
    )

    assert outcome == "reported"
    assert "drives exhausted, end disarmed" in detail
    assert run_calls == []
    assert events == []

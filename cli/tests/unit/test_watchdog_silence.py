"""x-c624: the SILENCE verdict and its apply lane.

A silent worker on an OPEN node is driven - a bounded resume, the same
mechanism WAKE already uses (`apply_verdict` delegates both verdicts to
`_apply_wake`). Never a liveness claim (d-10a72d88): the verdict reads a
quiet transcript and an open node, not whether the session is "alive".
Ending a row past a drive cap and handing it back through
`fno backlog advance` is a deferred follow-up, not this lane.
"""
from __future__ import annotations

from datetime import datetime, timezone

from fno.agents import watchdog
from fno.agents.watchdog import (
    Row,
    SILENCE,
    TailFacts,
    Verdict,
    verdicts,
)

NOW_1840 = datetime(2026, 8, 16, 18, 40, 0, tzinfo=timezone.utc).timestamp()


def _facts(age_min: float) -> TailFacts:
    epoch = NOW_1840 - age_min * 60
    return TailFacts((), epoch, "", None, None, ())


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
# AC2: apply delegates to the same drive mechanism as WAKE
# ---------------------------------------------------------------------------

def test_apply_silence_delegates_to_apply_wake(monkeypatch):
    """AC2: apply_verdict(SILENCE, ...) drives via _apply_wake, unchanged -
    no separate silence apply lane. Ending a row is a deferred follow-up."""
    v = Verdict("sess-1", "worker-1", "working", SILENCE,
                "open node x-1, transcript quiet 20m", "drive")
    calls = []

    def fake_wake(vv, *, cwd, runner):
        calls.append((vv, cwd))
        return "applied", "woke worker-1; message confirmed in transcript"

    monkeypatch.setattr(watchdog, "_apply_wake", fake_wake)

    outcome, detail = watchdog.apply_verdict(v, lanes="wake", cwd="/repo")

    assert outcome == "applied", detail
    assert len(calls) == 1
    assert calls[0] == (v, "/repo")

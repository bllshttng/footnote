"""Autonomous keep-going engine (x-3360): classifier + ceiling-bounded dispatch.

Covers the plan's three verification scenarios:
  1. 3 surviving carve-outs (deferred / oos-bug / other) -> exactly one think
     dispatch, one build dispatch, one file-only; each dispatch bumps the shared
     daily counter.
  2. ceiling already reached -> all filed, none dispatched, one cap line.
  3. non-fatal: a failing dispatch leaves the node filed (never lost).
"""
from __future__ import annotations

import pytest

from fno.retro.keep_going import (
    ARM_BUILD,
    ARM_FILE,
    ARM_THINK,
    OUTCOME_CAPPED,
    OUTCOME_DISPATCHED,
    OUTCOME_FAILED,
    OUTCOME_FILED,
    classify_followup,
    dispatch_followups,
    keep_going_enabled,
)
from fno.retro.land import LandResult
from fno.retro.types import KIND_CARVEOUT, Candidate


def _cand(subkind, *, kind=KIND_CARVEOUT):
    return Candidate(
        title="t", body="b", tier="node", priority="p2",
        source_pr=1, source_id="cv-1", extra={"kind": kind, "subkind": subkind},
    )


def _landed(subkind, node_id="x-1", *, kind=KIND_CARVEOUT):
    return LandResult("active", _cand(subkind, kind=kind), node_id=node_id)


# --- classifier (pure) ------------------------------------------------------


@pytest.mark.parametrize(
    "subkind,expected",
    [
        ("deferred", ARM_THINK),
        ("oos-bug", ARM_BUILD),
        ("backfill", ARM_FILE),  # unknown subkind -> safe default
        (None, ARM_FILE),
        ("", ARM_FILE),
    ],
)
def test_classify_carveout_subkind(subkind, expected):
    assert classify_followup(_cand(subkind)) == expected


def test_classify_non_carveout_is_file():
    # A review/deferred-finding candidate is never dispatch-eligible.
    assert classify_followup(_cand("deferred", kind="review")) == ARM_FILE


def test_classify_missing_extra_is_file():
    c = Candidate(title="t", body="b", tier="node", priority="p2",
                  source_pr=1, source_id="cv-1")
    assert classify_followup(c) == ARM_FILE


# --- dispatch pass ----------------------------------------------------------


class _Ceiling:
    """A hermetic shared counter standing in for spawn_think's daily counter."""

    def __init__(self, count=0, cap=20):
        self.count = count
        self.cap = cap
        self.think_calls = []
        self.build_calls = []
        self.lines = []

    # think_fn self-bumps (mirrors `fno think dispatch`); build_fn does not.
    def think(self, node_id, cwd):
        self.think_calls.append(node_id)
        self.count += 1
        return True

    def build(self, node_id, cwd):
        self.build_calls.append(node_id)
        return True

    def run(self, landed):
        return dispatch_followups(
            landed,
            echo=self.lines.append,
            count_fn=lambda: self.count,
            bump_fn=lambda: setattr(self, "count", self.count + 1),
            cap_fn=lambda _root: self.cap,
            think_fn=self.think,
            build_fn=self.build,
        )


def test_three_arms_dispatch_and_count():
    c = _Ceiling(count=0, cap=20)
    landed = [
        _landed("deferred", "x-think"),
        _landed("oos-bug", "x-build"),
        _landed("backfill", "x-file"),
    ]
    results = c.run(landed)

    assert c.think_calls == ["x-think"]
    assert c.build_calls == ["x-build"]
    # Each dispatch bumps the ONE shared counter (think self-bumps, build via
    # bump_fn); the file-only arm bumps nothing.
    assert c.count == 2
    outcomes = {r.node_id: r.outcome for r in results}
    assert outcomes == {
        "x-think": OUTCOME_DISPATCHED,
        "x-build": OUTCOME_DISPATCHED,
        "x-file": OUTCOME_FILED,
    }
    assert c.lines == []  # no cap line when under budget


def test_ceiling_reached_files_all_dispatches_none():
    c = _Ceiling(count=20, cap=20)  # already at the cap
    landed = [
        _landed("deferred", "x-think"),
        _landed("oos-bug", "x-build"),
        _landed("backfill", "x-file"),
    ]
    results = c.run(landed)

    assert c.think_calls == [] and c.build_calls == []
    assert c.count == 20  # nothing dispatched, nothing bumped
    outcomes = {r.node_id: r.outcome for r in results}
    assert outcomes["x-think"] == OUTCOME_CAPPED
    assert outcomes["x-build"] == OUTCOME_CAPPED
    assert outcomes["x-file"] == OUTCOME_FILED
    assert len(c.lines) == 1 and "cap reached" in c.lines[0]


def test_cap_zero_disables_ceiling():
    c = _Ceiling(count=999, cap=0)  # 0 => ceiling off, dispatch regardless
    results = c.run([_landed("deferred", "x-think")])
    assert c.think_calls == ["x-think"]
    assert results[0].outcome == OUTCOME_DISPATCHED


def test_failed_dispatch_leaves_node_filed():
    landed = [_landed("oos-bug", "x-build")]
    results = dispatch_followups(
        landed,
        echo=lambda _l: None,
        count_fn=lambda: 0,
        bump_fn=lambda: None,
        cap_fn=lambda _root: 20,
        think_fn=lambda n, c: True,
        build_fn=lambda n, c: False,  # spawn failed
    )
    assert results[0].outcome == OUTCOME_FAILED  # node stays filed, never lost


def test_landed_without_node_id_skipped():
    landed = [LandResult("failed", _cand("deferred"), node_id=None)]
    results = dispatch_followups(
        landed,
        count_fn=lambda: 0, bump_fn=lambda: None, cap_fn=lambda _r: 20,
        think_fn=lambda n, c: True, build_fn=lambda n, c: True,
    )
    assert results == []


# --- gate -------------------------------------------------------------------


def test_gate_env_override():
    assert keep_going_enabled(env={"FNO_KEEP_GOING": "1"}) is True
    assert keep_going_enabled(env={"FNO_KEEP_GOING": "off"}) is False
    assert keep_going_enabled(env={"FNO_KEEP_GOING": "banana"}) is False


# --- triage_pr wiring (integration) -----------------------------------------


def test_triage_pr_runs_keep_going_when_autonomous_and_enabled(tmp_path, monkeypatch):
    """A carve-out harvest in autonomous mode + FNO_KEEP_GOING=1 classifies the
    filed carve-outs and dispatches: deferred -> think, oos-bug -> build."""
    from fno.retro import keep_going as kg
    from fno.retro.routine import triage_pr

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "carveouts.jsonl").write_text(
        '{"id":"cv-d","session_id":"S1","kind":"deferred","need":"q","description":"a deferred decision"}\n'
        '{"id":"cv-b","session_id":"S1","kind":"oos-bug","description":"an out of scope bug"}\n',
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_KEEP_GOING", "1")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))  # isolate the daily counter

    # Record dispatches instead of shelling out; keep the ceiling wide open.
    think, build = [], []
    monkeypatch.setattr(kg, "_dispatch_think", lambda n, c: think.append(n) or True)
    monkeypatch.setattr(kg, "_spawn_target_worker", lambda n, c: build.append(n) or True)
    monkeypatch.setattr(kg, "_daily_cap", lambda _root: 20)
    monkeypatch.setattr(kg, "_daily_count", lambda: 0)
    monkeypatch.setattr(kg, "_bump_daily_count", lambda: None)

    ids = iter(["node-d", "node-b"])
    report = triage_pr(
        repo_root=tmp_path,
        pr_number=7,
        mode="autonomous",
        session_ids=["S1"],
        comments=[],  # skip review harvest
        create_fn=lambda **kw: next(ids),
    )

    arms = {f.node_id: f.arm for f in report.followups}
    assert arms == {"node-d": ARM_THINK, "node-b": ARM_BUILD}
    assert think == ["node-d"] and build == ["node-b"]


def test_triage_pr_skips_keep_going_when_disabled(tmp_path, monkeypatch):
    """Same harvest, gate OFF -> no followups (default posture)."""
    from fno.retro import keep_going as kg
    from fno.retro.routine import triage_pr

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "carveouts.jsonl").write_text(
        '{"id":"cv-b","session_id":"S1","kind":"oos-bug","description":"a bug"}\n',
        encoding="utf-8",
    )
    monkeypatch.setenv("FNO_KEEP_GOING", "0")
    called = []
    monkeypatch.setattr(kg, "_spawn_target_worker", lambda n, c: called.append(n) or True)

    report = triage_pr(
        repo_root=tmp_path, pr_number=7, mode="autonomous",
        session_ids=["S1"], comments=[], create_fn=lambda **kw: "node-b",
    )
    assert report.followups == [] and called == []

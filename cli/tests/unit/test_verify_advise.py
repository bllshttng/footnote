"""Tests for fno.verify_advise (W6 6.1: ship-gate verifier advisory).

The one invariant that matters: advisory failure never wedges the ship -
every path exits 0 and collapses failures to verdict "error" (AC6-ERR).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno import verify_advise as va


# -- read_plan_acs -----------------------------------------------------------

def test_read_plan_acs_section(tmp_path: Path) -> None:
    plan = tmp_path / "plan.md"
    plan.write_text(
        "# Plan\n\n## Acceptance Criteria\n\n- AC1-HP works\n- AC1-ERR errors\n\n## Other\nnope\n",
        encoding="utf-8",
    )
    acs = va.read_plan_acs(plan)
    assert acs is not None and "AC1-HP works" in acs and "nope" not in acs


def test_read_plan_acs_line_fallback(tmp_path: Path) -> None:
    plan = tmp_path / "plan.md"
    plan.write_text("# Plan\n\nsome text\n- AC3-EDGE: boundary case\n", encoding="utf-8")
    acs = va.read_plan_acs(plan)
    assert acs is not None and "AC3-EDGE" in acs


def test_read_plan_acs_dir_resolves_index(tmp_path: Path) -> None:
    plan_dir = tmp_path / "plan"
    plan_dir.mkdir()
    (plan_dir / "00-INDEX.md").write_text(
        "## Acceptance Criteria\n- AC1: indexed\n", encoding="utf-8"
    )
    acs = va.read_plan_acs(plan_dir)
    assert acs is not None and "indexed" in acs


def test_read_plan_acs_missing_or_bare(tmp_path: Path) -> None:
    assert va.read_plan_acs(tmp_path / "nope.md") is None
    bare = tmp_path / "bare.md"
    bare.write_text("# just prose, no criteria\n", encoding="utf-8")
    assert va.read_plan_acs(bare) is None


# -- parse_verdict -----------------------------------------------------------

@pytest.mark.parametrize(
    ("text", "want"),
    [
        ("reasoning...\nVERDICT: pass\n", "pass"),
        ("VERDICT: Concerns", "concerns"),
        ("verdict: FAIL extra", "fail"),
        ("no verdict line at all", None),
        ("VERDICT: maybe", None),
        ("", None),
    ],
)
def test_parse_verdict(text: str, want) -> None:
    assert va.parse_verdict(text) == want


# -- decide_verdict ----------------------------------------------------------

def test_decide_verdict_doc_ship_not_applicable(tmp_path: Path) -> None:
    v = va.decide_verdict(
        reason="DoneAdvisory", plan_path="plan.md", cwd=tmp_path, session_id="s"
    )
    assert v == "not_applicable"


def test_decide_verdict_no_plan_not_applicable(tmp_path: Path) -> None:
    v = va.decide_verdict(reason="DonePRGreen", plan_path="", cwd=tmp_path, session_id="s")
    assert v == "not_applicable"


def test_decide_verdict_no_acs_not_applicable(tmp_path: Path) -> None:
    (tmp_path / "plan.md").write_text("# prose only\n", encoding="utf-8")
    v = va.decide_verdict(
        reason="DonePRGreen", plan_path="plan.md", cwd=tmp_path, session_id="s"
    )
    assert v == "not_applicable"


def _plan_with_acs(tmp_path: Path) -> None:
    (tmp_path / "plan.md").write_text(
        "## Acceptance Criteria\n- AC1: it works\n", encoding="utf-8"
    )


def test_decide_verdict_spawn_failure_is_error(tmp_path: Path, monkeypatch) -> None:
    """AC6-ERR: verifier dies -> verdict error, no raise."""
    _plan_with_acs(tmp_path)
    monkeypatch.setattr(
        va, "run_verifier", lambda *a, **k: (_ for _ in ()).throw(RuntimeError("boom"))
    )
    v = va.decide_verdict(
        reason="DonePRGreen", plan_path="plan.md", cwd=tmp_path, session_id="s"
    )
    assert v == "error"


def test_decide_verdict_garbage_reply_is_error(tmp_path: Path, monkeypatch) -> None:
    _plan_with_acs(tmp_path)
    monkeypatch.setattr(va, "run_verifier", lambda *a, **k: "no verdict here")
    v = va.decide_verdict(
        reason="DonePRGreen", plan_path="plan.md", cwd=tmp_path, session_id="s"
    )
    assert v == "error"


def test_decide_verdict_unreadable_plan_is_error(tmp_path: Path, monkeypatch) -> None:
    """An exists-but-unreadable plan is a fault (error), not not_applicable."""
    _plan_with_acs(tmp_path)
    monkeypatch.setattr(
        va, "read_plan_acs", lambda p: (_ for _ in ()).throw(OSError("EACCES"))
    )
    v = va.decide_verdict(
        reason="DonePRGreen", plan_path="plan.md", cwd=tmp_path, session_id="s"
    )
    assert v == "error"


def test_emit_verdict_event_per_path_independence(tmp_path: Path, capsys) -> None:
    """A project-log write failure must not starve the global log (sigma P2)."""
    blocker = tmp_path / "blocker"
    blocker.write_text("a file, not a dir", encoding="utf-8")
    bad = blocker / "ev.jsonl"  # parent is a file -> append fails
    good = tmp_path / "gev.jsonl"
    va.emit_verdict_event(
        verdict="pass", node_id="x-1", pr_number=None, session_id="s",
        events_paths=[bad, good],
    )
    assert good.exists() and "verifier_verdict" in good.read_text()
    assert "event emit to" in capsys.readouterr().err


def test_decide_verdict_happy_path(tmp_path: Path, monkeypatch) -> None:
    _plan_with_acs(tmp_path)
    monkeypatch.setattr(va, "run_verifier", lambda *a, **k: "looks good\nVERDICT: pass")
    v = va.decide_verdict(
        reason="DonePRGreen", plan_path="plan.md", cwd=tmp_path, session_id="s"
    )
    assert v == "pass"


# -- event emit --------------------------------------------------------------

def test_emit_verdict_event_validates_and_lands(tmp_path: Path) -> None:
    events = tmp_path / "events.jsonl"
    va.emit_verdict_event(
        verdict="pass",
        node_id="x-47ab",
        pr_number=188,
        session_id="sid-1",
        events_paths=[events],
    )
    lines = [json.loads(ln) for ln in events.read_text().splitlines()]
    assert len(lines) == 1
    e = lines[0]
    assert e["type"] == "verifier_verdict" and e["source"] == "target"
    assert e["data"]["verdict"] == "pass" and e["data"]["graph_node_id"] == "x-47ab"
    assert e["data"]["pr_number"] == 188 and e["data"]["source"] == "ship-gate"


def test_emit_verdict_event_null_node_allowed(tmp_path: Path) -> None:
    """graph_node_id is required-but-nullable (present-null convention)."""
    events = tmp_path / "events.jsonl"
    va.emit_verdict_event(
        verdict="not_applicable",
        node_id=None,
        pr_number=None,
        session_id="",
        events_paths=[events],
    )
    e = json.loads(events.read_text().splitlines()[0])
    assert e["data"]["graph_node_id"] is None


# -- ledger stamp ------------------------------------------------------------

def test_stamp_ledger_updates_matching_row(tmp_path: Path) -> None:
    ledger = tmp_path / "ledger.json"
    ledger.write_text(
        json.dumps({"entries": [{"session_id": "sid-1"}, {"session_id": "sid-2"}]}),
        encoding="utf-8",
    )
    assert va.stamp_ledger("sid-1", "concerns", ledger) is True
    data = json.loads(ledger.read_text())
    assert data["entries"][0]["verifier_verdict"] == "concerns"
    assert "verifier_verdict" not in data["entries"][1]


def test_stamp_ledger_no_row_or_missing_file(tmp_path: Path) -> None:
    ledger = tmp_path / "ledger.json"
    assert va.stamp_ledger("sid-1", "pass", ledger) is False  # missing file
    ledger.write_text(json.dumps({"entries": []}), encoding="utf-8")
    assert va.stamp_ledger("sid-1", "pass", ledger) is False  # no row
    assert va.stamp_ledger("", "pass", ledger) is False  # empty session id


# -- exactly-once guard (AC6-HP) ----------------------------------------------

def _verdict_line(session_id: str) -> str:
    return json.dumps(
        {
            "ts": "2026-07-04T00:00:00Z",
            "type": "verifier_verdict",
            "source": "target",
            "data": {
                "graph_node_id": "x-47ab",
                "verdict": "pass",
                "source": "ship-gate",
                "session_id": session_id,
            },
        }
    )


def test_already_recorded(tmp_path: Path) -> None:
    events = tmp_path / "ev.jsonl"
    events.write_text("not json\n" + _verdict_line("sid-1") + "\n", encoding="utf-8")
    assert va.already_recorded("sid-1", events) is True
    assert va.already_recorded("sid-2", events) is False  # different session
    assert va.already_recorded("", events) is False  # no session id: can't dedup
    assert va.already_recorded("sid-1", tmp_path / "missing.jsonl") is False


def test_main_skips_when_already_recorded(tmp_path: Path, monkeypatch, capsys) -> None:
    """A retried finalize fire must not double-emit or re-spend on a spawn."""
    _plan_with_acs(tmp_path)
    events = tmp_path / "ev.jsonl"
    events.write_text(_verdict_line("sid-1") + "\n", encoding="utf-8")

    def no_spawn(*a, **k):
        raise AssertionError("spawn must not run on a retried fire")

    monkeypatch.setattr(va, "run_verifier", no_spawn)
    rc = va.main(
        [
            "--plan-path", "plan.md",
            "--session-id", "sid-1",
            "--reason", "DonePRGreen",
            "--cwd", str(tmp_path),
            "--events", str(events),
            "--global-events", str(tmp_path / "gev.jsonl"),
            "--ledger", str(tmp_path / "ledger.json"),
        ]
    )
    assert rc == 0
    assert "already recorded" in capsys.readouterr().out
    assert len(events.read_text().splitlines()) == 1  # no second event
    assert not (tmp_path / "gev.jsonl").exists()


# -- main: exit 0 everywhere (AC6-ERR) ---------------------------------------

def test_main_exits_zero_on_happy_path(tmp_path: Path, monkeypatch, capsys) -> None:
    _plan_with_acs(tmp_path)
    ledger = tmp_path / "ledger.json"
    ledger.write_text(json.dumps({"entries": [{"session_id": "sid-1"}]}), encoding="utf-8")
    monkeypatch.setattr(va, "run_verifier", lambda *a, **k: "VERDICT: pass")
    monkeypatch.setattr(va, "_pr_number", lambda cwd: 42)
    rc = va.main(
        [
            "--node-id", "x-47ab",
            "--plan-path", "plan.md",
            "--session-id", "sid-1",
            "--reason", "DonePRGreen",
            "--cwd", str(tmp_path),
            "--events", str(tmp_path / "ev.jsonl"),
            "--global-events", str(tmp_path / "gev.jsonl"),
            "--ledger", str(ledger),
        ]
    )
    assert rc == 0
    assert "verdict=pass" in capsys.readouterr().out
    # Event landed in BOTH logs; ledger row carries the field (AC6-HP).
    assert (tmp_path / "ev.jsonl").exists() and (tmp_path / "gev.jsonl").exists()
    assert json.loads(ledger.read_text())["entries"][0]["verifier_verdict"] == "pass"


def test_main_exits_zero_when_everything_fails(tmp_path: Path, monkeypatch, capsys) -> None:
    """Spawn dead, ledger unwritable, events dir a file: still exit 0."""
    _plan_with_acs(tmp_path)
    monkeypatch.setattr(
        va, "run_verifier", lambda *a, **k: (_ for _ in ()).throw(OSError("dead"))
    )
    monkeypatch.setattr(va, "_pr_number", lambda cwd: None)
    blocker = tmp_path / "blocker"
    blocker.write_text("i am a file, not a dir", encoding="utf-8")
    rc = va.main(
        [
            "--plan-path", "plan.md",
            "--session-id", "sid-1",
            "--reason", "DonePRGreen",
            "--cwd", str(tmp_path),
            "--events", str(blocker / "ev.jsonl"),  # parent is a file -> emit fails
            "--global-events", str(blocker / "gev.jsonl"),
            "--ledger", str(tmp_path / "no-ledger.json"),
        ]
    )
    assert rc == 0
    assert "verdict=error" in capsys.readouterr().out

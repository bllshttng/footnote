"""Tier-2 gate_escape spawn-cap auto-emit + parity (x-91b5).

Covers the spawn-cap half of the emit sites: the shared emit machinery, the
test-context guard (AC1-EDGE), fail-open (AC1-FR), dedup (AC2-INV), and the
Rust<->Python guard parity fixture (AC2-FR). The manual verb and the rebase
nudge are exercised in test_gate_escape_verb.py / test_pr_rebase-adjacent
tests.
"""
from __future__ import annotations

import json
from pathlib import Path

from fno.events import gate_escape as ge

FIXTURE = Path(__file__).parent / "fixtures" / "gate_escape_spawn_cap_parity.json"


def _events(p: Path) -> list[dict]:
    p = Path(p)
    if not p.exists():
        return []
    return [json.loads(line) for line in p.read_text().splitlines() if line.strip()]


def _escapes(p: Path, reason: str = "spawn-cap") -> list[dict]:
    return [
        e
        for e in _events(p)
        if e.get("type") == "gate_escape" and e.get("data", {}).get("reason") == reason
    ]


def test_parity_fixture_rows():
    """AC2-FR: should_emit_spawn_cap matches the shared fixture the Rust gate
    also reads. If this half and the Rust half disagree, one side drifted."""
    fixture = json.loads(FIXTURE.read_text())
    for sc in fixture["scenarios"]:
        assert ge.should_emit_spawn_cap(sc["env"]) is sc["expect"], sc["name"]


def test_emit_spawn_cap_lands_one_event(tmp_path):
    """AC1-HP: a spawn-cap emit appends exactly one gate_escape."""
    ev = tmp_path / "events.jsonl"
    out = ge.emit_gate_escape("spawn-cap", dedup_key="k", detail="bypass", events_path=ev)
    assert out == ev
    escapes = _escapes(ev)
    assert len(escapes) == 1
    assert escapes[0]["data"]["dedup_key"] == "k"


def test_dedup_collapses_burst(tmp_path):
    """AC2-INV: three emits sharing (reason, dedup_key) count once."""
    ev = tmp_path / "events.jsonl"
    for _ in range(3):
        ge.emit_gate_escape("spawn-cap", dedup_key="sess:day", events_path=ev)
    assert len(_escapes(ev)) == 1


def test_distinct_dedup_keys_count_separately(tmp_path):
    """A genuinely separate intervention (different bucket) is NOT collapsed."""
    ev = tmp_path / "events.jsonl"
    ge.emit_gate_escape("spawn-cap", dedup_key="day1", events_path=ev)
    ge.emit_gate_escape("spawn-cap", dedup_key="day2", events_path=ev)
    assert len(_escapes(ev)) == 2


def test_fail_open_on_unwritable_log(tmp_path, monkeypatch):
    """AC1-FR: an append failure never raises; a durable failure line is logged."""
    ev = tmp_path / "events.jsonl"
    import fno.events as events_mod

    def _boom(*a, **k):
        raise OSError("disk full")

    monkeypatch.setattr(events_mod, "append_event", _boom)
    out = ge.emit_gate_escape("spawn-cap", dedup_key="k", events_path=ev)
    assert out is None
    assert _escapes(ev) == []
    fail_log = tmp_path / "gate_escape_emit_failures.jsonl"
    assert fail_log.exists()
    lines = [json.loads(x) for x in fail_log.read_text().splitlines() if x.strip()]
    assert len(lines) == 1 and lines[0]["reason"] == "spawn-cap"


def test_bad_reason_raises_no_event(tmp_path):
    """A bad reason fails closed (ValidationError propagates); emits nothing."""
    import pytest

    from fno.events import ValidationError

    ev = tmp_path / "events.jsonl"
    with pytest.raises(ValidationError):
        ge.emit_gate_escape("flek", dedup_key="k", events_path=ev)
    assert not ev.exists()


def test_pr_and_dedup_key_together_raises(tmp_path):
    """The dedup contract is fail-loud: passing both pr AND dedup_key is a
    caller bug (already_emitted would OR-match and miscount), so it raises
    rather than silently emitting."""
    import pytest

    ev = tmp_path / "events.jsonl"
    with pytest.raises(ValueError):
        ge.emit_gate_escape("flake", pr=7, dedup_key="k", events_path=ev)
    assert not ev.exists()


def test_placeholder_pr_normalized_to_no_pr(tmp_path):
    """A pr<=0 (placeholder) is normalized to 'no PR' in one place, so the
    payload never carries a bogus pr and dedup does not key on it."""
    ev = tmp_path / "events.jsonl"
    ge.emit_gate_escape("flake", pr=0, dedup_key="k", events_path=ev)
    escapes = _escapes(ev, "flake")
    assert len(escapes) == 1
    assert "pr" not in escapes[0]["data"]


def test_run_gate_bypass_under_pytest_emits_nothing(tmp_path, monkeypatch):
    """AC1-EDGE: FNO_SPAWN_GATE=0 in a test context (PYTEST_CURRENT_TEST is set
    by pytest itself) must emit zero spawn-cap events even though the gate is
    bypassed."""
    ev = tmp_path / "events.jsonl"
    monkeypatch.setattr(ge, "canonical_events_path", lambda *a, **k: ev)
    monkeypatch.setenv("FNO_SPAWN_GATE", "0")
    from fno.agents import spawn_gate

    spawn_gate.run_gate("t", "bg")  # PYTEST_CURRENT_TEST set -> guard blocks the emit
    assert _escapes(ev) == []


def test_maybe_emit_fires_when_guard_says_yes(tmp_path, monkeypatch):
    """The gate wiring DOES emit when should_emit_spawn_cap is true. pytest
    always sets PYTEST_CURRENT_TEST, so the positive path is proven by forcing
    the guard true (the guard logic itself is covered by the parity fixture)."""
    ev = tmp_path / "events.jsonl"
    monkeypatch.setattr(ge, "canonical_events_path", lambda *a, **k: ev)
    monkeypatch.setattr(ge, "should_emit_spawn_cap", lambda *a, **k: True)
    from fno.agents import spawn_gate

    spawn_gate._maybe_emit_spawn_cap_escape()
    assert len(_escapes(ev)) == 1

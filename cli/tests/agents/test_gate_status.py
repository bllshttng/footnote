"""The read-only capacity probe behind `fno agents gate-status`.

The probe's verdict mapping (lanes at cap, the RAM floor, the CPU axis, the
king share) is decided inside the ONE Rust gate and pinned by its tests
(`crates/fno-agents/src/spawn_gate_verb.rs`). What stays Python-side is the
transport: the answer comes from the verb verbatim, and an unanswered gate is
``verdict: unknown``, never saturation and never a raise."""
from __future__ import annotations

import json

import pytest

from fno.agents import spawn_gate


def _stub_probe(monkeypatch, answer=None, error=None):
    from fno import rust_binary

    def fake(verb, payload, **_kwargs):
        assert verb == "spawn-gate"
        assert payload["mode"] == "probe"
        return answer

    def boom(*_a, **_k):
        raise error

    monkeypatch.setattr(
        rust_binary, "verb_call", boom if error is not None else fake
    )


def test_gate_status_prints_the_probe_answer_verbatim(monkeypatch, capsys):
    _stub_probe(
        monkeypatch,
        {
            "verdict": "accepted",
            "live_workers": 3,
            "max_live": 8,
            "lanes": {"zai": {"cap": 5, "live": 3, "counted": ["w1"]}},
            "rows": [],
        },
    )
    from fno.agents.gate_reads import cmd_gate_status

    cmd_gate_status()
    answer = json.loads(capsys.readouterr().out)
    assert answer["verdict"] == "accepted"
    assert answer["lanes"]["zai"]["live"] == 3


def test_probe_never_raises_and_reports_unknown_on_an_unanswered_gate(monkeypatch):
    from fno.rust_binary import VerbUnavailable

    _stub_probe(monkeypatch, error=VerbUnavailable("exited 1"))
    answer = spawn_gate.probe_capacity()
    assert answer["verdict"] == "unknown"
    assert answer["reason"] == "gate_unavailable"
    assert "exited 1" in answer["error"]


def test_probe_is_silent_and_raises_nothing_on_an_answer(monkeypatch, capsys):
    _stub_probe(monkeypatch, {"verdict": "refused", "reason": "ram_floor"})
    answer = spawn_gate.probe_capacity()
    assert answer["verdict"] == "refused"
    assert capsys.readouterr().out == ""

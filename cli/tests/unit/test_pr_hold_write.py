"""fno do pr hold: the CLI surface for the merge-hold writer in crates.

The writer itself lives in crates (merge_hold.rs) behind authorized-merge's
payload; these tests pin the CLI mapping only.
"""

from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.cli import app

RUNNER = CliRunner()


@pytest.fixture
def capture_verb_call(monkeypatch):
    calls: list[dict] = []
    posted = {"return": lambda verb, payload: {"outcome": "held", "exit_code": 0}}

    def fake(verb, payload):
        calls.append(payload)
        return posted["return"](verb, payload)

    monkeypatch.setattr("fno.rust_binary.verb_call", fake)
    return calls, posted


def _invoke(args):
    return RUNNER.invoke(app, ["do", "pr", "hold", *args])


def test_set_forwards_the_hold_payload_and_prints_the_receipt(capture_verb_call):
    calls, posted = capture_verb_call
    posted["return"] = lambda verb, payload: {
        "outcome": "held",
        "exit_code": 0,
        "node": "t-0001",
        "hold": {"reason": "R"},
    }
    result = _invoke(["set", "t-0001", "--reason", "R", "--release-when", "W", "--set-by", "crown"])
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output.strip().splitlines()[-1])
    assert receipt["outcome"] == "held"
    payload = calls[-1]
    assert payload["op"] == "hold-set"
    assert payload["node"] == "t-0001"
    assert payload["reason"] == "R"
    assert payload["graph"]


def test_refusals_map_to_their_exit_codes(capture_verb_call):
    calls, posted = capture_verb_call
    posted["return"] = lambda verb, payload: {
        "outcome": "refused",
        "exit_code": 3,
        "detail": "already held",
    }
    result = _invoke(["set", "t-0001", "--reason", "R", "--release-when", "W", "--set-by", "crown"])
    assert result.exit_code == 3
    assert "already held" in result.output
    assert calls


def test_a_writer_failure_exits_1(capture_verb_call):
    calls, posted = capture_verb_call
    posted["return"] = lambda verb, payload: {"outcome": "error", "exit_code": 1, "detail": "readback missed"}
    result = _invoke(["release", "t-0001", "--evidence", "E"])
    assert result.exit_code == 1
    assert "readback missed" in result.output


def test_an_unreachable_writer_exits_127(monkeypatch):
    def boom(verb, payload):
        raise __import__("fno").rust_binary.VerbUnavailable("binary missing")

    monkeypatch.setattr("fno.rust_binary.verb_call", boom)
    result = _invoke(["set", "t-0001", "--reason", "R", "--release-when", "W", "--set-by", "crown"])
    assert result.exit_code == 127


def test_unknown_action_refuses_locally(capture_verb_call):
    calls, _ = capture_verb_call
    result = _invoke(["fly", "t-0001"])
    assert result.exit_code == 2
    assert not calls

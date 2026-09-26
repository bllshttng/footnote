"""The dispatch doors pass the operator's blueprint floor with the row.

The lifecycle verb table answers in Rust (fno-agents effective_verb); the
floor is config, so the Python door must hand it over with the row. A dropped
floor silently answers the lean default, which is the safe direction, but an
operator's explicit "medium" that never reaches the table reads as an
intentional lean answer, so the plumbing is pinned here.
"""
from __future__ import annotations

from fno.agents.node_dispatch import _verb_answer
from fno.config import DispatchBlock


class _Client:
    def __init__(self, reply: dict):
        self.reply = reply
        self.seen: list[dict] = []

    def request(self, _verb: str, params: dict) -> dict:
        self.seen.append(params)
        return self.reply


def _patch(monkeypatch, floor: str | None):
    client = _Client({"verb": "/target", "note": "n"})

    class _Cfg:
        class dispatch:  # noqa: N801 - settings shim
            blueprint_floor = floor

    if floor is None:
        def _raise(_p=None):
            raise RuntimeError("unreadable config")

        monkeypatch.setattr("fno.config.load_settings_for_repo", _raise)
        monkeypatch.setattr("fno.config.load_settings", _raise)
    else:
        monkeypatch.setattr("fno.config.load_settings_for_repo", lambda _p=None: _Cfg)
        monkeypatch.setattr("fno.config.load_settings", lambda: _Cfg)
    monkeypatch.setattr(
        "fno.graph.store._client_for", lambda _graph: client
    )
    return client


def test_floor_rides_with_the_row(monkeypatch):
    client = _patch(monkeypatch, "medium")
    verb, note = _verb_answer({"id": "x-1", "cwd": "/repo"})
    assert (verb, note) == ("/target", "n")
    assert client.seen[0]["blueprint_floor"] == "medium"


def test_unreadable_config_omits_the_floor(monkeypatch):
    client = _patch(monkeypatch, None)
    verb, _note = _verb_answer({"id": "x-1", "cwd": "/repo"})
    assert verb == "/target"
    assert "blueprint_floor" not in client.seen[0]


def test_blueprint_floor_validator_degrades_a_typo_to_the_lean_default():
    assert DispatchBlock(blueprint_floor="spicy").blueprint_floor == "high"
    assert DispatchBlock(blueprint_floor="medium").blueprint_floor == "medium"
    assert DispatchBlock(blueprint_floor="high").blueprint_floor == "high"
    assert DispatchBlock().blueprint_floor == "high"


def test_blueprint_floor_validator_accepts_the_widened_ladder():
    assert DispatchBlock(blueprint_floor="low").blueprint_floor == "low"
    assert DispatchBlock(blueprint_floor="never").blueprint_floor == "never"


def test_floor_low_and_never_ride_with_the_row(monkeypatch):
    for floor in ("low", "never"):
        client = _patch(monkeypatch, floor)
        verb, _note = _verb_answer({"id": "x-1", "cwd": "/repo"})
        assert verb == "/target"
        assert client.seen[0]["blueprint_floor"] == floor

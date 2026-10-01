"""Tests for the update modal's release-notes bridge (fno.update).

The builder is native (crates/fno-agents/src/release_notes.rs) and carries
its git-fixture tests there. Here the bridge is judged: an unavailable
native leg degrades to None (the payload never blocks), a native answer
passes through untouched, and the readiness payload carries the result. The
tests monkeypatch ``verb_call`` because the pytest CI legs delete the
fno-agents debug binary, so these tests must never reach a real binary.
"""

from __future__ import annotations

import json
import types
from pathlib import Path

import pytest

from fno import update

_NOTES = {
    "highlights": [
        {"pr": 105, "url": None, "text": "card rows"},
        {"pr": 101, "url": None, "text": "new sidebar"},
    ],
    "groups": [
        {"area": "agents", "lines": [{"pr": 104, "url": None, "text": "stop the crash"}]}
    ],
    "hidden_line": "3 test/docs/ci/chore PRs hidden",
}


def test_bridge_none_when_native_leg_unavailable(monkeypatch: pytest.MonkeyPatch) -> None:
    def boom(verb, payload, unavailable, **kw):
        raise unavailable("no binary")

    monkeypatch.setattr("fno.rust_binary.verb_call", boom)
    assert update._release_notes("rev", Path("/src")) is None


def test_bridge_passes_payload_through(monkeypatch: pytest.MonkeyPatch) -> None:
    seen: dict = {}

    def fake(verb, payload, unavailable, **kw):
        seen["verb"] = verb
        seen["payload"] = payload
        return {"notes": _NOTES}

    monkeypatch.setattr("fno.rust_binary.verb_call", fake)
    notes = update._release_notes("rev123", Path("/src"))
    assert notes == _NOTES
    assert seen["verb"] == "release-notes"
    assert seen["payload"] == {"installed_rev": "rev123", "source": "/src"}


def test_update_readiness_carries_release_notes(monkeypatch, tmp_path) -> None:
    from fno import doctor

    src = tmp_path / "cli"
    src.mkdir()
    monkeypatch.setattr(doctor, "_read_marker", lambda: "aaa1111")
    monkeypatch.setattr(doctor, "_resolve_source", lambda source: src)
    monkeypatch.setattr(doctor, "_source_rev", lambda source: "bbb2222")
    monkeypatch.setattr(
        update, "_resolve_source_pin",
        lambda source: {"path": str(src), "decision": "allow"},
    )
    monkeypatch.setattr(update.shutil, "which", lambda name: "/usr/bin/fno")
    monkeypatch.setattr(update, "_cargo_installed_mux", lambda: None)
    monkeypatch.setattr(update, "running_components", lambda runner: [])
    monkeypatch.setattr(update, "_release_notes", lambda installed, source: _NOTES)
    result = update.update_readiness(
        runner=lambda cmd, **kw: types.SimpleNamespace(
            returncode=0,
            stdout=json.dumps([{"session": "main", "state": "live", "panes": 1, "wire_version": 47}]),
        )
    )
    assert result["release_notes"] == _NOTES

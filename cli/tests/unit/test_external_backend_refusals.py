"""Tracker-owned backlog verbs refuse under an external backend (task 4.2).

The census (scripts/diagnostics/tracker-consumers.py --verbs) is the
enumerating instrument; these tests pin its load-bearing behaviors in the
suite: every live registry verb carries exactly one classification, the
shared refusal fires on the wrapped callback BEFORE any graph read/write,
and an injected unguarded verb is named by the detector.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

import fno.graph.cli as graph_cli
from fno.cli import app

runner = CliRunner()


def _registry_labels():
    for group, typer_app in graph_cli.iter_backlog_registry():
        for info in typer_app.registered_commands:
            name = info.name or ""
            yield (f"{group} {name}" if group else name), info


def test_every_live_verb_is_classified_exactly_once():
    """The registry walk: no unclassified verb, no double classification, and
    the two positive controls (a known creation verb, a known surviving read
    verb) sit in the stated classes."""
    tracker_owned = footnote_owned = 0
    for label, info in _registry_labels():
        cb = info.callback
        assert cb is not None, label
        t = getattr(cb, "_fno_tracker_owned", False)
        f = getattr(cb, "_fno_footnote_owned", False)
        assert t or f, f"unclassified verb: {label}"
        assert not (t and f), f"double-classified verb: {label}"
        tracker_owned += t
        footnote_owned += f
    # Positive controls - absence means the registry drifted and the census
    # must be re-pinned, not silently passed.
    labels = dict(_registry_labels())
    assert getattr(labels["add"].callback, "_fno_tracker_owned", False)
    assert getattr(labels["get"].callback, "_fno_footnote_owned", False)
    assert tracker_owned > 10 and footnote_owned > 10


@pytest.mark.parametrize(
    "argv",
    [
        ["backlog", "add", "A thing"],
        ["backlog", "update", "EXT-1", "--priority", "p1"],
        ["backlog", "defer", "EXT-1", "--reason", "waiting"],
        ["backlog", "rank", "EXT-1", "--top"],
        ["backlog", "queue", "EXT-1"],
        ["backlog", "maintain"],
        ["backlog", "session", "add", "EXT-1", "--phase", "do"],
    ],
)
def test_tracker_owned_verbs_refuse_under_external(argv, tmp_path, monkeypatch):
    """The shared guard fires exit 1, names the backend, and no local graph
    write happens (the contradictory file is byte-identical after)."""
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": []}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setattr(graph_cli, "_graph_path", lambda: g)
    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: None)
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))

    r = runner.invoke(app, argv, catch_exceptions=False)
    assert r.exit_code == 1, r.output
    assert "github" in r.output and "refused" in r.output
    assert g.read_text() == json.dumps({"entries": []})


def test_footnote_owned_read_verb_still_works_under_external(tmp_path, monkeypatch):
    """The refusal is scoped: `get` (footnote-owned) still answers through
    the seam under the same backend selection."""
    from fno.tracker.types import NodeNotFound, TrackerCandidate, TrackerState

    class _T:
        name = "fake-external"

        def read(self, id):
            if id == "EXT-1":
                return TrackerCandidate(id=id, title="Ext item",
                                        state=TrackerState.open)
            raise NodeNotFound(id)

        def list_open(self):
            return []

        def close(self, id):
            raise AssertionError("not part of this read")

    sidecars = tmp_path / "sidecars"
    sidecars.mkdir()
    (sidecars / "EXT-1.json").write_text(json.dumps({"id": "EXT-1"}))
    import fno.tracker.sidecar as sidecar_store

    monkeypatch.setattr(sidecar_store, "sidecar_path",
                        lambda i: sidecars / f"{i}.json")
    monkeypatch.setattr("fno.tracker.get_tracker", lambda *a, **k: _T())
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": []}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")

    r = runner.invoke(app, ["backlog", "get", "EXT-1"], catch_exceptions=False)
    assert r.exit_code == 0, r.output
    assert json.loads(r.output)["title"] == "Ext item"


def test_census_names_an_injected_unguarded_verb():
    """The detector, not just the message: an injected callback with no
    marker is named unclassified (AC11's self-test behavior, pinned)."""
    import importlib.util
    from pathlib import Path

    spec = importlib.util.spec_from_file_location(
        "tracker_consumers",
        Path(__file__).resolve().parents[3] / "scripts" / "diagnostics" / "tracker-consumers.py",
    )
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    _marker_of = mod._marker_of

    class _FakeInfo:
        name = "inject-unmarked"
        callback = lambda: None  # noqa: E731

    assert _marker_of(_FakeInfo()) is None

    class _Guarded:
        _fno_tracker_owned = True

        def __call__(self):
            return None

    assert _marker_of(type("I", (), {"callback": _Guarded()})) == "tracker-owned"

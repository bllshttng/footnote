"""The starved-writer wait on the graph read path.

A writer descheduled between `store._write_json` and `store._write_sha256_sidecar`
leaves the two files disagreeing. The ordinary retry budget is fixed, so it cannot
outlast a writer starved for longer than that budget. These tests pin the wait to
the writer RELEASING, and pin the cap that keeps a wedged writer from hanging a read.
"""

from __future__ import annotations

import hashlib
import json
import time

import pytest

import fno.graph.load as load_mod
from fno.graph.load import GraphCorruptionError, _sidecar_path, load_graph

_CEILING_S = 2.0


def _diverged(tmp_path):
    """A graph whose sidecar is one write behind. Returns (path, entries, digest)."""
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [{"id": "x-a"}]}) + "\n")
    _sidecar_path(g).write_text(hashlib.sha256(g.read_bytes()).hexdigest() + "\n")
    entries = [{"id": "x-a"}, {"id": "x-b"}]
    g.write_text(json.dumps({"entries": entries}) + "\n")
    return g, entries, hashlib.sha256(g.read_bytes()).hexdigest()


def test_a_writer_held_past_the_retry_budget_is_waited_out(tmp_path, monkeypatch):
    g, entries, good = _diverged(tmp_path)
    probes = {"n": 0}

    def _held_then_released(_path):
        probes["n"] += 1
        if probes["n"] < load_mod._RETRY_ATTEMPTS * 2:
            return True
        _sidecar_path(g).write_text(good + "\n")
        return False

    monkeypatch.setattr(load_mod, "_writer_active", _held_then_released)
    monkeypatch.setattr(load_mod.time, "sleep", lambda _s: None)

    # `load_graph` runs the defaults pass, so compare ids rather than whole rows.
    assert [e["id"] for e in load_graph(g)] == [e["id"] for e in entries]
    # The wait outlasted the fixed budget. One extra retry could not have.
    assert probes["n"] > load_mod._RETRY_ATTEMPTS


def test_a_wedged_writer_still_raises_and_stays_bounded(tmp_path, monkeypatch):
    g, _entries, _good = _diverged(tmp_path)
    probes = {"n": 0}

    def _never_releases(_path):
        probes["n"] += 1
        return True

    monkeypatch.setattr(load_mod, "_writer_active", _never_releases)
    monkeypatch.setattr(load_mod.time, "sleep", lambda _s: None)

    start = time.monotonic()
    with pytest.raises(GraphCorruptionError):
        load_graph(g)
    assert time.monotonic() - start < _CEILING_S
    assert probes["n"] == load_mod._WRITER_WAIT_ATTEMPTS


def test_no_writer_means_no_extra_wait(tmp_path, monkeypatch):
    """Genuine corruption is not slowed down by the wait it does not need."""
    g, _entries, _good = _diverged(tmp_path)
    probes = {"n": 0}

    def _free(_path):
        probes["n"] += 1
        return False

    monkeypatch.setattr(load_mod, "_writer_active", _free)
    monkeypatch.setattr(load_mod.time, "sleep", lambda _s: None)

    with pytest.raises(GraphCorruptionError):
        load_graph(g)
    assert probes["n"] == 1


def test_the_probe_answers_false_when_fcntl_is_missing(tmp_path, monkeypatch):
    """The probe promises to leave the caller's verdict unchanged when it cannot run.

    The import used to sit above the `try`, so a platform without `fcntl` raised
    ImportError out of the read path instead of answering.
    """
    import builtins

    real_import = builtins.__import__

    def _no_fcntl(name, *args, **kwargs):
        if name == "fcntl":
            raise ImportError("no fcntl on this platform")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", _no_fcntl)
    assert load_mod._writer_active(tmp_path / "graph.json") is False

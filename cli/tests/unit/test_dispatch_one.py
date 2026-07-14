"""Unit tests for `fno dispatch one` (x-6f77): the mux leader+g porcelain.

The lane slot is held for real against an isolated `FNO_CLAIMS_ROOT`, so the cap
and the release-on-failure path are genuinely exercised. Selection (`_next_node`)
and the pane spawn (`dispatch_spawn_pane`) are monkeypatched - no real
`fno backlog next` subprocess, no real pane.
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest

from fno import dispatch
from fno.claims.lanes import active_lane_count


def test_registered_and_addressable():
    """The verb is wired into the root CLI and the single-command sub-app does
    not collapse (the no-op callback keeps `one` addressable)."""
    from fno.cli import LAZY_SUBCOMMANDS

    assert "dispatch" in LAZY_SUBCOMMANDS
    names = [c.name for c in dispatch.dispatch_app.registered_commands]
    assert "one" in names


class _FakeSpawnOK:
    pane_id = 7


def _wire(monkeypatch, tmp_path, *, next_node=None, spawn=None, max_lanes=1):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    monkeypatch.setattr(
        dispatch, "load_settings",
        lambda: SimpleNamespace(parallel=SimpleNamespace(max_lanes=max_lanes)),
    )
    monkeypatch.setattr(dispatch, "_next_node", lambda project: next_node)
    monkeypatch.setattr(dispatch, "_worker_agent_name", lambda nid, slug: f"target-{nid}")
    monkeypatch.setattr(dispatch, "resolve_provenance", lambda nid, slug: {})

    calls: list = []

    def fake_spawn(**kwargs):
        calls.append(kwargs)
        if spawn is not None:
            return spawn()
        return _FakeSpawnOK()

    monkeypatch.setattr(dispatch, "dispatch_spawn_pane", fake_spawn)
    return calls


def test_no_ready_work(monkeypatch, tmp_path):
    _wire(monkeypatch, tmp_path, next_node=None)
    v = dispatch._dispatch_one(session="main", node=None, project=None)
    assert v["outcome"] == "no-work"
    assert active_lane_count() == 0  # never touched a slot


def test_launched_holds_a_lane(monkeypatch, tmp_path):
    calls = _wire(monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "feat", "cwd": str(tmp_path)})
    v = dispatch._dispatch_one(session="work", node=None, project=None)
    assert v["outcome"] == "launched"
    assert v["node"] == "x-1"
    assert v["pane_id"] == 7
    assert calls[0]["session"] == "work"
    assert calls[0]["message"] == "/target no-merge x-1"
    assert active_lane_count() == 1  # slot held for the live lane


def test_lanes_full_when_cap_reached(monkeypatch, tmp_path):
    # max_lanes=1: the first dispatch takes the only slot, the second is refused
    # with no spawn and no new claim (AC-edge).
    calls = _wire(monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)}, max_lanes=1)
    assert dispatch._dispatch_one(session="s", node=None, project=None)["outcome"] == "launched"
    monkeypatch.setattr(dispatch, "_next_node", lambda project: {"id": "x-2", "slug": "b", "cwd": str(tmp_path)})
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "lanes-full"
    assert v["node"] == "x-2"
    assert len(calls) == 1  # the second never spawned
    assert active_lane_count() == 1


def test_same_node_second_dispatch_is_deduped(monkeypatch, tmp_path):
    # Two fast leader+g resolve _next_node to the SAME node before the first
    # worker claims it. The create-only dispatch:<id> reservation must make the
    # second a no-op (already-dispatching) - never a second spawn, and never a
    # release of the first worker's live lane slot (the P1 race).
    calls = _wire(monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)}, max_lanes=2)
    assert dispatch._dispatch_one(session="s", node=None, project=None)["outcome"] == "launched"
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "already-dispatching"
    assert v["node"] == "x-1"
    assert len(calls) == 1  # the second never spawned
    assert active_lane_count() == 1  # first worker's slot intact


def test_spawn_failure_releases_the_slot(monkeypatch, tmp_path):
    def boom():
        raise RuntimeError("mux pane spawn failed")

    _wire(monkeypatch, tmp_path, next_node={"id": "x-9", "slug": "z", "cwd": str(tmp_path)}, spawn=boom)
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "failed"
    assert "spawn failed" in v["detail"]
    assert active_lane_count() == 0  # slot released -> node re-dispatchable


# --- `fno dispatch resolve` --verb/--brief (US3) ---------------------------


def _resolve_cli(*args):
    from typer.testing import CliRunner

    return CliRunner().invoke(dispatch.dispatch_app, ["resolve", *args])


def test_resolve_verb_brief_json():
    """--verb assembles `<verb> {id}`; --brief rides env.TARGET_BRIEF, JSON out."""
    import json

    r = _resolve_cli("--node", "x-1", "--verb", "/think", "--brief", "hi there", "-J")
    assert r.exit_code == 0
    out = json.loads(r.stdout)
    assert out["command"] == "/think x-1"
    assert out["env"]["TARGET_BRIEF"] == "hi there"


def test_resolve_out_of_allowlist_verb_exits_2():
    """An out-of-allowlist verb refuses with exit 2 and no resolved tuple."""
    r = _resolve_cli("--node", "x-1", "--verb", "rm -rf; /target")
    assert r.exit_code == 2
    assert "allowlist" in (r.stdout + str(r.stderr)).lower() or "rm -rf" in (r.stdout + str(r.stderr))


def test_resolve_brief_bytes_reported_in_kv():
    """key=value output reports brief size (the brief may be multi-line)."""
    r = _resolve_cli("--node", "x-1", "--verb", "/target", "--brief", "abc")
    assert r.exit_code == 0
    assert "brief_bytes=3" in r.stdout

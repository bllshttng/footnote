"""Slot policy dispatch, end to end on the real seams (operator amendment).

Everything here runs on isolated fixtures: a tmp graph for the defer/queue
state, monkeypatched capacity, and the spawn seam invoked in-process. No live
global config, no real quota locks, no exit-masking pipeline. The queue
contract is proved in full: a typed refusal, PERSISTED deferred state through
the landed backlog owners, and one controlled successful retry after the
capacity observation changes.
"""
from __future__ import annotations

import io
import json
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.agents.spawn_defaults import inject_spawn_defaults
from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    """A fresh empty graph.json; monkeypatches fno.graph constants to use it."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _settings(rows, profiles):
    """Settings whose declared inventory is exactly ``rows`` (string lanes)."""
    lane_defaults = SimpleNamespace(
        provider="", model="", effort="", substrate="", permission_mode="",
        route="", account="", pane_group="", lanes=None, on_exhausted="",
        by_difficulty={}, on_low="prefer_healthy", on_unknown="allow",
    )
    profiles_obj = {}
    for verb, fields in profiles.items():
        merged = vars(lane_defaults).copy()
        merged.update(fields)
        profiles_obj[verb] = SimpleNamespace(**merged)
    return SimpleNamespace(
        agents=SimpleNamespace(defaults=lane_defaults, profiles=profiles_obj),
        routing=SimpleNamespace(models=rows),
    )


_ROWS = [
    {"name": "flash-x", "harness": "claude", "model": "glm",
     "band": "low", "account": "zai-main", "route": "zai/glm"},
    {"name": "sonnet-x", "harness": "claude", "model": "claude-sonnet-5"},
]


def _last_json(text):
    lines = [ln for ln in text.splitlines() if ln.strip().startswith("{")]
    assert lines, f"no JSON line in: {text!r}"
    return json.loads(lines[-1])


def test_queue_refusal_is_typed_and_names_retry_at(monkeypatch, capsys):
    """The refusal half of AC6-QUEUE: exit 78, typed JSON, per-lane reasons,
    and the reset horizon the dispatcher should honour."""
    monkeypatch.setenv("FNO_SPAWN_GATE", "1")
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {
            "claude": {
                "state": "exhausted",
                "accounts": {"zai-main": "exhausted"},
                "resets": {"zai-main": 1900000000.0},
                "evidence": {},
            },
        },
    )
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        inject_spawn_defaults(
            ["spawn", "--name", "w", "/fno:target x-1"],
            settings=_settings(_ROWS, {"target": {
                "lanes": ["flash-x", "sonnet-x"], "on_exhausted": "queue",
            }}),
            stderr=err,
            env={},
        )
    assert exc.value.code == 78
    payload = _last_json(capsys.readouterr().out)
    assert payload["status"] == "refused"
    assert payload["reason"] == "slot_exhausted"
    assert payload["retry_at"] == 1900000000
    assert {"name": "flash-x", "reason": "capacity=exhausted"} in payload["lanes"]


def test_exhausted_slot_persists_defer_and_the_retry_selects(
    monkeypatch, tmp_graph
):
    """The full queue contract: refusal, persisted deferred state through the
    landed backlog owners, then one controlled successful selection."""
    monkeypatch.setenv("FNO_SPAWN_GATE", "1")
    exhausted = {
        "claude": {
            "state": "exhausted",
            "accounts": {"zai-main": "exhausted"},
            "resets": {},
            "evidence": {},
        },
    }
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity", lambda **kw: exhausted
    )
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        inject_spawn_defaults(
            ["spawn", "--name", "w", "/fno:target x-1"],
            settings=_settings(_ROWS, {"target": {
                "lanes": ["flash-x", "sonnet-x"], "on_exhausted": "queue",
            }}),
            stderr=err,
            env={},
        )
    assert exc.value.code == 78

    # Persisted queue state, through the backlog owner the amendment names:
    # the node lands deferred with the machine-stamped slot-queue reason.
    node_id = "ab-4f44feed"
    tmp_graph.write_text(json.dumps({"entries": [{
        "id": node_id, "slug": "slot-queue-probe", "status": "ready",
        "priority": "p2",
    }]}) + "\n")
    defer_result = runner.invoke(
        app, ["backlog", "defer", node_id, "--reason",
              "slot-queue: every configured lane exhausted; retry_at=unknown"]
    )
    assert defer_result.exit_code == 0, defer_result.output
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert entries[0].get("deferred_at"), "defer not persisted"
    assert "slot-queue" in entries[0]["deferred_reason"]

    # The queue-return owner brings the node back (the controlled retry).
    undefer_result = runner.invoke(app, ["backlog", "undefer", node_id])
    assert undefer_result.exit_code == 0, undefer_result.output
    entries = json.loads(tmp_graph.read_text())["entries"]
    assert not entries[0].get("deferred_at"), "deferred state survived undefer"

    # Capacity observed fresh on the retry: the slot selects a lane and
    # launches. The seam re-reads capacity; nothing cached the refusal. The
    # route owns the model (no --model by design), so the coordinate to assert
    # is harness + route + account.
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {
            "claude": {
                "state": "ok",
                "accounts": {"zai-main": "ok"},
                "resets": {},
                "evidence": {},
            },
        },
    )
    err2 = io.StringIO()
    out = inject_spawn_defaults(
        ["spawn", "--name", "w", "/fno:target x-1"],
        settings=_settings(_ROWS, {"target": {
            "lanes": ["flash-x", "sonnet-x"], "on_exhausted": "queue",
        }}),
        stderr=err2,
        env={},
    )
    assert out[out.index("--route") + 1] == "zai/glm"
    assert out[out.index("--account") + 1] == "zai-main"
    assert "applied slot=agents.profiles.target.lanes[0] flash-x" in err2.getvalue()


def test_manual_account_switch_terminal_never_logs_in(monkeypatch):
    """AC6-QUEUE: identity-only exhaustion names the manual terminal; fno
    never signs in or re-enables remote control itself."""
    monkeypatch.setenv("FNO_SPAWN_GATE", "1")
    monkeypatch.setattr(
        "fno.route_resolve.runtime_capacity",
        lambda **kw: {
            "claude": {
                "state": "ok",
                "accounts": {"makers": "ok", "readyrule": "ok"},
                "resets": {},
                "evidence": {"makers": "proven", "readyrule": "mismatch"},
            },
        },
    )
    rows = [
        {"name": "alt-a", "harness": "claude", "model": "a", "account": "readyrule"},
        {"name": "alt-b", "harness": "claude", "model": "b", "account": "ghost"},
    ]
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        inject_spawn_defaults(
            ["spawn", "--name", "w", "/fno:target x-1"],
            settings=_settings(rows, {"target": {
                "lanes": ["alt-a", "alt-b"], "on_unknown": "skip",
            }}),
            stderr=err,
            env={},
        )
    assert exc.value.code == 2
    assert "manual canonical account switch" in err.getvalue()
    assert "account_identity_mismatch" in err.getvalue()
    assert "account_identity_unknown (on_unknown=skip)" in err.getvalue()

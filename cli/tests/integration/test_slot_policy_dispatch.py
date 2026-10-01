"""Slot policy dispatch, end to end on the real seams (operator amendment).

Everything here runs on isolated fixtures: a tmp graph for the defer/queue
state, a pinned runtime-state file for the verb's capacity read, and the
spawn seam invoked in-process. No live global config, no real quota locks, no
exit-masking pipeline. The queue contract is proved in full: a typed refusal,
PERSISTED deferred state through the landed backlog owners, and one controlled
successful retry after the capacity observation changes.
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import io
import json

import pytest
from typer.testing import CliRunner


requires_rust = pytest.mark.dev_build


from fno.agents.spawn_defaults import compose_spawn_argv
from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch):
    """A fresh empty graph.json; monkeypatches fno.graph constants to use it."""
    g = tmp_path / "graph.json"
    seed_graph(g, '{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_HTML", tmp_path / "graph.html")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _append_lanes(cfg, rows, profile):
    """Declare the lanes on the pinned config file: ``rows`` become
    [[routing.models]] blocks, ``profile`` rides [agents.profiles.target]."""
    lines = []
    for row in rows:
        lines.append("[[routing.models]]")
        for key, value in row.items():
            lines.append(f'{key} = "{value}"')
        lines.append("")
    lines.append("[agents.profiles.target]")
    for key, value in profile.items():
        if key == "lanes":
            # The caller passes the TOML array text verbatim.
            lines.append(f"lanes = {value}")
        else:
            lines.append(f'{key} = "{value}"')
    with open(cfg, "a") as f:
        f.write("\n" + "\n".join(lines) + "\n")


def _last_json(text):
    lines = [ln for ln in text.splitlines() if ln.strip().startswith("{")]
    assert lines, f"no JSON line in: {text!r}"
    return json.loads(lines[-1])


def _pin_capacity(monkeypatch, claude=None, codex=None, extra=None, active=None):
    """Pin the capacity readings the verb judges lanes with.

    The Python capacity read was deleted (x-1c38): the verb computes it from
    the runtime-state file, so a hermetic one rides in through env instead of
    a monkeypatched Python function. claude/codex pin one account record each
    (`cl-a` for claude, `cx-a` for codex); None leaves the harness with no
    record, which reads unknown. `extra` adds per-account readings as
    {harness: {account: state}} (a dict value may carry resets_at). `active`
    writes identity stamps as {harness: account}. Returns (config, state)
    paths so a test can move capacity mid-flight.
    """
    import os
    import tempfile
    import time as _time

    d = tempfile.mkdtemp(prefix="fno-cap-")
    records = []
    for harness, spec in (("claude", claude), ("codex", codex)):
        if spec is not None:
            records.append((f"{'cl' if harness == 'claude' else 'cx'}-a", harness, spec))
    for harness, accounts in (extra or {}).items():
        for account, spec in accounts.items():
            records.append((account, harness, spec))
    cfg = os.path.join(d, "config.toml")
    with open(cfg, "w") as f:
        f.write(f"state_dir = '{d}'\n")
        for account, harness, _spec in records:
            f.write(f'[[accounts.records]]\nid = "{account}"\nharness = "{harness}"\n')
    now = _time.time()

    def row(spec) -> dict:
        if isinstance(spec, dict):
            state, resets = spec.get("state", "ok"), spec.get("resets_at")
        else:
            state, resets = spec, None
        pct = {"ok": 5.0, "low": 95.0}.get(state, 100.0)
        return {
            "probed_at": now,
            "partial": False,
            "windows": [{"label": "daily", "used_pct": pct, "resets_at": resets}],
        }

    state = os.path.join(d, "state.json")
    with open(state, "w") as f:
        f.write(json.dumps({"usage": {a: row(spec) for a, _h, spec in records}}))
    for harness, account in (active or {}).items():
        os.makedirs(os.path.join(d, "providers"), exist_ok=True)
        with open(os.path.join(d, "providers", f".active-{harness}"), "w") as f:
            f.write(account)
    monkeypatch.setenv("FNO_AGENTS_HOME", d)
    # Same dir, not a child: the identity stamps live under <state_dir>/providers.
    monkeypatch.setenv("FNO_STATE_DIR", d)
    monkeypatch.setenv("FNO_CONFIG", cfg)
    monkeypatch.setenv("FNO_RUNTIME_STATE_PATH", state)
    return cfg, state


def test_queue_refusal_is_typed_and_names_retry_at(monkeypatch, capsys):
    """The refusal half of AC6-QUEUE: exit 78, typed JSON, per-lane reasons,
    and the reset horizon the dispatcher should honour."""
    monkeypatch.setenv("FNO_SPAWN_GATE", "1")
    cfg, _state = _pin_capacity(
        monkeypatch,
        claude="exhausted",
        extra={"claude": {"zai-main": {"state": "exhausted", "resets_at": 1900000000.0}}},
    )
    _append_lanes(
        cfg,
        [
            {"name": "flash-x", "harness": "claude", "model": "glm",
             "band": "low", "account": "zai-main", "route": "zai/glm"},
            {"name": "sonnet-x", "harness": "claude", "model": "claude-sonnet-5"},
        ],
        {"lanes": '["flash-x", "sonnet-x"]', "on_exhausted": "queue"},
    )
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        compose_spawn_argv(
            ["spawn", "--name", "w", "/fno:target x-1"],
            stderr=err,
        )
    assert exc.value.code == 78
    payload = _last_json(capsys.readouterr().out)
    assert payload["status"] == "refused"
    assert payload["reason"] == "slot_exhausted"
    assert payload["retry_at"] == 1900000000
    # the computed reading carries its provenance; the cause is what matters
    flash = next(lane for lane in payload["lanes"] if lane["name"] == "flash-x")
    assert flash["reason"].startswith("capacity=exhausted")


@requires_rust
def test_exhausted_slot_persists_defer_and_the_retry_selects(
    monkeypatch, tmp_graph
):
    """The full queue contract: refusal, persisted deferred state through the
    landed backlog owners, then one controlled successful selection."""
    monkeypatch.setenv("FNO_SPAWN_GATE", "1")
    cfg, _state = _pin_capacity(monkeypatch, claude="exhausted", extra={"claude": {"zai-main": "exhausted"}})
    _append_lanes(
        cfg,
        [
            {"name": "flash-x", "harness": "claude", "model": "glm",
             "band": "low", "account": "zai-main", "route": "zai/glm"},
            {"name": "sonnet-x", "harness": "claude", "model": "claude-sonnet-5"},
        ],
        {"lanes": '["flash-x", "sonnet-x"]', "on_exhausted": "queue"},
    )
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        compose_spawn_argv(
            ["spawn", "--name", "w", "/fno:target x-1"],
            stderr=err,
        )
    assert exc.value.code == 78

    # Persisted queue state, through the backlog owner the amendment names:
    # the node lands deferred with the machine-stamped slot-queue reason.
    node_id = "ab-4f44feed"
    seed_graph(tmp_graph, json.dumps({"entries": [{
        "id": node_id, "slug": "slot-queue-probe", "status": "ready",
        "priority": "p2",
    }]}) + "\n")
    defer_result = runner.invoke(
        app, ["backlog", "defer", node_id, "--reason",
              "slot-queue: every configured lane exhausted; retry_at=unknown"]
    )
    assert defer_result.exit_code == 0, defer_result.output
    # The store owns state; the json mirror can lag the last write.
    from fno.graph.store import read_graph_strict

    entries = read_graph_strict(tmp_graph)
    assert entries[0].get("deferred_at"), "defer not persisted"
    assert "slot-queue" in entries[0]["deferred_reason"]

    # The queue-return owner brings the node back (the controlled retry).
    undefer_result = runner.invoke(app, ["backlog", "undefer", node_id])
    assert undefer_result.exit_code == 0, undefer_result.output
    entries = read_graph_strict(tmp_graph)
    assert not entries[0].get("deferred_at"), "deferred state survived undefer"

    # Capacity observed fresh on the retry: the slot selects a lane and
    # launches. The seam re-reads capacity; nothing cached the refusal. The
    # route owns the model (no --model by design), so the coordinate to assert
    # is harness + route + account.
    cfg, _state = _pin_capacity(monkeypatch, claude="ok", extra={"claude": {"zai-main": "ok"}})
    _append_lanes(
        cfg,
        [
            {"name": "flash-x", "harness": "claude", "model": "glm",
             "band": "low", "account": "zai-main", "route": "zai/glm"},
            {"name": "sonnet-x", "harness": "claude", "model": "claude-sonnet-5"},
        ],
        {"lanes": '["flash-x", "sonnet-x"]', "on_exhausted": "queue"},
    )
    err2 = io.StringIO()
    out = compose_spawn_argv(
        ["spawn", "--name", "w", "/fno:target x-1"],
        stderr=err2,
    )
    assert out[out.index("--route") + 1] == "zai/glm"
    assert out[out.index("--account") + 1] == "zai-main"
    assert "applied slot=agents.profiles.target.lanes[0] flash-x" in err2.getvalue()


def test_manual_account_switch_terminal_never_logs_in(monkeypatch):
    """AC6-QUEUE: identity-only exhaustion names the manual terminal; fno
    never signs in or re-enables remote control itself."""
    monkeypatch.setenv("FNO_SPAWN_GATE", "1")
    cfg, _state = _pin_capacity(
        monkeypatch,
        extra={"claude": {"makers": "ok", "readyrule": "ok"}},
        active={"claude": "makers"},
    )
    _append_lanes(
        cfg,
        [
            {"name": "alt-a", "harness": "claude", "model": "a", "account": "readyrule"},
            {"name": "alt-b", "harness": "codex", "model": "b", "account": "ghost"},
        ],
        {"lanes": '["alt-a", "alt-b"]', "on_unknown": "skip"},
    )
    # The unknown arm needs a harness with NO stamp: under one proven stamp
    # every other claude account reads mismatch, so ghost rides codex.
    err = io.StringIO()
    with pytest.raises(SystemExit) as exc:
        compose_spawn_argv(
            ["spawn", "--name", "w", "/fno:target x-1"],
            stderr=err,
        )
    assert exc.value.code == 2
    assert "manual canonical account switch" in err.getvalue()
    assert "account_identity_mismatch" in err.getvalue()
    assert "account_identity_unknown (on_unknown=skip)" in err.getvalue()

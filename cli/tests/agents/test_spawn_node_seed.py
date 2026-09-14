"""CLI wiring for the node-seeded spawn: `fno agents spawn --node <id>` with no
typed message reads the node's encoded verb and brief (x-e53e change 1).

The seed render and the brief chain are unit-covered upstream; this file pins
the front door: a node-seeded pane carries verb + brief, the receipt names both
sources, an unencoded node refuses before any peer exists, and a typed message
wins over the node.
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture(autouse=True)
def _isolated(monkeypatch, tmp_path):
    for m in ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID"):
        monkeypatch.delenv(m, raising=False)
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    monkeypatch.setenv("FNO_STATE_ROOT", str(tmp_path / "state"))


_ENCODED = {
    "id": "x-1",
    "slug": "feat",
    "dispatch_verb": "/target",
    "difficulty": "low",
}


def _stub_pane_path(monkeypatch, *, rec=None, brief=("the brief", "explicit")):
    """Stub the graph read, the brief chain, the gate, provenance, and the pane
    spawn; return the kwargs the pane spawn received."""
    from fno.agents import mux_spawn, spawn_gate

    node_id = (rec or {}).get("id")
    if rec is not None:
        monkeypatch.setattr("fno.graph.load.load_graph", lambda: [rec])
    if brief is not None:
        monkeypatch.setattr(
            "fno.provenance.autobrief.resolve_dispatch_brief", lambda n: brief
        )

    class _Gate:
        def release(self) -> None:
            pass

    monkeypatch.setattr(spawn_gate, "run_gate", lambda *a, **k: _Gate())
    monkeypatch.setattr(
        mux_spawn,
        "resolve_provenance",
        lambda *a, **k: {"FNO_NODE": node_id} if node_id else {},
    )

    received: dict = {}

    def fake_pane(**kwargs):
        received.update(kwargs)
        return mux_spawn.MuxSpawnResult(
            name=kwargs["name"], provider=kwargs["provider"], session="s",
            pane_id=1, child_pid=None, session_uuid=None,
        )

    monkeypatch.setattr(mux_spawn, "dispatch_spawn_bounded_pane", fake_pane)
    return received


def _invoke(runner, *args):
    from fno.agents.cli import agents_app

    return runner.invoke(agents_app, ["spawn", "--name", "w1", *args])


def test_node_seeded_pane_carries_verb_and_brief(monkeypatch, runner):
    """AC1-HP: no typed message + encoded node -> the pane seed is the node's
    rendered verb command, the brief rides TARGET_BRIEF, and the receipt names
    both sources."""
    received = _stub_pane_path(monkeypatch, rec=dict(_ENCODED))
    result = _invoke(runner, "--node", "x-1", "--here", "--substrate", "pane")
    assert result.exit_code == 0, result.output
    assert received["message"] == "/target --no-merge x-1"
    assert received["provenance"]["TARGET_BRIEF"] == "the brief"
    receipt = json.loads(
        next(ln for ln in result.output.splitlines() if ln.startswith("{"))
    )
    assert receipt["verb_source"] == "declared"
    assert receipt["brief_source"] == "explicit"


def test_unencoded_node_refuses_before_any_peer(monkeypatch, runner):
    """AC1-ERR: a node with no dispatch_verb and no typed message exits
    non-zero naming the node and the encode remedy; no pane is created."""
    received = _stub_pane_path(monkeypatch, rec={"id": "x-2", "slug": "bare", "difficulty": "low"})
    result = _invoke(runner, "--node", "x-2", "--here", "--substrate", "pane")
    assert result.exit_code != 0
    assert "x-2" in result.output
    assert "fno backlog update x-2 --dispatch-verb" in result.output
    assert received == {}


def test_typed_message_wins_over_the_node(monkeypatch, runner):
    """A typed message is rung zero: the node is never consulted."""
    received = _stub_pane_path(
        monkeypatch,
        rec=dict(_ENCODED),
        # The stub leaves load_graph in place; a consulted graph would raise.
    )
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: (_ for _ in ()).throw(AssertionError("node must not be read")),
    )
    result = _invoke(runner, "--node", "x-1", "--session-phase", "do", "say hi directly", "--here", "--substrate", "pane")
    assert result.exit_code == 0, result.output
    assert received["message"] == "say hi directly"
    assert "TARGET_BRIEF" not in received["provenance"]
    assert "brief_source" not in result.output


def test_mux_session_forwards_to_the_pane_and_refuses_off_pane(monkeypatch, runner):
    """The dispatch-next porcelain pins its lane: --mux-session reaches
    dispatch_spawn_bounded_pane as `session`, and a non-pane substrate refuses."""
    received = _stub_pane_path(monkeypatch, rec=dict(_ENCODED))
    result = _invoke(
        runner, "--node", "x-1", "--here", "--substrate", "pane", "--mux-session", "work"
    )
    assert result.exit_code == 0, result.output
    assert received["session"] == "work"

    result = _invoke(runner, "--name", "w2", "hi", "--substrate", "thread", "--mux-session", "work")
    assert result.exit_code != 0
    assert "pane-only" in result.output


def test_account_stamps_fno_account_for_claude_panes(monkeypatch, runner):
    """(x-c914) The pane's birth account rides the provenance env so the mux
    reads it back for the sideline glyph; claude-gated like the row axis."""
    received = _stub_pane_path(monkeypatch, rec=dict(_ENCODED))
    import fno.agents.account_env as ae
    from fno.agents.account_env import AccountOverlay

    monkeypatch.setattr(
        ae, "resolve_account_overlay",
        lambda *a, **k: AccountOverlay("rr", {"CLAUDE_CONFIG_DIR": "/x/.claude"}, "config-dir"),
    )
    result = _invoke(
        runner, "--node", "x-1", "--here", "--substrate", "pane", "--account", "rr"
    )
    assert result.exit_code == 0, result.output
    assert received["provenance"]["FNO_ACCOUNT"] == "rr"


# ---- x-3873 change 1: the door ensures the worktree (AC1-*) ----------------


def test_node_seeded_spawn_launches_in_the_ensured_worktree(monkeypatch, runner, tmp_path):
    """AC1-HP: no typed message and no explicit cwd source -> the ensure runs
    once with the node's recorded cwd, the resolved --name and the resolved
    harness, and the worker launches in the path it printed."""
    received = _stub_pane_path(monkeypatch, rec=dict(_ENCODED, cwd="/repo"))
    seen: dict = {}

    def fake_ensure(recorded_cwd, agent_name, harness):
        seen["args"] = (str(recorded_cwd), agent_name, harness)
        return str(tmp_path / "wt")

    monkeypatch.setattr(
        "fno.agents.node_dispatch._worktree_ensure_for_launch", fake_ensure
    )
    result = _invoke(runner, "--node", "x-1", "--substrate", "pane")
    assert result.exit_code == 0, result.output
    assert seen["args"] == ("/repo", "w1", "claude")
    assert received["cwd"] == (tmp_path / "wt").resolve()


def test_ensure_refusal_holds_the_node(monkeypatch, runner):
    """AC1-ERR: a None ensure answer exits 2 naming the node and the hold;
    no peer is created."""
    received = _stub_pane_path(monkeypatch, rec=dict(_ENCODED))
    monkeypatch.setattr(
        "fno.agents.node_dispatch._worktree_ensure_for_launch", lambda *a: None
    )
    result = _invoke(runner, "--node", "x-1", "--substrate", "pane")
    assert result.exit_code == 2
    assert "worktree ensure refused or misconfigured for x-1" in result.output
    assert received == {}


def test_typed_here_skips_the_ensure(monkeypatch, runner):
    """AC1-EDGE (--here): the caller opted in; the ensure is never consulted."""
    received = _stub_pane_path(monkeypatch, rec=dict(_ENCODED))

    def boom(*a):
        raise AssertionError("ensure must not run")

    monkeypatch.setattr(
        "fno.agents.node_dispatch._worktree_ensure_for_launch", boom
    )
    result = _invoke(runner, "--node", "x-1", "--here", "--substrate", "pane")
    assert result.exit_code == 0, result.output


def test_typed_cwd_skips_the_ensure(monkeypatch, runner, tmp_path):
    """AC1-EDGE (--cwd): the caller's explicit dir wins, unchanged."""
    received = _stub_pane_path(monkeypatch, rec=dict(_ENCODED))

    def boom(*a):
        raise AssertionError("ensure must not run")

    monkeypatch.setattr(
        "fno.agents.node_dispatch._worktree_ensure_for_launch", boom
    )
    result = _invoke(
        runner,
        "--node", "x-1", "--substrate", "pane",
        "--cwd", str(tmp_path),
    )
    assert result.exit_code == 0, result.output
    assert received["cwd"] == tmp_path.resolve()

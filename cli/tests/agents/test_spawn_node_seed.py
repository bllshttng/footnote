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

from fno.rust_binary import find_dev_binary

requires_rust = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present (build with `cargo build -p fno-agents`)",
)


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


def test_typed_message_without_a_node_is_never_consulted(monkeypatch, runner):
    """Rung zero holds when there is no `--node`: a typed message passes
    through and the graph is never read."""
    received = _stub_pane_path(
        monkeypatch,
        rec=dict(_ENCODED),
    )
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: (_ for _ in ()).throw(AssertionError("node must not be read")),
    )
    result = _invoke(runner, "--session-phase", "do", "say hi directly", "--here", "--substrate", "pane")
    assert result.exit_code == 0, result.output
    assert received["message"] == "say hi directly"
    assert "TARGET_BRIEF" not in received["provenance"]


def test_typed_message_with_a_node_composes_the_nodes_command(monkeypatch, runner):
    """with `--node`, a prose message gains the node's derived
    command in front; the node row is read and the brief rides along."""
    received = _stub_pane_path(monkeypatch, rec=dict(_ENCODED))
    result = _invoke(runner, "--node", "x-1", "say hi directly", "--here", "--substrate", "pane")
    assert result.exit_code == 0, result.output
    assert received["message"] == "/fno:target x-1\n\nsay hi directly"
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


# ---------------------------------------------------------------------------
# the seam projects facts to `fno-agents node-seed` and applies the
# answer before any lane is chosen. The transport is stubbed here; the
# requires_rust test drives the real binary.
# ---------------------------------------------------------------------------


def _row(**fields):
    row = {"id": "x-1", "slug": "seed", "dispatch_verb": "/blueprint", "difficulty": "medium"}
    row.update(fields)
    return row


def _stub_row(monkeypatch, row):
    monkeypatch.setattr("fno.graph.load.load_graph", lambda: [row] if row else [])


def _stub_verb(monkeypatch, answer=None, *, unavailable=None):
    import fno.rust_binary as rb

    seen: list = []

    def _call(verb, payload, unavailable_cls, **kw):
        seen.append(payload)
        if unavailable is not None:
            raise unavailable_cls(str(unavailable))
        return answer

    monkeypatch.setattr(rb, "verb_call", _call)
    return seen


def _seed_args(*extra):
    # Real spawn argv: the name rides --name/--cwd, so the first positional
    # (when present) is the message seed.
    return ["spawn", *extra]


def test_seam_refuses_on_the_verbs_refuse_answer(monkeypatch):
    """AC3-HP: a refuse answer exits 2 with the seam prefix before any
    defaults injection; nothing downstream runs."""
    _stub_row(monkeypatch, _row())
    _stub_verb(monkeypatch, {"action": "refuse", "message": "--node x-1 derives /blueprint; the payload names /target. Drop the verb from the payload: --node supplies it."})
    monkeypatch.setattr(
        "fno.agents.spawn_defaults.inject_spawn_defaults",
        lambda *a, **k: (_ for _ in ()).throw(AssertionError("inject must not run")),
    )
    from fno.agents.rust_runtime import _node_seed_at_seam

    with pytest.raises(SystemExit) as exc:
        _node_seed_at_seam(_seed_args("/fno:target x-1", "--node", "x-1"))
    assert exc.value.code == 2


def test_seam_names_the_binary_when_the_verb_is_unavailable(monkeypatch, capsys):
    """AC3-HP: VerbUnavailable exits 2 naming the transport, never a silent
    fallback to the crown profile."""
    _stub_row(monkeypatch, _row())

    class _Unavailable(Exception):
        pass

    _stub_verb(monkeypatch, unavailable="fno-agents: no binary")
    import fno.rust_binary as rb

    monkeypatch.setattr(rb, "VerbUnavailable", _Unavailable)
    from fno.agents.rust_runtime import _node_seed_at_seam

    with pytest.raises(SystemExit) as exc:
        _node_seed_at_seam(_seed_args("port it", "--node", "x-1"))
    assert exc.value.code == 2
    assert "node-seed verb unavailable" in capsys.readouterr().err


def test_seam_skips_without_an_explicit_node_flag(monkeypatch):
    """AC3-HP: no `--node` (with or without FNO_NODE) calls no verb and reads
    no graph: the env var is provenance, never a decision."""
    monkeypatch.setenv("FNO_NODE", "x-1")

    def boom_graph():
        raise AssertionError("graph must not be read")

    monkeypatch.setattr("fno.graph.load.load_graph", boom_graph)
    seen = _stub_verb(monkeypatch, {"action": "pass"})
    from fno.agents.rust_runtime import _node_seed_at_seam

    args, node_verb = _node_seed_at_seam(_seed_args("port it"))
    assert seen == []
    assert node_verb is None
    assert args[1] == "port it"


def test_seam_projects_the_row_facts_into_the_payload(monkeypatch):
    """AC3-HP: row_found, effective/stored verb, family and the shifted seed
    slot ride one payload."""
    _stub_row(monkeypatch, _row(dispatch_verb="$fno:blueprint"))
    seen = _stub_verb(monkeypatch, {"action": "pass"})
    from fno.agents.rust_runtime import _node_seed_at_seam

    _node_seed_at_seam(_seed_args("port it", "--node", "x-1"))
    p = seen[0]
    assert p["row_found"] is True
    assert p["effective_verb"] == "/blueprint"
    assert p["stored_verb"] == "/blueprint"
    assert p["family"] == ["/target", "/blueprint"]
    assert p["crown"] is False
    assert p["resume"] is False
    assert p["argv"][0] == "spawn"
    assert p["seed_index"] == 1
    assert p["seed_form"] == "positional"


def test_seam_derive_failure_rides_as_derive_error(monkeypatch):
    from fno.agents.harness_map import DispatchResolveError
    from fno.agents import node_dispatch as nd

    _stub_row(monkeypatch, _row())
    monkeypatch.setattr(
        nd, "node_effective_verb",
        lambda row, **k: (_ for _ in ()).throw(DispatchResolveError("rung answers nothing")),
    )
    seen = _stub_verb(monkeypatch, {"action": "refuse", "message": "x"})
    from fno.agents.rust_runtime import _node_seed_at_seam

    with pytest.raises(SystemExit):
        _node_seed_at_seam(_seed_args("/fno:target x-1", "--node", "x-1"))
    assert seen[0]["derive_error"] == "rung answers nothing"
    assert seen[0]["effective_verb"] is None


def test_seam_compose_rewrites_the_seed_slot(monkeypatch):
    _stub_row(monkeypatch, _row())
    composed = ["spawn", "/fno:blueprint x-1\n\nport it", "--node", "x-1"]
    _stub_verb(monkeypatch, {"action": "compose", "verb": "blueprint", "argv": composed})
    from fno.agents.rust_runtime import _node_seed_at_seam

    args, node_verb = _node_seed_at_seam(_seed_args("port it", "--node", "x-1"))
    assert args == composed
    assert node_verb is None


def test_seam_profile_returns_the_derived_verb(monkeypatch):
    _stub_row(monkeypatch, _row())
    _stub_verb(monkeypatch, {"action": "profile", "verb": "blueprint"})
    from fno.agents.rust_runtime import _node_seed_at_seam

    args, node_verb = _node_seed_at_seam(_seed_args("--node", "x-1", "--substrate", "thread"))
    assert node_verb == "blueprint"
    assert args[1] == "--node"


def test_seam_crown_and_resume_pass_with_their_flags_set(monkeypatch):
    """AC4-EDGE: crown/resume spawns answer pass and carry the flag as a
    payload fact; the profile stays crown."""
    _stub_row(monkeypatch, _row())
    seen = _stub_verb(monkeypatch, {"action": "pass"})
    from fno.agents.rust_runtime import _node_seed_at_seam

    _node_seed_at_seam(_seed_args("--node", "x-1", "--crown"))
    assert seen[0]["crown"] is True
    _node_seed_at_seam(_seed_args("--node", "x-1", "--resume", "sid-9"))
    assert seen[1]["resume"] is True


@requires_rust
def test_seam_real_binary_refuses_a_disagreeing_verb(monkeypatch):
    """AC3-HP on the real transport: the compiled node-seed verb refuses the
    disagreeing family verb and passes the agreeing one; no stubbing of the
    answer, only of the graph row."""
    _stub_row(monkeypatch, _row())
    from fno.agents.rust_runtime import _node_seed_at_seam

    with pytest.raises(SystemExit) as exc:
        _node_seed_at_seam(_seed_args("/fno:target x-1", "--node", "x-1"))
    assert exc.value.code == 2

    args, node_verb = _node_seed_at_seam(_seed_args("/fno:blueprint x-1", "--node", "x-1"))
    assert node_verb is None

"""Unit tests for `fno agents dispatch next` (x-6f77, collapsed x-e53e): the mux
leader+g porcelain.

The verb owns no launch of its own since x-e53e: node selection, the ONE
preference resolver, the worktree resolver, and a single `fno agents spawn`
shellout. The subprocess boundary is faked at `subprocess.run` (the receipt
JSON the door prints comes back), the grid lane at `advance._grid_lane_for`, so
the REAL `resolve_node_spawn` runs and the verdict mapping is exercised
end to end. The family-2 guard, the spawn gate, provenance, and the pane live
in the door process and are covered by the spawn-door suites.
"""

from __future__ import annotations

import json
import subprocess as _subprocess
from types import SimpleNamespace

import pytest

from fno import dispatch


def test_registered_and_addressable():
    """The verb is named for what it does (x-e53e: `next` - select and resolve;
    the spawn door launches), and the pre-collapse `one` spelling stays as a
    hidden deprecated alias."""
    from fno.cli import LAZY_SUBCOMMANDS

    assert "dispatch" in LAZY_SUBCOMMANDS
    commands = {c.name: c for c in dispatch.dispatch_app.registered_commands}
    assert "next" in commands
    assert "one" in commands
    assert commands["one"].hidden is True
    assert not commands["next"].hidden


def _receipt_line(**over) -> str:
    receipt = {
        "name": "t-x-1", "short_id": "abcd1234", "harness": "claude",
        "status": "live", "mux_session": "s", "pane_id": 7, "bound": True,
        "seed": "submitted", "pane_observation": "painted",
    }
    receipt.update(over)
    return json.dumps(receipt)


def _wire(monkeypatch, tmp_path, *, next_node=None, proc=None, resolve=None):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    # A real selection projection row (x-0961/x-ebd2): planless low is the
    # law's straight-to-target intake, so the node-aware command resolve
    # derives instead of refusing. Caller-supplied values still win.
    if next_node is not None:
        next_node = {"difficulty": "low", "dispatch_verb": "", **next_node}
    monkeypatch.setattr(dispatch, "_next_node", lambda project: next_node)
    # The name mint reads its word-code payload from the fno-agents binary;
    # unit tests stub the mint and its code table at the sources the resolver
    # imports lazily (advance + naming).
    monkeypatch.setattr(
        "fno.backlog.advance._worker_agent_name",
        lambda node_id, node_slug, *, source=None, verb_code="t": f"{verb_code}-{node_id}",
    )
    monkeypatch.setattr(
        "fno.agents.naming.verb_code_for",
        lambda word: {"target": "t", "blueprint": "b", "review": "r", "think": "k"}.get(
            (word or "target").strip().lstrip("/$").removeprefix("fno:").lstrip("/")
        ) or "t",
    )
    # The grid consult shells `fno-agents route-slot` through the SAME
    # subprocess.run this file fakes; default it to a decline so `calls`
    # carries only the launch. Tests that want a pick use _grid().
    monkeypatch.setattr(
        "fno.backlog.advance._grid_lane_for",
        lambda node, *, model=None, provider=None, verb=None: (None, None, None, None, None),
    )
    # The account overlay resolves CLI-side before anything is spent; the
    # refusal test overrides this with a raising stub.
    monkeypatch.setattr(
        "fno.agents.account_env.resolve_account_overlay",
        lambda acc: SimpleNamespace(env={"CLAUDE_CONFIG_DIR": "/x/.claude-alt"}),
    )
    # The launch cwd resolver: the repo-root (never-policy) answer.
    monkeypatch.setattr(
        dispatch, "_worktree_ensure_for_launch", lambda cwd, name, harness: str(cwd)
    )
    if resolve is not None:
        monkeypatch.setattr(
            "fno.agents.node_dispatch.resolve_node_spawn", resolve
        )

    calls: list = []

    def fake_run(cmd, **kwargs):
        calls.append({"cmd": cmd, **kwargs})
        if proc is not None:
            return proc()
        return _subprocess.CompletedProcess(
            cmd, 0, stdout=_receipt_line() + "\n", stderr=""
        )

    monkeypatch.setattr(dispatch.subprocess, "run", fake_run)
    return calls


def _grid(monkeypatch, harness, *, model=None, route=None, account=None, why="grid"):
    monkeypatch.setattr(
        "fno.backlog.advance._grid_lane_for",
        lambda node, *, model=None, provider=None, verb=None: (
            harness, model, route, account, why
        ),
    )


def test_no_ready_work(monkeypatch, tmp_path):
    calls = _wire(monkeypatch, tmp_path, next_node=None)
    v = dispatch._dispatch_one(session="main", node=None, project=None)
    assert v["outcome"] == "no-work"
    assert calls == []


def test_launched_shells_spawn_with_the_resolved_preferences(monkeypatch, tmp_path):
    """The new answer (x-e53e change 3): a node whose grid lane resolves codex
    is dispatched onto codex - the pane pin and the session lane are the only
    things this verb pins. The seed command carries the codex surface, the
    worker name is the resolver's mint, and the receipt facts flow through."""
    calls = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "feat", "cwd": str(tmp_path)}
    )
    _grid(monkeypatch, "codex", why="grid:codex")
    v = dispatch._dispatch_one(session="work", node=None, project=None)
    assert v["outcome"] == "launched"
    assert v["node"] == "x-1"
    assert v["pane_id"] == 7
    assert v["seed_verified"] is True
    assert len(calls) == 1
    cmd = calls[0]["cmd"]
    spawn_at = cmd.index("spawn")
    assert cmd[spawn_at - 1 : spawn_at + 1] == ["agents", "spawn"]
    def val(flag):
        return cmd[cmd.index(flag) + 1]
    assert val("--harness") == "codex"
    assert val("--substrate") == "pane"
    assert val("--mux-session") == "work"
    assert val("--node") == "x-1"
    # The codex command surface, from the one resolver's render.
    assert cmd[-1] == "$fno:target --no-merge x-1"
    assert val("--cwd") == str(tmp_path)


def test_claude_grid_lane_renders_the_claude_surface(monkeypatch, tmp_path):
    calls = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "feat", "cwd": str(tmp_path)}
    )
    _grid(monkeypatch, "claude")
    v = dispatch._dispatch_one(session="work", node=None, project=None)
    assert v["outcome"] == "launched"
    assert calls[0]["cmd"][calls[0]["cmd"].index("--harness") + 1] == "claude"
    assert calls[0]["cmd"][-1] == "/target --no-merge x-1"


def test_parented_child_uses_parent_as_pane_group(monkeypatch, tmp_path):
    calls = _wire(
        monkeypatch,
        tmp_path,
        next_node={
            "id": "x-parser", "slug": "parser", "parent": "x-feature", "cwd": str(tmp_path),
        },
    )
    v = dispatch._dispatch_one(session="work", node=None, project=None)
    assert v["outcome"] == "launched"
    cmd = calls[0]["cmd"]
    assert cmd[cmd.index("--tab") + 1] == "x-feature"
    assert cmd[cmd.index("--name") + 1] == "t-x-parser"


def test_no_second_launcher_in_this_file(monkeypatch, tmp_path):
    """One launcher: the module holds no pane-spawn import to reach past the
    front door, and exactly one subprocess fires."""
    calls = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "feat", "cwd": str(tmp_path)}
    )
    assert not hasattr(dispatch, "dispatch_spawn_bounded_pane")
    assert not hasattr(dispatch, "dispatch_spawn_pane")
    v = dispatch._dispatch_one(session="work", node=None, project=None)
    assert v["outcome"] == "launched"
    assert len(calls) == 1


@pytest.mark.parametrize(
    ("seed", "observation", "expected"),
    [
        ("submitted", "unreadable", False),
        ("submitted", "painted", True),
        ("submitted", "blank", True),
        (None, None, False),
    ],
)
def test_seed_doubt_reaches_the_verdict(
    monkeypatch, tmp_path, seed, observation, expected
):
    """The two-field split (x-6f77 review): `submitted` alone certifies
    nothing; an unreadable pane keeps `seed_verified` false."""
    _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-1", "slug": "feat", "cwd": str(tmp_path)},
        proc=lambda: _subprocess.CompletedProcess(
            [], 0,
            stdout=_receipt_line(seed=seed, pane_observation=observation) + "\n",
            stderr="",
        ),
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["seed"] == seed
    assert v["pane_observation"] == observation
    assert v["seed_verified"] is expected


def _refused(stderr, code=2):
    return _subprocess.CompletedProcess([], code, stdout="", stderr=stderr)


def test_same_node_second_dispatch_maps_already_dispatching(monkeypatch, tmp_path):
    """The door's family-2 guard refused: a peer won the node handover (or the
    race re-closed it). The verdict is already-dispatching and no second
    reservation was ever taken in this file."""
    _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)},
        proc=lambda: _refused(
            "node dispatch refused: node=x-1 verdict=already-running "
            "reason=reservation-held prior_holder=spawn-handover:t-x-1; "
            "no worker launched",
        ),
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "already-dispatching"
    assert v["node"] == "x-1"


@pytest.mark.parametrize("reason", ["auto-deferred", "defer-failed"])
def test_manual_dispatch_preserves_family2_refusal_reason(
    monkeypatch, tmp_path, reason
):
    _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)},
        proc=lambda: _refused(
            f"node dispatch refused: node=x-1 verdict=refused reason={reason}; "
            "no worker launched",
        ),
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == reason


def test_guard_infra_fault_maps_failed(monkeypatch, tmp_path):
    _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)},
        proc=lambda: _refused(
            "node dispatch refused: node=x-1 verdict=error reason=corrupted; "
            "no worker launched",
        ),
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "failed"


def test_gate_refusal_keeps_its_receipt_and_exit_code(monkeypatch, tmp_path):
    """The door prints the gate receipt JSON and exits with the gate's code;
    the pre-port GateRefused propagated exactly that through this verb."""
    from fno.agents import spawn_gate

    gate_receipt = json.dumps({"reason": "slots-full", "retry_at": 123})
    _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-7", "slug": "g", "cwd": str(tmp_path)},
        proc=lambda: _subprocess.CompletedProcess(
            [], spawn_gate.EXIT_NO_WAIT, stdout=gate_receipt + "\n", stderr=""
        ),
    )
    with pytest.raises(SystemExit) as exc:
        dispatch._dispatch_one(session="s", node=None, project=None)
    assert exc.value.code == spawn_gate.EXIT_NO_WAIT


def test_spawn_failure_maps_failed(monkeypatch, tmp_path):
    _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-9", "slug": "z", "cwd": str(tmp_path)},
        proc=lambda: _refused("fno agents spawn: mux pane spawn failed", code=1),
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "failed"
    assert "spawn failed" in v["detail"]


def test_exit_zero_without_a_pane_receipt_is_failed(monkeypatch, tmp_path):
    _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)},
        proc=lambda: _subprocess.CompletedProcess([], 0, stdout="", stderr=""),
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "failed"
    assert "receipt" in v["detail"]


# --- preference resolution through the ONE resolver --------------------------


def test_lossy_node_projection_fails_the_verdict(monkeypatch, tmp_path):
    """x-0961: an explicit --node whose record lost the dispatch_verb key is a
    lossy projection - the resolver refuses before anything is spent."""
    calls = _wire(monkeypatch, tmp_path)
    monkeypatch.setattr(
        dispatch, "_lookup_node", lambda ref: {"id": "x-9", "slug": "s", "cwd": str(tmp_path)}
    )
    v = dispatch._dispatch_one(session="s", node="x-9", project=None)
    assert v["outcome"] == "failed"
    assert "lossy" in v["detail"]
    assert calls == []


def test_account_threads_to_argv_and_refuses_before_spawn(monkeypatch, tmp_path):
    """--account rides argv (the door applies the overlay where the harness is
    exec'd); a stale account fails the verdict before any subprocess."""
    calls = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)}
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None, account="rr")
    assert v["outcome"] == "launched"
    cmd = calls[0]["cmd"]
    assert cmd[cmd.index("--account") + 1] == "rr"

    from fno.agents.account_env import AccountResolutionError

    def boom(acc):
        raise AccountResolutionError("no such account 'rr'")

    monkeypatch.setattr("fno.agents.account_env.resolve_account_overlay", boom)
    v = dispatch._dispatch_one(session="s", node=None, project=None, account="rr")
    assert v["outcome"] == "failed"
    assert "rr" in v["detail"]
    assert len(calls) == 1  # the earlier launch only


def test_ensure_refusal_holds_the_node_and_spawns_nothing(monkeypatch, tmp_path):
    """An empty ensure answer (policy refusal / misconfig) is a failed verdict
    naming the hold, with no pane spawned."""
    calls = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-2", "slug": "b", "cwd": str(tmp_path)}
    )
    monkeypatch.setattr(
        dispatch, "_worktree_ensure_for_launch", lambda cwd, name, harness: None
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "failed"
    assert "canonical main" in v["detail"]
    assert calls == []


# --- the quota route stays the same decision the advance path reads ----------


def _route(action, **kw):
    from fno.agents.autonomous_route import AutonomousRoute

    return AutonomousRoute(action, kw.pop("reason", "test"), **kw)


def test_quota_deferred_verdict(monkeypatch, tmp_path):
    _wire(
        monkeypatch, tmp_path, next_node={"id": "x-3", "slug": "q", "cwd": str(tmp_path)}
    )
    monkeypatch.setattr(
        "fno.agents.autonomous_route.select_autonomous_route",
        lambda **kw: _route(
            "defer", source_record="ccr", window="4h", retry_at=1770000000.0
        ),
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "quota-deferred"
    assert v["provider"] == "ccr"
    assert v["headroom"] == "4h"


def test_cutover_pins_harness_and_record_and_no_merge(monkeypatch, tmp_path):
    """A cutover replaces harness + account together (one resolver answer), and
    quota must not change who may merge: TARGET_NO_MERGE rides the env."""
    calls = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-4", "slug": "c", "cwd": str(tmp_path)}
    )
    monkeypatch.setattr(
        "fno.agents.autonomous_route.select_autonomous_route",
        lambda **kw: _route(
            "cutover", source_record="ccm", record_id="rr2", harness="codex",
            account_env={"CODEX_HOME": "/x"}, window="2h", defer_fallback=True,
        ),
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "launched"
    cmd = calls[0]["cmd"]
    assert cmd[cmd.index("--harness") + 1] == "codex"
    assert cmd[cmd.index("--dispatch-account") + 1] == "rr2"
    assert calls[0]["env"]["TARGET_NO_MERGE"] == "1"


def test_explicit_node_never_quota_defers(monkeypatch, tmp_path):
    """LD#5: an explicit --node is a human verb - the quota probe never runs."""
    calls = _wire(monkeypatch, tmp_path)
    monkeypatch.setattr(
        dispatch, "_lookup_node",
        lambda ref: {
            "id": "x-5", "slug": "e", "cwd": str(tmp_path),
            "difficulty": "low", "dispatch_verb": "",
        },
    )
    monkeypatch.setattr(
        "fno.agents.autonomous_route.select_autonomous_route",
        lambda **kw: (_ for _ in ()).throw(AssertionError("quota probe must not run")),
    )
    v = dispatch._dispatch_one(session="s", node="x-5", project=None)
    assert v["outcome"] == "launched"
    assert len(calls) == 1


# --- `fno agents dispatch resolve` (query verbs, unchanged) ------------------


def _resolve_cli(*args):
    from typer.testing import CliRunner

    return CliRunner().invoke(dispatch.dispatch_app, ["resolve", *args])


def test_capabilities_query_ignores_dispatch_substrate_config():
    import json
    from typer.testing import CliRunner

    result = CliRunner().invoke(dispatch.dispatch_app, ["capabilities", "codex", "--json"])
    assert result.exit_code == 0
    out = json.loads(result.stdout)
    assert out["harness"] == "codex"
    assert out["ready_marker"] == "idle_prompt"
    # codex pins ["enter"] (measured against 0.148.0); this test is about the
    # capability query ignoring dispatch substrate config, not the value.
    assert out["submit_keys"] == ["enter"]
    assert out["resume_strategy"]["forms"]["headless_resume"]["tokens"] == [
        "codex", "exec", "resume", "{session_id}"
    ]


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


# ---------------------------------------------------------------------------
# x-d1f4: `fno agents dispatch resolve` auto-resolves the brief from --node
# ---------------------------------------------------------------------------


def test_resolve_auto_brief_from_node_details(monkeypatch):
    """With --node but no --brief, the porcelain resolves the node's brief chain
    (here details -> synthesis) into env.TARGET_BRIEF, so the /target bg shell
    dispatcher routing through it carries context, not an empty brief."""
    import json

    monkeypatch.setattr(
        dispatch, "_lookup_node",
        lambda ref: {
            "id": "x-9", "title": "Retry",
            "details": "exponential backoff " * 5,
            "difficulty": "low", "dispatch_verb": "",
        },
    )
    r = _resolve_cli("--node", "x-9", "-J")
    assert r.exit_code == 0
    out = json.loads(r.stdout)
    assert "exponential backoff" in out["env"]["TARGET_BRIEF"]
    assert out["brief_source"] == "synth-details"


def test_resolve_explicit_brief_still_wins_over_auto(monkeypatch):
    """An explicit --brief is rung 1: it rides verbatim and the node is never
    consulted for a SYNTHESIZED brief. The node still loads (x-ebd2: the
    lifecycle verb derives from it either way) - only the brief chain skips."""
    import json

    monkeypatch.setattr(
        dispatch, "_lookup_node",
        lambda ref: {"id": "x-9", "difficulty": "low", "dispatch_verb": ""},
    )
    import fno.provenance.autobrief as autobrief

    monkeypatch.setattr(
        autobrief, "resolve_dispatch_brief",
        lambda n: (_ for _ in ()).throw(AssertionError("must not synthesize a brief")),
    )
    r = _resolve_cli("--node", "x-9", "--brief", "hand set", "-J")
    assert r.exit_code == 0
    out = json.loads(r.stdout)
    assert out["env"]["TARGET_BRIEF"] == "hand set"
    assert out["brief_source"] == "explicit"


def test_resolve_honors_an_out_of_family_stored_verb(monkeypatch):
    """x-ebd2 LD2: an out-of-family stored verb (/think) keeps declared
    precedence through the resolve door - the lifecycle table abstains on it
    instead of deriving target/blueprint from the node's lifecycle."""
    import json

    monkeypatch.setattr(
        dispatch, "_lookup_node",
        lambda ref: {
            "id": "x-9", "difficulty": "high", "dispatch_verb": "/fno:think",
        },
    )
    r = _resolve_cli("--node", "x-9", "-J")
    assert r.exit_code == 0
    out = json.loads(r.stdout)
    assert out["command"] == "/think x-9"
    assert out["verb"] is None  # the lifecycle abstained; declared verb ran


def test_resolve_no_node_no_brief_is_none(monkeypatch):
    """No node + no brief -> no auto-resolve, brief_source=none, no TARGET_BRIEF."""
    import json

    r = _resolve_cli("--verb", "/target", "-J")
    assert r.exit_code == 0
    out = json.loads(r.stdout)
    assert out["brief_source"] == "none"
    assert out["env"].get("TARGET_BRIEF") is None

"""Unit tests for `fno agents dispatch one` (x-6f77): the mux leader+g porcelain.

The shared guard's claims are held for real against an isolated
`FNO_CLAIMS_ROOT`, so the reservation and the release-on-failure path are
genuinely exercised. Selection (`_next_node`), the family-2 guard
(`_spawn_guard_decision`), the spawn gate (`run_gate`), the worktree resolver
and the pane spawn (`dispatch_spawn_pane`) are monkeypatched - no real
`fno backlog next` subprocess, no real gate, no real pane.
"""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from fno import dispatch


def test_registered_and_addressable():
    """The verb is wired into the root CLI and the single-command sub-app does
    not collapse (the no-op callback keeps `one` addressable)."""
    from fno.cli import LAZY_SUBCOMMANDS

    assert "dispatch" in LAZY_SUBCOMMANDS
    names = [c.name for c in dispatch.dispatch_app.registered_commands]
    assert "one" in names


class _FakeSpawnOK:
    pane_id = 7
    # The dispatcher's `launched` return reports whether the worker actually
    # bound a session, not just whether a pane was created. A stub that omits it
    # is claiming a successful launch it cannot answer for.
    bound = True


class _FakeGate:
    """Stands in for run_gate's GateGuard: records its release."""

    def __init__(self):
        self.releases = 0

    def release(self):
        self.releases += 1


def _wire(monkeypatch, tmp_path, *, next_node=None, spawn=None):
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    monkeypatch.setattr(dispatch, "_next_node", lambda project: next_node)
    monkeypatch.setattr(dispatch, "_worker_agent_name", lambda nid, slug: f"target-{nid}")
    monkeypatch.setattr(dispatch, "resolve_provenance", lambda nid, slug: {})

    # The family-2 guard is faked at its import source (dispatch imports it
    # inside _dispatch_one from fno.agents.cli): dispatchable verdict with the
    # reservation + handover node claim a real guard takes.
    def fake_guard(node_id, holder, *, cwd=None, handover_holder=None):
        return {
            "verdict": "dispatchable",
            "reservation_key": f"dispatch:{node_id}",
            "reservation_holder": holder,
            "node_claim_key": f"node:{node_id}",
            "node_claim_holder": handover_holder or "spawn-handover:t",
        }, 0

    monkeypatch.setattr("fno.agents.cli._spawn_guard_decision", fake_guard)

    gate = _FakeGate()
    monkeypatch.setattr("fno.agents.spawn_gate.run_gate", lambda *a, **kw: gate)

    # The launch cwd resolver: the repo-root (never-policy) answer.
    monkeypatch.setattr(
        dispatch, "_worktree_ensure_for_launch", lambda cwd, name, harness: str(cwd)
    )

    calls: list = []

    def fake_spawn(**kwargs):
        calls.append(kwargs)
        if spawn is not None:
            return spawn()
        return _FakeSpawnOK()

    monkeypatch.setattr(dispatch, "dispatch_spawn_pane", fake_spawn)
    return calls, gate


def test_no_ready_work(monkeypatch, tmp_path):
    _wire(monkeypatch, tmp_path, next_node=None)
    v = dispatch._dispatch_one(session="main", node=None, project=None)
    assert v["outcome"] == "no-work"


def test_launched_passes_the_guard_and_spawns(monkeypatch, tmp_path):
    calls, gate = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "feat", "cwd": str(tmp_path)}
    )
    v = dispatch._dispatch_one(session="work", node=None, project=None)
    assert v["outcome"] == "launched"
    assert v["node"] == "x-1"
    assert v["pane_id"] == 7
    assert calls[0]["session"] == "work"
    assert calls[0]["message"] == "/target --no-merge x-1"
    # The gate's guard rode into the spawn (AC2-EDGE) and was released once the
    # registry row existed.
    assert calls[0]["provider_gate"] is gate
    assert gate.releases == 1


def _real_result(**fields):
    """A real `MuxSpawnResult`, never a SimpleNamespace.

    `_dispatch_one` reads its fields with `getattr(..., None)`, so a stub with a
    misspelled field name answers None and the test passes while measuring
    nothing. The dataclass refuses the typo instead."""
    from fno.agents.mux_spawn import MuxSpawnResult

    return MuxSpawnResult(
        name="w",
        provider="claude",
        session="main",
        pane_id=7,
        child_pid=None,
        session_uuid=None,
        bound=True,
        **fields,
    )


def test_seed_verified_is_false_for_a_pane_nobody_could_see(monkeypatch, tmp_path):
    """AC5-EDGE. The sharpest trap in the two-field split.

    `seed == "submitted"` used to be the whole test, and it was correct while
    the seed word also carried pane doubt. The moment an argv seed onto an
    unreadable frame started reporting `submitted` - which is honest about the
    payload - that expression began handing every dispatcher a false
    `seed_verified: true`. That is a worse lie than the `unattempted` this
    change removed, because it reads as proof rather than as an absence, and
    `dispatch_notice` renders it straight to an operator."""
    _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-1", "slug": "feat", "cwd": str(tmp_path)},
        spawn=lambda: _real_result(
            seed="submitted", seed_source="argv", pane_observation="unreadable"
        ),
    )

    v = dispatch._dispatch_one(session="work", node=None, project=None)

    assert v["outcome"] == "launched"
    assert v["seed"] == "submitted"
    assert v["pane_observation"] == "unreadable"
    assert v["seed_verified"] is False


@pytest.mark.parametrize("observation", ["painted", "blank"])
def test_seed_verified_survives_an_observed_pane(monkeypatch, tmp_path, observation):
    """The positive control: the clause narrows, it does not refuse everything.

    `blank` is included on purpose. An unpainted TUI is a live worker whose
    frame simply has not drawn, and folding it in with `unreadable` would fail
    every fast dispatch - the false negative this whole family started with."""
    _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-1", "slug": "feat", "cwd": str(tmp_path)},
        spawn=lambda: _real_result(
            seed="submitted", seed_source="argv", pane_observation=observation
        ),
    )

    v = dispatch._dispatch_one(session="work", node=None, project=None)

    assert v["seed_verified"] is True


def test_full_fleet_refuses_in_the_gate_not_a_verdict(monkeypatch, tmp_path):
    """The positive marker x-68fd asked for, as a test: `agents.max_live` at 1
    with one live registry row held must refuse, and the census must still
    report 1. `lanes-full` is gone from the vocabulary; a full fleet refuses
    instantly (no_wait) with the gate's own exit code."""
    from fno.agents import spawn_gate
    from fno.agents.registry import AgentEntry

    real_run_gate = spawn_gate.run_gate  # _wire stubs it; this test needs the real one
    _wire(monkeypatch, tmp_path, next_node={"id": "x-2", "slug": "b", "cwd": str(tmp_path)})
    monkeypatch.setattr("fno.agents.spawn_gate.run_gate", real_run_gate)
    monkeypatch.delenv("FNO_SPAWN_GATE", raising=False)  # conftest disables it

    def fake_settings():
        class _A:
            max_live = 1
            min_free_gb = 0.0
            max_load_per_cpu = 0.0
            provider_limits = {"zai": 5}

        class _S:
            agents = _A()

        return _S()

    monkeypatch.setattr("fno.config.load_settings", fake_settings)
    import os

    live_row = AgentEntry(
        name="held-worker", harness="claude", cwd="/tmp", log_path="/tmp/l",
        status="live", pid=os.getpid(),  # a live pid or the row reads dead
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [live_row])
    monkeypatch.setattr("fno.agents.session_procs.bg_socket_pid_map", lambda root=None: {})
    census = spawn_gate.census()
    monkeypatch.setattr(spawn_gate, "census", lambda: census)

    with pytest.raises(SystemExit) as exc:
        dispatch._dispatch_one(session="s", node=None, project=None)

    assert exc.value.code == spawn_gate.EXIT_NO_WAIT
    assert census.slot_count == 1  # the held row is still the only live slot


def test_same_node_second_dispatch_is_deduped(monkeypatch, tmp_path):
    # Two fast leader+g resolve _next_node to the SAME node before the first
    # worker claims it. The guard's dispatch:<id> reservation must make the
    # second a no-op (already-dispatching) - never a second spawn, and never a
    # release of the first worker's live reservation (the P1 race).
    calls, _gate = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)}
    )
    assert dispatch._dispatch_one(session="s", node=None, project=None)["outcome"] == "launched"

    def guard_refuses(node_id, holder, *, cwd=None, handover_holder=None):
        return {
            "verdict": "already-running",
            "reason": "reservation-held",
        }, 0

    monkeypatch.setattr("fno.agents.cli._spawn_guard_decision", guard_refuses)
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "already-dispatching"
    assert v["node"] == "x-1"
    assert len(calls) == 1  # the second never spawned


@pytest.mark.parametrize("reason", ["auto-deferred", "defer-failed"])
def test_manual_dispatch_preserves_family2_refusal_reason(monkeypatch, tmp_path, reason):
    calls, _gate = _wire(
        monkeypatch,
        tmp_path,
        next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)},
    )

    def guard_refuses(node_id, holder, *, cwd=None, handover_holder=None):
        return {"verdict": "refused", "reason": reason}, 0

    monkeypatch.setattr("fno.agents.cli._spawn_guard_decision", guard_refuses)

    verdict = dispatch._dispatch_one(session="s", node=None, project=None)

    assert verdict["outcome"] == reason
    assert calls == []


def test_spawn_failure_releases_the_reservation(monkeypatch, tmp_path):
    def boom():
        raise RuntimeError("mux pane spawn failed")

    _wire(monkeypatch, tmp_path, next_node={"id": "x-9", "slug": "z", "cwd": str(tmp_path)}, spawn=boom)
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "failed"
    assert "spawn failed" in v["detail"]
    assert _dispatch_claim_state("x-9") == "free"  # node re-dispatchable


# --- `--account` overlay threading (x-c914 piece 1) ------------------------


def test_account_threads_overlay_env(monkeypatch, tmp_path):
    # An --account resolves CLI-side to an env overlay (x-d012) and rides into
    # the spawn as account_env, so the worker bills the chosen account (AC1-HP).
    calls, _gate = _wire(monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)})
    monkeypatch.setattr(
        "fno.agents.account_env.resolve_account_overlay",
        lambda acc: SimpleNamespace(env={"CLAUDE_CONFIG_DIR": "/home/u/.claude-alt"}),
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None, account="rr")
    assert v["outcome"] == "launched"
    assert calls[0]["account_env"] == {"CLAUDE_CONFIG_DIR": "/home/u/.claude-alt"}
    # The birth account is also stamped into the pane provenance (FNO_ACCOUNT)
    # so the mux reads it back for the sideline glyph (x-c914 piece 2).
    assert calls[0]["provenance"]["FNO_ACCOUNT"] == "rr"


def test_no_account_is_byte_identical(monkeypatch, tmp_path):
    # account=None spawns exactly as pre-feature: account_env is None (AC2-HP).
    calls, _gate = _wire(monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)})
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "launched"
    assert calls[0]["account_env"] is None


def test_bad_account_fails_before_spawn(monkeypatch, tmp_path):
    # A stale/missing account fails the verdict (the x-d012 resolver's refusal)
    # rather than silently spawning under the default account (AC2-ERR). No
    # spawn, no lane slot held -> the node stays re-dispatchable.
    from fno.agents.account_env import AccountResolutionError

    calls, _gate = _wire(monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)})

    def boom(acc):
        raise AccountResolutionError("no such account 'rr'")

    monkeypatch.setattr("fno.agents.account_env.resolve_account_overlay", boom)
    v = dispatch._dispatch_one(session="s", node=None, project=None, account="rr")
    assert v["outcome"] == "failed"
    assert "rr" in v["detail"]
    assert len(calls) == 0  # never spawned


# --- `fno agents dispatch resolve` --verb/--brief (US3) ---------------------------


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
        lambda ref: {"id": "x-9", "title": "Retry", "details": "exponential backoff " * 5},
    )
    r = _resolve_cli("--node", "x-9", "-J")
    assert r.exit_code == 0
    out = json.loads(r.stdout)
    assert "exponential backoff" in out["env"]["TARGET_BRIEF"]
    assert out["brief_source"] == "synth-details"


def test_resolve_explicit_brief_still_wins_over_auto(monkeypatch):
    """An explicit --brief is rung 1: it rides verbatim and the node is never
    consulted for a synthesized brief."""
    import json

    monkeypatch.setattr(
        dispatch, "_lookup_node",
        lambda ref: (_ for _ in ()).throw(AssertionError("must not look up node")),
    )
    r = _resolve_cli("--node", "x-9", "--brief", "hand set", "-J")
    assert r.exit_code == 0
    out = json.loads(r.stdout)
    assert out["env"]["TARGET_BRIEF"] == "hand set"
    assert out["brief_source"] == "explicit"


def test_resolve_no_node_no_brief_is_none(monkeypatch):
    """No node + no brief -> no auto-resolve, brief_source=none, no TARGET_BRIEF."""
    import json

    r = _resolve_cli("--verb", "/target", "-J")
    assert r.exit_code == 0
    out = json.loads(r.stdout)
    assert out["brief_source"] == "none"
    assert out["env"].get("TARGET_BRIEF") is None


# --- Hold release on every exit --------------------------------------------
#
# `_dispatch_one` holds three things at once: the gate guard, the `dispatch:<id>`
# reservation, and the handover `node:<id>` claim. The release funnel catches
# `BaseException`, so a `GateRefused` (which subclasses `SystemExit`) or an
# error in the provenance/cutover block between the guard and the spawn
# releases everything. A leaked reservation refuses the node's own relaunch as
# `already-running`; a leaked node claim wedges the node for the handover TTL.


def _dispatch_claim_state(node_id: str) -> str:
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    key = f"dispatch:{node_id}"
    return claim_status(key, root=claims_root_for(key))["state"]


def _node_claim_state(node_id: str) -> str:
    from fno.claims.core import claim_status
    from fno.claims.io import claims_root_for

    key = f"node:{node_id}"
    return claim_status(key, root=claims_root_for(key))["state"]


def test_gate_refusal_releases_both_holds_and_keeps_its_exit_code(monkeypatch, tmp_path):
    """A `GateRefused` is a `SystemExit`, so `except Exception` never saw it.
    Both holds must read free AFTER the refusal, and the refusal keeps its own
    exit code rather than being flattened into a verdict."""
    from fno.agents.spawn_gate import EXIT_NO_WAIT, GateRefused

    def refuse(*a, **kw):
        raise GateRefused(EXIT_NO_WAIT)

    # The refusal comes from the GATE now, not the spawn.
    _wire(monkeypatch, tmp_path, next_node={"id": "x-7", "slug": "g", "cwd": str(tmp_path)})
    monkeypatch.setattr("fno.agents.spawn_gate.run_gate", refuse)

    with pytest.raises(SystemExit) as exc:
        dispatch._dispatch_one(session="s", node=None, project=None)

    assert exc.value.code == EXIT_NO_WAIT
    assert _dispatch_claim_state("x-7") == "free"
    assert _node_claim_state("x-7") == "free"


def test_provenance_error_before_the_spawn_releases_both_holds(monkeypatch, tmp_path):
    """The guard takes its claims before the provenance/cutover block runs, and
    that block must sit inside every handler."""
    _wire(monkeypatch, tmp_path, next_node={"id": "x-8", "slug": "p", "cwd": str(tmp_path)})

    def boom(nid, slug):
        raise RuntimeError("provenance resolver exploded")

    monkeypatch.setattr(dispatch, "resolve_provenance", boom)

    v = dispatch._dispatch_one(session="s", node=None, project=None)

    assert v["outcome"] == "failed"
    assert "provenance resolver exploded" in v["detail"]
    assert _dispatch_claim_state("x-8") == "free"
    assert _node_claim_state("x-8") == "free"


def test_a_release_that_raises_never_strands_the_other_hold(monkeypatch, tmp_path):
    """The funnel must not leak on a release fault. A raising release used to
    re-enter through `except Exception` and release a second time, which can
    free a claim another spawner has since taken - and a second raise escapes the
    handler entirely, leaking both holds. One bad hold never blocks the other."""
    _wire(monkeypatch, tmp_path, next_node={"id": "x-5", "slug": "r", "cwd": str(tmp_path)})

    calls: list[str] = []

    def bad_release(*claims):
        calls.append("release")
        raise RuntimeError("claims store briefly unreadable")

    monkeypatch.setattr("fno.agents.cli._release_dispatch_claims", bad_release)

    def boom(nid, slug):
        raise RuntimeError("provenance resolver exploded")

    monkeypatch.setattr(dispatch, "resolve_provenance", boom)

    v = dispatch._dispatch_one(session="s", node=None, project=None)

    assert v["outcome"] == "failed"
    assert calls == ["release"], "released once, not twice"
    # The reservation is the hold that CAN still be freed, and it must be.
    assert _dispatch_claim_state("x-5") == "free"


def test_keyboard_interrupt_releases_both_holds_and_propagates(monkeypatch, tmp_path):
    """`KeyboardInterrupt` is the other `BaseException` a live dispatch meets. It
    must free the holds on the way out and still reach the caller."""

    def interrupt():
        raise KeyboardInterrupt

    _wire(monkeypatch, tmp_path, next_node={"id": "x-6", "slug": "k", "cwd": str(tmp_path)}, spawn=interrupt)

    with pytest.raises(KeyboardInterrupt):
        dispatch._dispatch_one(session="s", node=None, project=None)

    assert _dispatch_claim_state("x-6") == "free"
    assert _node_claim_state("x-6") == "free"


# --- worktree-ensure launch cwd (x-3f84 W5, plan change 5) ------------------


def test_launch_cwd_comes_from_worktree_ensure(monkeypatch, tmp_path):
    """AC5-HP: a node whose recorded cwd is the canonical checkout launches in
    the path ensure resolved, never the canonical root."""
    calls, _gate = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-1", "slug": "a", "cwd": str(tmp_path)}
    )
    resolved = tmp_path / "wt" / "target-x-1"
    monkeypatch.setattr(
        dispatch, "_worktree_ensure_for_launch", lambda cwd, name, harness: str(resolved)
    )
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "launched"
    assert calls[0]["cwd"] == resolved


def test_ensure_refusal_holds_the_node_and_spawns_nothing(monkeypatch, tmp_path):
    """AC5-ERR: an empty ensure answer (policy refusal / misconfig) is a failed
    verdict naming the hold, with both claims released and no pane spawned."""
    calls, _gate = _wire(
        monkeypatch, tmp_path, next_node={"id": "x-2", "slug": "b", "cwd": str(tmp_path)}
    )
    monkeypatch.setattr(dispatch, "_worktree_ensure_for_launch", lambda cwd, name, harness: None)
    v = dispatch._dispatch_one(session="s", node=None, project=None)
    assert v["outcome"] == "failed"
    assert "canonical main" in v["detail"]
    assert calls == []
    assert _dispatch_claim_state("x-2") == "free"

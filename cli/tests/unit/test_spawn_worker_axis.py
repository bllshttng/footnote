"""One harness axis in `_spawn_worker`, and a receipt for every spawn (x-374b).

The specimen: an auto-continue dispatch launched a claude worker whose first
user line was `$fno:target x-30c2`, the codex spelling. Two values were computed
from two sources - the COMMAND surface came from `resolve_dispatch`, which reads
the stage table, while the LAUNCH binary came from `provider or "claude"`. These
tests pin them to one resolve.

No test spawns: `advance.subprocess.run` is mocked and the receipt is read back
from an isolated events path.
"""

from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from fno.backlog import advance


def _node_row(
    node_id: str, difficulty: str | None = "low", verb: str | None = None
) -> dict:
    """The minimal node dict tests pass to the dispatcher.

    Key presence is what the projection check reads; difficulty low derives
    /target, matching what the builtin path asserted before the None branch
    was deleted. An out-of-family ``verb`` rides the row so the lifecycle
    table abstains and the explicit verb wins, as the deleted None path did."""
    return {
        "id": node_id,
        "dispatch_verb": verb or "",
        "difficulty": difficulty,
    }

_REAL_SUBPROCESS_RUN = advance.subprocess.run


def _settings(*, stage_harness: str = "", legacy_harness: str = "", allowed_verbs=()):
    """A settings stub whose stage table names `stage_harness` for /target."""
    profile = SimpleNamespace(provider=stage_harness)
    return SimpleNamespace(
        agents=SimpleNamespace(
            profiles={"target": profile} if stage_harness else {},
            defaults=SimpleNamespace(permission_mode=""),
        ),
        dispatch=SimpleNamespace(
            harness=legacy_harness, substrate="", command="",
            allowed_verbs=list(allowed_verbs),
        ),
        auto_merge=SimpleNamespace(grant=None),
    )


def _capture(monkeypatch, settings):
    """Mock the spawn subprocess; return the dict holding the argv sent."""
    captured: dict = {}

    def fake_run(cmd, **kwargs):
        # The mint is a real pre-spawn subprocess (x-84b2): serve it with the
        # real binary and keep it out of the capture, which pins the SPAWN argv.
        parts = [str(part) for part in cmd]
        if {"name-mint", "name-codes", "name-parse"} & set(parts):
            return _REAL_SUBPROCESS_RUN(cmd, **kwargs)
        captured["cmd"] = cmd
        # A full session id, not a head-8: a bare 8-hex aimed at codex is a
        # 65.5-second timestamp bucket and the spawn seam refuses it by shape.
        return SimpleNamespace(
            returncode=0,
            stdout='{"name":"w","session_id":"0de85539-1a2b-7c3d-8e4f-5a6b7c8d9e0f"}',
            stderr="",
        )

    monkeypatch.setattr(advance.subprocess, "run", fake_run)
    monkeypatch.setattr("fno.config.load_settings", lambda: settings)
    # Record the resolver's own answer. An argv mismatch then names WHY in its
    # failure message (which rung supplied the harness and the command) instead
    # of leaving a bare `assert False` for someone to re-derive.
    from fno.agents import harness_map as _hm

    _real = _hm.resolve_dispatch

    def _spy(**kw):
        out = _real(**kw)
        captured["resolved"] = out
        captured["asked"] = kw.get("harness")
        return out

    monkeypatch.setattr(_hm, "resolve_dispatch", _spy)
    # The grid is a separate axis; keep it out of these argv assertions.
    monkeypatch.setattr(
        advance, "_grid_lane_for", lambda node, **kw: (None, None, None, None, "grid=test-stub")
    )
    return captured


def _flag(cmd, name):
    return cmd[cmd.index(name) + 1]


def _message(cmd):
    """The dispatched command is the last argv element."""
    return cmd[-1]


def _why(captured):
    """The resolver trail, for a failure that must name its own cause."""
    r = captured.get("resolved") or {}
    return (
        f"asked harness={captured.get('asked')!r} -> "
        f"harness={r.get('harness')!r} command={r.get('command')!r} "
        f"decision={r.get('decision')!r}"
    )


# --- the argv pair: one harness, one spelling ------------------------------


def test_stage_table_harness_drives_both_launch_and_command(monkeypatch):
    """The specimen. Stage table says codex; nothing is pinned.

    Before: `--harness claude` (from `provider or "claude"`) carrying a
    `$fno:target` message no claude worker can run.
    """
    captured = _capture(monkeypatch, _settings(stage_harness="codex"))
    advance._spawn_worker("x-0000", None, "slug", node=_node_row("x-0000"))
    assert _flag(captured["cmd"], "--harness") == "codex", _why(captured)
    assert _message(captured["cmd"]).startswith("$fno:target"), _why(captured)


def test_explicit_harness_pins_both(monkeypatch):
    """Same config, `harness="claude"` explicit: claude launch, claude spelling."""
    captured = _capture(monkeypatch, _settings(stage_harness="codex"))
    advance._spawn_worker("x-0000", None, "slug", harness="claude", node=_node_row("x-0000"))
    assert _flag(captured["cmd"], "--harness") == "claude", _why(captured)
    assert _message(captured["cmd"]).startswith("/target"), _why(captured)


def test_provider_pins_the_surface_not_only_the_launch(monkeypatch):
    """`provider` IS the harness axis here, so it must reach the resolver."""
    captured = _capture(monkeypatch, _settings(stage_harness="claude"))
    advance._spawn_worker("x-0000", None, "slug", provider="codex", node=_node_row("x-0000"))
    assert _flag(captured["cmd"], "--harness") == "codex", _why(captured)
    assert _message(captured["cmd"]).startswith("$fno:target"), _why(captured)


def test_no_config_falls_back_to_the_resolvers_builtin(monkeypatch):
    """Nothing set anywhere: the resolver owns the claude fallback, not `prov`."""
    captured = _capture(monkeypatch, _settings())
    advance._spawn_worker("x-0000", None, "slug", node=_node_row("x-0000"))
    assert _flag(captured["cmd"], "--harness") == "claude", _why(captured)
    assert _message(captured["cmd"]).startswith("/target"), _why(captured)


def test_launch_harness_disagreeing_with_the_surface_refuses(monkeypatch):
    """An explicit harness and an explicit, different provider is the split
    this node exists to close: refuse rather than ship a mismatched pair."""
    captured = _capture(monkeypatch, _settings())
    with pytest.raises(advance.SpawnError) as exc:
        advance._spawn_worker("x-0000", None, "slug", harness="claude", provider="codex", node=_node_row("x-0000"))
    assert "harness" in str(exc.value)
    assert "cmd" not in captured, "refused before spawning"


# --- the machine-dispatch spawn env ------------------------------------------


def _resolve(monkeypatch, source):
    monkeypatch.setattr(
        "fno.config.load_settings", lambda: _settings(allowed_verbs=("target",))
    )
    monkeypatch.setattr(
        advance, "_grid_lane_for", lambda node, **kw: (None, None, None, None, None)
    )
    from fno.agents.node_dispatch import resolve_node_spawn

    # the dispatcher refuses a node with no dict; the env seam under test
    # still has to clear that gate, so hand it minimal verb evidence.
    return resolve_node_spawn(
        "x-0000", None, "slug", node={"dispatch_verb": "target"}, verb="target",
        source=source,
    )


def test_machine_source_dispatch_carries_trigger_and_no_identity(monkeypatch):
    """A merge-triggered dispatch is asked for by no session: the spawn env
    carries the dispatcher trigger and no ambient identity marker."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "dispatcher-session-1")
    args = _resolve(monkeypatch, source="ac")
    assert "CLAUDE_CODE_SESSION_ID" not in args.env
    assert args.env["FNO_SPAWN_TRIGGER"] == "dispatch:ac"


def test_reconcile_source_names_rd_in_the_trigger(monkeypatch):
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "dispatcher-session-1")
    args = _resolve(monkeypatch, source="rd")
    assert args.env["FNO_SPAWN_TRIGGER"] == "dispatch:rd"
    assert "CLAUDE_CODE_SESSION_ID" not in args.env


def test_human_and_blueprint_sources_keep_the_ambient_edge(monkeypatch):
    """A source-less (attended) spawn was asked for by a session: the env
    keeps the parent edge and carries no trigger."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "dispatcher-session-1")
    args = _resolve(monkeypatch, source=None)
    assert args.env.get("CLAUDE_CODE_SESSION_ID") == "dispatcher-session-1"
    assert "FNO_SPAWN_TRIGGER" not in args.env


# --- the receipt ------------------------------------------------------------


def _rows(events_path: Path, kind: str) -> list[dict]:
    if not events_path.exists():
        return []
    out = []
    for line in events_path.read_text().splitlines():
        if not line.strip():
            continue
        row = json.loads(line)
        if row.get("type") == kind:
            out.append(row)
    return out


def test_spawn_emits_one_dispatch_spawned_row(monkeypatch, tmp_path):
    """The receipt names the resolved pair, so the file distinguishes callers."""
    captured = _capture(monkeypatch, _settings(stage_harness="codex"))
    ev = tmp_path / "events.jsonl"
    advance._spawn_worker(
        "x-0000", None, "slug", caller="_converge_one", events_path=ev, node=_node_row("x-0000"),
    )
    rows = _rows(ev, "dispatch_spawned")
    assert len(rows) == 1
    data = rows[0]["data"]
    assert data["harness"] == _flag(captured["cmd"], "--harness")
    assert data["command"] == _message(captured["cmd"])
    assert data["caller"] == "_converge_one"
    assert data["node_id"] == "x-0000"
    assert data["substrate"] == _flag(captured["cmd"], "--substrate")
    assert data["grid"] == "grid=test-stub"
    assert data["account"] == ""


def test_receipt_names_the_pinned_account_record(monkeypatch, tmp_path):
    """A record whose config points at the wrong login makes every other field
    name a lane it did not bill, and only the record id makes that readable."""
    _capture(monkeypatch, _settings())
    ev = tmp_path / "events.jsonl"
    advance._spawn_worker(
        "x-0000", None, "slug", dispatch_account="ccr", events_path=ev, node=_node_row("x-0000"),
    )
    assert _rows(ev, "dispatch_spawned")[0]["data"]["account"] == "ccr"


def test_failed_spawn_emits_no_receipt(monkeypatch, tmp_path):
    """A receipt is proof of a launch, so a non-zero exit leaves none."""

    def fake_run(cmd, **kwargs):
        parts = [str(part) for part in cmd]
        if {"name-mint", "name-codes", "name-parse"} & set(parts):
            return _REAL_SUBPROCESS_RUN(cmd, **kwargs)
        return SimpleNamespace(returncode=1, stdout="", stderr="boom")

    monkeypatch.setattr(advance.subprocess, "run", fake_run)
    monkeypatch.setattr("fno.config.load_settings", lambda: _settings())
    monkeypatch.setattr(
        advance, "_grid_lane_for", lambda node, **kw: (None, None, None, None, None)
    )
    ev = tmp_path / "events.jsonl"
    with pytest.raises(advance.SpawnError):
        advance._spawn_worker("x-0000", None, "slug", events_path=ev, node=_node_row("x-0000"))
    assert _rows(ev, "dispatch_spawned") == []


# --- the roster read that silently rendered the wrong spelling -------------


def test_codex_target_spelling_survives_an_unreadable_verb_roster(monkeypatch):
    """The command surface must not depend on a plugin-root read.

    `footnote_verbs()` needs a resolvable plugin root. A dispatching process
    does not always have one, and a failed read returns an empty roster that is
    indistinguishable from "this verb is not ours". Falling through rendered
    `/target` for a codex worker: the exact wrong-spelling launch this node
    closes, arriving as an ordinary pass-through rather than a failed read.
    """
    from fno.agents import harness_map

    monkeypatch.setattr(harness_map, "footnote_verbs", lambda: frozenset())
    assert (
        harness_map.dispatch_command("codex", allow_merge=False)
        == "$fno:target --no-merge {id}"
    )
    assert harness_map.dispatch_command("claude", allow_merge=False) == (
        "/target --no-merge {id}"
    )


# --- the failure capture ----------------------------------------------------


def test_failed_spawn_captures_the_refusal_tail(monkeypatch):
    """Head-capture recorded the advisory and cut the refusal: warnings print
    during the pre-flight reads, the refusal prints immediately before exit.
    The tail keeps it (the specimen error named an unstamped-row warning on a
    box the load gate was refusing)."""
    captured = _capture(monkeypatch, _settings())

    def fake_run(cmd, **kwargs):
        parts = [str(part) for part in cmd]
        if {"name-mint", "name-codes", "name-parse"} & set(parts):
            return _REAL_SUBPROCESS_RUN(cmd, **kwargs)
        return SimpleNamespace(
            returncode=79,
            stdout="",
            stderr=(
                "1 live row(s) were minted without a provider stamp "
                "(harness=claude, origin=operator)\n"
                "fno agents spawn: applied slot=agents.profiles.blueprint.lanes[0] "
                "claude-opus-5\n"
                "spawn-gate: the fleet holds 96.20/12.00 cores (801.7% of capacity), "
                "over the max_fleet_cpu_share ceiling 50.0%; refusing to spawn "
                "(--force to bypass)\n"
            ),
        )

    monkeypatch.setattr(advance.subprocess, "run", fake_run)
    with pytest.raises(advance.SpawnError) as exc:
        advance._spawn_worker("x-0000", None, "slug", node=_node_row("x-0000"))
    assert "refusing to spawn" in str(exc.value)
    assert "max_fleet_cpu_share ceiling" in str(exc.value)
    assert "cmd" not in captured, "failure came from the subprocess, not the argv"


def test_failed_spawn_single_line_stderr_is_captured_whole(monkeypatch):
    """Nothing to reorder; the one line survives intact."""
    _capture(monkeypatch, _settings())

    def fake_run(cmd, **kwargs):
        parts = [str(part) for part in cmd]
        if {"name-mint", "name-codes", "name-parse"} & set(parts):
            return _REAL_SUBPROCESS_RUN(cmd, **kwargs)
        return SimpleNamespace(returncode=79, stdout="", stderr="boom: no spawn\n")

    monkeypatch.setattr(advance.subprocess, "run", fake_run)
    with pytest.raises(advance.SpawnError) as exc:
        advance._spawn_worker("x-0000", None, "slug", node=_node_row("x-0000"))
    assert "boom: no spawn" in str(exc.value)
    assert exc.value.detail == "boom: no spawn"
def test_grid_lane_for_pinned_model_takes_its_declared_row(monkeypatch):
    """AC7-HP: a node pinned to gpt-5.6-sol with no provider resolves the
    codex row through the slot and returns the row's coordinates."""
    from fno import route_resolve as _rr

    monkeypatch.setattr(_rr, "resolve_inventory", lambda: {})
    monkeypatch.setattr(_rr, "runtime_capacity", lambda **kw: {})
    seen: dict = {}

    def fake_resolve_slot(profile_verb, node, capacity, *, inventory=None,
                          explicit_model_value=None, **kw):
        seen["model"] = explicit_model_value
        return (
            {"harness": "codex", "model": "gpt-5.6-sol", "pin_row": "codex-sol"},
            ["slot=operator-pin-override row=codex-sol harness=codex"],
            "pick",
        )

    monkeypatch.setattr(_rr, "resolve_slot", fake_resolve_slot)
    got = advance._grid_lane_for(
        {"difficulty": "medium", "plan_path": "/plans/p.md"},
        model="gpt-5.6-sol",
        provider=None,
    )
    assert seen["model"] == "gpt-5.6-sol"
    assert got == ("codex", "gpt-5.6-sol", None, None, None)


def test_grid_lane_for_pinned_model_without_a_row_declines(monkeypatch):
    """A pinned model the rows do not declare dispatches on the default: the
    answer is all-None with the slot's terminal as the reason."""
    from fno import route_resolve as _rr

    monkeypatch.setattr(_rr, "resolve_inventory", lambda: {})
    monkeypatch.setattr(_rr, "runtime_capacity", lambda **kw: {})

    def fake_resolve_slot(*a, **kw):
        return (
            None,
            ["slot=operator-pin-override (a typed model/vendor/route outranks the lanes)"],
            "unarmed",
        )

    monkeypatch.setattr(_rr, "resolve_slot", fake_resolve_slot)
    got = advance._grid_lane_for(
        {"difficulty": "medium", "plan_path": "/plans/p.md"},
        model="opus",
        provider=None,
    )
    assert got == (
        None,
        None,
        None,
        None,
        "slot=operator-pin-override (a typed model/vendor/route outranks the lanes)",
    )


# --- the blueprint reuse arm (x-3582) ---------------------------------------


def _bp_settings(stage_harness: str = ""):
    """The axis settings plus a blueprint profile: the resolver refuses a verb
    its stage table cannot answer, and a blueprint dispatch resolves one."""
    settings = _settings(stage_harness=stage_harness)
    settings.agents.profiles["blueprint"] = SimpleNamespace(provider=stage_harness)
    settings.dispatch.allowed_verbs = ["target", "blueprint"]
    return settings


def _reuse_seams(monkeypatch, *, candidate=None, receipt_row=None, verdict="dispatchable"):
    """Stub every seam the reuse arm touches; record calls for the assertions."""
    seen: dict = {"retask_calls": [], "releases": []}

    def fake_planner(entries, **kw):
        seen["planner_kw"] = kw
        return candidate

    def fake_guard(node_id, holder, **kw):
        seen["guard"] = {"node_id": node_id, "holder": holder, **kw}
        if verdict != "dispatchable":
            return {"verdict": verdict, "reason": "reservation-held"}, 0
        return {
            "verdict": "dispatchable",
            "reservation_key": f"dispatch:{node_id}",
            "reservation_holder": holder,
            "node_claim_key": f"node:{node_id}",
            "node_claim_holder": kw["handover_holder"],
        }, 0

    def fake_release(*claims):
        seen["releases"] = list(claims)

    def fake_retask(worker, **kw):
        seen["retask_calls"].append({"worker": worker, **kw})
        return receipt_row

    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [
            {"id": "x-e1"},
            {"id": "x-aaaa", "parent": "x-e1"},
            {"id": "x-bbbb", "parent": "x-e1"},
        ],
    )
    monkeypatch.setattr("fno.agents.registry.load_registry", lambda: [])
    monkeypatch.setattr("fno.agents.retask.finished_planner", fake_planner)
    monkeypatch.setattr("fno.agents.cli._spawn_guard_decision", fake_guard)
    monkeypatch.setattr("fno.agents.cli._release_dispatch_claims", fake_release)
    monkeypatch.setattr("fno.agents.retask.run_retask", fake_retask)
    return seen


def test_blueprint_dispatch_retasks_a_finished_planner_and_spawns_nothing(
    monkeypatch, tmp_path
):
    """AC1-HP: the reused planner's session id is the launch proof; the row
    names the transaction, and no spawn subprocess runs."""
    captured = _capture(monkeypatch, _bp_settings(stage_harness="codex"))
    seen = _reuse_seams(
        monkeypatch,
        candidate=SimpleNamespace(name="ac-bp-x-aaaa-slug", substrate="thread"),
        receipt_row={
            "status": "retasked",
            "cleared": True,
            "current_session_id": "1a2b3c4d-1111-2222-3333-444455556666",
            "registry_name": "bp-x-bbbb-renamed",
        },
    )
    ev = tmp_path / "events.jsonl"
    got = advance._spawn_worker(
        "x-bbbb", None, "slug", verb="blueprint", caller="advance", events_path=ev, node=_node_row("x-bbbb", difficulty="medium"),
    )
    assert got == "1a2b3c4d-1111-2222-3333-444455556666"
    assert "cmd" not in captured, "a reuse dispatch must not spawn"
    rows = _rows(ev, "dispatch_spawned")
    assert len(rows) == 1
    data = rows[0]["data"]
    assert data["retask"] == "retasked"
    assert data["reused_worker"] == "ac-bp-x-aaaa-slug"
    assert data["short_id"] == "1a2b3c4d-1111-2222-3333-444455556666"
    assert data["agent_name"] == "bp-x-bbbb-renamed"
    assert data["substrate"] == "thread"
    assert data["caller"] == "advance"
    assert seen["retask_calls"][0]["worker"] == "ac-bp-x-aaaa-slug"


def test_retask_refused_before_clear_falls_through_to_one_cold_spawn(
    monkeypatch, tmp_path
):
    """AC1-ERR: the pre-/clear refusal is named on the cold row, the guard's
    claims are released, and exactly one cold spawn runs."""
    captured = _capture(monkeypatch, _bp_settings())
    seen = _reuse_seams(
        monkeypatch,
        candidate=SimpleNamespace(name="ac-bp-x-aaaa-slug", substrate="thread"),
        receipt_row={
            "status": "refused",
            "cleared": False,
            "reason": "thread_view_unavailable",
        },
    )
    ev = tmp_path / "events.jsonl"
    advance._spawn_worker("x-bbbb", None, "slug", verb="blueprint", events_path=ev, node=_node_row("x-bbbb", difficulty="medium"))
    assert "cmd" in captured, "the refusal falls through to one cold spawn"
    data = _rows(ev, "dispatch_spawned")[0]["data"]
    assert data["retask_fallthrough"] == "ac-bp-x-aaaa-slug: thread_view_unavailable"
    assert "retask" not in data
    keys = {pair[0] for pair in seen["releases"]}
    assert keys == {"dispatch:x-bbbb", "node:x-bbbb"}


def test_retask_refused_after_clear_raises_and_spawns_nothing(monkeypatch, tmp_path):
    """AC1-EDGE: a refusal after /clear leaves a blank renamed row; the arm
    raises instead of cold-spawning over it."""
    captured = _capture(monkeypatch, _bp_settings())
    _reuse_seams(
        monkeypatch,
        candidate=SimpleNamespace(name="ac-bp-x-aaaa-slug", substrate="thread"),
        receipt_row={"status": "refused", "cleared": True, "reason": "rename_refused"},
    )
    ev = tmp_path / "events.jsonl"
    with pytest.raises(advance.SpawnError) as exc:
        advance._spawn_worker("x-bbbb", None, "slug", verb="blueprint", events_path=ev, node=_node_row("x-bbbb", difficulty="medium"))
    assert "ac-bp-x-aaaa-slug" in str(exc.value)
    assert "rename_refused" in str(exc.value)
    assert "cmd" not in captured
    assert _rows(ev, "dispatch_spawned") == []


def test_guard_refusal_skips_as_already_running_without_retask(monkeypatch, tmp_path):
    """AC2-ERR: a non-dispatchable guard verdict is the benign already-running
    skip, before the retask transaction is ever tried."""
    captured = _capture(monkeypatch, _bp_settings())
    seen = _reuse_seams(
        monkeypatch,
        candidate=SimpleNamespace(name="ac-bp-x-aaaa-slug", substrate="thread"),
        receipt_row={"status": "retasked", "current_session_id": "s", "registry_name": "r"},
        verdict="already-running",
    )
    with pytest.raises(advance.SpawnAlreadyRunning) as exc:
        advance._spawn_worker("x-bbbb", None, "slug", verb="blueprint", node=_node_row("x-bbbb", difficulty="medium"))
    assert "reservation-held" in str(exc.value)
    assert not seen["retask_calls"]
    assert "cmd" not in captured


def test_target_dispatch_never_reads_the_registry_for_reuse(monkeypatch, tmp_path):
    """AC3-HP: a target dispatch keeps today's shape; no reuse read, no
    retask keys on the row."""
    captured = _capture(monkeypatch, _settings())

    def _boom(*_a, **_kw):
        raise AssertionError("reuse read on a target dispatch")

    monkeypatch.setattr("fno.agents.registry.load_registry", _boom)
    monkeypatch.setattr("fno.agents.retask.finished_planner", _boom)
    ev = tmp_path / "events.jsonl"
    advance._spawn_worker("x-0000", None, "slug", events_path=ev, node=_node_row("x-0000"))
    assert "cmd" in captured
    data = _rows(ev, "dispatch_spawned")[0]["data"]
    assert "retask" not in data
    assert "retask_fallthrough" not in data

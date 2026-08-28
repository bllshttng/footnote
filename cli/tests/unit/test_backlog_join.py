"""Unit tests for ``fno backlog join`` (x-8d1d, epic x-956c joiner 2).

``join_node`` resolves the holder's worktree from the LIVE node claim,
computes the bound plan's ready-graph width, and spawns
``min(workers, width - 1)`` ``/fno:execute waves <plan>`` joiners there as
visitors. The graph and claim seams are monkeypatched; the plan fixtures are
real files (the width walk runs the actual parser); the spawn subprocess is
recorded, never run.
"""

from __future__ import annotations

import json

import pytest

from fno.backlog import advance
from fno.backlog.advance import JoinRefuse, SpawnError, _plan_parallel_width, join_node

PARALLEL_PLAN = """---
title: t
status: ready
---

## Execution Strategy

```yaml
execution_mode: parallel
waves:
  - wave: 1
    mode: parallel
    tasks: ['1.1', '1.2', '1.3']
tasks:
  - id: '1.1'
    title: a
    surface: ['src/a.py']
  - id: '1.2'
    title: b
    surface: ['src/b.py']
  - id: '1.3'
    title: c
    surface: ['src/c.py']
```
"""

SEQUENTIAL_PLAN = """---
title: t
status: ready
---

## Execution Strategy

```yaml
execution_mode: sequential
waves:
  - wave: 1
    tasks: ['1.1']
  - wave: 2
    tasks: ['2.1']
  - wave: 3
    tasks: ['3.1']
  - wave: 4
    tasks: ['4.1']
  - wave: 5
    tasks: ['5.1']
tasks:
  - id: '1.1'
    title: a
  - id: '2.1'
    title: b
  - id: '3.1'
    title: c
  - id: '4.1'
    title: d
  - id: '5.1'
    title: e
```
"""


# Four single-task waves banded high / medium / medium / low; the later
# waves declare explicit empty blockers so the whole graph is ready at once
# (width 4). Distinct bands: three lanes.
BANDED_PLAN = """---
title: banded
status: ready
---

## Execution Strategy

```yaml
execution_mode: parallel
waves:
  - wave: 1
    mode: sequential
    difficulty: high
    tasks: ['1.1']
  - wave: 2
    mode: sequential
    difficulty: medium
    tasks: ['1.2']
  - wave: 3
    mode: sequential
    difficulty: medium
    tasks: ['1.3']
  - wave: 4
    mode: sequential
    difficulty: low
    tasks: ['1.4']
tasks:
  - id: '1.1'
    title: a
  - id: '1.2'
    title: b
    blocked_by: []
  - id: '1.3'
    title: c
    blocked_by: []
  - id: '1.4'
    title: d
    blocked_by: []
```
"""


def _wire(monkeypatch, tmp_path, plan_text, *, claim_state="live", worktree=True):
    """Mock join's seams; returns the recorded spawn calls."""
    calls: list[dict] = []
    entry: dict = {"id": "x-8d1d", "slug": "joiner-2"}
    if plan_text is not None:
        plan = tmp_path / "plan.md"
        plan.write_text(plan_text)
        entry["plan_path"] = str(plan)
    status: dict = {"key": "node:x-8d1d", "state": claim_state}
    if worktree:
        status["metadata"] = {"worktree": str(tmp_path / "wt")}
        (tmp_path / "wt").mkdir(exist_ok=True)

    monkeypatch.setattr("fno.graph.store.read_graph", lambda *_a, **_k: [entry])
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr("fno.claims.core.claim_status", lambda key, root=None: status)
    monkeypatch.setattr(advance, "_claims_root_for", lambda key: tmp_path / "claims")
    # Plan fixtures are absolute; skip resolve_plan_path's git rev-parse so the
    # recorded spawn calls carry ONLY join's own spawns.
    monkeypatch.setattr(
        "fno.graph.collision.resolve_plan_path", lambda p: tmp_path / "plan.md"
    )
    # The already-joined guard reads the registry; point it at an absent file
    # so the suite never depends on this machine's live roster rows.
    monkeypatch.setattr(
        "fno.paths.agents_registry_path", lambda: tmp_path / "registry-absent.json"
    )

    class _Proc:
        returncode = 0
        stdout = '{"name": "j", "short_id": "abcd1234"}\n'
        stderr = ""

    monkeypatch.setattr(
        advance.subprocess, "run",
        lambda cmd, **_kw: (calls.append({"cmd": cmd, "env": _kw.get("env")}), _Proc())[1],
    )
    return calls


def test_join_spawns_width_minus_one_workers_with_lead_hub(tmp_path, monkeypatch):
    """AC1-HP: three disjoint parallel tasks -> two joiners, first is lead."""
    calls = _wire(monkeypatch, tmp_path, PARALLEL_PLAN)
    receipt = join_node("x-8d1d", 3)
    shapeless = {"band": "", "harness": None, "model": None}
    assert receipt == {
        "node": "x-8d1d",
        "worktree": str(tmp_path / "wt"),
        "width": 3,
        "spawned": ["j-x-8d1d-1", "j-x-8d1d-2"],
        "lead": "j-x-8d1d-1",
        # An unbanded plan degrades to joiner 2's shapeless spawn: no grid
        # call, the caller's default lane.
        "lanes": {
            "j-x-8d1d-1": shapeless,
            "j-x-8d1d-2": shapeless,
        },
    }
    assert len(calls) == 2
    for call, name in zip(calls, receipt["spawned"]):
        cmd = call["cmd"]
        assert cmd[cmd.index("--substrate") + 1] == "thread"
        # Explicit lane: an untagged spawn lets a codex-scoped default model
        # inject and trip the vendor-mismatch refusal.
        assert cmd[cmd.index("--harness") + 1] == "claude"
        assert cmd[cmd.index("--cwd") + 1] == str(tmp_path / "wt")
        assert cmd[cmd.index("--name") + 1] == name
        assert cmd[-1] == f"/fno:execute waves {tmp_path / 'plan.md'}"
        # Unbanded: no band rides the spawn env.
        assert "FNO_WORKER_BAND" not in call_env(call, "FNO_WORKER_BAND")
    # Joiner 1 is the mail hub; joiner 2's brief NAMES the hub (LD 3). Both
    # briefs carry the claim-before-dispatch instruction (waves.md 3e).
    assert "lead joiner" in call_env(calls[0], "TARGET_BRIEF")
    assert receipt["lead"] in call_env(calls[1], "TARGET_BRIEF")
    for call in calls:
        assert "task update" in call_env(call, "TARGET_BRIEF")


def call_env(call, key):
    return (call["env"] or {}).get(key, "")


def test_join_worker_count_capped_at_width_minus_one(tmp_path, monkeypatch):
    calls = _wire(monkeypatch, tmp_path, PARALLEL_PLAN)
    receipt = join_node("x-8d1d", 5)
    assert len(calls) == 2
    assert len(receipt["spawned"]) == 2


def test_sequential_plan_refuses_width_one(tmp_path, monkeypatch):
    """AC1-ERR: five sequential waves -> exit 3, width 1."""
    _wire(monkeypatch, tmp_path, SEQUENTIAL_PLAN)
    assert _plan_parallel_width(tmp_path / "plan.md") == 1
    with pytest.raises(JoinRefuse) as excinfo:
        join_node("x-8d1d", 3)
    assert excinfo.value.code == 3
    assert "width 1: a second worker has nothing to pull" in str(excinfo.value)


def test_shared_surface_serializes_and_shrinks_width(tmp_path):
    """The partition walk mirrors the orchestrator: 1.1/1.2 share a file, so
    the wave's ready width is 2, never 3."""
    plan = PARALLEL_PLAN.replace("surface: ['src/b.py']", "surface: ['src/a.py']")
    (tmp_path / "plan.md").write_text(plan)
    assert _plan_parallel_width(tmp_path / "plan.md") == 2


def test_no_live_claim_refuses_exit_2(tmp_path, monkeypatch):
    _wire(monkeypatch, tmp_path, PARALLEL_PLAN, claim_state="free")
    with pytest.raises(JoinRefuse) as excinfo:
        join_node("x-8d1d", 3)
    assert excinfo.value.code == 2
    assert "nothing to join" in str(excinfo.value)


def test_live_claim_without_worktree_refuses_exit_2(tmp_path, monkeypatch):
    _wire(monkeypatch, tmp_path, PARALLEL_PLAN, worktree=False)
    with pytest.raises(JoinRefuse) as excinfo:
        join_node("x-8d1d", 3)
    assert excinfo.value.code == 2


def test_join_writes_the_brief_file_into_the_holder_worktree(tmp_path, monkeypatch):
    """The brief must survive the daemon fork: TARGET_BRIEF cannot reach a
    serving session, so the file channel carries the hub + claim step."""
    _wire(monkeypatch, tmp_path, PARALLEL_PLAN)
    join_node("x-8d1d", 3)
    brief = (tmp_path / "wt" / ".fno" / "join-briefs" / "x-8d1d.md").read_text()
    assert "mail hub: j-x-8d1d-1" in brief
    assert "task update x-8d1d" in brief


def test_missing_holder_worktree_refuses_exit_2(tmp_path, monkeypatch):
    """A live claim whose worktree was deleted has nothing to join."""
    entry_plan = tmp_path / "plan.md"
    entry_plan.write_text(PARALLEL_PLAN)
    monkeypatch.setattr(
        "fno.graph.store.read_graph",
        lambda *_a, **_k: [{"id": "x-8d1d", "plan_path": str(entry_plan)}],
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: tmp_path / "graph.json")
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, root=None: {
            "key": key, "state": "live",
            "metadata": {"worktree": str(tmp_path / "gone")},
        },
    )
    monkeypatch.setattr(advance, "_claims_root_for", lambda key: tmp_path / "claims")
    monkeypatch.setattr(
        "fno.graph.collision.resolve_plan_path", lambda p: entry_plan
    )
    with pytest.raises(JoinRefuse) as excinfo:
        join_node("x-8d1d", 3)
    assert excinfo.value.code == 2
    assert "is gone" in excinfo.value.message


def test_zero_workers_still_spawns_one(tmp_path, monkeypatch):
    """A direct join_node(workers=0) must not return a receipt claiming a
    lead that was never spawned."""
    calls = _wire(monkeypatch, tmp_path, PARALLEL_PLAN)
    receipt = join_node("x-8d1d", 0)
    assert receipt["spawned"] == ["j-x-8d1d-1"]
    assert len(calls) == 1


def test_unsolvable_task_graph_refuses_exit_4_with_cause(tmp_path, monkeypatch):
    """A cycle or a blocker naming an unknown id is a malformed plan, not a
    narrow one: exit 4 names the stuck tasks instead of blaming the width."""
    calls = _wire(monkeypatch, tmp_path, PARALLEL_PLAN.replace(
        "surface: ['src/c.py']",
        "surface: ['src/c.py']\n    blocked_by: ['9.9']",
    ))
    with pytest.raises(JoinRefuse) as excinfo:
        join_node("x-8d1d", 3)
    assert excinfo.value.code == 4
    assert "unsolvable" in excinfo.value.message
    assert "1.3" in excinfo.value.message
    assert not calls


def test_no_bound_plan_refuses_exit_4(tmp_path, monkeypatch):
    _wire(monkeypatch, tmp_path, None)
    with pytest.raises(JoinRefuse) as excinfo:
        join_node("x-8d1d", 3)
    assert excinfo.value.code == 4


def test_unknown_node_refuses_exit_2(tmp_path, monkeypatch):
    _wire(monkeypatch, tmp_path, PARALLEL_PLAN)
    with pytest.raises(JoinRefuse) as excinfo:
        join_node("x-9999", 3)
    assert excinfo.value.code == 2


def test_second_join_refuses_while_joiners_live(tmp_path, monkeypatch):
    """Exit 5: live j-<node>-* workers make join non-idempotent. The live
    proof hit this when a JOINER re-ran join on its own node - the rewrite
    truncated the brief and nearly duplicated the spawns."""
    calls = _wire(monkeypatch, tmp_path, BANDED_PLAN)
    reg = tmp_path / "registry.json"
    reg.write_text(json.dumps({"schema_version": 2, "agents": [
        {"name": "j-x-8d1d-1", "harness_session_id": "s-1", "status": "live"},
        {"name": "j-x-8d1d-2", "harness_session_id": "s-2", "status": "stopped"},
    ]}))
    monkeypatch.setattr("fno.paths.agents_registry_path", lambda: reg)
    with pytest.raises(JoinRefuse) as excinfo:
        join_node("x-8d1d", 5)
    assert excinfo.value.code == 5
    assert "already joined by j-x-8d1d-1" in excinfo.value.message
    assert not calls  # refused before any spawn, brief untouched


def test_join_allowed_when_no_live_joiner_rows(tmp_path, monkeypatch):
    """Stopped rows do not block a re-join; an unreadable registry neither."""
    calls = _wire(monkeypatch, tmp_path, BANDED_PLAN)
    reg = tmp_path / "registry.json"
    reg.write_text(json.dumps({"schema_version": 2, "agents": [
        {"name": "j-x-8d1d-1", "harness_session_id": "s-1", "status": "stopped"},
    ]}))
    monkeypatch.setattr("fno.paths.agents_registry_path", lambda: reg)
    receipt = join_node("x-8d1d", 5)
    assert len(receipt["spawned"]) == 3
    monkeypatch.setattr(
        "fno.paths.agents_registry_path", lambda: tmp_path / "absent.json"
    )
    calls.clear()
    receipt = join_node("x-8d1d", 5)
    assert len(receipt["spawned"]) == 3


def test_lead_spawn_requires_launch_identity(tmp_path, monkeypatch):
    """A thread spawn with exit 0 but no receipt is not a join: the lead
    refusal fails the call instead of reporting a phantom worker."""
    calls = _wire(monkeypatch, tmp_path, PARALLEL_PLAN)

    class _Bare:
        returncode = 0
        stdout = "spawned ok\n"
        stderr = ""

    monkeypatch.setattr(
        advance.subprocess, "run",
        lambda cmd, **_kw: (calls.append({"cmd": cmd}), _Bare())[1],
    )
    with pytest.raises(SpawnError):
        join_node("x-8d1d", 3)
    assert calls  # the spawn was attempted, its receipt just proved nothing


# ---------------------------------------------------------------------------
# Per-band lanes (x-dadc): one worker per band present, the grid resolves
# each band's lane, the band rides both env and the brief's table.
# ---------------------------------------------------------------------------


def test_join_spawns_one_worker_per_distinct_band(tmp_path, monkeypatch):
    """AC2-HP: waves banded high/medium/medium/low -> three workers (high,
    medium, low), each on the lane the grid picks for ITS band."""
    calls = _wire(monkeypatch, tmp_path, BANDED_PLAN)
    picked: list[str] = []

    def _grid(node, *, model, provider):
        band = node["difficulty"]
        picked.append(band)
        return {"high": ("claude", "glm-x"), "medium": ("codex", "gpt-x"),
                "low": ("claude", "glm-sm")}[band]

    monkeypatch.setattr(advance, "_grid_lane_for", _grid)
    receipt = join_node("x-8d1d", 5)
    assert receipt["spawned"] == ["j-x-8d1d-1", "j-x-8d1d-2", "j-x-8d1d-3"]
    # Highest band first; the lead carries it.
    assert receipt["lead"] == "j-x-8d1d-1"
    assert picked == ["high", "medium", "low"]
    assert receipt["lanes"] == {
        "j-x-8d1d-1": {"band": "high", "harness": "claude", "model": "glm-x"},
        "j-x-8d1d-2": {"band": "medium", "harness": "codex", "model": "gpt-x"},
        "j-x-8d1d-3": {"band": "low", "harness": "claude", "model": "glm-sm"},
    }
    for call, name in zip(calls, receipt["spawned"]):
        cmd = call["cmd"]
        lane = receipt["lanes"][name]
        assert cmd[cmd.index("--harness") + 1] == lane["harness"]
        assert cmd[cmd.index("--model") + 1] == lane["model"]
        assert call_env(call, "FNO_WORKER_BAND") == lane["band"]
        assert f"your band is {lane['band']}" in call_env(call, "TARGET_BRIEF")


def test_band_count_capped_by_width_rule(tmp_path, monkeypatch):
    """Three bands on a width-3 plan: the holder is one of the three workers,
    so only the two highest bands get a joiner."""
    calls = _wire(monkeypatch, tmp_path, BANDED_PLAN.replace(
        "title: d\n    blocked_by: []", "title: d\n    blocked_by: ['1.1']",
    ))
    monkeypatch.setattr(
        advance, "_grid_lane_for", lambda *_a, **_k: ("claude", "glm-x")
    )
    receipt = join_node("x-8d1d", 3)
    assert receipt["width"] == 3
    assert receipt["spawned"] == ["j-x-8d1d-1", "j-x-8d1d-2"]
    assert [lane["band"] for lane in receipt["lanes"].values()] == ["high", "medium"]
    assert len(calls) == 2


def test_grid_declined_spawns_default_lane_and_records_it(tmp_path, monkeypatch):
    """AC2-ERR: a (None, None) grid answer is a decline, not a failure: the
    worker rides the caller's default lane and the receipt says so."""
    calls = _wire(monkeypatch, tmp_path, BANDED_PLAN)
    monkeypatch.setattr(advance, "_grid_lane_for", lambda *_a, **_k: (None, None))
    receipt = join_node("x-8d1d", 5)
    assert len(calls) == 3
    for call in calls:
        assert call["cmd"][call["cmd"].index("--harness") + 1] == "claude"
        assert "--model" not in call["cmd"]
    assert receipt["lanes"] == {
        "j-x-8d1d-1": {"band": "high", "harness": None, "model": None,
                       "grid": "declined"},
        "j-x-8d1d-2": {"band": "medium", "harness": None, "model": None,
                       "grid": "declined"},
        "j-x-8d1d-3": {"band": "low", "harness": None, "model": None,
                       "grid": "declined"},
    }


def test_banded_brief_carries_the_band_table(tmp_path, monkeypatch):
    """The band's durable channel is the brief file: a daemon-forked worker
    never sees the env export (x-6de8), so waves.md reads the table."""
    _wire(monkeypatch, tmp_path, BANDED_PLAN)
    join_node("x-8d1d", 5)
    brief = (tmp_path / "wt" / ".fno" / "join-briefs" / "x-8d1d.md").read_text()
    assert "| j-x-8d1d-1 | high |" in brief
    assert "| j-x-8d1d-2 | medium |" in brief
    assert "| j-x-8d1d-3 | low |" in brief


def test_explicit_model_skips_the_band_grid(tmp_path, monkeypatch):
    """An operator-typed model is authority over the grid: the real resolver
    declines unpinned work when a model is pinned, so the joiners ride the
    caller's default lane and the model rides every spawn."""
    calls = _wire(monkeypatch, tmp_path, BANDED_PLAN)
    receipt = join_node("x-8d1d", 5, model="my-model")
    assert len(calls) == 3
    for call in calls:
        assert call["cmd"][call["cmd"].index("--model") + 1] == "my-model"
        assert call["cmd"][call["cmd"].index("--harness") + 1] == "claude"
        assert call_env(call, "FNO_WORKER_BAND") in ("high", "medium", "low")
    assert all(lane["harness"] is None for lane in receipt["lanes"].values())
    assert all(lane.get("grid") == "declined" for lane in receipt["lanes"].values())


# ---------------------------------------------------------------------------
# The roster-name rung in resolve_task_holder (joiner 1's holder contract,
# completed for daemon-forked workers where the env export cannot reach)
# ---------------------------------------------------------------------------


def _identity(monkeypatch, tmp_path, session_id):
    import json
    from types import SimpleNamespace

    reg = tmp_path / "registry.json"
    reg.write_text(json.dumps({"schema_version": 2, "agents": [
        {"name": "j-x-9734-2", "harness_session_id": session_id, "status": "live"},
    ]}))
    monkeypatch.setattr("fno.paths.agents_registry_path", lambda: reg)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda env, **_k: SimpleNamespace(
            session_id=session_id, harness="claude", disposition="proven"
        ),
    )


def test_task_holder_prefers_env_name(tmp_path, monkeypatch):
    from fno.claims.self_identity import resolve_task_holder

    holder, why = resolve_task_holder({"FNO_WORKER_NAME": "j-x-9734-1"})
    assert (holder, why) == ("j-x-9734-1", "")


def test_task_holder_reads_roster_binding_when_env_missing(tmp_path, monkeypatch):
    """The daemon fork drops the env export; the spawn-time registry row
    proves the same name for the worker it bound."""
    from fno.claims.self_identity import resolve_task_holder

    sid = "721b0775-8b99-4f8f-a067-b9642e8ced8a"
    _identity(monkeypatch, tmp_path, sid)
    holder, why = resolve_task_holder({})
    assert (holder, why) == ("j-x-9734-2", "")


def test_task_holder_ambiguous_registry_falls_back(tmp_path, monkeypatch):
    import json
    from types import SimpleNamespace

    from fno.claims.self_identity import resolve_task_holder

    sid = "aaaa0000-0000-0000-0000-000000000000"
    reg = tmp_path / "registry.json"
    reg.write_text(json.dumps({"schema_version": 2, "agents": [
        {"name": "row-a", "harness_session_id": sid},
        {"name": "row-b", "harness_session_id": sid},
    ]}))
    monkeypatch.setattr(
        "fno.paths.agents_registry_path", lambda: reg
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda env, **_k: SimpleNamespace(
            session_id=sid, harness="claude", disposition="proven"
        ),
    )
    holder, _why = resolve_task_holder({})
    assert holder == sid


def test_task_holder_missing_registry_keeps_session_id(tmp_path, monkeypatch):
    from types import SimpleNamespace

    from fno.claims.self_identity import resolve_task_holder

    sid = "bbbb0000-0000-0000-0000-000000000000"
    reg = tmp_path / "absent.json"
    monkeypatch.setattr(
        "fno.paths.agents_registry_path", lambda: reg
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda env, **_k: SimpleNamespace(
            session_id=sid, harness="claude", disposition="proven"
        ),
    )
    holder, why = resolve_task_holder({})
    assert (holder, why) == (sid, "")

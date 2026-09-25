from __future__ import annotations

import json
import subprocess as _subprocess
from types import SimpleNamespace

import pytest

from fno.agents import naming
from fno.agents.registry import AgentEntry

# The mint is a real pre-spawn subprocess; fakes route it here.
_REAL_SUBPROCESS_RUN = _subprocess.run

_NAME_VERBS = frozenset({"name-mint", "name-codes", "name-parse"})


def _is_name_verb(command) -> bool:
    return bool(_NAME_VERBS & {str(p) for p in command})


@pytest.fixture(autouse=True)
def _cold_name_codes_cache():
    """A fake that answers name-codes with pane text must fail on every run.

    ``naming._codes`` is an lru_cache'd shellout, so a fake that skips the
    name-verb route passes only when an earlier test in the same xdist worker
    already warmed the cache. Clearing it around every test makes that fake
    fail deterministically; routing through ``_is_name_verb`` is the fix, this
    fixture is the detector. The routed fake still reaches the real binary,
    which is what the sibling fakes already pay.
    """
    naming._codes.cache_clear()
    yield
    naming._codes.cache_clear()


def _row(**overrides) -> AgentEntry:
    values = {
        "name": "bp-xbdb9-retask",
        "cwd": "/repo",
        "log_path": "",
        "harness": "codex",
        "provider": None,
        "model": "gpt-5.6-sol",
        "effort": "high",
        "harness_session_id": "old-session",
        "substrate": "pane",
        "mux": {"session": "main", "pane_id": 12},
    }
    values.update(overrides)
    return AgentEntry(**values)


def _settings(**target):
    defaults = SimpleNamespace(
        provider="", model="", effort="", substrate="", permission_mode="",
        route="", account="", pane_group="", lanes=[],
    )
    profile = SimpleNamespace(**{**vars(defaults), **target})
    return SimpleNamespace(
        agents=SimpleNamespace(defaults=defaults, profiles={"target": profile}, max_lanes={}),
        model_routing=None,
    )


@pytest.fixture(autouse=True)
def _graph_with_target_node(monkeypatch):
    """Verb resolution loads the node record; default it to a planless low
    node so the probe resolves the target verb. A test overrides this by
    monkeypatching load_graph again inside its own body."""
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{"id": "x-bdb9", "difficulty": "low"}],
    )


def test_retask_node_resolution_canonicalizes_slug_and_bare_hex(monkeypatch):
    import fno.agents.retask as retask

    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{"id": "x-bdb9", "slug": "retask-destination"}],
    )
    monkeypatch.setattr("fno.graph._constants.node_id_prefix", lambda: "x-")

    assert retask._resolve_retask_node("retask-destination") == "x-bdb9"
    assert retask._resolve_retask_node("bdb9") == "x-bdb9"


def test_explicit_model_and_effort_override_target_profile():
    from fno.agents.retask import resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        model="gpt-5.6-luna",
        effort="xhigh",
        env={},
    )

    assert target.model == "gpt-5.6-luna"
    assert target.effort == "xhigh"


def _patch_rename_call(monkeypatch, sink, receipt):
    """Intercept only the transaction hand-off; the spawn-defaults resolver
    makes its own verb_call rounds that must reach the real binary."""
    from fno.rust_binary import verb_call as real_verb_call

    def fake(verb, payload, unavailable, *, timeout):
        if verb == "rename":
            sink.append(payload)
            return receipt
        return real_verb_call(verb, payload, unavailable, timeout=timeout)

    monkeypatch.setattr("fno.rust_binary.verb_call", fake)


def test_run_retask_hands_the_transaction_to_fno_agents(monkeypatch):
    """The thin front resolves row, coordinate, target_command and the pane
    ref, then hands the whole transaction to the fno-agents rename payload."""
    import fno.agents.retask as retask

    row = _row()
    monkeypatch.setattr(
        retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row)
    )
    calls: list[dict] = []
    receipt = {"status": "retasked", "switch": "skipped_same_tier"}
    _patch_rename_call(monkeypatch, calls, receipt)

    got = retask.run_retask(
        "bp-xbdb9-retask",
        node="x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )

    assert got is receipt
    assert len(calls) == 1
    payload = calls[0]
    assert payload["op"] == "retask"
    assert payload["worker"] == "bp-xbdb9-retask"
    assert payload["node"] == "x-bdb9"
    assert payload["mux"] == {"session": "main", "pane_id": 12}
    assert payload["target"]["harness"] == "codex"
    assert payload["target"]["verb"] == "target"
    assert payload["target_command"] == "$fno:target --no-merge x-bdb9"


def test_run_retask_renders_a_non_target_verb_command(monkeypatch):
    import fno.agents.retask as retask

    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{
            "id": "x-bdb9",
            "difficulty": "medium",
            "dispatch_verb": "/fno:blueprint",
        }],
    )
    row = _row()
    monkeypatch.setattr(
        retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row)
    )
    payloads: list[dict] = []
    _patch_rename_call(monkeypatch, payloads, {"status": "retasked"})

    retask.run_retask(
        "bp-xbdb9-retask",
        node="x-bdb9",
        settings=_settings(provider="codex"),
        env={},
    )

    assert payloads[0]["target"]["verb"] == "blueprint"
    assert payloads[0]["target_command"] == "/blueprint x-bdb9"


def test_run_retask_converts_a_transport_failure_into_a_refused_receipt(monkeypatch):
    from fno.agents.retask import RetaskTransportError
    import fno.agents.retask as retask

    row = _row()
    monkeypatch.setattr(
        retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row)
    )

    def fail(verb, payload, unavailable, *, timeout):
        raise RetaskTransportError("pane_send_timeout", detail="pane 12 went away")

    monkeypatch.setattr("fno.rust_binary.verb_call", fail)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "pane_send_timeout"
    assert receipt["detail"] == "pane 12 went away"
    assert receipt["cleared"] is False
    assert receipt["target_submit_confirmed"] is False


def test_run_retask_on_a_thread_hands_the_joined_pane_to_fno_agents(monkeypatch):
    """A portal already showing the thread is joined in Python; the payload
    carries the joined pane, not the row's (absent) mux ref."""
    import fno.agents.retask as retask

    row = _row(substrate="thread", mux=None, fno_id="F")
    monkeypatch.setattr(
        retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row)
    )
    monkeypatch.setattr(retask, "resolve_mux_session", lambda *_args, **_kwargs: "sess")
    payloads: list[dict] = []
    _patch_rename_call(monkeypatch, payloads, {"status": "retasked"})

    def run(command, **_kwargs):
        if _is_name_verb(command):
            return _REAL_SUBPROCESS_RUN(command, **_kwargs)
        if "thread" in command:
            return SimpleNamespace(
                returncode=0,
                stdout="portal 3: already showing bp-xbdb9-retask\n",
                stderr="",
            )
        if "pane" in command and "ls" in command:
            return SimpleNamespace(
                returncode=0,
                stdout=json.dumps(
                    [{"name": "bp-xbdb9-retask", "fno_id": "F", "pane_id": 41}]
                ),
                stderr="",
            )
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(retask.subprocess, "run", run)

    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "retasked"
    assert payloads[0]["mux"] == {"session": "sess", "pane_id": 41}


def test_run_retask_converts_thread_viewport_transport_error_to_structured_refusal(monkeypatch):
    """A resolve_thread_viewport failure must not escape run_retask as a bare exception."""
    import fno.agents.retask as retask

    row = _row(harness="claude", substrate="thread", mux=None, fno_id=None)
    monkeypatch.setattr(
        retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row)
    )

    receipt = retask.run_retask("bp-thread-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert "worker_has_no_thread_ref" in receipt["reason"]
    assert receipt["target_submit_confirmed"] is False


def test_run_retask_thread_door_refusal_carries_the_door_stderr(monkeypatch) -> None:
    """AC1-ERR: the refusal keeps reason thread_view_unavailable and adds the
    door's own stderr line as detail, so a caller can tell a reach miss from a
    broken pipe."""
    import fno.agents.retask as retask

    row = _row(harness="claude", substrate="thread", mux=None, fno_id="F")
    stderr_line = "fno mux thread: portal reach: no live row answers bp-xbdb9-retask"
    monkeypatch.setattr(
        retask, "resolve_agent", lambda *_a, **_k: SimpleNamespace(entry=row)
    )
    monkeypatch.setattr(retask, "resolve_mux_session", lambda *_a, **_k: "main")

    def run(_argv, **_kwargs):
        return SimpleNamespace(returncode=1, stdout="", stderr=stderr_line)

    monkeypatch.setattr(retask.subprocess, "run", run)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "thread_view_unavailable"
    assert receipt["detail"] == stderr_line
    assert receipt["cleared"] is False


def test_planless_blueprint_node_resolves_a_blueprint_coordinate(monkeypatch):
    """The node's dispatch_verb drives the profile, so a planless blueprint
    node resolves a blueprint coordinate, not the target profile's tier."""
    import fno.agents.retask as retask

    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{
            "id": "x-bdb9",
            "difficulty": "medium",
            "dispatch_verb": "/fno:blueprint",
        }],
    )
    settings = _settings(
        provider="claude",
        model="claude-opus-5",
        permission_mode="bypassPermissions",
    )
    settings.agents.profiles = {
        "target": settings.agents.profiles["target"],
        "blueprint": settings.agents.profiles["target"],
    }

    target = retask.resolve_target_coordinate("x-bdb9", settings=settings, env={})

    assert target.verb == "blueprint"
    assert target.harness == "claude"
    assert target.model == "claude-opus-5"


def _fake_parse_many(names):
    rows = []
    for name in names:
        parts = (name or "").split("-")
        verb = parts[1] if len(parts) > 2 else ""
        node = parts[2] if len(parts) > 2 else ""
        rows.append(naming.DispatchName(name, parts[0] if parts else "", verb, node, ""))
    return rows


def _bp_graph() -> dict:
    return {
        "x-e1": {"parent": None},
        "x-aaaa": {
            "parent": "x-e1",
            "sessions": [{"phase": "blueprint", "ended_at": "2026-09-15T00:31:00Z"}],
        },
        "x-bbbb": {"parent": "x-e1"},
        "x-e2": {"parent": None},
        "x-cccc": {"parent": "x-e2", "sessions": [{"phase": "blueprint", "ended_at": "2026-09-15T00:31:00Z"}]},
    }


def _eligible_row(**overrides) -> AgentEntry:
    values = dict(
        name="ac-bp-x-aaaa-slug",
        substrate="thread",
        inside_leg={"state": "done", "received_at": "2026-09-15T00:30:00Z"},
        node="x-aaaa",
    )
    values.update(overrides)
    return _row(**values)


def _pick(graph, rows, node_id="x-bbbb"):
    import fno.agents.retask as retask

    return retask.finished_planner(
        rows,
        node_id=node_id,
        graph=graph,
        project_id="proj",
        project_of=lambda cwd: "proj" if cwd == "/repo" else "other",
    )


def test_finished_planner_picks_the_earliest_finished_row_on_the_epic(monkeypatch):
    monkeypatch.setattr("fno.agents.retask.parse_many", _fake_parse_many)
    late = _eligible_row(
        name="ab-bp-x-aaaa-late",
        inside_leg={"state": "done", "received_at": "2026-09-15T01:00:00Z"},
    )
    early = _eligible_row()
    got = _pick(_bp_graph(), [late, early])
    assert got is not None and got.name == "ac-bp-x-aaaa-slug"


@pytest.mark.parametrize(
    "overrides",
    [
        pytest.param({"status": "exited"}, id="status-exited"),
        pytest.param(
            {"inside_leg": {"state": "working", "received_at": "2026-09-15T00:30:00Z"}},
            id="state-working",
        ),
        pytest.param(
            {"inside_leg": {"state": "blocked", "received_at": "2026-09-15T00:30:00Z"}},
            id="state-blocked",
        ),
        pytest.param({"name": "cl-t-x-aaaa-slug"}, id="verb-t"),
        pytest.param({"name": "ac-bp-x-cccc-slug", "node": "x-cccc"}, id="other-epic"),
        pytest.param({"cwd": "/elsewhere"}, id="other-project"),
    ],
)
def test_finished_planner_skips_every_row_that_fails_one_filter(monkeypatch, overrides):
    monkeypatch.setattr("fno.agents.retask.parse_many", _fake_parse_many)
    assert _pick(_bp_graph(), [_eligible_row(**overrides)]) is None


def test_finished_planner_skips_rows_the_graph_filters_refuse(monkeypatch):
    monkeypatch.setattr("fno.agents.retask.parse_many", _fake_parse_many)
    graph = _bp_graph()
    # Dispatched node with no epic parent: nothing is reusable.
    graph["x-bbbb"] = {"parent": None}
    assert _pick(graph, [_eligible_row()]) is None
    # Candidate's node carries a blueprint session that never ended.
    graph = _bp_graph()
    graph["x-aaaa"]["sessions"] = [{"phase": "blueprint", "ended_at": None}]
    assert _pick(graph, [_eligible_row()]) is None
    # Candidate row IS the dispatched node.
    assert _pick(_bp_graph(), [_eligible_row()], node_id="x-aaaa") is None


def test_ready_target_node_keeps_the_zai_lane_coordinate(tmp_path, monkeypatch):
    """A ready node still resolves the target profile, so the lane an opus
    row cannot take stays visible in the coordinate."""
    import fno.agents.retask as retask

    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\nkind: quick-plan\n---\n")
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{
            "id": "x-bdb9",
            "difficulty": "medium",
            "dispatch_verb": None,
            "cwd": str(tmp_path),
            "plan_path": "plan.md",
        }],
    )
    settings = _settings(route="zai/glm-5.3-flash[1m]")

    target = retask.resolve_target_coordinate("x-bdb9", settings=settings, env={})

    assert target.provider == "zai"
    assert target.model == "glm-5.3-flash[1m]"


def test_unresolvable_dispatch_verb_refuses_instead_of_guessing(monkeypatch):
    import pytest

    import fno.agents.retask as retask
    from fno.agents.harness_map import DispatchResolveError

    monkeypatch.setattr("fno.graph.load.load_graph", lambda: [])

    with pytest.raises(DispatchResolveError):
        retask.resolve_target_coordinate("x-bdb9", env={})


def test_run_retask_refuses_when_the_dispatch_verb_cannot_resolve(monkeypatch):
    import fno.agents.retask as retask

    monkeypatch.setattr("fno.graph.load.load_graph", lambda: [])
    row = _row()
    monkeypatch.setattr(retask, "resolve_agent", lambda *_a, **_k: SimpleNamespace(entry=row))

    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "dispatch_verb_unresolved"
    assert receipt["detail"]


def test_thread_viewport_refusal_names_the_substrate_and_cause() -> None:
    """AC6-EDGE: an absent thread ref reads as a row defect, not a broken pipe."""
    from fno.agents.retask import RetaskTransportError, resolve_thread_viewport

    entry = _row(harness="claude", substrate="thread", mux=None, fno_id=None)

    with pytest.raises(RetaskTransportError) as excinfo:
        resolve_thread_viewport(entry)

    message = str(excinfo.value)
    assert "worker_has_no_thread_ref" in message
    assert entry.name in message
    assert "thread" in message


def test_thread_viewport_reaches_by_registry_name_and_joins_the_opened_pane(
    monkeypatch,
) -> None:
    """AC1-HP: the door is keyed by the row name; the join still matches fno_id."""
    import fno.agents.retask as retask

    entry = _row(harness="claude", substrate="thread", mux=None, fno_id="F", name="bp-x")
    calls: list[list[str]] = []

    def run(argv, **_kwargs):
        calls.append(list(argv))
        if "thread" in argv:
            return SimpleNamespace(returncode=0, stdout="", stderr="")
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps([{"name": "bp-x", "fno_id": "F", "pane_id": 7}]),
            stderr="",
        )

    monkeypatch.setattr(retask, "resolve_mux_session", lambda *_args, **_kwargs: "main")
    monkeypatch.setattr(retask.subprocess, "run", run)

    assert retask.resolve_thread_viewport(entry) == ("main", 7)
    assert calls[0] == [
        "fno", "mux", "thread", "--server", "main", "bp-x",
        "--portal", "new", "--tab", "new",
    ]

from __future__ import annotations

import json
import subprocess as _subprocess
from dataclasses import replace
from types import SimpleNamespace

import pytest

from fno.agents import naming
from fno.agents.registry import AgentEntry

# The mint is a real pre-spawn subprocess (x-84b2); fakes route it here.
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


def _screen_verdict(
    *,
    matched: bool = True,
    rule_id: str | None = "idle_prompt",
    state: str | None = "idle",
) -> dict:
    return {"matched": matched, "rule_id": rule_id, "state": state}


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


def test_same_tier_builds_target_payload_without_executable_switch_commands():
    from fno.agents.harness_map import dispatch_command
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = detect_retask(_row(), target, node="x-bdb9")

    assert receipt["outcome"] == "retask_ready"
    assert receipt["payload"]["target_command"] == dispatch_command("codex").format(id="x-bdb9")
    assert receipt["payload"]["switch"] == {"required": False}
    assert receipt["payload"]["execution"] == {"mode": "read_only_plan"}


def test_tier_mismatch_builds_mechanism_neutral_switch_pending_payload():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-luna", effort="xhigh"),
        env={},
    )
    receipt = detect_retask(_row(), target, node="x-bdb9")

    assert receipt["outcome"] == "switch_pending"
    assert receipt["payload"]["switch"] == {
        "required": True,
        "from": {"model": "gpt-5.6-sol", "effort": "high"},
        "to": {"model": "gpt-5.6-luna", "effort": "xhigh"},
        "mechanism": "pending_operator_decision",
    }
    assert receipt["payload"]["execution"] == {"mode": "read_only_plan"}


def test_default_target_vendor_preserves_the_registry_vendor_axis():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = detect_retask(_row(provider="openai"), target, node="x-bdb9")

    assert receipt["outcome"] == "retask_ready"


@pytest.mark.parametrize(
    ("target_override", "reason", "live_mode"),
    [
        ({"harness": "claude"}, "harness", None),
        ({"provider": "zai", "route": "zai/glm-5.3", "model": "glm-5.3"}, "provider", None),
        ({"substrate": "bg"}, "substrate", None),
        ({"permission_mode": "yolo"}, "permission_mode", "bypassPermissions"),
        ({"permission_mode": "bypassPermissions"}, "permission_mode_unobserved", None),
        ({"account": "work"}, "account", None),
    ],
)
def test_incompatible_axis_requires_spawn_before_any_payload(target_override, reason, live_mode):
    """AC2-ERR: the mode compare is against the live worker, and an
    unobservable mode fails closed; a legacy None launch_account counts as
    different."""
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    target = replace(target, **target_override)
    receipt = detect_retask(
        _row(), target, node="x-bdb9", live_permission_mode=live_mode
    )

    assert receipt == {"outcome": "spawn_required", "reason": reason}


def test_matching_live_permission_and_account_retasks_ready() -> None:
    """AC2-HP: equal live mode and account clear the compare the presence
    test always failed."""
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    target = replace(
        target,
        harness="claude",
        provider=None,
        permission_mode="bypassPermissions",
        account="zai",
    )
    receipt = detect_retask(
        _row(
            harness="claude",
            substrate="thread",
            mux=None,
            fno_id="F",
            provider="anthropic",
            launch_account="zai",
        ),
        target,
        node="x-bdb9",
        live_permission_mode="bypassPermissions",
    )

    assert receipt["outcome"] == "retask_ready"


def test_live_permission_mode_reads_the_last_transcript_record(tmp_path, monkeypatch):
    from fno.agents.retask import _live_permission_mode

    transcript = tmp_path / "session.jsonl"
    transcript.write_text(
        json.dumps({"type": "user", "message": "hi"})
        + "\n"
        + json.dumps({"type": "permission-mode", "permissionMode": "default"})
        + "\nnot json\n"
        + json.dumps({"type": "permission-mode", "permissionMode": "bypassPermissions"})
        + "\n"
        + json.dumps({"type": "user", "message": "go"})
        + "\n"
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._mux_recipient_transcript", lambda _entry: transcript
    )
    claude_row = _row(harness="claude", substrate="thread", mux=None, fno_id="F")

    assert _live_permission_mode(claude_row) == "bypassPermissions"

    # A record torn by a concurrent append does not decide; the last complete
    # record does.
    with transcript.open("a") as handle:
        handle.write('{"type":"permission-mode","permissionMode":"yolo"')
    assert _live_permission_mode(claude_row) == "bypassPermissions"

    # Another harness never reads a transcript at all.
    assert _live_permission_mode(_row()) is None


def test_non_mux_worker_is_refused_without_a_target_payload():
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    receipt = detect_retask(_row(mux=None), target, node="x-bdb9")

    assert receipt == {"outcome": "refused", "reason": "worker_has_no_mux_ref"}


@pytest.mark.parametrize(
    ("row_override", "reason"),
    [
        ({"status": "stopped"}, "worker_not_live"),
        ({"harness_session_id": None}, "worker_has_no_session_id"),
    ],
)
def test_unusable_worker_is_refused_with_a_named_positive_verdict(row_override, reason):
    from fno.agents.retask import detect_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    receipt = detect_retask(_row(**row_override), target, node="x-bdb9")

    assert receipt == {"outcome": "refused", "reason": reason}


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


def test_execute_retask_same_tier_orders_clear_rename_status_then_target():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    sends: list[tuple[str, bool]] = []
    tiers: list[tuple[str, str]] = []
    frames = iter([
        "› Ask Codex to do anything\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
    ])

    def send(text: str, submit: bool) -> bool:
        sends.append((text, submit))
        return True

    receipt = execute_retask(
        _row(screen_state=None),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        ready_frame=lambda _frame: _screen_verdict(),
        send=send,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
        project_tier=lambda model, effort: tiers.append((model, effort)),
    )

    assert receipt == {
        "status": "retasked",
        "cleared": True,
        "session_restamped": True,
        "switch": "skipped_same_tier",
        "switch_verified": True,
        "target_submit_confirmed": True,
        "registry_name": "target-x-bdb9",
        "source_session_id": "old-session",
        "current_session_id": "new-session",
        "transition": "succession",
        "registry_rows": 1,
        "lineage_recorded": True,
    }
    assert [text for text, _submit in sends] == [
        "/clear", "/status", "$fno:target --no-merge x-bdb9"
    ]
    assert tiers == [("gpt-5.6-sol", "high")]


def test_execute_retask_refuses_source_pr_before_clear():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    sends: list[tuple[str, bool]] = []
    receipt = execute_retask(
        _row(),
        target,
        node="x-bdb9",
        read_frame=lambda: pytest.fail("source PR guard must run first"),
        ready_frame=lambda _frame: _screen_verdict(),
        send=lambda text, submit: sends.append((text, submit)) or True,
        restamp=lambda: pytest.fail("source PR guard must run first"),
        rename=lambda _name: pytest.fail("source PR guard must run first"),
        source_preflight=lambda _entry: {
            "status": "refused",
            "reason": "source_pr_not_green",
            "pr": 1168,
            "head": "source-head",
            "verdict": "red",
            "blockers": {"failing": 4},
        },
    )

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "source_pr_not_green"
    assert receipt["pr"] == 1168
    assert sends == []


def test_source_preflight_joins_exact_session_and_refuses_open_non_green(
    monkeypatch,
):
    import fno.agents.retask as retask

    row = _row()
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": 1168,
            "sessions": [{"harness": "codex", "session_id": "old-session"}],
        }],
    )
    monkeypatch.setattr(
        retask.subprocess,
        "run",
        lambda *_args, **_kwargs: SimpleNamespace(
            returncode=1,
            stdout=json.dumps({
                "pr_state": "OPEN",
                "green": False,
                "head_sha": "source-head",
                "verdict": "red",
                "checks": {"failing": 4},
            }),
        ),
    )

    receipt = retask._source_preflight(row)

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "source_pr_not_green"
    assert receipt["source_node_id"] == "x-source"
    assert receipt["pr"] == 1168


@pytest.mark.parametrize(
    "source_overrides",
    [
        {"status": "done", "merge_status": "merged"},
        {"status": "superseded"},
    ],
)
def test_source_preflight_trusts_a_graph_closed_source_node(monkeypatch, source_overrides):
    import fno.agents.retask as retask

    row = _row()
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": 2042,
            "sessions": [{"harness": "codex", "session_id": "old-session"}],
            **source_overrides,
        }],
    )
    monkeypatch.setattr(
        retask.subprocess,
        "run",
        lambda *_args, **_kwargs: pytest.fail("no pr status read"),
    )

    receipt = retask._source_preflight(row)

    assert receipt["status"] == "ready"
    assert receipt["source_node_id"] == "x-source"


@pytest.mark.parametrize(
    "source_overrides",
    [
        {"status": "in_review"},
        {"status": "done", "merge_status": None},
    ],
)
def test_source_preflight_reads_an_open_source_pr_once_without_refresh(
    monkeypatch,
    source_overrides,
):
    import fno.agents.retask as retask

    row = _row()
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": 1168,
            "sessions": [{"harness": "codex", "session_id": "old-session"}],
            **source_overrides,
        }],
    )
    calls: list[list[str]] = []

    def run(command, **_kwargs):
        calls.append(list(command))
        return SimpleNamespace(
            returncode=1,
            stdout=json.dumps({
                "pr_state": "OPEN",
                "green": True,
                "head_sha": "source-head",
                "verdict": "green",
            }),
        )

    monkeypatch.setattr(retask.subprocess, "run", run)

    receipt = retask._source_preflight(row)

    assert receipt["status"] == "ready"
    assert calls == [["fno", "do", "pr", "status", "1168"]]


def test_source_preflight_folds_a_pr_status_failure_into_the_unknown_refusal(
    monkeypatch,
):
    import fno.agents.retask as retask

    row = _row()
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": 1168,
            "status": "in_review",
            "sessions": [{"harness": "codex", "session_id": "old-session"}],
        }],
    )

    def run(*_args, **_kwargs):
        raise _subprocess.TimeoutExpired(cmd="fno do pr status", timeout=60)

    monkeypatch.setattr(retask.subprocess, "run", run)

    receipt = retask._source_preflight(row)

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "source_pr_status_unknown"
    assert "error" in receipt


def test_source_preflight_multi_phase_entries_on_one_node_are_not_ambiguous(
    monkeypatch,
):
    import fno.agents.retask as retask

    row = _row()
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [{
            "id": "x-source",
            "cwd": "/repo",
            "pr_number": None,
            "sessions": [
                {"harness": "codex", "session_id": "old-session", "phase": "think"},
                {"harness": "codex", "session_id": "old-session", "phase": "blueprint"},
            ],
        }],
    )

    receipt = retask._source_preflight(row)

    assert receipt["status"] == "ready"
    assert receipt["source_node_id"] == "x-source"


def test_source_preflight_two_distinct_nodes_stay_ambiguous(monkeypatch):
    import fno.agents.retask as retask

    row = _row()
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda: [
            {
                "id": "x-one",
                "pr_number": None,
                "sessions": [{"harness": "codex", "session_id": "old-session"}],
            },
            {
                "id": "x-two",
                "pr_number": None,
                "sessions": [{"harness": "codex", "session_id": "old-session"}],
            },
        ],
    )

    receipt = retask._source_preflight(row)

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "source_node_ambiguous"


def test_execute_retask_accepts_x_dfe7_succession_receipt_and_names_one_row():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    sends: list[tuple[str, bool]] = []
    frames = iter([
        "› Ask Codex to do anything\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
    ])
    receipt = execute_retask(
        _row(),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        ready_frame=lambda _frame: _screen_verdict(),
        send=lambda text, submit: sends.append((text, submit)) or True,
        restamp=lambda: {
            "classification": "succession",
            "predecessor_session_id": "old-session",
            "current_session_id": "new-session",
            "registry_rows": 1,
            "lineage_recorded": True,
        },
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["transition"] == "succession"
    assert receipt["source_session_id"] == "old-session"
    assert receipt["current_session_id"] == "new-session"
    assert receipt["registry_rows"] == 1
    assert receipt["lineage_recorded"] is True
    assert receipt["target_submit_confirmed"] is True


def test_run_retask_parses_codex_clear_receipt_before_accepting_successor(monkeypatch):
    import fno.agents.retask as retask

    row = _row()
    target = retask.RetaskCoordinate(
        harness="codex", provider=None, model="gpt-5.6-sol", effort="high",
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    successor = SimpleNamespace(
        name=row.name,
        harness="codex",
        harness_session_id="new-session",
        predecessor_session_ids=["old-session"],
        forked_from_session_id=None,
    )
    reads = iter([
        "› Ask Codex to do anything\n",
        "To continue this session, run codex resume old-session\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
    ])
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})
    monkeypatch.setattr(retask, "load_registry", lambda **_kwargs: [successor])
    renamed: list[dict] = []
    monkeypatch.setattr(
        retask,
        "rename_agent",
        lambda *_args, **kwargs: renamed.append(kwargs)
        or SimpleNamespace(name="target-x-bdb9"),
    )
    monkeypatch.setattr("fno.agents.registry.project_verified_tier", lambda *_args, **_kwargs: None)
    monkeypatch.setattr("fno.agents.mux_spawn._pane_osc_title", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: _screen_verdict(),
    )

    def run(command, **_kwargs):
        if _is_name_verb(command):
            return _REAL_SUBPROCESS_RUN(command, **_kwargs)
        if "read" in command:
            return SimpleNamespace(returncode=0, stdout=next(reads), stderr="")
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(retask.subprocess, "run", run)

    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "retasked"
    assert receipt["source_session_id"] == "old-session"
    assert receipt["current_session_id"] == "new-session"
    assert receipt["transition"] == "succession"
    assert receipt["registry_rows"] == 1
    assert renamed == [{"node": "x-bdb9", "registry_path": None}]


def test_run_retask_on_a_thread_a_portal_already_shows_submits_into_that_portal(monkeypatch):
    import fno.agents.retask as retask

    row = _row(substrate="thread", mux=None, fno_id="F")
    target = retask.RetaskCoordinate(
        harness="codex", provider=None, model="gpt-5.6-sol", effort="high",
        substrate="thread", permission_mode=None, route=None, account=None,
    )
    successor = SimpleNamespace(
        name=row.name,
        harness="codex",
        harness_session_id="new-session",
        predecessor_session_ids=["old-session"],
        forked_from_session_id=None,
    )
    reads = iter([
        "› Ask Codex to do anything\n",
        "To continue this session, run codex resume old-session\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
    ])
    commands: list[list[str]] = []
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})
    monkeypatch.setattr(retask, "load_registry", lambda **_kwargs: [successor])
    monkeypatch.setattr(
        retask,
        "rename_agent",
        lambda *_args, **_kwargs: SimpleNamespace(name="target-x-bdb9"),
    )
    monkeypatch.setattr("fno.agents.registry.project_verified_tier", lambda *_args, **_kwargs: None)
    monkeypatch.setattr("fno.agents.mux_spawn._pane_osc_title", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: _screen_verdict(),
    )
    monkeypatch.setattr(retask, "resolve_mux_session", lambda *_args, **_kwargs: "sess")

    def run(command, **_kwargs):
        if _is_name_verb(command):
            return _REAL_SUBPROCESS_RUN(command, **_kwargs)
        commands.append([str(p) for p in command])
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
        if "read" in command:
            return SimpleNamespace(returncode=0, stdout=next(reads), stderr="")
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(retask.subprocess, "run", run)

    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "retasked"
    door_calls = [c for c in commands if "thread" in c]
    assert len(door_calls) == 1, "one control-door call, joined not reopened"
    pane_ops = [
        c for c in commands
        if any(word in c for word in ("read", "wait", "send"))
    ]
    assert pane_ops, "the transaction ran over the joined pane"
    assert all("41" in c for c in pane_ops), "every pane op names the joined pane"
    submit_ops = [c for c in pane_ops if "send" in c and any("x-bdb9" in p for p in c)]
    assert len(submit_ops) == 1, "the target submit rode the joined pane"


def test_run_retask_succession_verdict_rides_the_shared_classifier(monkeypatch):
    """A shared-verdict refusal from x-dfe7's classifier refuses the retask."""
    import fno.agents.retask as retask

    row = _row()
    target = retask.RetaskCoordinate(
        harness="codex", provider=None, model="gpt-5.6-sol", effort="high",
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    successor = SimpleNamespace(
        name=row.name,
        harness="codex",
        harness_session_id="new-session",
        predecessor_session_ids=["old-session"],
        forked_from_session_id=None,
    )
    reads = iter([
        "› Ask Codex to do anything\n",
        "To continue this session, run codex resume old-session\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
    ])
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})
    monkeypatch.setattr(retask, "load_registry", lambda **_kwargs: [successor])
    monkeypatch.setattr(retask, "classify_session_transition", lambda *_args: "deferred")
    monkeypatch.setattr(retask, "rename_agent", lambda *_args, **_kwargs: pytest.fail("classifier refusal must stop before rename"))
    monkeypatch.setattr("fno.agents.registry.project_verified_tier", lambda *_args, **_kwargs: None)
    monkeypatch.setattr("fno.agents.mux_spawn._pane_osc_title", lambda *_args, **_kwargs: None)
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: _screen_verdict(),
    )

    def run(command, **_kwargs):
        if _is_name_verb(command):
            return _REAL_SUBPROCESS_RUN(command, **_kwargs)
        if "read" in command:
            return SimpleNamespace(returncode=0, stdout=next(reads), stderr="")
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(retask.subprocess, "run", run)

    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "session_transition_not_succession"
    assert receipt["cleared"] is True
    assert "registry_name" not in receipt


@pytest.mark.parametrize(
    "transition",
    [
        {
            "classification": "branch",
            "reason": "session_transition_not_succession",
            "predecessor_session_id": "old-session",
            "current_session_id": "new-session",
        },
        {
            "classification": "succession",
            "predecessor_session_id": "other-session",
            "current_session_id": "new-session",
            "registry_rows": 1,
            "lineage_recorded": True,
        },
        {
            "classification": "succession",
            "predecessor_session_id": "old-session",
            "current_session_id": "new-session",
            "registry_rows": 2,
            "lineage_recorded": True,
        },
    ],
)
def test_execute_retask_refuses_any_transition_weaker_than_one_succession_row(transition):
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    sends: list[tuple[str, bool]] = []
    receipt = execute_retask(
        _row(),
        target,
        node="x-bdb9",
        read_frame=lambda: "› Ask Codex to do anything\n",
        ready_frame=lambda _frame: _screen_verdict(),
        send=lambda text, submit: sends.append((text, submit)) or True,
        restamp=lambda: transition,
        rename=lambda _name: pytest.fail("rename must wait for succession proof"),
    )

    assert receipt["status"] == "refused"
    assert receipt["cleared"] is True
    assert receipt["target_submit_confirmed"] is False
    assert sends == [("/clear", True)]


def test_execute_retask_refuses_live_busy_even_when_cached_snapshot_is_idle():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = execute_retask(
        _row(screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: "painted but busy",
        ready_frame=lambda _frame: _screen_verdict(rule_id="working", state="working"),
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_not_idle"
    assert receipt["cleared"] is False
    assert receipt["target_submit_confirmed"] is False


def test_execute_retask_names_readable_unmatched_frame_as_unobserved():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    sends: list[tuple[str, bool]] = []
    receipt = execute_retask(
        _row(screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: "painted but no known manifest rule",
        ready_frame=lambda _frame: _screen_verdict(matched=False, rule_id=None, state=None),
        send=lambda text, submit: sends.append((text, submit)) or True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_state_unobserved"
    assert receipt["cleared"] is False
    assert sends == []


def test_execute_retask_names_missing_live_verdict_as_unobserved():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = execute_retask(
        _row(screen_state=None),
        target,
        node="x-bdb9",
        read_frame=lambda: "readable pane frame",
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_state_unobserved"
    assert receipt["cleared"] is False


def test_execute_retask_fails_closed_on_legacy_boolean_live_verdict():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = execute_retask(
        _row(screen_state=None),
        target,
        node="x-bdb9",
        read_frame=lambda: "readable pane frame",
        ready_frame=lambda _frame: False,
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_state_unobserved"
    assert receipt["cleared"] is False


def test_execute_retask_keeps_empty_frame_unreadable_distinct():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-sol", effort="high"),
        env={},
    )
    receipt = execute_retask(
        _row(screen_state=None),
        target,
        node="x-bdb9",
        read_frame=lambda: "",
        ready_frame=lambda _frame: pytest.fail("empty frame must not be evaluated"),
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "pane_frame_unreadable"
    assert receipt["cleared"] is False


def test_execute_retask_codex_menu_walk_verifies_each_target_before_submit():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-luna", effort="xhigh"),
        env={},
    )
    sends: list[tuple[str, bool]] = []
    tiers: list[tuple[str, str]] = []
    frames = iter([
        "› Ask Codex to do anything\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
        "Select Model and Effort\n› 1. gpt-5.6-sol (current)\n  3. gpt-5.6-luna\n",
        "Select Model and Effort\n  1. gpt-5.6-sol\n› 3. gpt-5.6-luna (current)\n",
        "Select Reasoning Level for gpt-5.6-luna\n› 2. Medium (default)\n  4. Extra high\n",
        "Select Reasoning Level for gpt-5.6-luna\n  2. Medium (default)\n› 4. Extra high\n",
        "Model: gpt-5.6-luna (reasoning xhigh, summaries auto)",
    ])

    def send(text: str, submit: bool) -> bool:
        sends.append((text, submit))
        return True

    receipt = execute_retask(
        _row(screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        ready_frame=lambda _frame: _screen_verdict(),
        send=send,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
        project_tier=lambda model, effort: tiers.append((model, effort)),
    )

    assert receipt["status"] == "retasked"
    assert receipt["switch"] == "switched"
    assert receipt["switch_verified"] is True
    assert receipt["target_submit_confirmed"] is True
    assert sends[0] == ("/clear", True)
    assert sends[-1] == ("$fno:target --no-merge x-bdb9", True)
    assert ("/model", True) in sends
    assert ("", True) in sends
    assert any(text == "\x1b[B" and not submit for text, submit in sends)
    assert tiers == [("gpt-5.6-sol", "high"), ("gpt-5.6-luna", "xhigh")]


def test_execute_retask_claude_uses_direct_strategy_commands():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="claude", model="new-model", effort="xhigh"),
        env={},
    )
    sends: list[tuple[str, bool]] = []
    frames = iter([
        "ready",
        "Model: old-model (reasoning high, summaries auto)",
        "Model: new-model (reasoning xhigh, summaries auto)",
    ])

    receipt = execute_retask(
        _row(
            harness="claude",
            model="old-model",
            effort="high",
            screen_state={"state": "idle", "rule": "live_prompt_box"},
        ),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        ready_frame=lambda _frame: _screen_verdict(rule_id="live_prompt_box"),
        send=lambda text, submit: (sends.append((text, submit)) or True),
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["status"] == "retasked"
    assert ("/model new-model", True) in sends
    assert ("/effort xhigh", True) in sends
    assert [text for text, _submit in sends if text.startswith("/model")] == ["/model new-model"]


def test_execute_retask_uses_verified_tier_when_target_axes_are_omitted():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9", settings=_settings(provider="codex"), env={}
    )
    sends: list[tuple[str, bool]] = []
    frames = iter([
        "› Ask Codex to do anything\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
    ])
    receipt = execute_retask(
        _row(model=None, effort=None, screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        ready_frame=lambda _frame: _screen_verdict(),
        send=lambda text, submit: (sends.append((text, submit)) or True),
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["status"] == "retasked"
    assert receipt["switch"] == "skipped_same_tier"


def test_run_retask_converts_mux_timeout_to_structured_refusal(monkeypatch):
    import fno.agents.retask as retask

    row = _row(screen_state={"state": "idle", "rule": "idle_prompt"})
    target = retask.RetaskCoordinate(
        harness="codex", provider=None, model="gpt-5.6-sol", effort="high",
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})

    def timeout(*_args, **_kwargs):
        raise retask.subprocess.TimeoutExpired("fno mux", 10)

    monkeypatch.setattr(retask.subprocess, "run", timeout)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "pane_read_timeout"
    assert receipt["target_submit_confirmed"] is False


def test_run_retask_converts_thread_viewport_transport_error_to_structured_refusal(monkeypatch):
    """A resolve_thread_viewport failure must not escape run_retask as a bare exception."""
    import fno.agents.retask as retask

    row = _row(harness="claude", substrate="thread", mux=None, fno_id=None)
    target = retask.RetaskCoordinate(
        harness="claude", provider=None, model=None, effort=None,
        substrate="thread", permission_mode=None, route=None, account=None,
    )
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})

    receipt = retask.run_retask("bp-thread-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert "worker_has_no_thread_ref" in receipt["reason"]
    assert receipt["target_submit_confirmed"] is False


def test_run_retask_passes_live_osc_title_to_manifest_evaluator(monkeypatch):
    import fno.agents.retask as retask

    row = _row(screen_state=None)
    target = retask.RetaskCoordinate(
        harness="codex", provider=None, model="gpt-5.6-sol", effort="high",
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    sends: list[str] = []
    observed: dict[str, object] = {}
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})
    monkeypatch.setattr(
        "fno.agents.mux_spawn._pane_osc_title",
        lambda *_args, **_kwargs: "⠋ Working",
    )

    def evaluate(_harness, _frame, _runner, *, osc_title=None, **_kwargs):
        observed["osc_title"] = osc_title
        if osc_title:
            return {"matched": True, "rule_id": "busy", "state": "working"}
        return {"matched": True, "rule_id": "idle_prompt", "state": "idle"}

    monkeypatch.setattr("fno.agents.mux_spawn._evaluate_manifest_screen", evaluate)

    def run(command, **_kwargs):
        if "send" in command:
            sends.append(command[command.index("--text") + 1])
        return SimpleNamespace(returncode=0, stdout="live frame", stderr="")

    monkeypatch.setattr(retask.subprocess, "run", run)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["reason"] == "pane_not_idle"
    assert observed == {"osc_title": "⠋ Working"}
    assert sends == []


def _claude_thread_row(**overrides) -> AgentEntry:
    """A claude thread worker with predecessor lineage, for full-transit tests."""
    values = {"harness": "claude", "substrate": "thread", "mux": None, "fno_id": "F",
              "predecessor_session_ids": ["old-session"]}
    values.update(overrides)
    return _row(**values)


def _stub_claude_succession(monkeypatch) -> None:
    """Shared seams for run_retask tests that must reach past /clear."""
    import fno.agents.retask as retask

    monkeypatch.setattr(retask, "resolve_mux_session", lambda *_a, **_k: "main")
    monkeypatch.setattr(retask, "classify_session_transition", lambda *_a, **_k: "succession")
    monkeypatch.setattr(
        retask,
        "load_registry",
        lambda **_kwargs: [SimpleNamespace(
            name="bp-xbdb9-retask", harness="claude",
            harness_session_id="new-session",
            forked_from_session_id="old-session",
            predecessor_session_ids=["old-session"],
        )],
    )
    monkeypatch.setattr(
        retask,
        "rename_agent",
        lambda *_args, **_kwargs: SimpleNamespace(name="target-x-bdb9"),
    )
    monkeypatch.setattr(
        "fno.agents.registry.project_verified_tier", lambda *_args, **_kwargs: None
    )
    monkeypatch.setattr("fno.agents.mux_spawn._pane_osc_title", lambda *_args: None)


def test_run_retask_retasks_a_claude_thread_worker_whose_title_is_none(monkeypatch):
    """AC4-HP (x-3ea6): a rule-bordered idle composer with an unreadable title
    is a verdict, not a refusal - the manifest's grid rules decide."""
    import fno.agents.retask as retask

    row = _claude_thread_row(model="old-model", effort="high")
    target = retask.RetaskCoordinate(
        harness="claude", provider=None, model="old-model", effort="high",
        substrate="thread", permission_mode=None, route=None, account=None,
    )
    sends: list[str] = []
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})
    _stub_claude_succession(monkeypatch)
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: {
            "matched": True,
            "rule_id": "live_prompt_box",
            "state": "idle",
        },
    )

    def run(command, **_kwargs):
        if _is_name_verb(command):
            return _REAL_SUBPROCESS_RUN(command, **_kwargs)
        if "send" in command:
            sends.append(command[command.index("--text") + 1])
        if "ls" in command:
            return SimpleNamespace(
                returncode=0,
                stdout=json.dumps([{"name": "bp-xbdb9-retask", "fno_id": "F", "pane_id": 7}]),
                stderr="",
            )
        return SimpleNamespace(
            returncode=0,
            stdout=(
                "─── t-name ─\n❯ \n────────────\n"
                "Model: old-model (reasoning effort high)\n"
                "To continue this session, run codex resume old-session"
            ),
            stderr="",
        )

    monkeypatch.setattr(retask.subprocess, "run", run)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "retasked", receipt
    assert "pane title unreadable" not in str(receipt)
    assert "/clear" in sends


def test_run_retask_exit_23_on_clear_reports_view_left_worker(monkeypatch):
    """AC4-ERR (x-3ea6): the send gate's identity refusal on /clear names the
    cause, carries the gate's stderr as detail, and keeps cleared false."""
    import fno.agents.retask as retask

    row = _claude_thread_row()
    target = retask.RetaskCoordinate(
        harness="claude", provider=None, model=None, effort=None,
        substrate="thread", permission_mode=None, route=None, account=None,
    )
    stderr_line = (
        "fno mux pane send: pane 7 is the portal for bp-xbdb9-retask (attach deadbee1) "
        "but its child runs claude agents; the viewer left that session"
    )
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})
    _stub_claude_succession(monkeypatch)
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: _screen_verdict(rule_id="live_prompt_box"),
    )

    def run(command, **_kwargs):
        if "send" in command:
            return SimpleNamespace(returncode=23, stdout="", stderr=f"{stderr_line}\n")
        if "ls" in command:
            return SimpleNamespace(
                returncode=0,
                stdout=json.dumps([{"name": "bp-xbdb9-retask", "fno_id": "F", "pane_id": 7}]),
                stderr="",
            )
        return SimpleNamespace(returncode=0, stdout="frame", stderr="")

    monkeypatch.setattr(retask.subprocess, "run", run)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "view_left_worker"
    assert receipt["detail"] == stderr_line
    assert receipt["cleared"] is False
    assert receipt["session_restamped"] is False


def test_run_retask_exit_23_without_the_portal_marker_names_the_family(monkeypatch):
    """AC4-ERR (x-3ea6): an identity refusal that is NOT the portal gate keeps
    the family reason with the gate's own line as detail."""
    import fno.agents.retask as retask

    row = _claude_thread_row()
    target = retask.RetaskCoordinate(
        harness="claude", provider=None, model=None, effort=None,
        substrate="thread", permission_mode=None, route=None, account=None,
    )
    stderr_line = (
        "fno mux pane send: pane 7 carries label bp-xbdb9-retask but no session "
        "id resolves for it; re-address by session id through fno mux where"
    )
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})
    _stub_claude_succession(monkeypatch)
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: _screen_verdict(rule_id="live_prompt_box"),
    )

    def run(command, **_kwargs):
        if "send" in command:
            return SimpleNamespace(returncode=23, stdout="", stderr=f"{stderr_line}\n")
        if "ls" in command:
            return SimpleNamespace(
                returncode=0,
                stdout=json.dumps([{"name": "bp-xbdb9-retask", "fno_id": "F", "pane_id": 7}]),
                stderr="",
            )
        return SimpleNamespace(returncode=0, stdout="frame", stderr="")

    monkeypatch.setattr(retask.subprocess, "run", run)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "identity_refused"
    assert receipt["detail"] == stderr_line


def test_run_retask_exit_23_after_clear_keeps_the_partial_state_truthful(monkeypatch):
    """AC4-ERR (x-3ea6): the gate refusing a later send must not unreport the
    /clear that already landed - cleared and the restamp stay in the receipt."""
    import fno.agents.retask as retask

    row = _claude_thread_row()
    target = retask.RetaskCoordinate(
        harness="claude", provider=None, model=None, effort=None,
        substrate="thread", permission_mode=None, route=None, account=None,
    )
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})
    _stub_claude_succession(monkeypatch)
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: _screen_verdict(rule_id="live_prompt_box"),
    )

    def run(command, **_kwargs):
        if _is_name_verb(command):
            return _REAL_SUBPROCESS_RUN(command, **_kwargs)
        if "send" in command:
            text = command[command.index("--text") + 1]
            if text == "/clear":
                return SimpleNamespace(returncode=0, stdout="", stderr="")
            return SimpleNamespace(
                returncode=23,
                stdout="",
                stderr="fno mux pane send: pane 7 is the portal for bp-xbdb9-retask "
                "(attach deadbee1) but its child runs claude agents; the viewer left that session\n",
            )
        if "ls" in command:
            return SimpleNamespace(
                returncode=0,
                stdout=json.dumps([{"name": "bp-xbdb9-retask", "fno_id": "F", "pane_id": 7}]),
                stderr="",
            )
        return SimpleNamespace(
            returncode=0,
            stdout="frame\nTo continue this session, run codex resume old-session",
            stderr="",
        )

    monkeypatch.setattr(retask.subprocess, "run", run)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "view_left_worker"
    assert receipt["cleared"] is True
    assert receipt["session_restamped"] is True
    assert receipt["registry_name"] == "target-x-bdb9"


def test_run_retask_timeout_mid_transaction_reports_the_true_pane_state(monkeypatch):
    """A transport death after /clear + restamp + rename must not claim the
    pane is untouched: the receipt names the cleared, renamed pane."""
    import fno.agents.retask as retask

    row = _row(screen_state={"state": "idle", "rule": "idle_prompt"})
    target = retask.RetaskCoordinate(
        harness="codex", provider=None, model="gpt-5.6-sol", effort="high",
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    monkeypatch.setattr(retask, "resolve_agent", lambda *_args, **_kwargs: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_args, **_kwargs: target)
    monkeypatch.setattr(retask, "_source_preflight", lambda _entry: {"status": "ready"})
    monkeypatch.setattr(
        retask,
        "load_registry",
        lambda **_kwargs: [SimpleNamespace(name=row.name, harness_session_id="new-session")],
    )
    monkeypatch.setattr(
        retask,
        "rename_agent",
        lambda *_args, **_kwargs: SimpleNamespace(name="target-x-bdb9"),
    )
    # Readiness normally shells to the Rust manifest engine; pin its verdict
    # so the only subprocess traffic is the pane reads/sends under test.
    monkeypatch.setattr(
        "fno.agents.mux_spawn._evaluate_manifest_screen",
        lambda *_args, **_kwargs: {"matched": True, "rule_id": "idle_prompt", "state": "idle"},
    )
    monkeypatch.setattr("fno.agents.mux_spawn._pane_osc_title", lambda *_args: None)

    calls = {"n": 0}

    def timeout_after_two(*_args, **_kwargs):
        # 1: initial frame read, 2: /clear send; die while settling the clear.
        calls["n"] += 1
        if calls["n"] > 2:
            raise retask.subprocess.TimeoutExpired("fno mux", 15)
        return SimpleNamespace(returncode=0, stdout="frame")

    monkeypatch.setattr(retask.subprocess, "run", timeout_after_two)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "pane_wait_timeout"
    assert receipt["cleared"] is True
    assert receipt["session_restamped"] is False


def test_menu_delta_exact_match_beats_substring_and_shortest_wins():
    from fno.agents.retask import _menu_delta

    frame = "› 1. gpt-5.6-sol-mini\n  3. gpt-5.6-sol\n"
    assert _menu_delta(frame, "gpt-5.6-sol") == 2
    assert _menu_delta(frame, "sol") == 2
    assert _menu_delta(frame, "luna") is None


def test_execute_retask_refuses_missing_positive_menu_row_before_target():
    from fno.agents.retask import execute_retask, resolve_target_coordinate

    target = resolve_target_coordinate(
        "x-bdb9",
        settings=_settings(provider="codex", model="gpt-5.6-luna", effort="xhigh"),
        env={},
    )
    frames = iter([
        "› Ask Codex to do anything\n",
        "Model: gpt-5.6-sol (reasoning high, summaries auto)",
        "Select Model and Effort\n› 1. gpt-5.6-sol (current)\n",
    ])

    receipt = execute_retask(
        _row(screen_state={"state": "idle", "rule": "idle_prompt"}),
        target,
        node="x-bdb9",
        read_frame=lambda: next(frames),
        ready_frame=lambda _frame: _screen_verdict(),
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "model_row_missing"
    assert receipt["target_submit_confirmed"] is False


def test_execute_retask_refuses_unsupported_harness_before_clear():
    from fno.agents.retask import RetaskCoordinate, execute_retask

    target = RetaskCoordinate(
        harness="gemini", provider=None, model=None, effort=None,
        substrate="pane", permission_mode=None, route=None, account=None,
    )
    receipt = execute_retask(
        _row(harness="gemini", model=None, effort=None, screen_state={"state": "idle", "rule": "unsupported"}),
        target,
        node="x-bdb9",
        read_frame=lambda: "ready",
        send=lambda _text, _submit: True,
        restamp=lambda: "new-session",
        rename=lambda _name: "target-x-bdb9",
    )

    assert receipt["reason"] == "unsupported_switch_strategy"
    assert receipt["cleared"] is False


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


def test_run_retask_thread_door_refusal_carries_the_door_stderr(monkeypatch) -> None:
    """AC1-ERR: the refusal keeps reason thread_view_unavailable and adds the
    door's own stderr line as detail, so a caller can tell a reach miss from a
    broken pipe."""
    import fno.agents.retask as retask

    row = _row(harness="claude", substrate="thread", mux=None, fno_id="F")
    target = retask.RetaskCoordinate(
        harness="claude", provider=None, model=None, effort=None,
        substrate="thread", permission_mode=None, route=None, account=None,
    )
    stderr_line = "fno mux thread: portal reach: no live row answers bp-xbdb9-retask"
    monkeypatch.setattr(retask, "resolve_agent", lambda *_a, **_k: SimpleNamespace(entry=row))
    monkeypatch.setattr(retask, "resolve_target_coordinate", lambda *_a, **_k: target)
    monkeypatch.setattr(retask, "resolve_mux_session", lambda *_a, **_k: "main")
    monkeypatch.setattr("fno.agents.dispatch._mux_recipient_transcript", lambda _entry: None)

    def run(_argv, **_kwargs):
        return SimpleNamespace(returncode=1, stdout="", stderr=stderr_line)

    monkeypatch.setattr(retask.subprocess, "run", run)
    receipt = retask.run_retask("bp-xbdb9-retask", node="x-bdb9", env={})

    assert receipt["status"] == "refused"
    assert receipt["reason"] == "thread_view_unavailable"
    assert receipt["detail"] == stderr_line
    assert receipt["cleared"] is False


def test_planless_blueprint_node_retasks_an_opus_claude_worker(tmp_path, monkeypatch):
    """AC3-HP: the node's dispatch_verb drives the profile, so a planless
    blueprint node reaches an opus anthropic worker instead of refusing on
    the target profile's glm route."""
    import fno.agents.retask as retask
    from fno.agents.harness_map import normalize_command

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
    row = _row(
        harness="claude",
        provider="claude",
        model="claude-opus-5",
        substrate="thread",
        mux=None,
        fno_id="F",
    )
    transcript = tmp_path / "session.jsonl"
    transcript.write_text(
        json.dumps({"type": "permission-mode", "permissionMode": "bypassPermissions"})
        + "\n"
    )
    monkeypatch.setattr(
        "fno.agents.dispatch._mux_recipient_transcript", lambda _entry: transcript
    )

    target = retask.resolve_target_coordinate(
        "x-bdb9", settings=settings, env={}
    )
    receipt = retask.detect_retask(
        row, target, node="x-bdb9",
        live_permission_mode=retask._live_permission_mode(row),
    )

    assert receipt["outcome"] == "retask_ready"
    assert receipt["payload"]["target"]["verb"] == "blueprint"
    assert receipt["payload"]["target_command"] == normalize_command(
        "/blueprint {id}", "claude"
    ).format(id="x-bdb9")


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


def test_ready_target_node_keeps_the_zai_lane_and_refuses_an_opus_row(
    tmp_path, monkeypatch
):
    """AC3-ERR: a ready node still resolves the target profile, so law
    d-20293d74 holds - an opus anthropic row cannot take the glm lane."""
    import fno.agents.retask as retask

    plan = tmp_path / "plan.md"
    plan.write_text("---\nstatus: ready\n---\n")
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
    row = _row(
        harness="claude",
        provider="claude",
        model="claude-opus-5",
        substrate="thread",
        mux=None,
        fno_id="F",
    )
    monkeypatch.setattr("fno.agents.dispatch._mux_recipient_transcript", lambda _entry: None)

    target = retask.resolve_target_coordinate("x-bdb9", settings=settings, env={})
    receipt = retask.detect_retask(
        row, target, node="x-bdb9",
        live_permission_mode=retask._live_permission_mode(row),
    )

    assert receipt == {"outcome": "spawn_required", "reason": "provider"}


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

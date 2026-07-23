"""End-to-end AC coverage for the cross-model review panel (ab-6c8f4c61).

Drives the per-agent selector (``worker.review.build_review_runner``) with fakes
for the codex/gemini dispatch and the claude adapter, so every AC is exercised
through the real resolution + runner + fallback machinery without spawning a
single subprocess.
"""
from __future__ import annotations

import asyncio
from pathlib import Path

import pytest

from fno.agents.dispatch import DispatchAskError, SpawnResult
from fno.review.orchestrator import AGENT_NAMES
from fno.worker.review import build_review_runner

CORRECTNESS = ("code_reviewer", "silent_failure_hunter", "type_design_analyzer")


def _base_prompts() -> dict[str, str]:
    return {name: f"PROMPT for {name}" for name in AGENT_NAMES}


class _RecordingDispatch:
    """Fake dispatch_spawn: records calls, returns a canned reply or raises."""

    def __init__(self, reply: str = "[]", raises: Exception | None = None) -> None:
        self.reply = reply
        self.raises = raises
        self.calls: list[dict] = []

    def __call__(self, **kwargs):
        self.calls.append(kwargs)
        if self.raises is not None:
            raise self.raises
        return SpawnResult(
            kind="once",
            name=kwargs["name"],
            provider=kwargs["provider"],
            short_id="sess",
            reply=self.reply,
        )

    @property
    def providers(self) -> list[str]:
        return [c["provider"] for c in self.calls]


class _FakeClaudeDispatch:
    """Fake canonical Claude dispatch: returns a canned reply."""

    def __init__(self, stdout: str = "[]") -> None:
        self.stdout = stdout
        self.calls = 0

    def __call__(self, **_kwargs: object) -> object:
        self.calls += 1
        return SpawnResult(kind="once", name="claude", provider="claude", short_id="sess", reply=self.stdout)


def _run(runner, agent: str, prompt: str = "p", diff: str = "d"):
    return asyncio.run(runner(agent, prompt, diff))


# --- AC1-HP: default correctness subset cross-models to codex ---


def test_ac1_default_subset_routes_correctness_to_codex() -> None:
    dispatch = _RecordingDispatch(reply='[{"severity": "high", "message": "x"}]')
    adapter = _FakeClaudeDispatch("[]")
    runner, prompts, provider_set = build_review_runner(
        agent_providers={},
        cross_model_enabled=True,
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        base_prompts=_base_prompts(),
        cwd=Path("."),
        dispatch=dispatch,
        claude_adapter=adapter,
    )
    assert runner is not None
    # Cache dimension is the per-agent routing (codex review P2), not just kinds.
    routing = dict(p.split("=") for p in provider_set)
    assert routing["code_reviewer"] == "codex"
    assert routing["silent_failure_hunter"] == "codex"
    assert routing["type_design_analyzer"] == "codex"
    assert routing["ux_flow_tester"] == "claude"

    # A correctness agent dispatches on codex.
    cr = _run(runner, "code_reviewer")
    assert cr.provider == "codex"
    assert cr.ok is True
    assert dispatch.providers == ["codex"]

    # A non-correctness agent runs on claude (no codex dispatch).
    ux = _run(runner, "ux_flow_tester")
    assert ux.provider == "claude"
    assert dispatch.providers == ["codex"]  # unchanged - no new dispatch


# --- AC2-HP: operator map pins providers ---


def test_ac2_operator_map_pins_gemini() -> None:
    dispatch = _RecordingDispatch(reply="[]")
    adapter = _FakeClaudeDispatch("[]")
    runner, prompts, provider_set = build_review_runner(
        agent_providers={"ux_flow_tester": "gemini", "type_design_analyzer": "gemini"},
        cross_model_enabled=False,  # engaged via the explicit map
        implementer_provider="claude",
        available_providers=["claude", "codex", "gemini"],
        base_prompts=_base_prompts(),
        cwd=Path("."),
        dispatch=dispatch,
        claude_adapter=adapter,
    )
    assert runner is not None
    ux = _run(runner, "ux_flow_tester")
    assert ux.provider == "gemini"
    assert dispatch.providers == ["gemini"]

    # An unnamed agent (would be `alternate` under the unset default) -> claude.
    cr = _run(runner, "code_reviewer")
    assert cr.provider == "claude"
    assert dispatch.providers == ["gemini"]  # no new dispatch


# --- AC3-ERR: unparseable cross-model findings -> terminal soft fail ---


def test_ac3_prose_reply_is_terminal_soft_fail() -> None:
    dispatch = _RecordingDispatch(reply="looks good, no issues!")
    adapter = _FakeClaudeDispatch("[]")
    runner, _, _ = build_review_runner(
        agent_providers={"code_reviewer": "gemini"},
        cross_model_enabled=False,
        implementer_provider="claude",
        available_providers=["claude", "gemini"],
        base_prompts=_base_prompts(),
        cwd=Path("."),
        dispatch=dispatch,
        claude_adapter=adapter,
    )
    cr = _run(runner, "code_reviewer")
    assert cr.ok is False
    assert "findings-parse-failed" in cr.error
    assert cr.provider == "gemini"
    # Terminal: NOT retried on claude (the provider answered, just not in JSON).
    assert adapter.calls == 0
    # Panel completes for the rest: an unnamed agent still verdicts on claude.
    other = _run(runner, "multi_device_checker")
    assert other.ok is True
    assert other.provider == "claude"


# --- AC4-EDGE: single-provider degradation ---


def test_ac4_single_provider_degrades_to_claude() -> None:
    dispatch = _RecordingDispatch()
    adapter = _FakeClaudeDispatch("[]")
    runner, prompts, provider_set = build_review_runner(
        agent_providers={},
        cross_model_enabled=True,
        implementer_provider="claude",
        available_providers=["claude"],  # nothing differs
        base_prompts=_base_prompts(),
        cwd=Path("."),
        dispatch=dispatch,
        claude_adapter=adapter,
    )
    assert all(p.endswith("=claude") for p in provider_set)
    assert len(provider_set) == len(AGENT_NAMES)
    cr = _run(runner, "code_reviewer")
    assert cr.provider == "claude"
    assert cr.note and "cross-model unavailable" in cr.note
    assert dispatch.calls == []  # never dispatched off-claude


# --- AC5-FR: provider lockout mid-panel -> fall through to claude ---


def test_ac5_lockout_falls_through_to_claude() -> None:
    dispatch = _RecordingDispatch(raises=DispatchAskError("locked out", exit_code=13))
    adapter = _FakeClaudeDispatch("[]")
    runner, _, _ = build_review_runner(
        agent_providers={"code_reviewer": "codex"},
        cross_model_enabled=False,
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        base_prompts=_base_prompts(),
        cwd=Path("."),
        dispatch=dispatch,
        claude_adapter=adapter,
    )
    cr = _run(runner, "code_reviewer")
    # Dispatch was attempted on codex, failed, fell through to claude.
    assert dispatch.providers == ["codex"]
    assert adapter.calls == 1
    assert cr.ok is True
    assert cr.provider == "claude"
    assert cr.note and "fell back to claude" in cr.note

    # Another agent is unaffected (still routes claude cleanly).
    other = _run(runner, "ux_flow_tester")
    assert other.ok is True
    assert other.provider == "claude"


# --- AC6-UI: attribution + headless determinism ---


def test_ac6_prompts_carry_json_contract_and_forbid_interactive() -> None:
    runner, prompts, provider_set = build_review_runner(
        agent_providers={},
        cross_model_enabled=True,
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        base_prompts=_base_prompts(),
        cwd=Path("."),
    )
    assert runner is not None
    for body in prompts.values():
        assert "JSON array" in body
        assert "headless" in body
        assert "interactive" in body  # forbids clarifying/interactive questions


def test_ac6_attribution_present_on_every_outcome() -> None:
    dispatch = _RecordingDispatch(reply="[]")
    adapter = _FakeClaudeDispatch("[]")
    runner, _, _ = build_review_runner(
        agent_providers={},
        cross_model_enabled=True,
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        base_prompts=_base_prompts(),
        cwd=Path("."),
        dispatch=dispatch,
        claude_adapter=adapter,
    )
    for agent in AGENT_NAMES:
        outcome = _run(runner, agent)
        assert outcome.provider in ("claude", "codex")


# --- Cross-model OFF is byte-for-byte unchanged ---


def test_unknown_agent_key_warns(capsys: pytest.CaptureFixture) -> None:
    """A typo'd agent name in the map is ignored with a warning (Failure Mode)."""
    runner, _, _ = build_review_runner(
        agent_providers={"code_revewer": "codex"},  # typo: should be code_reviewer
        cross_model_enabled=False,
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        base_prompts=_base_prompts(),
    )
    assert runner is not None  # engaged (non-empty map)
    err = capsys.readouterr().err
    assert "unknown agent" in err
    assert "code_revewer" in err


def test_off_path_returns_no_runner() -> None:
    runner, prompts, provider_set = build_review_runner(
        agent_providers={},
        cross_model_enabled=False,
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        base_prompts=_base_prompts(),
    )
    assert (runner, prompts, provider_set) == (None, None, None)


def test_off_path_report_has_no_cross_model_lines() -> None:
    """An all-claude result (no provider set) renders the legacy report."""
    from fno.review.orchestrator import OrchestratorResult, WorkerOutcome
    from fno.review.report_builder import render_artifact_markdown

    result = OrchestratorResult(
        findings=[],
        workers_completed=6,
        workers_failed=0,
        suspicious=False,
        duration_seconds=1.0,
        outcomes=[WorkerOutcome(agent=a, ok=True) for a in AGENT_NAMES],
    )
    md = render_artifact_markdown("sess", result, "ready-to-merge")
    assert "Cross-model" not in md
    assert "[codex]" not in md
    assert "[claude]" not in md


def test_report_renders_attribution_softfail_and_cost_line() -> None:
    """ON-path report: provider tags, soft-fail note, degradation note, cost line."""
    from fno.review.orchestrator import OrchestratorResult, WorkerOutcome
    from fno.review.report_builder import render_artifact_markdown

    outcomes = [
        WorkerOutcome(agent="code_reviewer", ok=True, provider="codex", model="gpt-x"),
        WorkerOutcome(agent="ux_flow_tester", ok=True, provider="claude"),
        WorkerOutcome(
            agent="silent_failure_hunter", ok=False, provider="gemini",
            error="findings-parse-failed: bad json",
        ),
        WorkerOutcome(
            agent="type_design_analyzer", ok=True, provider="claude",
            note="cross-model unavailable: ran on claude",
        ),
    ]
    result = OrchestratorResult(
        findings=[], workers_completed=3, workers_failed=1,
        suspicious=False, duration_seconds=1.0, outcomes=outcomes,
    )
    md = render_artifact_markdown("sess", result, "done-with-concerns")
    assert "[codex/gpt-x]" in md
    assert "[claude]" in md
    assert "[gemini]" in md
    assert "agent errored (unparseable findings)" in md
    assert "cross-model unavailable: ran on claude" in md
    assert "Billed a second provider's quota this run: codex, gemini" in md


# --- codex review P2: cache dimension correctness ---


def test_cache_dimension_distinguishes_agent_assignments() -> None:
    """Same kinds assigned to different agents must not collide on the key."""
    _, _, dim_a = build_review_runner(
        agent_providers={"code_reviewer": "codex"},
        cross_model_enabled=False,
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        base_prompts=_base_prompts(),
    )
    _, _, dim_b = build_review_runner(
        agent_providers={"silent_failure_hunter": "codex"},
        cross_model_enabled=False,
        implementer_provider="claude",
        available_providers=["claude", "codex"],
        base_prompts=_base_prompts(),
    )
    # Both route exactly one agent to codex, but DIFFERENT agents -> distinct dims.
    assert dim_a != dim_b


def test_orchestrator_caches_under_actual_routing_on_fallback(tmp_path) -> None:
    """A codex-requested agent that ran on claude caches under claude routing,
    so a later codex-requested read misses it and re-runs (codex review P2)."""
    from fno.review import cache as _cache
    from fno.review.orchestrator import (
        Finding,
        WorkerOutcome,
        orchestrate_review_parallel,
    )

    async def runner(agent: str, prompt: str, diff: str) -> WorkerOutcome:
        # Simulate a fallback: requested codex, actually ran on claude.
        return WorkerOutcome(
            agent=agent,
            ok=True,
            provider="claude",
            findings=[Finding(agent=agent, severity="low", message="x")],
        )

    prompts = {"code_reviewer": "p"}
    requested = ["code_reviewer=codex"]
    orchestrate_review_parallel(
        "diff",
        prompts=prompts,
        runner=runner,
        agents=["code_reviewer"],
        session_id="s1",
        artifacts_dir=tmp_path,
        git_sha_value="sha",
        provider_set=requested,
    )
    ph = _cache.prompt_hash(prompts)
    requested_key = _cache.cache_key("s1", "sha", ph, requested)
    actual_key = _cache.cache_key("s1", "sha", ph, ["code_reviewer=claude"])
    # Written under the ACTUAL (claude) routing, NOT the requested (codex) one.
    assert _cache.cache_path(actual_key, artifacts_dir=tmp_path).exists()
    assert not _cache.cache_path(requested_key, artifacts_dir=tmp_path).exists()

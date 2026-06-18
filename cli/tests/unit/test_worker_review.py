"""Tests for fno.worker.review - internal orchestrator entrypoint.

After Phase 06, worker/review.py dispatches the internal sigma-review
orchestrator (orchestrate_review_parallel + score_findings + write_artifact)
rather than polling GitHub.

AC2-END-TO-END: With mocked runner, fno review produces an artifact whose
  frontmatter passes gate check for quality_check_passed.
AC2-VERDICT-BLOCKED: critical finding -> verdict: blocked; gate returns ok=False.
AC2-VERDICT-READY: info-only findings -> verdict: ready-to-merge; gate passes.
AC2-CACHED: second call on same session+sha returns action=cached, no workers spawned.

Fix tests:
C1: score_findings uses default resolver (batched `claude -p` when the CLI
    is on PATH, pass-through otherwise).
H2: all-findings-below-threshold produces done-with-concerns (not ready-to-merge).
H6: worker layer does not call write_cache (orchestrator owns writes).
"""
from __future__ import annotations

import asyncio
import os
import subprocess
from pathlib import Path
from typing import Any
from unittest.mock import AsyncMock, MagicMock, call, patch

import pytest
import yaml

from fno.review.orchestrator import Finding, WorkerOutcome


@pytest.fixture(autouse=True)
def _isolate_scorer_from_path(monkeypatch: pytest.MonkeyPatch) -> None:
    """Default tests use pass-through scoring regardless of whether the dev
    machine has `claude` on PATH. Tests that want to exercise the resolver
    explicitly patch ``shutil.which`` or ``_resolve_default_scorer`` themselves.

    Both patches are applied via monkeypatch so they auto-restore at test
    teardown; the one-shot warning flag stays isolated per test.
    """
    import shutil as _shutil

    real_which = _shutil.which

    def _which_stub(name: str, *args, **kwargs):
        if name == "claude":
            return None
        return real_which(name, *args, **kwargs)

    monkeypatch.setattr("shutil.which", _which_stub)

    import fno.review.confidence_scorer as _cs_mod

    # Use monkeypatch so the original value is restored at teardown;
    # setting the attribute directly would leak into the next test.
    monkeypatch.setattr(_cs_mod, "_no_claude_warned", True)


# ---- WorkerOutcome attribution fields (T1.2, ab-6c8f4c61) ----


def test_worker_outcome_provider_model_default_none() -> None:
    """provider/model default to None so the all-claude path is unchanged."""
    outcome = WorkerOutcome(agent="code_reviewer", ok=True)
    assert outcome.provider is None
    assert outcome.model is None


def test_worker_outcome_provider_model_settable() -> None:
    """Runners may attribute a provider/model for the report (AC6-UI)."""
    outcome = WorkerOutcome(
        agent="code_reviewer", ok=True, provider="codex", model="gpt-5.1-codex"
    )
    assert outcome.provider == "codex"
    assert outcome.model == "gpt-5.1-codex"


# ---- Helpers ----

def _make_state(tmp_path: Path, session_id: str = "sess-abc123", extra: dict | None = None) -> Path:
    state = {
        "status": "IN_PROGRESS",
        "session_id": session_id,
        "pr_number": None,
    }
    if extra:
        state.update(extra)
    content = "---\n" + yaml.dump(state, default_flow_style=False) + "---\n# State\n"
    path = tmp_path / "target-state.md"
    path.write_text(content, encoding="utf-8")
    return path


def _make_runner(findings: list[Finding]) -> Any:
    """Return an async WorkerRunner that returns the given findings."""
    async def runner(agent: str, prompt: str, diff: str) -> WorkerOutcome:
        return WorkerOutcome(agent=agent, ok=True, findings=findings, duration_seconds=0.01)
    return runner


def _make_critical_runner() -> Any:
    """Runner that returns one critical finding."""
    return _make_runner([
        Finding(agent="code_reviewer", severity="critical", message="null deref")
    ])


def _make_info_runner() -> Any:
    """Runner that returns one info finding."""
    return _make_runner([
        Finding(agent="code_reviewer", severity="info", message="consider renaming")
    ])


def _make_empty_runner() -> Any:
    """Runner that returns zero findings."""
    return _make_runner([])


# ---- AC2-END-TO-END ----

class TestWorkerReviewEndToEnd:
    """AC2-END-TO-END: orchestrator path produces a gate-satisfying artifact."""

    def test_review_function_exists(self) -> None:
        from fno.worker.review import review  # noqa: F401
        assert callable(review)

    def test_review_returns_dict_with_action(self, tmp_path: Path) -> None:
        from fno.worker.review import review

        state_path = _make_state(tmp_path)
        artifacts_dir = tmp_path / "artifacts"

        result = review(
            diff_context="diff --git ...",
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id="sess-abc123",
            runner=_make_empty_runner(),
            git_sha_value="deadbeef0001",
        )

        assert isinstance(result, dict)
        assert result["action"] in ("reviewed", "cached")

    def test_artifact_satisfies_gate_schema(self, tmp_path: Path) -> None:
        from fno.worker.review import review

        session_id = "sess-gate-check"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        result = review(
            diff_context="some diff text",
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session_id,
            runner=_make_info_runner(),
            git_sha_value="deadbeef0002",
        )

        assert result["action"] in ("reviewed", "cached")

        # Load the written artifact directly
        artifact_path = artifacts_dir / f"review-{session_id}.md"
        assert artifact_path.exists(), f"Artifact not written: {artifact_path}"

        # Parse frontmatter
        text = artifact_path.read_text(encoding="utf-8")
        assert text.startswith("---")
        rest = text[3:].lstrip("\n")
        end = rest.find("\n---")
        fm = yaml.safe_load(rest[:end])

        assert fm["phase"] == "review"
        assert fm["session_id"] == session_id
        assert "verdict" in fm
        assert "findings_critical" in fm
        assert "findings_high" in fm


# ---- AC2-VERDICT-BLOCKED ----

class TestWorkerReviewVerdictBlocked:
    """AC2-VERDICT-BLOCKED: critical finding -> verdict: blocked."""

    def test_critical_finding_produces_blocked_verdict(self, tmp_path: Path) -> None:
        from fno.worker.review import review

        session_id = "sess-blocked"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        result = review(
            diff_context="diff ...",
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session_id,
            runner=_make_critical_runner(),
            git_sha_value="deadbeef0003",
        )

        assert result["verdict"] == "blocked"

        artifact_path = artifacts_dir / f"review-{session_id}.md"
        text = artifact_path.read_text(encoding="utf-8")
        rest = text[3:].lstrip("\n")
        fm = yaml.safe_load(rest[:rest.find("\n---")])
        assert fm["verdict"] == "blocked"
        assert fm["findings_critical"] >= 1



# ---- AC2-VERDICT-READY ----

class TestWorkerReviewVerdictReady:
    """AC2-VERDICT-READY: info-only findings -> verdict: ready-to-merge."""

    def test_info_only_produces_ready(self, tmp_path: Path) -> None:
        from fno.worker.review import review

        session_id = "sess-ready"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        result = review(
            diff_context="diff ...",
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session_id,
            runner=_make_info_runner(),
            git_sha_value="deadbeef0005",
        )

        assert result["verdict"] == "ready-to-merge"



# ---- AC2-CACHED ----

class TestWorkerReviewCached:
    """AC2-CACHED: second call on same session+sha returns action=cached."""

    def test_second_call_is_cached(self, tmp_path: Path) -> None:
        from fno.worker.review import review

        session_id = "sess-cache"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"
        sha = "deadbeef0007"

        call_count = 0

        async def counting_runner(agent: str, prompt: str, diff: str) -> WorkerOutcome:
            nonlocal call_count
            call_count += 1
            # Return a real finding so the result is NOT suspicious.
            # Suspicious results are not cached (by orchestrator policy, H6 fix),
            # so we need a non-suspicious result to test cache behaviour.
            return WorkerOutcome(
                agent=agent,
                ok=True,
                findings=[Finding(agent=agent, severity="info", message="all good")],
                duration_seconds=0.01,
            )

        # First call - runs workers
        result1 = review(
            diff_context="diff ...",
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session_id,
            runner=counting_runner,
            git_sha_value=sha,
        )
        first_call_count = call_count

        # Second call on same sha - should hit cache
        result2 = review(
            diff_context="diff ...",
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session_id,
            runner=counting_runner,
            git_sha_value=sha,
        )

        # Workers should NOT be spawned a second time
        assert call_count == first_call_count, (
            f"Workers spawned again on cache hit: call_count={call_count} after first={first_call_count}"
        )
        assert result2["cached"] is True

    def test_no_cache_flag_bypasses_cache(self, tmp_path: Path) -> None:
        from fno.worker.review import review

        session_id = "sess-no-cache"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"
        sha = "deadbeef0008"

        call_count = 0

        async def counting_runner(agent: str, prompt: str, diff: str) -> WorkerOutcome:
            nonlocal call_count
            call_count += 1
            return WorkerOutcome(agent=agent, ok=True, findings=[], duration_seconds=0.01)

        # First call
        review(
            diff_context="diff ...",
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session_id,
            runner=counting_runner,
            git_sha_value=sha,
        )
        first_call_count = call_count

        # Second call with no_cache=True
        review(
            diff_context="diff ...",
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session_id,
            runner=counting_runner,
            git_sha_value=sha,
            no_cache=True,
        )

        # Workers should be called again since cache bypassed
        assert call_count > first_call_count


# ---- C1: scorer resolver is consulted and its scorer is invoked ----

class TestC1ScorerResolver:
    """C1: score_findings must use default resolver (batched claude -p when
    present, pass-through otherwise). The worker must not bypass the resolver.
    """

    def test_resolver_is_called_and_scorer_runs(self, tmp_path: Path) -> None:
        """Whatever the default resolver returns, the scorer must actually run."""
        from fno.worker.review import review

        sentinel_calls: list[Finding] = []

        def sentinel_scorer(f: Finding) -> int:
            sentinel_calls.append(f)
            return 100

        session_id = "sess-c1-resolver"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        with patch(
            "fno.review.confidence_scorer._resolve_default_scorer",
            return_value=sentinel_scorer,
        ) as mock_resolve:
            review(
                diff_context="diff ...",
                state_path=state_path,
                artifacts_dir=artifacts_dir,
                session_id=session_id,
                runner=_make_info_runner(),
                git_sha_value="deadbeef-c1a",
                no_cache=True,
            )
            # _resolve_default_scorer must be called (not bypassed by hardcoded pass_through)
            mock_resolve.assert_called()

        # sentinel scorer must have been invoked on the findings
        assert len(sentinel_calls) > 0, "sentinel scorer was never called - scorer kwarg was hardcoded"

    def test_without_claude_binary_uses_pass_through(self, tmp_path: Path) -> None:
        """When `claude` is not on PATH, the pass-through path is selected."""
        from fno.worker.review import review

        session_id = "sess-c1-passthrough"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        with patch("shutil.which", return_value=None):
            import fno.review.confidence_scorer as cs_mod
            cs_mod._no_claude_warned = False
            result = review(
                diff_context="diff ...",
                state_path=state_path,
                artifacts_dir=artifacts_dir,
                session_id=session_id,
                runner=_make_info_runner(),
                git_sha_value="deadbeef-c1b",
                no_cache=True,
            )
        # Should complete without error, using pass-through
        assert result["action"] == "reviewed"

    def test_end_to_end_claude_subprocess_path(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        """Full path: review() -> score_findings() -> _resolve_default_scorer()
        -> claude_scorer_batch -> subprocess.run('claude', '-p').

        Integration sanity check that the wiring holds end-to-end (catches
        regressions like forgetting the __batch__ marker or renaming the
        resolver output).
        """
        import json as _json

        from fno.worker.review import review

        # Override the autouse fixture's shutil.which stub so `claude` appears
        # available; monkeypatch teardown restores the autouse stub after.
        monkeypatch.setattr("shutil.which", lambda name, *a, **k: "/usr/local/bin/claude" if name == "claude" else None)

        session_id = "sess-c1-e2e-subprocess"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        # `claude -p --output-format json` envelope wrapping a JSON array.
        # _make_info_runner yields a runner whose outputs contain info-level
        # findings; we want them all to survive the batch scorer so we feed
        # back a high score for each.
        def _subprocess_stub(cmd: list[str], **kwargs: object) -> subprocess.CompletedProcess:
            # Batch scorer sends a JSON payload as the last positional arg; we
            # count its entries to know how many scores to return.
            payload_json = cmd[-1]
            try:
                n = len(_json.loads(payload_json))
            except Exception:
                n = 1
            scores_str = "[" + ", ".join(["95"] * n) + "]"
            return subprocess.CompletedProcess(
                args=cmd,
                returncode=0,
                stdout=_json.dumps({"type": "result", "result": scores_str}),
                stderr="",
            )

        monkeypatch.setattr("subprocess.run", _subprocess_stub)

        result = review(
            diff_context="diff ...",
            state_path=state_path,
            artifacts_dir=artifacts_dir,
            session_id=session_id,
            runner=_make_info_runner(),
            git_sha_value="deadbeef-c1e2e",
            no_cache=True,
        )

        # If the wiring is right the review completes cleanly and the verdict
        # reflects info-only findings that survived the (stubbed) batch scorer.
        assert result["action"] == "reviewed"
        assert result["verdict"] == "ready-to-merge"


# ---- H2: all-findings-below-threshold produces done-with-concerns ----

class TestH2ThresholdDrop:
    """H2: when all raw findings drop below threshold, verdict must be done-with-concerns."""

    def test_all_below_threshold_yields_done_with_concerns(self, tmp_path: Path) -> None:
        """10 critical findings all scored 50 (below 80) -> done-with-concerns."""
        from fno.worker.review import review

        session_id = "sess-h2-threshold"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        # Runner returns 10 critical findings
        critical_findings = [
            Finding(agent="code_reviewer", severity="critical", message=f"crit-{i}")
            for i in range(10)
        ]

        async def high_findings_runner(agent: str, prompt: str, diff: str) -> WorkerOutcome:
            return WorkerOutcome(agent=agent, ok=True, findings=critical_findings, duration_seconds=0.01)

        # Scorer that always returns 50 (below threshold of 80) - all findings dropped
        low_scorer = MagicMock(return_value=50)

        with patch(
            "fno.review.confidence_scorer._resolve_default_scorer",
            return_value=low_scorer,
        ):
            result = review(
                diff_context="diff ...",
                state_path=state_path,
                artifacts_dir=artifacts_dir,
                session_id=session_id,
                runner=high_findings_runner,
                git_sha_value="deadbeef-h2",
                no_cache=True,
            )

        # Must NOT be ready-to-merge when raw findings existed but all were dropped
        assert result["verdict"] != "ready-to-merge", (
            "Expected done-with-concerns when all findings dropped below threshold, "
            f"got {result['verdict']!r}"
        )
        assert result["verdict"] == "done-with-concerns"


# ---- H6: worker layer does not call write_cache ----

class TestH6NoDuplicateCacheWrite:
    """H6: worker layer must not write cache (orchestrator owns writes)."""

    def test_worker_does_not_call_write_cache_on_normal_result(self, tmp_path: Path) -> None:
        """Normal result: orchestrator writes cache at most once; worker never writes."""
        from fno.worker.review import review
        from fno.review import cache as _cache

        session_id = "sess-h6-normal"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        write_calls: list = []

        original_write = _cache.write_cache

        def tracking_write(key, body, **kwargs):
            write_calls.append(key)
            return original_write(key, body, **kwargs)

        with patch("fno.review.cache.write_cache", side_effect=tracking_write):
            review(
                diff_context="diff ...",
                state_path=state_path,
                artifacts_dir=artifacts_dir,
                session_id=session_id,
                runner=_make_info_runner(),
                git_sha_value="deadbeef-h6a",
                no_cache=False,
            )

        # Orchestrator is allowed to write once for a clean result;
        # worker must not add a second write.
        assert len(write_calls) <= 1, (
            f"Expected at most 1 cache write (by orchestrator), got {len(write_calls)}"
        )

    def test_worker_does_not_call_write_cache_on_suspicious_result(self, tmp_path: Path) -> None:
        """Suspicious result (all-clean): orchestrator skips write; worker must also not write."""
        from fno.worker.review import review
        from fno.review import cache as _cache

        session_id = "sess-h6-suspicious"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        write_calls: list = []

        original_write = _cache.write_cache

        def tracking_write(key, body, **kwargs):
            write_calls.append(key)
            return original_write(key, body, **kwargs)

        with patch("fno.review.cache.write_cache", side_effect=tracking_write):
            review(
                diff_context="diff ...",
                state_path=state_path,
                artifacts_dir=artifacts_dir,
                session_id=session_id,
                runner=_make_empty_runner(),  # all-clean = suspicious
                git_sha_value="deadbeef-h6b",
                no_cache=False,
            )

        # Orchestrator skips write for suspicious; worker must also skip.
        assert len(write_calls) == 0, (
            f"Expected 0 cache writes for suspicious result, got {len(write_calls)}"
        )


# ---- C2: default runner construction + PID tracking ----

class TestC2DefaultRunnerConstruction:
    """C2: when runner=None, review() must construct a default runner that
    tracks PIDs and passes the SAME tracked_pids list to orchestrate_review_parallel.
    """

    def test_default_runner_is_constructed_when_runner_none(self, tmp_path: Path) -> None:
        """AC1: monkeypatch make_async_runner, call review(runner=None).
        Assert spy was called exactly once with worker_pids as a list.
        """
        from fno.worker import review as review_mod

        spy_calls: list[dict] = []

        def fake_make_async_runner(*, worker_pids, timeout=None, adapter=None):
            spy_calls.append({"worker_pids": worker_pids})
            # Return an async runner that returns quickly
            async def _runner(agent: str, prompt: str, diff: str):
                from fno.review.orchestrator import WorkerOutcome, Finding
                return WorkerOutcome(
                    agent=agent,
                    ok=True,
                    findings=[Finding(agent=agent, severity="info", message="ok")],
                    duration_seconds=0.01,
                )
            return _runner

        session_id = "sess-c2-default"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        with patch(
            "fno.worker.review.make_async_runner",
            side_effect=fake_make_async_runner,
        ):
            review_mod.review(
                diff_context="diff ...",
                state_path=state_path,
                artifacts_dir=artifacts_dir,
                session_id=session_id,
                runner=None,
                git_sha_value="deadbeef-c2a",
                no_cache=True,
            )

        assert len(spy_calls) == 1, (
            f"make_async_runner should be called exactly once, got {len(spy_calls)}"
        )
        call0 = spy_calls[0]
        # worker_pids must be a list (not None)
        assert isinstance(call0["worker_pids"], list), (
            f"worker_pids should be a list, got {type(call0['worker_pids'])!r}"
        )

    def test_pid_list_is_same_object_passed_to_orchestrator(self, tmp_path: Path) -> None:
        """AC2: the worker_pids list passed to make_async_runner and the one
        passed to orchestrate_review_parallel must be the SAME object (same identity).
        """
        from fno.worker import review as review_mod

        captured: dict = {}

        def fake_make_async_runner(*, worker_pids, timeout=None, adapter=None):
            captured["factory_pids"] = worker_pids

            async def _runner(agent: str, prompt: str, diff: str):
                from fno.review.orchestrator import WorkerOutcome, Finding
                return WorkerOutcome(
                    agent=agent,
                    ok=True,
                    findings=[Finding(agent=agent, severity="info", message="ok")],
                    duration_seconds=0.01,
                )
            return _runner

        original_orchestrate = None

        def fake_orchestrate(diff_context, *, runner, worker_pids=None, **kwargs):
            captured["orchestrate_pids"] = worker_pids
            # Call original to keep side-effects (artifact writing etc.)
            from fno.review.orchestrator import orchestrate_review_parallel as real_fn
            return real_fn(diff_context, runner=runner, worker_pids=worker_pids, **kwargs)

        session_id = "sess-c2-identity"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        with patch("fno.worker.review.make_async_runner", side_effect=fake_make_async_runner), \
             patch("fno.worker.review.orchestrate_review_parallel", side_effect=fake_orchestrate):
            review_mod.review(
                diff_context="diff ...",
                state_path=state_path,
                artifacts_dir=artifacts_dir,
                session_id=session_id,
                runner=None,
                git_sha_value="deadbeef-c2b",
                no_cache=True,
            )

        assert "factory_pids" in captured and "orchestrate_pids" in captured, (
            "spy did not capture both pids references"
        )
        assert captured["factory_pids"] is captured["orchestrate_pids"], (
            "factory worker_pids and orchestrate worker_pids must be the SAME list object"
        )

    def test_explicit_runner_bypasses_default_construction(self, tmp_path: Path) -> None:
        """AC3: when runner is explicitly provided, make_async_runner must NOT be called."""
        from fno.worker import review as review_mod

        spy_calls: list = []

        def fake_make_async_runner(*, worker_pids, timeout=None, adapter=None):
            spy_calls.append(True)
            async def _runner(agent, prompt, diff):
                from fno.review.orchestrator import WorkerOutcome
                return WorkerOutcome(agent=agent, ok=True, duration_seconds=0.01)
            return _runner

        session_id = "sess-c2-explicit"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        with patch("fno.worker.review.make_async_runner", side_effect=fake_make_async_runner):
            review_mod.review(
                diff_context="diff ...",
                state_path=state_path,
                artifacts_dir=artifacts_dir,
                session_id=session_id,
                runner=_make_info_runner(),  # explicit runner provided
                git_sha_value="deadbeef-c2c",
                no_cache=True,
            )

        assert len(spy_calls) == 0, (
            f"make_async_runner should NOT be called when runner is explicitly provided, got {len(spy_calls)} calls"
        )


# ---- C3: make_async_runner call-site correctness (no TypeError) ----

class TestC3MakeAsyncRunnerCallSite:
    """C3: review() must call make_async_runner with keyword-only args.

    The real make_async_runner(*, timeout, adapter, worker_pids) takes NO
    positional parameters. Passing run_via_claude_code as a positional used
    to raise TypeError on every cache-miss invocation (abfc6ae regression).
    """

    def test_review_runner_none_does_not_raise_type_error(self, tmp_path: Path) -> None:
        """AC1-HP: review(runner=None) constructs the default runner without TypeError.

        Patch orchestrate_review_parallel so no real subprocess is spawned,
        but do NOT mock make_async_runner - we want the real call to exercise.
        """
        from fno.worker import review as review_mod
        from fno.review.orchestrator import OrchestratorResult

        session_id = "sess-c3-no-typeerror"
        state_path = _make_state(tmp_path, session_id=session_id)
        artifacts_dir = tmp_path / "artifacts"

        # Fake orchestrate that returns immediately without spawning workers
        def fake_orchestrate(diff_context, *, runner, worker_pids=None, **kwargs):
            from fno.review.orchestrator import orchestrate_review_parallel as real_fn
            # Return a minimal result without actually calling anything
            return OrchestratorResult(
                findings=[],
                workers_completed=0,
                workers_failed=0,
                suspicious=False,
                duration_seconds=0.01,
            )

        with patch(
            "fno.worker.review.orchestrate_review_parallel",
            side_effect=fake_orchestrate,
        ):
            # This must NOT raise TypeError
            result = review_mod.review(
                diff_context="diff ...",
                state_path=state_path,
                artifacts_dir=artifacts_dir,
                session_id=session_id,
                runner=None,
                git_sha_value="deadbeef-c3",
                no_cache=True,
            )

        assert result["action"] == "reviewed", (
            f"Expected action=reviewed, got {result['action']!r}"
        )

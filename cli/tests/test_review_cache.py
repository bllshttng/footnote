"""Tests for fno.review.cache - git_sha-keyed result cache.

TDD sequence:
1. Unit tests (AC1-*) - run while module doesn't exist to confirm RED.
2. Integration tests (AC2-*) - confirm cache hit/miss/no-cache flag.
"""

from __future__ import annotations

import dataclasses
import json
import os
from pathlib import Path
from unittest.mock import patch

import pytest

from fno.review.orchestrator import (
    AGENT_NAMES,
    Finding,
    OrchestratorResult,
    WorkerOutcome,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_prompts() -> dict[str, str]:
    return {name: f"Prompt for {name}" for name in AGENT_NAMES}


def _make_finding(agent: str = "code_reviewer", severity: str = "high") -> Finding:
    return Finding(
        agent=agent,
        severity=severity,
        message="test finding",
        file="src/foo.py",
        line=42,
        confidence=85,
        raw="raw output text",
    )


def _make_result(findings: list[Finding] | None = None) -> OrchestratorResult:
    f = findings if findings is not None else [_make_finding()]
    return OrchestratorResult(
        findings=f,
        workers_completed=6,
        workers_failed=0,
        suspicious=False,
        duration_seconds=1.23,
    )


# ---------------------------------------------------------------------------
# Unit tests (AC1-*)
# ---------------------------------------------------------------------------

class TestCacheKey:
    """AC1-DETERMINISTIC: cache_key is stable for same inputs, different for any change."""

    def test_same_inputs_produce_same_key(self) -> None:
        from fno.review.cache import cache_key

        k1 = cache_key("session-abc", "deadbeef", "prompthash123")
        k2 = cache_key("session-abc", "deadbeef", "prompthash123")
        assert k1 == k2

    def test_different_session_id_produces_different_key(self) -> None:
        from fno.review.cache import cache_key

        k1 = cache_key("session-abc", "deadbeef", "prompthash123")
        k2 = cache_key("session-xyz", "deadbeef", "prompthash123")
        assert k1 != k2

    def test_different_git_sha_produces_different_key(self) -> None:
        from fno.review.cache import cache_key

        k1 = cache_key("session-abc", "deadbeef", "prompthash123")
        k2 = cache_key("session-abc", "cafebabe", "prompthash123")
        assert k1 != k2

    def test_different_prompt_hash_produces_different_key(self) -> None:
        from fno.review.cache import cache_key

        k1 = cache_key("session-abc", "deadbeef", "prompthash123")
        k2 = cache_key("session-abc", "deadbeef", "prompthash456")
        assert k1 != k2

    def test_key_is_lowercase_hex_string(self) -> None:
        from fno.review.cache import cache_key

        k = cache_key("session-abc", "deadbeef", "prompthash123")
        assert k == k.lower()
        assert all(c in "0123456789abcdef" for c in k)
        assert len(k) == 64  # SHA-256 = 32 bytes = 64 hex chars

    def test_single_byte_change_produces_different_key(self) -> None:
        from fno.review.cache import cache_key

        k1 = cache_key("session-abc", "deadbeef", "prompthash123")
        k2 = cache_key("session-abC", "deadbeef", "prompthash123")  # capital C
        assert k1 != k2

    # --- provider-set dimension (cross-model panel, ab-6c8f4c61) ---

    def test_empty_provider_set_reproduces_legacy_key(self) -> None:
        """None / empty provider_set must hash identically to the 3-arg key."""
        from fno.review.cache import cache_key

        legacy = cache_key("session-abc", "deadbeef", "prompthash123")
        assert cache_key("session-abc", "deadbeef", "prompthash123", None) == legacy
        assert cache_key("session-abc", "deadbeef", "prompthash123", []) == legacy
        assert cache_key("session-abc", "deadbeef", "prompthash123", set()) == legacy

    def test_provider_set_changes_key(self) -> None:
        """A non-empty provider set must NOT collide with the all-claude key."""
        from fno.review.cache import cache_key

        legacy = cache_key("session-abc", "deadbeef", "prompthash123")
        cross = cache_key(
            "session-abc", "deadbeef", "prompthash123", ["claude", "codex"]
        )
        assert cross != legacy

    def test_provider_set_is_order_independent(self) -> None:
        from fno.review.cache import cache_key

        a = cache_key("s", "sha", "ph", ["codex", "claude"])
        b = cache_key("s", "sha", "ph", ["claude", "codex"])
        assert a == b

    def test_build_cache_body_omits_provider_set_when_absent(self) -> None:
        """Legacy body is byte-identical: no provider_set line when unset."""
        from fno.review.cache import build_cache_body

        body = build_cache_body("k", "sess", "sha", _make_result())
        assert "provider_set:" not in body

    def test_build_cache_body_includes_sorted_provider_set(self) -> None:
        from fno.review.cache import build_cache_body

        body = build_cache_body(
            "k", "sess", "sha", _make_result(), provider_set=["codex", "claude"]
        )
        assert "provider_set: claude,codex" in body


class TestPromptHash:
    """prompt_hash must be stable and order-independent."""

    def test_same_prompts_produce_same_hash(self) -> None:
        from fno.review.cache import prompt_hash

        prompts = {"agent_a": "text a", "agent_b": "text b"}
        h1 = prompt_hash(prompts)
        h2 = prompt_hash(prompts)
        assert h1 == h2

    def test_dict_order_does_not_affect_hash(self) -> None:
        """Dicts with same content in different insertion order must hash the same."""
        from fno.review.cache import prompt_hash

        p1 = {"agent_a": "text a", "agent_b": "text b"}
        p2 = {"agent_b": "text b", "agent_a": "text a"}
        assert prompt_hash(p1) == prompt_hash(p2)

    def test_different_text_produces_different_hash(self) -> None:
        from fno.review.cache import prompt_hash

        h1 = prompt_hash({"agent_a": "text a"})
        h2 = prompt_hash({"agent_a": "text b"})
        assert h1 != h2

    def test_hash_is_lowercase_hex_64_chars(self) -> None:
        from fno.review.cache import prompt_hash

        h = prompt_hash({"agent_a": "text a"})
        assert len(h) == 64
        assert h == h.lower()
        assert all(c in "0123456789abcdef" for c in h)


class TestGitSha:
    """git_sha() helper returns HEAD sha, 'unknown', or 'empty-tree'."""

    def test_returns_string(self) -> None:
        from fno.review.cache import git_sha

        result = git_sha()
        assert isinstance(result, str)
        assert len(result) > 0

    def test_returns_unknown_outside_git_repo(self, tmp_path: Path) -> None:
        from fno.review.cache import git_sha

        result = git_sha(repo_path=tmp_path)
        assert result == "unknown"

    def test_returns_empty_tree_for_repo_with_no_commits(self, tmp_path: Path) -> None:
        """A fresh 'git init' with no commits yet should return 'empty-tree'."""
        import subprocess

        subprocess.run(["git", "init", str(tmp_path)], check=True, capture_output=True)
        from fno.review.cache import git_sha

        result = git_sha(repo_path=tmp_path)
        assert result == "empty-tree"

    def test_returns_sha_for_repo_with_commits(self, tmp_path: Path) -> None:
        """A repo with at least one commit returns a 40-char hex sha."""
        import subprocess

        subprocess.run(["git", "init", str(tmp_path)], check=True, capture_output=True)
        (tmp_path / "file.txt").write_text("hello")
        subprocess.run(
            ["git", "-C", str(tmp_path), "add", "."],
            check=True, capture_output=True,
        )
        subprocess.run(
            ["git", "-C", str(tmp_path), "commit", "--no-gpg-sign", "-m", "init",
             "--author", "Test <test@test.com>"],
            check=True, capture_output=True,
            env={**os.environ, "GIT_AUTHOR_NAME": "Test", "GIT_AUTHOR_EMAIL": "t@t.com",
                 "GIT_COMMITTER_NAME": "Test", "GIT_COMMITTER_EMAIL": "t@t.com"},
        )
        from fno.review.cache import git_sha

        result = git_sha(repo_path=tmp_path)
        assert len(result) == 40
        assert all(c in "0123456789abcdef" for c in result)


class TestCachePath:
    """cache_path returns the expected path under artifacts_dir/review-cache/."""

    def test_path_structure(self, tmp_path: Path) -> None:
        from fno.review.cache import cache_path

        key = "abc123def456"
        p = cache_path(key, artifacts_dir=tmp_path)
        assert p == tmp_path / "review-cache" / f"{key}.md"

    def test_different_keys_produce_different_paths(self, tmp_path: Path) -> None:
        from fno.review.cache import cache_path

        p1 = cache_path("key1", artifacts_dir=tmp_path)
        p2 = cache_path("key2", artifacts_dir=tmp_path)
        assert p1 != p2


class TestReadWriteCache:
    """Round-trip: write then read returns the original body."""

    def test_read_returns_none_on_miss(self, tmp_path: Path) -> None:
        from fno.review.cache import read_cache

        result = read_cache("nonexistent-key", artifacts_dir=tmp_path)
        assert result is None

    def test_write_then_read_returns_body(self, tmp_path: Path) -> None:
        from fno.review.cache import read_cache, write_cache

        key = "test-key-abc123"
        body = "---\nphase: review\n---\n[]\n"
        write_cache(key, body, artifacts_dir=tmp_path)
        result = read_cache(key, artifacts_dir=tmp_path)
        assert result == body

    def test_write_creates_directory(self, tmp_path: Path) -> None:
        from fno.review.cache import write_cache

        artifacts = tmp_path / "nested" / "artifacts"
        write_cache("some-key", "body text", artifacts_dir=artifacts)
        assert (artifacts / "review-cache" / "some-key.md").exists()

    def test_read_swallows_errors_on_corrupt_file(
        self, tmp_path: Path, capsys
    ) -> None:
        """read_cache swallows errors and returns None instead of crashing."""
        from fno.review.cache import cache_path, read_cache

        key = "corrupt-key"
        p = cache_path(key, artifacts_dir=tmp_path)
        p.parent.mkdir(parents=True, exist_ok=True)
        # Write something that simulates a corrupt/unreadable state by making
        # the path a directory instead of a file.
        p.mkdir()

        result = read_cache(key, artifacts_dir=tmp_path)
        assert result is None


class TestAtomicWrite:
    """AC1-ATOMIC: write_cache is atomic - no partial file if process killed mid-write."""

    def test_uses_atomic_replace_not_direct_write(self, tmp_path: Path) -> None:
        """write_cache must not write directly to the target path.

        We verify atomicity by checking that the final file exists and is complete
        after a successful write (the tmp file must be cleaned up).
        """
        from fno.review.cache import cache_path, write_cache

        key = "atomic-test-key"
        body = "---\nphase: review\n---\nsome body\n"
        write_cache(key, body, artifacts_dir=tmp_path)

        target = cache_path(key, artifacts_dir=tmp_path)
        assert target.exists()
        assert target.read_text(encoding="utf-8") == body

        # No .tmp files left behind.
        cache_dir = tmp_path / "review-cache"
        tmp_files = list(cache_dir.glob("*.tmp*"))
        assert tmp_files == [], f"tmp files left behind: {tmp_files}"


class TestFindingsRoundTrip:
    """Findings serialize to JSON and deserialize back to equal Finding instances."""

    def test_finding_round_trip_via_cache_body(self, tmp_path: Path) -> None:
        from fno.review.cache import (
            build_cache_body,
            read_cache,
            reconstruct_result,
            write_cache,
        )

        findings = [
            Finding(agent="code_reviewer", severity="critical", message="null deref",
                    file="src/main.py", line=10, confidence=90, raw="full text"),
            Finding(agent="silent_failure_hunter", severity="info",
                    message="minor thing", file=None, line=None, confidence=None, raw=""),
        ]
        result = OrchestratorResult(
            findings=findings,
            workers_completed=6,
            workers_failed=0,
            suspicious=False,
            duration_seconds=2.5,
        )
        key = "roundtrip-key"
        session_id = "sess-001"
        git_sha_val = "cafebabe12345678"

        body = build_cache_body(key, session_id, git_sha_val, result)
        write_cache(key, body, artifacts_dir=tmp_path)
        raw = read_cache(key, artifacts_dir=tmp_path)
        assert raw is not None

        reconstructed = reconstruct_result(raw)
        assert len(reconstructed.findings) == len(findings)
        for orig, restored in zip(findings, reconstructed.findings):
            assert orig == restored

        assert reconstructed.workers_completed == result.workers_completed
        assert reconstructed.workers_failed == result.workers_failed


# ---------------------------------------------------------------------------
# Integration tests (AC2-*)
# ---------------------------------------------------------------------------

class TestCacheIntegration:
    """Integration tests: orchestrate_review_parallel with cache enabled/disabled."""

    def _make_async_runner(self, call_log: list[str]):
        """Returns an async runner that records which agents are called."""
        import asyncio

        async def runner(agent: str, prompt: str, diff: str) -> WorkerOutcome:
            call_log.append(agent)
            return WorkerOutcome(
                agent=agent,
                ok=True,
                findings=[Finding(agent=agent, severity="info", message="ok")],
            )

        return runner

    def test_ac2_cache_hit_skips_workers_on_second_call(
        self, tmp_path: Path
    ) -> None:
        """AC2-CACHE-HIT: second call with same git_sha returns cached result without spawning workers."""
        from fno.review.orchestrator import orchestrate_review_parallel

        prompts = _make_prompts()
        session_id = "cache-hit-test"
        git_sha_val = "abc123def456abc1"

        call_log: list[str] = []
        runner = self._make_async_runner(call_log)

        # First call - workers are dispatched.
        result1 = orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=runner,
            session_id=session_id,
            artifacts_dir=tmp_path,
            cache_enabled=True,
            git_sha_value=git_sha_val,
        )
        assert result1.workers_completed == 6
        first_call_count = len(call_log)
        assert first_call_count == 6, f"Expected 6 worker calls, got {first_call_count}"

        # Second call with same git_sha - must NOT call workers.
        call_log.clear()
        result2 = orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=runner,
            session_id=session_id,
            artifacts_dir=tmp_path,
            cache_enabled=True,
            git_sha_value=git_sha_val,
        )
        assert len(call_log) == 0, (
            f"Expected 0 worker calls on cache hit, got {len(call_log)}: {call_log}"
        )
        assert len(result2.findings) == len(result1.findings)

    def test_ac2_cache_invalidates_on_different_git_sha(
        self, tmp_path: Path
    ) -> None:
        """AC2-CACHE-INVALIDATES-ON-COMMIT: different git_sha -> cache miss, workers dispatched."""
        from fno.review.orchestrator import orchestrate_review_parallel

        prompts = _make_prompts()
        session_id = "cache-invalidate-test"

        call_log: list[str] = []
        runner = self._make_async_runner(call_log)

        # First call.
        orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=runner,
            session_id=session_id,
            artifacts_dir=tmp_path,
            cache_enabled=True,
            git_sha_value="sha-version-1",
        )
        assert len(call_log) == 6

        # Second call with different git_sha - must spawn workers again.
        call_log.clear()
        orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=runner,
            session_id=session_id,
            artifacts_dir=tmp_path,
            cache_enabled=True,
            git_sha_value="sha-version-2",
        )
        assert len(call_log) == 6, (
            f"Expected 6 worker calls on cache miss (new sha), got {len(call_log)}"
        )

    def test_ac2_no_cache_flag_always_dispatches_workers(
        self, tmp_path: Path
    ) -> None:
        """AC2-NO-CACHE-FLAG: cache_enabled=False bypasses cache on both read and write."""
        from fno.review.orchestrator import orchestrate_review_parallel

        prompts = _make_prompts()
        session_id = "no-cache-flag-test"
        git_sha_val = "fixed-sha-value"

        call_log: list[str] = []
        runner = self._make_async_runner(call_log)

        # First call with cache disabled.
        orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=runner,
            session_id=session_id,
            artifacts_dir=tmp_path,
            cache_enabled=False,
            git_sha_value=git_sha_val,
        )
        assert len(call_log) == 6

        # Second call with same sha but cache still disabled.
        call_log.clear()
        orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=runner,
            session_id=session_id,
            artifacts_dir=tmp_path,
            cache_enabled=False,
            git_sha_value=git_sha_val,
        )
        assert len(call_log) == 6, (
            f"Expected 6 worker calls when cache_enabled=False, got {len(call_log)}"
        )

    def test_ac2_cache_not_written_when_workers_failed(
        self, tmp_path: Path
    ) -> None:
        """Bad runs (workers_failed > 0) must NOT be written to cache."""
        import asyncio
        from fno.review.cache import cache_key, cache_path, prompt_hash, read_cache
        from fno.review.orchestrator import orchestrate_review_parallel

        prompts = _make_prompts()
        session_id = "bad-run-test"
        git_sha_val = "bad-run-sha"

        async def failing_runner(agent: str, prompt: str, diff: str) -> WorkerOutcome:
            if agent == "code_reviewer":
                raise RuntimeError("simulated crash")
            return WorkerOutcome(
                agent=agent,
                ok=True,
                findings=[Finding(agent=agent, severity="info", message="ok")],
            )

        result = orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=failing_runner,
            session_id=session_id,
            artifacts_dir=tmp_path,
            cache_enabled=True,
            git_sha_value=git_sha_val,
        )
        assert result.workers_failed > 0

        # Cache must NOT have been written.
        ph = prompt_hash(prompts)
        ck = cache_key(session_id, git_sha_val, ph)
        cached = read_cache(ck, artifacts_dir=tmp_path)
        assert cached is None, "Should not cache a bad run"

    def test_ac2_cache_not_written_when_suspicious(
        self, tmp_path: Path
    ) -> None:
        """Suspicious runs (all-clean panel) must NOT be written to cache."""
        import asyncio
        from fno.review.cache import cache_key, cache_path, prompt_hash, read_cache
        from fno.review.orchestrator import orchestrate_review_parallel

        prompts = _make_prompts()
        session_id = "suspicious-run-test"
        git_sha_val = "suspicious-sha"

        async def clean_runner(agent: str, prompt: str, diff: str) -> WorkerOutcome:
            return WorkerOutcome(agent=agent, ok=True, findings=[])

        result = orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=clean_runner,
            session_id=session_id,
            artifacts_dir=tmp_path,
            cache_enabled=True,
            git_sha_value=git_sha_val,
        )
        assert result.suspicious is True

        # Cache must NOT have been written.
        ph = prompt_hash(prompts)
        ck = cache_key(session_id, git_sha_val, ph)
        cached = read_cache(ck, artifacts_dir=tmp_path)
        assert cached is None, "Should not cache a suspicious run"

    def test_ac2_cache_disabled_when_session_id_missing(
        self, tmp_path: Path
    ) -> None:
        """When session_id is None, cache is implicitly disabled."""
        from fno.review.orchestrator import orchestrate_review_parallel

        call_log: list[str] = []
        runner = self._make_async_runner(call_log)
        prompts = _make_prompts()

        # First call - no session_id, so no cache.
        orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=runner,
            session_id=None,
            artifacts_dir=tmp_path,
            cache_enabled=True,
            git_sha_value="some-sha",
        )
        assert len(call_log) == 6

        # Second call with same params - still no cache due to no session_id.
        call_log.clear()
        orchestrate_review_parallel(
            "DIFF",
            prompts=prompts,
            runner=runner,
            session_id=None,
            artifacts_dir=tmp_path,
            cache_enabled=True,
            git_sha_value="some-sha",
        )
        assert len(call_log) == 6, "Should not cache without session_id"

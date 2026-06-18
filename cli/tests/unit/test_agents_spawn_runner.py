"""Tests for review.runners.agents_spawn_runner (codex/gemini dispatch)."""
from __future__ import annotations

import asyncio
import time
from pathlib import Path

import pytest

from fno.agents.dispatch import DispatchAskError, SpawnResult
from fno.review.runners.agents_spawn_runner import (
    DISPATCH_FAILURE_PREFIX,
    PARSE_FAILURE_PREFIX,
    is_retryable_failure,
    make_async_runner,
    run_via_agents_spawn,
)


def _ok_dispatch(reply: str):
    def _dispatch(**kwargs):
        return SpawnResult(
            kind="once",
            name=kwargs["name"],
            provider=kwargs["provider"],
            short_id="sess-1",
            reply=reply,
        )

    return _dispatch


def test_happy_path_parses_findings() -> None:
    outcome = run_via_agents_spawn(
        "code_reviewer",
        "prompt",
        "diff",
        provider="codex",
        cwd=Path("."),
        dispatch=_ok_dispatch('[{"severity": "high", "message": "boom"}]'),
    )
    assert outcome.ok is True
    assert outcome.provider == "codex"
    assert len(outcome.findings) == 1
    assert outcome.findings[0].severity == "high"


def test_empty_findings_is_ok() -> None:
    outcome = run_via_agents_spawn(
        "code_reviewer", "p", "d", provider="gemini", cwd=Path("."),
        dispatch=_ok_dispatch("[]"),
    )
    assert outcome.ok is True
    assert outcome.findings == []
    assert outcome.provider == "gemini"


def test_prose_reply_is_terminal_soft_fail() -> None:
    outcome = run_via_agents_spawn(
        "code_reviewer", "p", "d", provider="codex", cwd=Path("."),
        dispatch=_ok_dispatch("looks fine to me!"),
    )
    assert outcome.ok is False
    assert outcome.error.startswith(PARSE_FAILURE_PREFIX)
    assert is_retryable_failure(outcome) is False  # AC3-ERR: not retried


def test_dispatch_error_is_retryable_soft_fail() -> None:
    def _boom(**kwargs):
        raise DispatchAskError("provider locked out", exit_code=13)

    outcome = run_via_agents_spawn(
        "code_reviewer", "p", "d", provider="codex", cwd=Path("."),
        dispatch=_boom,
    )
    assert outcome.ok is False
    assert outcome.error.startswith(DISPATCH_FAILURE_PREFIX)
    assert "exit=13" in outcome.error
    assert is_retryable_failure(outcome) is True  # AC5-FR: fall through to claude
    assert outcome.provider == "codex"


def test_no_reply_text_is_soft_fail() -> None:
    class _NoReply:
        reply = None

    outcome = run_via_agents_spawn(
        "code_reviewer", "p", "d", provider="codex", cwd=Path("."),
        dispatch=lambda **kw: _NoReply(),
    )
    assert outcome.ok is False
    assert outcome.error.startswith(DISPATCH_FAILURE_PREFIX)


def test_timeout_is_retryable_soft_fail() -> None:
    def _slow(**kwargs):
        time.sleep(0.6)
        return SpawnResult(kind="once", name="n", provider="codex",
                           short_id="s", reply="[]")

    outcome = run_via_agents_spawn(
        "code_reviewer", "p", "d", provider="codex", cwd=Path("."),
        timeout=0.15, dispatch=_slow,
    )
    assert outcome.ok is False
    assert outcome.error == "timeout"
    assert is_retryable_failure(outcome) is True


def test_async_runner_wraps_sync() -> None:
    runner = make_async_runner(
        provider="codex", cwd=Path("."),
        dispatch=_ok_dispatch('[{"severity": "low", "message": "x"}]'),
    )
    outcome = asyncio.run(runner("code_reviewer", "p", "d"))
    assert outcome.ok is True
    assert outcome.provider == "codex"


def test_dispatch_invoked_with_once_true() -> None:
    captured: dict = {}

    def _capture(**kwargs):
        captured.update(kwargs)
        return SpawnResult(kind="once", name=kwargs["name"],
                          provider=kwargs["provider"], short_id="s", reply="[]")

    run_via_agents_spawn(
        "code_reviewer", "prompt-body", "diff-body",
        provider="codex", cwd=Path("/tmp/x"), dispatch=_capture,
    )
    assert captured["once"] is True
    assert captured["provider"] == "codex"
    assert "DIFF CONTEXT:" in captured["message"]
    assert "diff-body" in captured["message"]
    assert captured["name"].startswith("review-code_reviewer-codex-")


def test_dispatch_timeout_floored_to_one() -> None:
    """int(0.x) == 0 would mean 'no timeout'; floor at 1s (gemini review)."""
    captured: dict = {}

    def _capture(**kwargs):
        captured.update(kwargs)
        return SpawnResult(kind="once", name=kwargs["name"],
                          provider=kwargs["provider"], short_id="s", reply="[]")

    run_via_agents_spawn(
        "code_reviewer", "p", "d", provider="codex", cwd=Path("."),
        timeout=0.15, dispatch=_capture,
    )
    assert captured["timeout"] == 1

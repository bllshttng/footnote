"""Credential-free contract tests for Grok's ACP stdio lane."""
from __future__ import annotations

import os
import json
import shutil
import subprocess
import uuid
from pathlib import Path

import pytest


GROK = shutil.which("grok")
LIVE = os.environ.get("FNO_GROK_LIVE") == "1"


def _driver():
    try:
        from fno.agents.harnesses import grok
    except ModuleNotFoundError as exc:
        pytest.fail(f"Grok ACP driver is missing: {exc}")
    return grok


@pytest.mark.skipif(GROK is None, reason="grok binary is not on PATH")
def test_AC4_HP_real_initialize_and_session_list_are_positive_markers(tmp_path):
    driver = _driver()
    session_id = str(uuid.uuid4())
    session = driver.GrokStdioSession(
        cwd=tmp_path,
        session_id=session_id,
        argv=driver.grok_stdio_argv(session_id),
    )
    with session:
        initialized = session.initialize()
        assert initialized["protocolVersion"] == 1
        assert session.proc is not None and session.proc.poll() is None

        listed = session.session_list()
        assert listed["sessions"] == []

        with pytest.raises(driver.GrokAuthenticationRequired) as refused:
            session.session_new()
        assert "grok login --device-code" in str(refused.value)
        assert "Authentication required" in str(refused.value)


@pytest.mark.skipif(GROK is None, reason="grok binary is not on PATH")
def test_AC3_HP_argv_holds_caller_assigned_id_and_effort():
    driver = _driver()
    session_id = str(uuid.uuid4())
    argv = driver.grok_stdio_argv(session_id, model="grok-4.6", effort="xhigh")
    assert argv == [
        "grok",
        "--session-id",
        session_id,
        "--model",
        "grok-4.6",
        "--reasoning-effort",
        "xhigh",
        "agent",
        "stdio",
    ]


def test_AC3_EDGE_jsonl_keeps_notifications_and_correlates_response_ids():
    driver = _driver()
    frames = list(
        driver.iter_jsonl(
            iter(
                [
                    b'{"jsonrpc":"2.0","method":"session/update","params":{}}\n',
                    b'{"jsonrpc":"2.0","id":7,"result":{"ok":true}}\n',
                ]
            )
        )
    )
    assert frames[0]["method"] == "session/update"
    assert frames[1]["id"] == 7


def test_AC3_EXIT_zero_with_not_authenticated_output_is_refused():
    driver = _driver()
    completed = subprocess.CompletedProcess(
        ["grok", "models"],
        0,
        stdout="You are not authenticated.\n",
        stderr="",
    )
    with pytest.raises(driver.GrokAuthenticationRequired):
        driver.require_authenticated(completed)


def test_request_payloads_are_standard_acp_messages():
    driver = _driver()
    assert driver.initialize_params()["protocolVersion"] == 1
    assert driver.session_list_params("/tmp/repo") == {"cwd": "/tmp/repo"}
    assert driver.session_new_params("/tmp/repo") == {
        "cwd": "/tmp/repo",
        "mcpServers": [],
    }


def test_AC4_LIVE_gate_is_explicitly_preserved_by_hermetic_runner():
    from fno.hermetic import _RUNNER_PASSTHROUGH

    assert "FNO_GROK_LIVE" in _RUNNER_PASSTHROUGH


def test_AC7_EVIDENCE_grok_paste_fixture_records_measured_submit_trials():
    fixture = Path(__file__).parent / "fixtures" / "grok-paste-trials.txt"
    assert fixture.is_file(), "the measured Grok pane fixture is missing"
    text = fixture.read_text(encoding="utf-8")
    assert "payload_bytes=6330" in text
    assert "payload_lines=42" in text
    assert "paste_to_enter_delay_ms=200" in text
    assert "trial 3 SUBMITTED" in text


@pytest.mark.skipif(
    not (LIVE and GROK),
    reason="live Grok ACP journey requires FNO_GROK_LIVE=1 and grok on PATH",
)
def test_AC9_HP_live_turn_answers_the_planted_token(tmp_path, monkeypatch):
    driver = _driver()
    user = os.environ.get("USER", "")
    real_home = next(
        (
            candidate
            for candidate in (os.path.join("/Users", user), os.path.join("/home", user), "/root")
            if os.path.isdir(candidate)
        ),
        None,
    )
    assert real_home is not None, "the live Grok journey could not locate the real HOME"
    monkeypatch.setenv("HOME", real_home)
    monkeypatch.setenv("USERPROFILE", real_home)

    session_id = str(uuid.uuid4())
    with driver.GrokStdioSession(session_id, tmp_path) as session:
        assert session.initialize()["protocolVersion"] == 1
        created = session.session_new()
        assert created
        # This assertion used to read `session.session_id == created`, which is
        # a tautology: session_new() assigns self.session_id then returns that
        # same value, so it could not fail for ANY grok behavior. It read as
        # proof of a caller-assigned id guarantee that does not exist.
        #
        # MEASURED against live grok 1.0.13: session/new mints its own id and
        # ignores the --session-id argv; session/list never shows the assigned
        # value. Pin the contract fno actually has, so a grok that starts
        # honoring the argv fails HERE and loudly, instead of silently changing
        # fno's identity model underneath the roster.
        assert created != session_id, (
            "grok session/new echoed fno's assigned id; this driver is built on "
            "a callee-minted contract and that contract has changed"
        )
        try:
            session.prompt(
                "Reply with exactly the planted token GROK_ACP_TOKEN_829. Do not call tools."
            )
        except RuntimeError as exc:  # noqa: PERF203 - one call, not a loop
            # Narrow on purpose: only an exhausted subscription skips. A plain
            # transient 429 still FAILS, because a driver that spams the server
            # into rate limiting is a defect this test exists to catch.
            if "usage-exhausted" not in str(exc):
                raise
            pytest.skip(f"grok subscription quota is spent, not a driver defect: {exc}")
        assert "GROK_ACP_TOKEN_829" in json.dumps(session.notifications)


def test_review_P1_any_auth_phrasing_keeps_the_typed_exit_13(monkeypatch):
    """session_new must agree with require_authenticated on what auth means.

    It exact-matched "Authentication required", so every other phrasing fell
    through to result() as a bare RuntimeError and lost the typed exit 13 that
    callers branch on. require_authenticated has always scanned AUTH_MARKERS.
    """
    driver = _driver()
    session = driver.GrokStdioSession("sid", ".")
    monkeypatch.setattr(
        session,
        "request",
        lambda *a, **k: {"error": {"message": "Not authenticated", "data": "run grok login"}},
    )
    with pytest.raises(driver.GrokAuthenticationRequired) as refused:
        session.session_new()
    assert refused.value.exit_code == 13
    assert "grok login --device-code" in str(refused.value)


def test_review_P2_a_silent_child_cannot_hang_the_read_forever(monkeypatch):
    """An unbounded stdout read is a hang, not a wait.

    Driven with a child that never writes and never exits, which is exactly the
    silent-grok shape: stdout stays open, so the old `while True: read1()` loop
    blocked in initialize/session_new/prompt with no way out.
    """
    driver = _driver()
    monkeypatch.setattr(driver, "GROK_REQUEST_TIMEOUT_S", 1.0)
    session = driver.GrokStdioSession("sid", ".", argv=["sleep", "30"])
    with session:
        with pytest.raises(RuntimeError) as hung:
            session.request("initialize", {})
    assert "exceeded" in str(hung.value)

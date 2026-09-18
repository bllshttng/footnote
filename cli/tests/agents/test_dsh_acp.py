"""Contract tests for the DeepSeek Harness (dsh) ACP stdio lane."""
from __future__ import annotations

import json
import os
import re
import shutil
import uuid
from pathlib import Path

import pytest


DSH = shutil.which("dsh")
LIVE = os.environ.get("FNO_DSH_LIVE") == "1"
FIXTURE = Path(__file__).parent / "fixtures" / "dsh-acp-trials.txt"
UUID_RE = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


def _driver():
    try:
        from fno.agents.harnesses import dsh
    except ModuleNotFoundError as exc:
        pytest.fail(f"dsh ACP driver is missing: {exc}")
    return dsh


def _trials() -> dict[str, str]:
    assert FIXTURE.is_file(), "the measured dsh ACP fixture is missing"
    lines = FIXTURE.read_text(encoding="utf-8").splitlines()
    return dict(line.split("=", 1) for line in lines if "=" in line)


def test_AC3_HP_argv_is_the_acp_profile():
    assert _driver().dsh_acp_argv() == ["dsh", "--profile", "acp"]


def test_AC2_HP_fixture_records_the_measured_live_readings():
    trials = _trials()
    assert trials["initialize.agentInfo.name"] == "deepseek-harness-acp"
    assert trials["initialize.protocolVersion"] == "1"
    assert UUID_RE.fullmatch(trials["session_new.sessionId"])
    assert trials["session_list_after_close.contains_id"] == "True"
    assert trials["prompt_without_key.shape"] == "jsonrpc-error"


def test_AC3_ERR_unkeyed_turn_raises_the_typed_credential_refusal():
    """The measured unkeyed-turn error, replayed by a child that is not dsh."""
    driver = _driver()
    text = _trials()["prompt_without_key.text"]
    response = {"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": text}}
    fake = f"printf '%s\\n' {json.dumps(json.dumps(response))}; cat > /dev/null"
    session = driver.DshAcpSession(cwd=".", argv=["sh", "-c", fake])
    session.session_id = "sid"
    with session:
        with pytest.raises(driver.DshCredentialRequired) as refused:
            session.prompt("hello")
    assert refused.value.exit_code == 13
    assert "DEEPSEEK_API_KEY" in str(refused.value)
    assert "no API key for provider route" in str(refused.value)


def test_AC4_LIVE_gate_is_explicitly_preserved_by_hermetic_runner():
    from fno.hermetic import _RUNNER_PASSTHROUGH

    assert "FNO_DSH_LIVE" in _RUNNER_PASSTHROUGH


def test_session_resume_selects_the_session_for_the_next_prompt(monkeypatch):
    driver = _driver()
    session = driver.DshAcpSession(cwd=".")
    calls = []

    def request(method, params):
        calls.append((method, params))
        return {"result": {}}

    monkeypatch.setattr(session, "request", request)
    session.session_resume("resumed-session")
    session.prompt("hello")

    assert calls[-1] == (
        "session/prompt",
        {
            "sessionId": "resumed-session",
            "prompt": [{"type": "text", "text": "hello"}],
        },
    )


@pytest.mark.smoke
@pytest.mark.skipif(DSH is None, reason="dsh binary is not on PATH")
def test_AC3_HP_real_session_is_minted_and_listed_after_close(tmp_path):
    """A live dsh answers the handshake, mints a UUID and lists it once closed.

    Key-independent: no step here runs a turn. dsh hides active sessions from
    session/list, so the listing is read after session/close.
    """
    driver = _driver()
    home = tmp_path / "dsh-home"
    home.mkdir()
    env = {**os.environ, "DSH_HOME": str(home)}
    with driver.DshAcpSession(cwd=tmp_path, env=env) as session:
        initialized = session.initialize()
        assert initialized["agentInfo"]["name"] == "deepseek-harness-acp"

        minted = session.session_new()
        assert UUID_RE.fullmatch(minted)

        session.session_close(minted)
        sessions = session.session_list()["sessions"]
        listed = [s for s in sessions if s.get("sessionId") == minted]
        assert listed, f"closed session {minted} is not in session/list: {sessions}"
        assert Path(listed[0]["cwd"]).resolve() == tmp_path.resolve()


@pytest.mark.smoke
@pytest.mark.skipif(
    not (LIVE and DSH),
    reason="live dsh ACP journey requires FNO_DSH_LIVE=1 and dsh on PATH",
)
def test_AC4_LIVE_turn_answers_the_planted_token(tmp_path, monkeypatch):
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
    assert real_home is not None, "the live dsh journey could not locate the real HOME"
    monkeypatch.setenv("HOME", real_home)
    monkeypatch.setenv("USERPROFILE", real_home)

    token = f"DSH_ACP_TOKEN_{uuid.uuid4().hex}"
    with driver.DshAcpSession(tmp_path) as session:
        session.initialize()
        session.session_new()
        session.prompt(f"Reply with exactly the planted token {token}. Do not call tools.")
        assert token in json.dumps(session.notifications)

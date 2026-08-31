"""Credential-free contract tests for kimi's ACP stdio lane."""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from pathlib import Path

import pytest


KIMI = shutil.which("kimi")
LIVE = os.environ.get("FNO_KIMI_LIVE") == "1"


def _driver():
    try:
        from fno.agents.harnesses import kimi
    except ModuleNotFoundError as exc:
        pytest.fail(f"Kimi ACP driver is missing: {exc}")
    return kimi


@pytest.mark.skipif(KIMI is None, reason="kimi binary is not on PATH")
def test_AC5_HP_real_initialize_and_session_list_are_positive_markers(tmp_path):
    """The real binary completes the ACP handshake and answers session/list.

    Login-state-independent on purpose: initialize and session/list answer
    unauthenticated (measured against 0.38.0), so an operator completing kimi
    onboarding cannot turn this test red with no code change. The typed
    refusal is covered by test_AC4_STDERR below and hermetically beside it.
    """
    driver = _driver()
    with driver.KimiAcpSession(cwd=tmp_path) as session:
        initialized = session.initialize()
        assert initialized["protocolVersion"] == 1
        assert initialized["agentInfo"]["name"] == "Kimi Code CLI"
        assert initialized["agentInfo"]["version"]
        assert session.proc is not None and session.proc.poll() is None

        listed = session.session_list()
        assert isinstance(listed.get("sessions"), list)
        assert listed["nextCursor"] is None


@pytest.mark.skipif(KIMI is None, reason="kimi binary is not on PATH")
def test_AC4_STDERR_session_new_refusal_carries_both_halves_and_names_the_action(tmp_path):
    """The typed refusal carries the JSON-RPC half AND the stderr condition.

    The JSON-RPC error says only "Authentication required"; stderr names the
    actual state ("no provider configured"). A driver reading stdout alone
    produces a refusal that names no condition, and this test fails for it
    because the stderr half is asserted, not merely available.
    """
    driver = _driver()
    with driver.KimiAcpSession(cwd=tmp_path) as session:
        try:
            minted = session.session_new(add_dirs=[tmp_path])
        except driver.KimiAuthenticationRequired as refused:
            assert refused.exit_code == 13
            assert "Authentication required" in str(refused)
            assert "no provider configured" in str(refused)
            assert "kimi login" in str(refused)
            return
        # A provider now exists (onboarding completed between measurements).
        # The minted id is then the positive marker, and the refusal halves
        # are pinned hermetically by the test below.
        assert minted


def test_AC4_STDERR_hermetic_stderr_half_is_asserted_not_ambient():
    """The stderr attachment is driver behavior, not a live-binary accident.

    Driven with a child that emits the auth error on stdout and the provider
    condition on stderr, so the assertion holds with kimi absent from PATH
    and cannot drift with a kimi version bump.
    """
    driver = _driver()
    fake = (
        'printf "%s\\n" \'{"jsonrpc":"2.0","id":1,"error":{"code":-32000,'
        '"message":"Authentication required"}}\'; '
        'printf "%s\\n" "no provider configured; complete onboarding via /login" >&2; '
        "sleep 2"
    )
    session = driver.KimiAcpSession(cwd=".", argv=["sh", "-c", fake])
    with session:
        with pytest.raises(driver.KimiAuthenticationRequired) as refused:
            session.session_new()
    assert refused.value.exit_code == 13
    assert "Authentication required" in str(refused.value)
    assert "no provider configured" in str(refused.value)
    assert "kimi login" in str(refused.value)


def test_review_P1_stderr_settle_cannot_turn_into_a_second_hang(monkeypatch):
    """The bounded stderr wait must stay bounded on a silent child.

    The settle exists so an auth refusal carries the stderr condition; a
    child that never writes stderr would turn that diagnostic into a second
    hang if the wait were unbounded. The refusal must still raise, naming the
    JSON-RPC half it did get.
    """
    driver = _driver()
    monkeypatch.setattr(driver, "STDERR_SETTLE_S", 1.0)
    # `cat > /dev/null` exits on stdin EOF, so close() reaps it instead of
    # burning its full wait timeout the way a `sleep` child forced.
    fake = (
        'printf "%s\\n" \'{"jsonrpc":"2.0","id":1,"error":{"code":-32000,'
        '"message":"Authentication required"}}\'; cat > /dev/null'
    )
    session = driver.KimiAcpSession(cwd=".", argv=["sh", "-c", fake])
    with session:
        started = time.monotonic()
        with pytest.raises(driver.KimiAuthenticationRequired) as refused:
            session.session_new()
        elapsed = time.monotonic() - started
    assert elapsed < 4.0, f"the stderr settle blocked for {elapsed:.1f}s; it is unbounded"
    assert "Authentication required" in str(refused.value)


@pytest.mark.skipif(KIMI is None, reason="kimi binary is not on PATH")
def test_AC5_AUTH_stream_lane_emits_the_version_frame_before_failing():
    """`kimi -p` emits the system.version frame, then exits 1 without a model.

    That frame is the positive marker that the stream lane exists, available
    with no credential and no model tokens. The exit status is read directly
    off the CompletedProcess; read through a pipe it would be the last
    stage's, which is the mistake that produced one wrong fail-open reading
    on pi.
    """
    completed = subprocess.run(
        ["kimi", "-p", "say ok", "--output-format", "stream-json"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert completed.returncode == 1
    assert '"type":"system.version"' in completed.stdout


def test_AC3_HP_argv_has_no_session_id_and_maps_the_model_alias():
    """No caller-assigned id exists for kimi - the field that differs from grok.

    grok's argv carries a --session-id the server then ignores; kimi has no
    such flag at all (measured help 0.38.0), so there is nothing to pass and
    nothing to be silently ignored. --model is the global alias flag.
    """
    driver = _driver()
    assert driver.kimi_acp_argv() == ["kimi", "acp"]
    assert driver.kimi_acp_argv(model="kimi-k2") == [
        "kimi",
        "acp",
        "--model",
        "kimi-k2",
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


def test_request_payloads_are_standard_acp_messages_with_a_grant_key():
    driver = _driver()
    assert driver.initialize_params()["protocolVersion"] == 1
    assert driver.session_list_params() == {}
    assert driver.session_new_params("/tmp/repo") == {
        "cwd": "/tmp/repo",
        "mcpServers": [],
    }
    # The writable-dirs grant rides the create call on kimi's declared
    # additionalDirectories capability (AC6 composition half).
    assert driver.session_new_params("/tmp/repo", add_dirs=["/a", "/b"]) == {
        "cwd": "/tmp/repo",
        "mcpServers": [],
        "additionalDirectories": ["/a", "/b"],
    }


def test_AC4_LIVE_gate_is_explicitly_preserved_by_hermetic_runner():
    from fno.hermetic import _RUNNER_PASSTHROUGH

    assert "FNO_KIMI_LIVE" in _RUNNER_PASSTHROUGH


def test_AC9_EVIDENCE_kimi_paste_fixture_records_measured_trials():
    fixture = Path(__file__).parent / "fixtures" / "kimi-paste-trials.txt"
    assert fixture.is_file(), "the measured kimi pane fixture is missing"
    text = fixture.read_text(encoding="utf-8")
    assert "payload_bytes=" in text
    assert "payload_lines=" in text
    assert "paste_to_enter_delay_ms=" in text
    assert "trial 3 SUBMITTED" in text


def test_review_P2_a_silent_child_cannot_hang_the_read_forever(monkeypatch):
    """An unbounded stdout read is a hang, not a wait.

    Driven with a child that never writes, so stdout stays open and the read
    loop would block initialize forever. The child (`cat` into /dev/null)
    exits on stdin EOF, so close() reaps it instead of burning its full wait
    timeout the way a `sleep` child forced.
    """
    driver = _driver()
    monkeypatch.setattr(driver, "KIMI_REQUEST_TIMEOUT_S", 1.0)
    session = driver.KimiAcpSession(cwd=".", argv=["sh", "-c", "cat > /dev/null"])
    with session:
        with pytest.raises(RuntimeError) as hung:
            session.request("initialize", {})
    assert "exceeded" in str(hung.value)


def test_review_P2b_an_unwatchable_stream_refuses_instead_of_blocking(monkeypatch):
    """The select failure path must not reopen the hang it was added to close."""
    import select as _select

    driver = _driver()
    monkeypatch.setattr(driver, "KIMI_REQUEST_TIMEOUT_S", 30.0)

    def _unwatchable(*_a, **_k):
        raise OSError(9, "Bad file descriptor")

    monkeypatch.setattr(_select, "select", _unwatchable)
    session = driver.KimiAcpSession(cwd=".", argv=["sh", "-c", "cat > /dev/null"])
    with session:
        started = time.monotonic()
        with pytest.raises(RuntimeError) as refused:
            session.request("initialize", {})
        elapsed = time.monotonic() - started
    # It must refuse promptly, not ride the 30s deadline and not block forever.
    assert elapsed < 5.0, f"refusal took {elapsed:.1f}s; it blocked"
    assert "cannot be bounded" in str(refused.value)


def test_review_R3_a_server_request_is_never_read_as_the_response(monkeypatch):
    """A server-to-client request (id + method) is not this request's response."""
    driver = _driver()
    session = driver.KimiAcpSession(cwd=".")
    monkeypatch.setattr(session, "send", lambda message: None)

    def _wire(frames):
        stream = iter(frames)
        monkeypatch.setattr(session, "events", lambda: stream)

    # Distinct id: swallowed today, the turn hangs to the deadline.
    _wire(
        [
            {
                "jsonrpc": "2.0",
                "id": 5,
                "method": "session/request_permission",
                "params": {},
            }
        ]
    )
    with pytest.raises(RuntimeError) as refused:
        session.request("session/prompt", {})
    assert "session/request_permission" in str(refused.value)
    assert "fno answers no server requests" in str(refused.value)

    # Id collision with our own request: returned as the response today.
    session._request_id = 0
    _wire(
        [
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/request_permission",
                "params": {},
            }
        ]
    )
    with pytest.raises(RuntimeError) as collided:
        session.request("session/prompt", {})
    assert "fno answers no server requests" in str(collided.value)

    # A real response past an unrelated notification still lands.
    session._request_id = 0
    _wire(
        [
            {"jsonrpc": "2.0", "method": "session/update", "params": {}},
            {"jsonrpc": "2.0", "id": 1, "result": {"ok": True}},
        ]
    )
    assert session.request("session/prompt", {})["result"] == {"ok": True}
    assert session.notifications[-1]["method"] == "session/update"


def test_review_R3_send_to_a_dead_child_fails_typed():
    """A broken pipe must raise the lane's typed error, never a raw OSError."""
    driver = _driver()
    session = driver.KimiAcpSession(cwd=".", argv=["true"])
    session.start()
    assert session.proc is not None
    session.proc.wait(timeout=5)
    with pytest.raises(driver.DispatchAskError) as typed:
        session.send({"jsonrpc": "2.0", "id": 1, "method": "x", "params": {}})
    assert "broken pipe" in str(typed.value)
    session.close()

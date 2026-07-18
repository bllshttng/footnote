"""fno mail status: sent_unclaimed count (x-39a4 task 1.3, AC1-EDGE).

Reuses the same sent-unclaimed predicate as notify-self; a present, honest zero
is rendered, never an omitted line.
"""
from __future__ import annotations

from datetime import datetime, timedelta, timezone

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.paths_testing import use_tmpdir

MARKERS = ("CODEX_THREAD_ID", "CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID")
MY_SID = "abcd1234ffff"
MY_HANDLE = "claude-abcd1234"


def _ts_ago(seconds: int) -> str:
    dt = datetime.now(tz=timezone.utc) - timedelta(seconds=seconds)
    return dt.strftime("%Y-%m-%dT%H:%M:%SZ")


@pytest.fixture
def env(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    for m in MARKERS:
        monkeypatch.delenv(m, raising=False)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", MY_SID)
    return tmp_path


def _send(from_, to, body, *, ts=None):
    from fno.bus.log import Envelope, append
    append(Envelope.new(from_=from_, to=to, kind="send", body=body, ts=ts))


def test_collect_status_counts_unclaimed(env):
    from fno.mail.cli import _collect_status
    _send(MY_HANDLE, "carol", "old", ts=_ts_ago(3600))
    snap = _collect_status("proj", env)
    assert snap["sent_unclaimed"] == 1


def test_collect_status_zero_is_present(env):
    from fno.mail.cli import _collect_status
    snap = _collect_status("proj", env)
    assert snap["sent_unclaimed"] == 0


def test_status_renders_sent_unclaimed_line(env):
    _send(MY_HANDLE, "carol", "old", ts=_ts_ago(3600))
    res = CliRunner().invoke(app, ["mail", "status", "--from", "proj"])
    assert res.exit_code == 0
    assert "sent unclaimed: 1" in res.stdout


def test_status_renders_honest_zero(env):
    res = CliRunner().invoke(app, ["mail", "status", "--from", "proj"])
    assert res.exit_code == 0
    assert "sent unclaimed: 0" in res.stdout


def test_status_json_has_key(env):
    import json
    res = CliRunner().invoke(app, ["mail", "status", "--from", "proj", "--json"])
    assert res.exit_code == 0
    assert "sent_unclaimed" in json.loads(res.stdout)

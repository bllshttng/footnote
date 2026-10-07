"""`fno-agents mail-backfill run` at its real surface.

The archive gate (a cross-session row is audit-only, never deliverable)
and the argv contract through the real binary, on fixture transcripts.
The engine's internals (join attribution, archive id, idempotent apply)
are covered by the Rust suite; this file guards the Python-side gate and
the transport seam only.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

from fno.bus.log import Envelope, is_deliverable

_SENDER_SESSION = "0199aaaa-1111-7000-8000-aaaaaaaaaaaa"
_RECEIVER_SESSION = "0199bbbb-2222-7000-8000-bbbbbbbbbbbb"
_BODY = "hold ack: nothing running, tests green on both legs"


@pytest.fixture
def _rust_bin():
    """Pin a build carrying the mail-backfill verb; skip when absent (the
    Rust suite covers the engine regardless)."""
    candidate = os.environ.get("FNO_AGENTS_TEST_BIN")
    if not candidate:
        root = Path(__file__).resolve().parents[3] / "crates" / "fno-agents" / "target"
        for profile in ("debug", "release"):
            probe = root / profile / "fno-agents"
            if probe.exists():
                candidate = str(probe)
                break
    if not candidate or not Path(candidate).exists():
        pytest.skip("no fno-agents build with the mail-backfill verb")
    return candidate


def _fixture_root(tmp_path):
    root = tmp_path / "projects"
    sender = root / "-Users-x-proj"
    sender.mkdir(parents=True)
    (sender / "s.jsonl").write_text(
        json.dumps({
            "type": "assistant",
            "sessionId": _SENDER_SESSION,
            "uuid": "row-1",
            "timestamp": "2026-10-07T10:00:00.000Z",
            "message": {"content": [{"type": "tool_use", "name": "SendMessage",
                "input": {"to": "jordan", "summary": "hold ack", "message": _BODY}}]},
        }) + "\n",
        encoding="utf-8",
    )
    receiver = root / "-Users-y-proj"
    receiver.mkdir(parents=True)
    (receiver / "r.jsonl").write_text(
        json.dumps({
            "type": "user",
            "sessionId": _RECEIVER_SESSION,
            "timestamp": "2026-10-07T10:00:01.000Z",
            "message": {"content": (
                f'<cross-session-message from="uds:/tmp/cc-socks/4242.sock" '
                f'from-name="worker-1">\n{_BODY}\n</cross-session-message>'
            )},
        }) + "\n",
        encoding="utf-8",
    )
    return root


def test_archive_gate_and_fixture_join_through_the_real_binary(
    _rust_bin, tmp_path, monkeypatch
):
    """The archive gate (a cross-session row is never deliverable, and the
    unclaimed nag skips it: draining either would hand the recipient a
    second copy of its own history) and the argv contract through the real
    binary on fixture halves."""
    monkeypatch.setenv("FNO_BUS_DIR", str(tmp_path / "bus"))
    row = Envelope.new(
        from_="a", to="b", kind="send", body="x", delivery="cross-session"
    )
    assert is_deliverable(row) is False
    from fno.bus.log import append as bus_append
    from fno.mail.landed import _sent_unclaimed

    bus_append(row)
    assert _sent_unclaimed("a", -1) == []

    root = _fixture_root(tmp_path)
    proc = subprocess.run(
        [_rust_bin, "mail-backfill", "run", "--root", str(root)],
        capture_output=True, text=True, timeout=120,
    )
    assert proc.returncode == 0, proc.stderr
    summary = json.loads(proc.stdout)
    assert summary["joined"] == 1
    assert summary["rows"][0]["from_session"] == _SENDER_SESSION
    assert summary["rows"][0]["to_session"] == _RECEIVER_SESSION

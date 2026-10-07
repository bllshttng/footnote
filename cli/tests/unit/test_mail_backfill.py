"""`fno agents mail backfill`: outage-era traffic back into the store.

The store skipped agent-to-agent messages that went over the harness's
native cross-session transport while `fno mail send` was down. Both halves
of each message live in the harness transcripts, so the engine joins the
sender's native SendMessage call with the receiver's cross-session block
and writes an audit-only row (delivery=cross-session) that never
re-delivers. Idempotent by msg_id: the archive id is a deterministic
function of the sender session and the source row, so a re-run skips what
already landed. The engine is Rust (`fno-agents mail-backfill run`); the
tests drive it through the real binary.
"""
from __future__ import annotations

import json
import os
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.bus.log import Envelope, bus_log_path, is_deliverable
from fno.mail.cli import mail_app
from fno.paths_testing import use_tmpdir

runner = CliRunner()

_SENDER_SESSION = "0199aaaa-1111-7000-8000-aaaaaaaaaaaa"
_RECEIVER_SESSION = "0199bbbb-2222-7000-8000-bbbbbbbbbbbb"
_SOCKET = "uds:/tmp/cc-socks/4242.sock"


@pytest.fixture
def _tmp_state(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    return tmp_path


@pytest.fixture
def _rust_bin(monkeypatch):
    """Pin a build carrying the mail-backfill verb.

    The deployed fleet binary predates the verb, so the test resolves the
    checkout's own build (or FNO_AGENTS_TEST_BIN) and skips when neither
    exists. The Rust contract is covered twice over by cargo test, which
    runs regardless.
    """
    candidate = os.environ.get("FNO_AGENTS_TEST_BIN")
    if not candidate:
        root = (
            Path(__file__).resolve().parents[3]
            / "crates"
            / "fno-agents"
            / "target"
        )
        for profile in ("debug", "release"):
            probe = root / profile / "fno-agents"
            if probe.exists():
                candidate = str(probe)
                break
    if not candidate or not Path(candidate).exists():
        pytest.skip("no fno-agents build with the mail-backfill verb")
    monkeypatch.setenv("FNO_AGENTS_BIN", candidate)


def _send_row(ts, body, uuid="row-1"):
    return {
        "type": "assistant",
        "sessionId": _SENDER_SESSION,
        "uuid": uuid,
        "timestamp": ts,
        "message": {
            "content": [
                {
                    "type": "tool_use",
                    "name": "SendMessage",
                    "input": {"to": _SOCKET, "summary": "hold ack", "message": body},
                }
            ]
        },
    }


def _receive_row(ts, body):
    return {
        "type": "user",
        "sessionId": _RECEIVER_SESSION,
        "timestamp": ts,
        "message": {
            "content": (
                f'<cross-session-message from="{_SOCKET}" from-name="worker-1">'
                f"\n{body}\n</cross-session-message>"
            )
        },
    }


def _store_transcripts(root, send_body):
    sender_dir = root / "-Users-x-proj"
    sender_dir.mkdir(parents=True)
    (sender_dir / "s.jsonl").write_text(
        json.dumps(_send_row("2026-10-07T10:00:00.000Z", send_body)) + "\n",
        encoding="utf-8",
    )
    receiver_dir = root / "-Users-y-proj"
    receiver_dir.mkdir(parents=True)
    (receiver_dir / "r.jsonl").write_text(
        json.dumps(_receive_row("2026-10-07T10:00:01.000Z", send_body)) + "\n",
        encoding="utf-8",
    )


def test_a_cross_session_row_is_never_redelivered():
    """The row archives a delivery that already happened; draining it would
    hand the recipient a second copy of its own history."""
    row = Envelope.new(
        from_="a", to="b", kind="send", body="x", delivery="cross-session"
    )
    assert is_deliverable(row) is False


def test_backfill_scan_apply_and_idempotent_rerun(_tmp_state, _rust_bin):
    """End to end on fixture transcripts: dry run joins the halves, apply
    writes one provenance-complete row, a second apply writes nothing."""
    root = _tmp_state / "projects"
    _store_transcripts(root, "hold ack: nothing running")

    def _run(*args):
        return runner.invoke(
            mail_app,
            ["backfill", "--root", str(root), *args],
        )

    dry = json.loads(_run().output)
    assert dry["joined"] == 1
    assert dry["rows"][0]["from_session"] == _SENDER_SESSION
    assert dry["rows"][0]["to_session"] == _RECEIVER_SESSION

    applied = json.loads(_run("--apply").output)
    assert applied["written"] == 1

    rows = [
        json.loads(line)
        for line in bus_log_path().read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert len(rows) == 1
    row = rows[0]
    assert row["delivery"] == "cross-session"
    assert row["from"] == "worker-1"
    assert row["to"] == _RECEIVER_SESSION
    assert row["from_session"] == _SENDER_SESSION
    assert row["meta"]["to_session"] == _RECEIVER_SESSION
    assert row["meta"]["transport"] == "claude-cross-session"
    assert row["meta"]["sender_transcript"].endswith("s.jsonl")
    assert row["meta"]["receiver_transcript"].endswith("r.jsonl")
    assert row["body"] == "hold ack: nothing running"
    assert row["id"].startswith("fmail-bf-")

    again = json.loads(_run("--apply").output)
    assert again["written"] == 0


def test_backfill_scan_skips_rows_outside_the_window(_tmp_state, _rust_bin):
    """The window bounds the scan: only sends stamped inside it join, even
    when both pairs live in the scanned files."""
    root = _tmp_state / "projects"
    _store_transcripts(root, "hold ack: nothing running")
    late = root / "-Users-z-proj"
    late.mkdir()
    (late / "s.jsonl").write_text(
        json.dumps(_send_row("2026-10-07T18:00:00.000Z", "a later message", uuid="row-2"))
        + "\n",
        encoding="utf-8",
    )
    (late / "r.jsonl").write_text(
        json.dumps(_receive_row("2026-10-07T18:00:01.000Z", "a later message")) + "\n",
        encoding="utf-8",
    )
    result = runner.invoke(
        mail_app,
        [
            "backfill", "--root", str(root),
            "--since", "2026-10-07T15:00:00Z", "--until", "2026-10-07T19:00:00Z",
        ],
    )
    rows = json.loads(result.output)
    assert [r["body_head"] for r in rows] == ["a later message"]


def test_backfill_scan_never_joins_a_mismatched_body(_tmp_state, _rust_bin):
    """The join demands the body head agree: a block on the same socket
    carrying different text is not this send's landing proof."""
    root = _tmp_state / "projects"
    _store_transcripts(root, "hold ack: nothing running")
    other = root / "-Users-y-proj" / "other.jsonl"
    other.write_text(
        json.dumps(_receive_row("2026-10-07T10:00:02.000Z", "an entirely different report"))
        + "\n",
        encoding="utf-8",
    )
    result = runner.invoke(mail_app, ["backfill", "--root", str(root)])
    assert json.loads(result.output)["joined"] == 1

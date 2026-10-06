"""mail send's subject and the JSON receipt.

The send receipt is one JSON line {msg_id, subject, to, status} on every
lane. The subject flag lives on the Rust front, which peels it off the
argv and hands it over as FNO_MAIL_SUBJECT; this verb reads that env, and
the subject rides the wire envelope (the delivered header's third field),
the durable bus row, and the receipt.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.bus.log import Envelope, from_json_line, to_json_line
from fno.paths_testing import use_tmpdir


def test_json_receipt_four_keys_always() -> None:
    from fno.mail.receipts import json_receipt

    row = json.loads(json_receipt("fmail-1", to="peer", status="delivered (hosted)"))
    assert set(row) == {"msg_id", "subject", "to", "status"}
    assert row["subject"] is None
    row = json.loads(
        json_receipt("fmail-1", to="peer", status="queued (durable)", subject="s")
    )
    assert row["subject"] == "s"


@pytest.fixture
def isolated(tmp_path: Path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "inbox"))
    monkeypatch.setenv("FNO_INBOX_TEST_MODE", "1")
    return tmp_path


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def _invoke(isolated, runner: CliRunner, *args: str):
    from fno.mail.cli import mail_app

    return runner.invoke(mail_app, ["send", *args])


def test_kind_lane_send_subject_rides_bus_row_and_receipt(isolated, runner, monkeypatch) -> None:
    from fno.bus.log import iter_messages
    from fno.inbox.store import read_unread_threads

    monkeypatch.setenv("FNO_MAIL_SUBJECT", "schema freeze Friday")
    result = _invoke(
        isolated,
        runner,
        "--to-project", "acme-docs", "--kind", "heads-up",
        "--from-name", "acme-web",
        "locked schema change, impact is a migration",
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output.strip().splitlines()[0])
    assert set(receipt) == {"msg_id", "subject", "to", "status"}
    assert receipt["subject"] == "schema freeze Friday"
    assert receipt["to"] == "acme-docs"
    assert "queued (durable)" in receipt["status"]
    # The kind lane's durable row carries the inbox kind, not "send".
    rows = [m for m in iter_messages() if m.subject == "schema freeze Friday"]
    assert rows and rows[-1].to == "acme-docs"
    threads = read_unread_threads("acme-docs")
    assert len(threads) == 1

    # No env, no subject: the receipt's subject reads null.
    monkeypatch.delenv("FNO_MAIL_SUBJECT")
    plain = _invoke(
        isolated,
        runner,
        "--to-project", "acme-docs", "--kind", "fyi",
        "--from-name", "acme-web", "plain body",
    )
    assert plain.exit_code == 0, plain.output
    assert json.loads(plain.output.strip().splitlines()[0])["subject"] is None

    # --raw strips the envelope, so a subject has nothing to ride.
    monkeypatch.setenv("FNO_MAIL_SUBJECT", "s")
    refused = _invoke(isolated, runner, "peer", "hi", "--raw")
    assert refused.exit_code == 2, refused.output
    assert "FNO_MAIL_SUBJECT" in refused.output

    # The bus row serializes with the subject and parses it back; a row from
    # before the field existed parses None and re-serializes without it.
    env = Envelope.new(from_="a", to="b", kind="send", body="hi", subject="gate fix")
    line = to_json_line(env)
    assert '"subject":"gate fix"' in line
    assert from_json_line(line).subject == "gate fix"
    legacy = from_json_line(
        '{"v":1,"id":"fmail-aaaaaaaaaaaa","ts":"2026-10-06T00:00:00Z",'
        '"thread":"fmail-aaaaaaaaaaaa","from":"a","to":"b","kind":"send","body":"x"}'
    )
    assert legacy.subject is None
    assert "subject" not in to_json_line(legacy)

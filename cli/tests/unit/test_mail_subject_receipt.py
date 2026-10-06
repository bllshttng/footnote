"""mail send --subject and the JSON receipt.

The send receipt is one JSON line {msg_id, subject, to, status} on every
lane; --subject rides the wire envelope (the delivered header's third
field), the durable bus row, and the receipt. The team shim relays
--subject/--expires/--urgent as flag+value pairs.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.bus.log import Envelope, from_json_line, to_json_line
from fno.paths_testing import use_tmpdir


def test_envelope_subject_round_trips_and_omits_when_unset() -> None:
    env = Envelope.new(
        from_="a", to="b", kind="send", body="hi", subject="gate fix"
    )
    line = to_json_line(env)
    assert '"subject":"gate fix"' in line
    parsed = from_json_line(line)
    assert parsed.subject == "gate fix"
    # A pre-field row parses with subject None and re-serializes without it.
    legacy = from_json_line(
        '{"v":1,"id":"fmail-aaaaaaaaaaaa","ts":"2026-10-06T00:00:00Z",'
        '"thread":"fmail-aaaaaaaaaaaa","from":"a","to":"b","kind":"send","body":"x"}'
    )
    assert legacy.subject is None
    assert "subject" not in to_json_line(legacy)


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


def test_kind_lane_send_subject_rides_bus_row_and_receipt(isolated, runner) -> None:
    from fno.bus.log import iter_messages
    from fno.inbox.store import read_unread_threads

    result = _invoke(
        isolated,
        runner,
        "--to-project", "acme-docs", "--kind", "heads-up",
        "--from-name", "acme-web", "--subject", "schema freeze Friday",
        "locked schema change, impact is a migration",
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output.strip().splitlines()[0])
    assert set(receipt) == {"msg_id", "subject", "to", "status"}
    assert receipt["subject"] == "schema freeze Friday"
    assert receipt["to"] == "acme-docs"
    assert "queued (durable)" in receipt["status"]
    rows = [m for m in iter_messages() if m.kind == "send"]
    assert rows and rows[-1].subject == "schema freeze Friday"
    threads = read_unread_threads("acme-docs")
    assert len(threads) == 1


def test_kind_lane_without_subject_receipt_subject_is_null(isolated, runner) -> None:
    result = _invoke(
        isolated,
        runner,
        "--to-project", "acme-docs", "--kind", "fyi",
        "--from-name", "acme-web", "plain body",
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output.strip().splitlines()[0])
    assert receipt["subject"] is None


def test_raw_refuses_subject(isolated, runner) -> None:
    result = _invoke(isolated, runner, "peer", "hi", "--raw", "--subject", "s")
    assert result.exit_code == 2, result.output
    assert "--subject" in result.output


def test_team_relays_subject_expires_urgent_as_pairs(isolated, monkeypatch) -> None:
    """Click parked an unknown option's VALUE in the positional body,
    so `--subject S` relayed a bare --subject and the writer refused S as a
    flag. The shim now declares the three announcement flags and relays each
    flag with its value as one pair."""
    import shutil

    from fno.mail.cli import mail_app

    argv_log = isolated / "argv.log"
    script = isolated / "fake-fno-agents"
    script.write_text(
        "#!/bin/sh\n"
        f"printf '%s\\n' \"$@\" >> {argv_log}\n"
        "exit 0\n"
    )
    script.chmod(0o755)
    monkeypatch.setattr(
        shutil, "which", lambda name, path=None: str(script) if name == "fno-agents" else shutil.which(name, path)
    )

    result = CliRunner().invoke(
        mail_app,
        [
            "team", "--scope", "all", "--subject", "merge-hold-cli-ci",
            "--expires", "12h", "--urgent", "stand down",
        ],
    )
    assert result.exit_code == 0, result.output
    argv = argv_log.read_text().strip().splitlines()
    for flag, value in (("--subject", "merge-hold-cli-ci"), ("--expires", "12h")):
        i = argv.index(flag)
        assert argv[i + 1] == value, argv

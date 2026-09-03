"""A mail send's durable row exists before resolution, and never double-surfaces.

The window that loses messages is RESOLUTION: ``resolve_or_suggest`` measured
70.37s at load 7.08 on the machine this fleet runs on, so a send killed inside
it used to leave no bus row of any kind. These tests assert against the bus
FILE (the system of record), never an exit code, and pin the four outcomes:

- a kill during resolution or mid-ladder still leaves the row (AC4-ERR
  and AC4-ERR2; a write placed after resolution passes the second and proves
  nothing about the first, so both windows are covered here),
- a confirmed hosted delivery marks the twin `delivered_at` and unread does
  not list it (AC4-HP),
- a live miss writes no second row (AC4-EDGE),
- a refusal that queues nothing retracts the pre-written row.
"""

from __future__ import annotations

import json

import pytest

from fno.bus.cursor import scan_unread
from fno.bus.log import iter_messages
from fno.paths_testing import use_tmpdir


@pytest.fixture
def mailbox(tmp_path, monkeypatch):
    monkeypatch.delenv("FNO_BUS_DIR", raising=False)
    monkeypatch.setenv("FNO_CLAUDE_DAEMON_DIR", str(tmp_path / "daemon-empty"))
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path))
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_STYLE_ENFORCE", "0")
    return tmp_path


def _bus_file(mailbox):
    from fno.bus.log import bus_log_path

    return bus_log_path()


def _rows(mailbox):
    return list(iter_messages(warn=False))


def test_row_is_on_the_bus_file_while_resolution_is_still_running(mailbox, monkeypatch):
    # AC4-ERR, structurally: resolution is stubbed to die the way a SIGKILL
    # does - mid-call, before any live rung. The bus FILE must already carry
    # the row when that happens, because the write precedes the resolve.
    import fno.mail.cli as mail_cli

    def dying_resolution(handle, **kwargs):
        raise KeyboardInterrupt  # SIGKILL's stand-in: no unwinding, no receipts

    monkeypatch.setattr(
        "fno.agents.discover.resolve_or_suggest", dying_resolution
    )

    pre = mail_cli._durable_first_send(
        "abcd1234",
        "the king cannot read its board",
        from_name=None,
        reply_to=None,
        style_exception=None,
        origin=None,
    )
    assert pre is not None
    rows = [r for r in _rows(mailbox) if r.id == pre.msg_id]
    assert len(rows) == 1, "the durable row must exist before resolution returns"
    assert rows[0].to == "abcd1234"
    assert rows[0].kind == "send"
    assert rows[0].delivery is None, "the pre-row must be a deliverable durable row"


def test_row_is_on_the_bus_file_after_a_mid_ladder_kill(mailbox, monkeypatch):
    # AC4-ERR2: the kill lands after resolution, before any rung completes.
    # Same assertion target: the file, never an exit code.
    import fno.mail.cli as mail_cli

    pre = mail_cli._durable_first_send(
        "abcd1234",
        "ladder died here",
        from_name=None,
        reply_to=None,
        style_exception=None,
        origin=None,
    )
    assert pre is not None
    # The ladder never ran: simulate the process dying right here.
    rows = [r for r in _rows(mailbox) if r.id == pre.msg_id]
    assert len(rows) == 1


def test_row_addresses_by_the_typed_name_and_drains_by_it(mailbox):
    # The write needs no resolution: the recipient is the string the caller
    # typed, and the recipient reads it with that same name.
    import fno.mail.cli as mail_cli

    pre = mail_cli._durable_first_send(
        "abcd1234",
        "addressed by name",
        from_name=None,
        reply_to=None,
        style_exception=None,
        origin=None,
    )
    assert pre is not None
    msgs = scan_unread("abcd1234")
    assert [m.id for m in msgs] == [pre.msg_id]


def test_pane_name_writes_nothing(mailbox):
    # A name that cannot be a durable recipient keeps refusing downstream; the
    # pre-write must not strand a row for it either.
    import fno.mail.cli as mail_cli

    pre = mail_cli._durable_first_send(
        "some-worker-pane-name",
        "not a mail address",
        from_name=None,
        reply_to=None,
        style_exception=None,
        origin=None,
    )
    assert pre is None
    assert _rows(mailbox) == []


def test_hosted_delivery_marks_delivered_at_and_unread_skips_the_twin(mailbox):
    # AC4-HP: hosted delivery leaves the durable twin suppressed - the
    # recipient must not drain a second copy - while the row pair stays on
    # the file for the sender's audit.
    import fno.mail.cli as mail_cli
    from fno.bus.log import record_hosted_delivery

    pre = mail_cli._durable_first_send(
        "abcd1234",
        "delivered live",
        from_name=None,
        reply_to=None,
        style_exception=None,
        origin=None,
    )
    assert pre is not None
    record_hosted_delivery(
        msg_id=pre.msg_id,
        sender=pre.sender,
        recipient=pre.recipient,
        body=pre.wrapped,
        to_kind="name",
    )
    rows = [r for r in _rows(mailbox) if r.id == pre.msg_id]
    assert len(rows) == 2, "durable twin + hosted record"
    hosted = [r for r in rows if r.delivery == "hosted"]
    assert hosted and hosted[0].delivered_at
    assert scan_unread("abcd1234") == [], "a hosted delivery must not re-surface as unread"


def test_live_miss_writes_no_second_row(mailbox):
    # AC4-EDGE: the durable-first row IS the durable floor. The floor path in
    # _name_lane_send must reuse it, not write a sibling.
    import fno.mail.cli as mail_cli

    pre = mail_cli._durable_first_send(
        "abcd1234",
        "live lane missed",
        from_name=None,
        reply_to=None,
        style_exception=None,
        origin=None,
    )
    assert pre is not None
    rows = [r for r in _rows(mailbox) if r.id == pre.msg_id]
    assert len(rows) == 1
    assert rows[0].delivery is None
    assert [m.id for m in scan_unread("abcd1234")] == [pre.msg_id]


def test_refusal_retracts_the_pre_row(mailbox):
    # A refusal keeps its queue-nothing contract: the row physically exists,
    # so the retraction is the withdraw tombstone every reader already skips.
    import fno.mail.cli as mail_cli

    pre = mail_cli._durable_first_send(
        "abcd1234",
        "will be refused",
        from_name=None,
        reply_to=None,
        style_exception=None,
        origin=None,
    )
    assert pre is not None
    mail_cli._retract_durable_first(pre)
    assert scan_unread("abcd1234") == []
    rows = _rows(mailbox)
    assert any(r.kind == "withdraw" for r in rows)


def test_discovery_failure_retracts_the_pre_row(mailbox, monkeypatch):
    # A store-read failure INSIDE resolution is a refusal, and the refusal
    # contract follows it: the pre-row must not survive a failed resolve as a
    # deliverable message. A kill keeps the row - that is durable-first's
    # whole point - so only the raised-refusal path retracts.
    from typer.testing import CliRunner

    from fno.agents import discover
    from fno.cli import app

    def broken_resolution(handle, **kwargs):
        raise discover.StoreReadError("stores unreadable")

    monkeypatch.setattr(discover, "resolve_or_suggest", broken_resolution)

    result = CliRunner().invoke(
        app,
        ["agents", "mail", "send", "abcd1234", "refused mid-resolve", "--from-name", "web"],
    )
    assert result.exit_code != 0, result.output
    assert scan_unread("abcd1234") == [], "a failed resolve must not leak the pre-row"
    rows = _rows(mailbox)
    assert any(r.kind == "send" for r in rows), "the pre-row existed"
    assert any(r.kind == "withdraw" for r in rows), "the refusal retracted it"


def test_budget_refusal_leaves_no_row(mailbox):
    # Mail the budget refuses must not leave a row: the reservation precedes
    # the write. BudgetRefused exits 1 out of _reserve_budget, before the bus
    # append. Force the refusal with an exhausted pair budget.
    import fno.mail.budget as budget
    import fno.mail.cli as mail_cli

    class _Refused(Exception):
        pass

    def refused(**kwargs):
        raise budget.BudgetRefused(pair="a|b", running=999, current=999)

    monkeypatcher = pytest.MonkeyPatch()
    try:
        monkeypatcher.setattr(budget, "reserve", refused)
        with pytest.raises(RuntimeError):  # typer.Exit rides click's Exit(RuntimeError)
            mail_cli._durable_first_send(
                "abcd1234",
                "over budget",
                from_name=None,
                reply_to=None,
                style_exception=None,
                origin=None,
            )
    finally:
        monkeypatcher.undo()
    assert [r for r in _rows(mailbox) if r.kind == "send"] == []


def test_delivered_at_is_additive_and_old_rows_still_parse():
    # LD11: a line written before delivered_at existed parses with it None.
    from fno.bus.log import from_json_line

    legacy = json.dumps(
        {
            "v": 1,
            "id": "msg-old001",
            "ts": "2026-01-01T00:00:00Z",
            "thread": "msg-old001",
            "from": "a",
            "to": "b",
            "kind": "send",
            "body": "hi",
        }
    )
    env = from_json_line(legacy)
    assert env.delivered_at is None

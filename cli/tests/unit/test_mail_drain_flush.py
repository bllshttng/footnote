"""`fno agents mail drain-self` must flush the bodies before it advances the cursor.

The SessionStart drain hook reads drain-self through a command substitution, so
stdout is a pipe and therefore block-buffered. `print` alone leaves the bodies in
this process's buffer; if the cursor is advanced first and the hook's wall-clock
bound then SIGTERMs the interpreter, the buffer is discarded while the mail is
already marked consumed. The message is gone permanently.

The ordering is one line of code guarding a data-loss path, so it gets one test.
"""

from __future__ import annotations

import dataclasses

import pytest


@dataclasses.dataclass
class _Msg:
    id: str
    from_: str
    to: str
    kind: str
    ts: str
    body: str


class _CountingStdout:
    """Stands in for a block-buffered pipe: writes land in a buffer, and only
    flush() moves them to `delivered`."""

    def __init__(self) -> None:
        self.buffer: list[str] = []
        self.delivered: list[str] = []
        self.flushes = 0

    def write(self, s: str) -> int:
        self.buffer.append(s)
        return len(s)

    def flush(self) -> None:
        self.flushes += 1
        self.delivered.extend(self.buffer)
        self.buffer.clear()

    def isatty(self) -> bool:
        return False


@pytest.fixture
def _drained(monkeypatch):
    """Run cmd_drain_self against one unread message, capturing flush ordering."""
    from fno import harness_identity
    from fno.bus import cursor as cursor_mod
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-1",
        from_="alice",
        to="cl-abcd1234",
        kind="note",
        ts="2026-07-28T00:00:00Z",
        body="the payload that must not be lost",
    )

    class _Ident:
        harness = "claude"
        session_id = "abcd1234"
        disposition = "single"

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity", lambda: _Ident()
    )
    monkeypatch.setattr(
        harness_identity, "canonical_handle", lambda sid: "cl-abcd1234"
    )
    monkeypatch.setattr(
        cursor_mod, "scan_unread", lambda handle: [msg] if handle == "cl-abcd1234" else []
    )

    observed: dict[str, object] = {}
    fake_out = _CountingStdout()

    def _advance(handle, msg_id):
        # Snapshot what a SIGTERM landing right here would have delivered.
        observed["handle"] = handle
        observed["msg_id"] = msg_id
        observed["delivered_at_ack"] = list(fake_out.delivered)
        observed["still_buffered_at_ack"] = list(fake_out.buffer)
        return True

    monkeypatch.setattr(cursor_mod, "advance_cursor", _advance)
    monkeypatch.setattr("sys.stdout", fake_out)

    mail_cli.cmd_drain_self(json_out=False)

    return observed, fake_out, msg


def test_bodies_are_flushed_before_the_cursor_advances(_drained) -> None:
    observed, fake_out, msg = _drained

    assert observed, "advance_cursor was never called, so nothing was acked"
    delivered = "".join(observed["delivered_at_ack"])  # type: ignore[arg-type]
    assert msg.body in delivered, (
        "the cursor advanced while the body was still in this process's stdout "
        "buffer; a SIGTERM here loses the message with the mail marked consumed"
    )
    assert not observed["still_buffered_at_ack"], (
        f"unflushed output remained at ack time: {observed['still_buffered_at_ack']!r}"
    )
    # The cursor must advance to the LAST drained id, or the drained tail
    # re-surfaces on the next wake.
    assert observed["msg_id"] == msg.id
    assert observed["handle"] == "cl-abcd1234"


def test_the_flush_is_what_delivers_it(_drained) -> None:
    """Positive control: the assertion above is only meaningful because the fake
    stdout genuinely withholds writes until flush()."""
    _observed, fake_out, _msg = _drained
    assert fake_out.flushes >= 1, "drain-self never flushed stdout at all"


def test_ac3_err_hook_serialization_failure_leaves_cursor_for_retry(monkeypatch) -> None:
    from fno import harness_identity
    from fno.bus import cursor as cursor_mod
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-retry",
        from_="alice",
        to="cl-abcd1234",
        kind="note",
        ts="2026-08-03T00:00:00Z",
        body="retry me",
    )

    class _Ident:
        harness = "claude"
        session_id = "abcd1234"
        disposition = "single"

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity", lambda: _Ident()
    )
    monkeypatch.setattr(
        harness_identity, "canonical_handle", lambda sid: "cl-abcd1234"
    )
    monkeypatch.setattr(
        cursor_mod, "scan_unread", lambda handle: [msg] if handle == "cl-abcd1234" else []
    )
    monkeypatch.setattr(mail_cli, "_sent_unclaimed", lambda handle, ttl: [])
    advances: list[tuple[str, str]] = []
    monkeypatch.setattr(
        cursor_mod,
        "advance_cursor",
        lambda handle, msg_id: advances.append((handle, msg_id)),
    )
    monkeypatch.setattr(
        mail_cli.json,
        "dumps",
        lambda *args, **kwargs: (_ for _ in ()).throw(TypeError("cannot serialize")),
    )

    mail_cli.cmd_notify_self()

    assert advances == []


@pytest.mark.parametrize("failure_stage", ["write", "flush"])
def test_ac3_err_hook_output_failure_retries_then_acks_once(
    monkeypatch, failure_stage
) -> None:
    from fno import harness_identity
    from fno.bus import cursor as cursor_mod
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-output-retry",
        from_="alice",
        to="cl-abcd1234",
        kind="note",
        ts="2026-08-03T00:00:00Z",
        body="retry after output failure",
    )

    class _Ident:
        harness = "claude"
        session_id = "abcd1234"
        disposition = "single"

    class _FailingStdout(_CountingStdout):
        def write(self, s: str) -> int:
            if failure_stage == "write":
                raise OSError("write failed")
            return super().write(s)

        def flush(self) -> None:
            if failure_stage == "flush":
                raise OSError("flush failed")
            super().flush()

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity", lambda: _Ident()
    )
    monkeypatch.setattr(
        harness_identity, "canonical_handle", lambda sid: "cl-abcd1234"
    )
    monkeypatch.setattr(
        cursor_mod, "scan_unread", lambda handle: [msg] if handle == "cl-abcd1234" else []
    )
    monkeypatch.setattr(mail_cli, "_sent_unclaimed", lambda handle, ttl: [])
    advances: list[tuple[str, str]] = []
    monkeypatch.setattr(
        cursor_mod,
        "advance_cursor",
        lambda handle, msg_id: advances.append((handle, msg_id)),
    )

    with monkeypatch.context() as output_patch:
        output_patch.setattr("sys.stdout", _FailingStdout())
        mail_cli.cmd_notify_self()

    assert advances == []

    retry_out = _CountingStdout()
    with monkeypatch.context() as output_patch:
        output_patch.setattr("sys.stdout", retry_out)
        mail_cli.cmd_notify_self()

    assert advances == [("cl-abcd1234", msg.id)]
    assert msg.body in "".join(retry_out.delivered)


def test_ac2_hp_hook_json_is_flushed_before_cursor_advances(monkeypatch) -> None:
    import json

    from fno import harness_identity
    from fno.bus import cursor as cursor_mod
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-hook",
        from_="alice",
        to="cl-abcd1234",
        kind="note",
        ts="2026-08-03T00:00:00Z",
        body="the active-turn payload",
    )

    class _Ident:
        harness = "claude"
        session_id = "abcd1234"
        disposition = "single"

    fake_out = _CountingStdout()
    observed: dict[str, object] = {}
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity", lambda: _Ident()
    )
    monkeypatch.setattr(
        harness_identity, "canonical_handle", lambda sid: "cl-abcd1234"
    )
    monkeypatch.setattr(
        cursor_mod, "scan_unread", lambda handle: [msg] if handle == "cl-abcd1234" else []
    )
    monkeypatch.setattr(mail_cli, "_sent_unclaimed", lambda handle, ttl: [])

    def _advance(handle, msg_id):
        observed["handle"] = handle
        observed["msg_id"] = msg_id
        observed["delivered_at_ack"] = "".join(fake_out.delivered)
        observed["still_buffered_at_ack"] = list(fake_out.buffer)
        return True

    monkeypatch.setattr(cursor_mod, "advance_cursor", _advance)
    monkeypatch.setattr("sys.stdout", fake_out)

    mail_cli.cmd_notify_self()

    payload = json.loads(observed["delivered_at_ack"])
    context = payload["hookSpecificOutput"]["additionalContext"]
    assert msg.body in context
    assert observed["still_buffered_at_ack"] == []
    assert observed["handle"] == "cl-abcd1234"
    assert observed["msg_id"] == msg.id

"""`fno mail drain-self` must flush the bodies before it advances the cursor.

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

    monkeypatch.setattr(
        harness_identity, "resolve_harness_identity", lambda: _Ident()
    )
    monkeypatch.setattr(
        harness_identity, "canonical_handle", lambda sid: "cl-abcd1234"
    )
    monkeypatch.setattr(cursor_mod, "scan_unread", lambda handle: [msg])

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


def test_the_flush_is_what_delivers_it(_drained) -> None:
    """Positive control: the assertion above is only meaningful because the fake
    stdout genuinely withholds writes until flush()."""
    _observed, fake_out, _msg = _drained
    assert fake_out.flushes >= 1, "drain-self never flushed stdout at all"

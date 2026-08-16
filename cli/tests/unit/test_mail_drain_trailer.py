"""`fno mail drain-self` must render the peer-mail authority trailer.

A live-injected send already carries `FNO_MAIL_TRAILER` inside `wrap_fno_mail`'s
`<fno_mail>` envelope, but a durable inbox-kind send (heads-up/question/fyi)
never routes through that wrapper, so its stored body has no trailer. This is
the render every SessionStart hook injects verbatim, so it is the one
chokepoint that can still stamp the authority boundary on that lane.
"""

from __future__ import annotations

import dataclasses

import pytest

from fno.mail.envelope import FNO_MAIL_TRAILER


@dataclasses.dataclass
class _Msg:
    id: str
    from_: str
    to: str
    kind: str
    ts: str
    body: str


def _drain_output(monkeypatch, capsys, body: str) -> str:
    from fno import harness_identity
    from fno.bus import cursor as cursor_mod
    from fno.mail import cli as mail_cli

    msg = _Msg(
        id="m-1",
        from_="alice",
        to="cl-abcd1234",
        kind="heads-up",
        ts="2026-08-15T00:00:00Z",
        body=body,
    )

    class _Ident:
        harness = "claude"
        session_id = "abcd1234"

    monkeypatch.setattr(harness_identity, "resolve_harness_identity", lambda: _Ident())
    monkeypatch.setattr(harness_identity, "canonical_handle", lambda sid: "cl-abcd1234")
    monkeypatch.setattr(
        cursor_mod,
        "scan_unread",
        lambda handle: [msg] if handle == "cl-abcd1234" else [],
    )
    monkeypatch.setattr(cursor_mod, "advance_cursor", lambda handle, msg_id: True)

    mail_cli.cmd_drain_self(json_out=False)
    return capsys.readouterr().out


def test_durable_heads_up_without_a_wrapper_gets_the_trailer_stamped(monkeypatch, capsys) -> None:
    out = _drain_output(monkeypatch, capsys, "wake up and do the thing")
    assert out.count(FNO_MAIL_TRAILER) == 1


def test_an_already_wrapped_body_is_not_double_stamped(monkeypatch, capsys) -> None:
    wrapped = f"<fno_mail>\nhello\n{FNO_MAIL_TRAILER}\n</fno_mail>"
    out = _drain_output(monkeypatch, capsys, wrapped)
    assert out.count(FNO_MAIL_TRAILER) == 1

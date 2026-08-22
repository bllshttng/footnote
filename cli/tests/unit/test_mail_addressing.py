"""Addressing: full sender provenance, and the codex short-address refusal.

Node x-3a64, absorbing x-05ae. `from` in an envelope is a compact DISPLAY
handle: the first eight characters of the session id. Under UUIDv4 that is 32
random bits and safe. Under UUIDv7 the first 48 bits are a truncated millisecond
timestamp, so head-8 is a ~65.536-second clock bucket and every codex session
started inside one shares it. Measured: three landed on `01a025f8` in a night,
and `mail reply` then refused as ambiguous with no disambiguator and no route
back to the thread.

Kept in one module rather than split across the three legacy mail-test files,
because these four behaviors are one contract and a reader chasing "how do I
address a codex worker" should find the answer in one place.
"""
from __future__ import annotations

import pytest

from fno.harness_identity import (
    CODEX_SHORT_ADDRESS_RULE,
    canonical_handle,
    is_unsafe_short_address,
)
from fno.mail.envelope import fno_mail_open, wrap_fno_mail
from fno.mail.reply_resolve import sender_from_transcript_text

# Two real-shaped UUIDv7 ids from one clock bucket: identical head-8, distinct
# tails. This is the collision, written down.
V7_A = "01a025f8-09af-74b2-a461-998a18e3bfca"
V7_B = "01a025f8-eff1-7133-ad3c-e21b2febdae1"


def test_two_v7_siblings_collide_on_the_display_handle():
    """The premise, asserted rather than assumed: if this ever stops holding,
    every test below is testing a problem that no longer exists."""
    assert canonical_handle(V7_A) == canonical_handle(V7_B)
    assert V7_A != V7_B


def test_the_envelope_carries_the_full_session_when_given_one():
    tag = fno_mail_open(
        from_="01a025f8", harness="codex", model="m", from_session=V7_A
    )
    assert f'from_session="{V7_A}"' in tag
    assert tag.startswith('<fno_mail from="01a025f8"')


def test_an_envelope_without_it_is_byte_unchanged():
    """Additive, and rendered last, so every pre-existing producer emits exactly
    the bytes it emitted before."""
    assert fno_mail_open(from_="a", harness="claude-code", model="m") == (
        '<fno_mail from="a" harness="claude-code" model="m">'
    )


def test_the_reply_resolver_prefers_the_full_session_over_the_handle():
    """A transcript-recovered reply must target the id that cannot collide."""
    text = wrap_fno_mail(
        "hi",
        from_="01a025f8",
        harness="codex",
        model="m",
        id="msg-882e18",
        from_session=V7_A,
    )
    assert sender_from_transcript_text(text, "msg-882e18") == V7_A


def test_a_legacy_envelope_still_resolves_through_its_handle():
    """An envelope written before the attribute existed is still sitting in live
    transcripts. Preferring the full id must not break reading the old shape."""
    text = wrap_fno_mail("hi", from_="01a025f8", harness="codex", model="m", id="msg-1")
    assert sender_from_transcript_text(text, "msg-1") == "01a025f8"


@pytest.mark.parametrize(
    "token,harness,unsafe",
    [
        ("01a025f8", "codex", True),
        ("01a025f8", "claude", False),  # v4 head-8 is 32 random bits
        (V7_A, "codex", False),  # a full id is the answer, never the problem
        ("blueprint-x-ce6e", "codex", False),  # a registry name is not a slice
        ("", "codex", False),
    ],
)
def test_unsafe_short_address_keys_on_shape_and_harness(token, harness, unsafe):
    assert is_unsafe_short_address(token, harness) is unsafe


def test_the_refusal_names_what_to_use_instead():
    """A refusal that does not name the working address just moves the wall."""
    assert "full session_id" in CODEX_SHORT_ADDRESS_RULE
    assert "pane" in CODEX_SHORT_ADDRESS_RULE


def test_a_name_lane_row_replies_to_the_full_session(tmp_path, monkeypatch):
    """The regression this half exists for, exercised end to end.

    `_name_lane_send` stamps `to_kind="name"` on every record it writes, and it
    is the lane that carries `from_session`. A reply that consulted the full id
    only for `to_kind == "session"` therefore never read the value it had just
    been taught to store, and the ambiguous-handle refusal still fired.
    """
    from typer.testing import CliRunner

    from fno.bus.log import Envelope, append
    from fno.mail.cli import mail_app
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    append(
        Envelope.new(
            id="msg-882e18",
            from_=canonical_handle(V7_A),
            to="me",
            kind="send",
            body="hi",
            to_kind="name",
            from_session=V7_A,
        )
    )

    seen: dict = {}
    monkeypatch.setattr(
        "fno.mail.cli._reply_to_name_handle",
        lambda *_a, **kw: seen.update(kw),
    )
    result = CliRunner().invoke(mail_app, ["reply", "ok", "--to", "msg-882e18"])

    assert result.exit_code == 0, result.output
    assert seen.get("target") == V7_A, seen


@pytest.mark.parametrize(
    "extra",
    [
        ["--raw"],
        ["--to-project", "footnote"],
        ["--kind", "heads-up"],
    ],
    ids=["raw", "to-project", "kind"],
)
def test_the_short_address_refusal_covers_the_lanes_that_return_early(
    tmp_path, monkeypatch, extra
):
    """An address rule that only covers the lanes reached last is not a rule.

    The refusal sat below the `--raw`, `--to-project`, `--kind` and job-address
    returns, so `send <codex-head-8> '/verb' --raw` still fired a verb at
    whichever colliding session discovery happened to list. `--raw` is the worst
    of them: it runs a verb rather than leaving a message to be read.
    """
    from types import SimpleNamespace

    from typer.testing import CliRunner

    import fno.agents.registry as registry
    from fno.mail.cli import mail_app
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    entry = SimpleNamespace(
        name="codex-worker", harness="codex",
        harness_session_id=V7_A, session_id=V7_A,
        mux={"session": "main", "pane_id": 3}, cwd="/w", status="live",
    )
    monkeypatch.setattr(registry, "load_registry", lambda *_a, **_k: [entry])

    result = CliRunner().invoke(mail_app, ["send", V7_A[:8], "/fno:review", *extra])

    assert result.exit_code == 2, result.output
    assert "addresses codex" in (result.stderr or result.output)

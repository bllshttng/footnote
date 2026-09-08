"""x-6346 - `--to-king <scope>` resolves the crown at SEND time.

Succession moves the crown row; it does not move the mail handle a peer learned
while that handle was crowned. Resolving the ROLE off the registry at send time
is what removes the stale handle, and a forwarding pointer written at abdication
would only be the same recorded identity one hop over.

Covers AC1 (one live holder resolves to it), AC2 (no live holder refuses and
queues nothing; a successor resolves to the successor), the split-crown refusal,
and the delivery-time recipient stamp that AC3 asks for.
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def env(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "agents"))
    return tmp_path


def _register(name, *, scope=None, level=None, status="live", session=None):
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    try:
        existing = list(load_registry())
    except Exception:
        existing = []
    existing.append(
        AgentEntry(
            name=name,
            harness="claude",
            cwd="/tmp",
            log_path=f"/tmp/{name}.log",
            short_id=f"id-{name}",
            status=status,
            harness_session_id=session or f"session-{name}",
            crown_level=level,
            crown_scope=scope,
        )
    )
    write_registry(existing)


# ---------------------------------------------------------------------------
# Resolver
# ---------------------------------------------------------------------------

def test_ac1_one_live_holder_resolves_to_it(env):
    from fno.agents.crown import resolve_to_king

    _register("king-fno", scope="fno", level=1)
    _register("worker")

    assert resolve_to_king("fno") == ["king-fno"]


def test_ac2_abdicated_holder_does_not_answer_for_the_crown(env):
    """The node's own repro. A cleared crown on a still-live row is what
    `king done` leaves behind, and a terminal row is what it leaves when the
    session ends too. Neither may answer for the crown."""
    from fno.agents.crown import resolve_to_king

    _register("former-king")
    _register("gone-king", scope="fno", level=1, status="exited")

    assert resolve_to_king("fno") == []


def test_ac2_successor_resolves_not_the_abdicated_handle(env):
    from fno.agents.crown import resolve_to_king

    _register("former-king")
    _register("king-fno-g5", scope="fno", level=1)

    assert resolve_to_king("fno") == ["king-fno-g5"]


def test_split_crown_returns_both_and_never_picks_one(env):
    from fno.agents.crown import resolve_to_king

    _register("king-a", scope="fno", level=1)
    _register("king-b", scope="fno", level=1)

    assert resolve_to_king("fno") == ["king-a", "king-b"]


def test_a_different_scope_does_not_answer(env):
    from fno.agents.crown import resolve_to_king

    _register("king-other", scope="x-119e", level=2)

    assert resolve_to_king("fno") == []


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def test_cli_to_king_refuses_when_the_crown_is_vacant(env):
    from fno.mail.cli import mail_app

    _register("former-king")
    res = CliRunner().invoke(mail_app, ["send", "--to-king", "fno", "ping"])

    assert res.exit_code == 16
    assert "no live row holds this crown" in res.output


def test_cli_to_king_refuses_a_split_crown_naming_both(env):
    from fno.mail.cli import mail_app

    _register("king-a", scope="fno", level=1)
    _register("king-b", scope="fno", level=1)
    res = CliRunner().invoke(mail_app, ["send", "--to-king", "fno", "ping"])

    assert res.exit_code == 17
    assert "king-a" in res.output and "king-b" in res.output


def test_cli_to_king_refuses_a_second_addressing_mode(env):
    from fno.mail.cli import mail_app

    res = CliRunner().invoke(
        mail_app, ["send", "--to-king", "fno", "--to-project", "p", "ping"]
    )
    assert res.exit_code == 2
    assert "mutually exclusive" in res.output


def test_cli_to_king_hands_the_holder_to_the_name_lane(env, monkeypatch):
    """AC1: the resolved holder, not a handle a peer remembered, is what the
    ordinary name lane is asked to deliver to."""
    import fno.agents.dispatch as dispatch
    from fno.mail.cli import mail_app

    _register("king-fno-g5", scope="fno", level=1)
    seen: dict = {}

    def _fake_send(**kwargs):
        seen.update(kwargs)
        return dispatch.DispatchSendResult(msg_id="msg-1", delivery="hosted")

    monkeypatch.setattr(dispatch, "dispatch_send", _fake_send)
    res = CliRunner().invoke(mail_app, ["send", "--to-king", "fno", "ping"])

    assert res.exit_code == 0, res.output
    assert seen["name"] == "king-fno-g5"
    assert seen["message"] == "ping"


# ---------------------------------------------------------------------------
# The recipient-crown stamp is a DELIVERY reading, not a stored one
# ---------------------------------------------------------------------------

def test_durable_floor_carries_no_recipient_crown(env, tmp_path, monkeypatch):
    """A durable body is rendered now and read whenever the recipient next
    drains. A crown baked into it is a recorded identity that outlives its own
    reading, which is the defect the stamp exists to close - so the durable copy
    carries none while the live envelope carries one."""
    import fno.agents.dispatch as dispatch
    import fno.mail.envelope as envelope
    from fno.harness_identity import canonical_handle
    from fno.inbox.store import read_all_threads

    _register("king-fno", scope="fno", level=1, session="session-king")
    envelope.fleet_has_crown_at.cache_clear()
    envelope.crown_at.cache_clear()
    monkeypatch.setattr(dispatch, "_deliver_live", lambda *a, **k: False)
    monkeypatch.setattr(
        dispatch, "_registered_family1_state", lambda _entry: "working"
    )

    result = dispatch.dispatch_send(
        name="king-fno", message="ping", provider=None, cwd=tmp_path
    )
    assert result.delivery == "durable"

    threads = read_all_threads(canonical_handle("session-king"))
    body = threads[0].messages[0].body
    assert "your crown" not in body

    # Positive control on the same fleet and the same recipient: the live
    # envelope DOES stamp it, so the absence above is the rule and not a
    # crownless fleet or an unresolvable row.
    assert "-- your crown: L1 fno" in envelope.wrap_fno_mail(
        "ping",
        from_="peer",
        harness="codex",
        model="m",
        to_session="session-king",
    )

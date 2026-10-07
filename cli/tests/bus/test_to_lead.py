"""x-6346 - `--to-lead <scope>` resolves the role at SEND time.

Succession moves the role row; it does not move the mail handle a peer learned
while that handle was promoted. Resolving the ROLE off the registry at send time
is what removes the stale handle, and a forwarding pointer written at departure
would only be the same recorded identity one hop over.

Covers AC1 (one live holder resolves to it), AC2 (no live holder refuses and
queues nothing; a successor resolves to the successor), the split-role refusal,
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
            role_level=level,
            role_scope=scope,
        )
    )
    write_registry(existing)


# ---------------------------------------------------------------------------
# Resolver
# ---------------------------------------------------------------------------

def test_ac1_one_live_holder_resolves_to_it(env):
    from fno.agents.role import resolve_to_lead

    _register("lead-fno", scope="fno", level=1)
    _register("worker")

    assert resolve_to_lead("fno") == ["lead-fno"]


def test_ac2_stepped_down_holder_does_not_answer_for_the_role(env):
    """The node's own repro. A cleared role on a still-live row is what
    `lead done` leaves behind, and a terminal row is what it leaves when the
    session ends too. Neither may answer for the role."""
    from fno.agents.role import resolve_to_lead

    _register("former-lead")
    _register("gone-lead", scope="fno", level=1, status="exited")

    assert resolve_to_lead("fno") == []


def test_ac2_successor_resolves_not_the_stepped_down_handle(env):
    from fno.agents.role import resolve_to_lead

    _register("former-lead")
    _register("lead-fno-g5", scope="fno", level=1)

    assert resolve_to_lead("fno") == ["lead-fno-g5"]


def test_split_role_returns_both_and_never_picks_one(env):
    from fno.agents.role import resolve_to_lead

    _register("lead-a", scope="fno", level=1)
    _register("lead-b", scope="fno", level=1)

    assert resolve_to_lead("fno") == ["lead-a", "lead-b"]


def test_a_different_scope_does_not_answer(env):
    from fno.agents.role import resolve_to_lead

    _register("lead-other", scope="x-119e", level=2)

    assert resolve_to_lead("fno") == []


def test_a_set_role_answers_to_each_member_epic(env):
    from fno.agents.role import resolve_to_lead

    _register("lead-set", scope="x-119e,x-4d9b", level=2)

    assert resolve_to_lead("x-4d9b") == ["lead-set"]
    assert resolve_to_lead("x-119e") == ["lead-set"]
    assert resolve_to_lead("x-4d9b,x-119e") == ["lead-set"]
    assert resolve_to_lead("x-aaaa") == []
    assert resolve_to_lead("x-4d9b,x-aaaa") == []


def test_an_epic_lead_and_a_set_lead_over_it_both_answer(env):
    from fno.agents.role import resolve_to_lead

    _register("lead-set", scope="x-119e,x-4d9b", level=2)
    _register("lead-one", scope="x-4d9b", level=2)

    assert resolve_to_lead("x-4d9b") == ["lead-one", "lead-set"]


def test_a_portfolio_role_does_not_answer_for_one_of_its_projects(env, monkeypatch):
    """Projects go through the real resolver, never a stub."""
    from fno.agents.role import resolve_to_lead
    from fno.projects import resolve as proj_resolve

    cfg = env / "config.toml"
    cfg.write_text(
        '[work.workspaces.ws1]\n'
        'projects = [{ name = "alpha" }, { name = "beta" }]\n',
        encoding="utf-8",
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", cfg)
    proj_resolve._clear_cache()
    try:
        _register("lead-portfolio", scope="alpha,beta", level=0)
        _register("lead-alpha", scope="alpha", level=1)

        assert resolve_to_lead("alpha") == ["lead-alpha"]
        assert resolve_to_lead("alpha,beta") == ["lead-portfolio"]
    finally:
        proj_resolve._clear_cache()


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def test_cli_to_lead_resolves_one_member_of_a_set_role(env, monkeypatch):
    import fno.agents.dispatch as dispatch
    from fno.mail.cli import mail_app

    _register("lead-set", scope="x-119e,x-4d9b", level=2)
    monkeypatch.setattr(
        dispatch,
        "dispatch_send",
        lambda **_kw: dispatch.DispatchSendResult(msg_id="msg-1", delivery="hosted"),
    )
    res = CliRunner().invoke(mail_app, ["send", "--to-lead", "x-4d9b", "ping"])

    assert res.exit_code == 0, res.output
    assert "--to-lead x-4d9b: resolved to lead-set" in res.output


def test_cli_to_lead_refuses_when_the_role_is_vacant(env):
    from fno.mail.cli import mail_app

    _register("former-lead")
    res = CliRunner().invoke(mail_app, ["send", "--to-lead", "fno", "ping"])

    assert res.exit_code == 16
    assert "no live row holds this role" in res.output


def test_cli_to_lead_refuses_a_split_role_naming_both(env):
    from fno.mail.cli import mail_app

    _register("lead-a", scope="fno", level=1)
    _register("lead-b", scope="fno", level=1)
    res = CliRunner().invoke(mail_app, ["send", "--to-lead", "fno", "ping"])

    assert res.exit_code == 17
    assert "lead-a" in res.output and "lead-b" in res.output


def test_cli_to_lead_refuses_a_second_addressing_mode(env):
    from fno.mail.cli import mail_app

    res = CliRunner().invoke(
        mail_app, ["send", "--to-lead", "fno", "--to-project", "p", "ping"]
    )
    assert res.exit_code == 2
    assert "mutually exclusive" in res.output


def test_cli_to_lead_hands_the_holder_to_the_name_lane(env, monkeypatch):
    """AC1: the resolved holder, not a handle a peer remembered, is what the
    ordinary name lane is asked to deliver to."""
    import fno.agents.dispatch as dispatch
    from fno.mail.cli import mail_app

    _register("lead-fno-g5", scope="fno", level=1)
    seen: dict = {}

    def _fake_send(**kwargs):
        seen.update(kwargs)
        return dispatch.DispatchSendResult(msg_id="msg-1", delivery="hosted")

    monkeypatch.setattr(dispatch, "dispatch_send", _fake_send)
    res = CliRunner().invoke(mail_app, ["send", "--to-lead", "fno", "ping"])

    assert res.exit_code == 0, res.output
    assert seen["name"] == "lead-fno-g5"
    assert seen["message"] == "ping"


# ---------------------------------------------------------------------------
# The recipient-role stamp is a DELIVERY reading, not a stored one
# ---------------------------------------------------------------------------

def test_durable_floor_carries_no_recipient_role(env, tmp_path, monkeypatch):
    """A durable body is rendered now and read whenever the recipient next
    drains. A role baked into it is a recorded identity that outlives its own
    reading, which is the defect the stamp exists to close - so the durable copy
    carries none while the live envelope carries one."""
    import fno.agents.dispatch as dispatch
    from fno.harness_identity import canonical_handle
    from fno.inbox.store import read_all_threads

    _register("lead-fno", scope="fno", level=1, session="session-lead")
    monkeypatch.setattr(dispatch, "_deliver_live", lambda *a, **k: False)
    monkeypatch.setattr(
        dispatch, "_registered_family1_state", lambda _entry: "working"
    )

    result = dispatch.dispatch_send(from_name="lead", 
        name="lead-fno", message="ping", provider=None, cwd=tmp_path
    )
    assert result.delivery == "durable"

    threads = read_all_threads(canonical_handle("session-lead"))
    body = threads[0].messages[0].body
    assert "to_rank" not in body

    # The delivered shape retired the wire attrs to_rank rode on, so the old
    # live-envelope positive control has no text left to assert on. The durable
    # copy is the plain delivered header either way.
    assert body.splitlines()[0].startswith("`@lead · fmail-"), body[:80]
    assert "ping" in body

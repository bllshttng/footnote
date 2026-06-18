"""Task 3.3 - --to-project anycast resolution (US6).

Project/cwd is demoted from address to resolver: registry cwd->project mapping
(plus the config.inbox.peers `project:` hint) resolves a destination project to
exactly one live peer, a durable queue, or an ambiguous-candidate error.

Covers AC6-HP (one live -> live delivery, recipient recorded), AC6-ERR (two live
-> error + candidate list, deliver to none), AC6-UI (zero live -> queued durable
with msg-id), AC6-FR (malformed peers hint -> degrade to registry mapping).
"""
from __future__ import annotations

import json

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def env(tmp_path, monkeypatch):
    """Isolate state (registry) and co-isolate the inbox/bus under tmp."""
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_INBOX_ROOT", str(tmp_path / "agents"))
    return tmp_path


def _project_cwd(tmp_path, project: str):
    """Create a cwd whose .fno/settings.yaml resolves to `project`."""
    d = tmp_path / project
    (d / ".fno").mkdir(parents=True, exist_ok=True)
    (d / ".fno" / "settings.yaml").write_text(
        f"project: {project}\n", encoding="utf-8"
    )
    return d


def _register(name, project_cwd, *, status="live", last_message_at=None):
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    existing = []
    try:
        existing = list(load_registry())
    except Exception:
        existing = []
    existing.append(
        AgentEntry(
            name=name,
            provider="claude",
            cwd=str(project_cwd),
            log_path=f"/tmp/{name}.log",
            claude_short_id=f"id-{name}",
            status=status,
            last_message_at=last_message_at,
        )
    )
    write_registry(existing)


# ---------------------------------------------------------------------------
# Resolver - the core one/none/many logic
# ---------------------------------------------------------------------------

def test_ac6_hp_one_live_peer_resolves_to_it(env, tmp_path):
    from fno.agents.dispatch import resolve_to_project

    cwd = _project_cwd(tmp_path, "projA")
    _register("alpha", cwd, status="live")

    res = resolve_to_project("projA")
    assert res.recipient == "alpha"
    assert res.durable is False
    assert res.ambiguous is False


def test_ac6_ui_zero_live_resolves_durable(env, tmp_path):
    from fno.agents.dispatch import resolve_to_project

    cwd = _project_cwd(tmp_path, "projA")
    _register("alpha", cwd, status="orphaned")  # registered but not live

    res = resolve_to_project("projA")
    assert res.recipient is None
    assert res.durable is True
    assert res.ambiguous is False


def test_ac6_err_two_live_is_ambiguous_without_any(env, tmp_path):
    from fno.agents.dispatch import resolve_to_project

    cwd = _project_cwd(tmp_path, "projA")
    _register("alpha", cwd, status="live")
    _register("bravo", cwd, status="live")

    res = resolve_to_project("projA")
    assert res.recipient is None
    assert res.ambiguous is True
    assert res.live_candidates == ["alpha", "bravo"]


def test_any_tiebreak_picks_most_recent_then_lexicographic(env, tmp_path):
    from fno.agents.dispatch import resolve_to_project

    cwd = _project_cwd(tmp_path, "projA")
    _register("alpha", cwd, status="live", last_message_at="2026-06-07T10:00:00Z")
    _register("bravo", cwd, status="live", last_message_at="2026-06-07T12:00:00Z")

    res = resolve_to_project("projA", any_=True)
    assert res.recipient == "bravo"  # most recent last_message_at wins
    assert res.ambiguous is False


def test_any_tiebreak_lexicographic_on_equal_ts(env, tmp_path):
    from fno.agents.dispatch import resolve_to_project

    cwd = _project_cwd(tmp_path, "projA")
    _register("zulu", cwd, status="live", last_message_at="2026-06-07T12:00:00Z")
    _register("alpha", cwd, status="live", last_message_at="2026-06-07T12:00:00Z")

    res = resolve_to_project("projA", any_=True)
    assert res.recipient == "alpha"  # equal ts -> lexicographic name


def test_ac6_fr_malformed_peers_hint_degrades_to_registry(env, tmp_path, monkeypatch):
    # A malformed config.inbox.peers (a string, not a mapping) must not crash
    # resolution; it degrades to the registry cwd mapping alone.
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.write_text(
        "schema_version: 1\n"
        f"config:\n  state_dir: {tmp_path}/.fno/\n"
        "  inbox:\n    peers: not-a-mapping\n",
        encoding="utf-8",
    )
    from fno import config as _cfg
    _cfg.load_settings.cache_clear()
    import fno.paths as _paths
    _paths._settings.cache_clear()

    cwd = _project_cwd(tmp_path, "projA")
    _register("alpha", cwd, status="live")

    from fno.agents.dispatch import resolve_to_project
    res = resolve_to_project("projA")
    assert res.recipient == "alpha"  # registry mapping still works


def test_peers_project_hint_associates_peer_without_matching_cwd(env, tmp_path, monkeypatch):
    # The config.inbox.peers.<name>.project hint adds an association even when
    # the peer's cwd does not resolve to the project.
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.write_text(
        "schema_version: 1\n"
        f"config:\n  state_dir: {tmp_path}/.fno/\n"
        "  inbox:\n    peers:\n      alpha:\n        project: projA\n",
        encoding="utf-8",
    )
    from fno import config as _cfg
    _cfg.load_settings.cache_clear()
    import fno.paths as _paths
    _paths._settings.cache_clear()
    # The peers hint is read from cwd (the running agent's project); run there.
    monkeypatch.chdir(tmp_path)

    # alpha's cwd resolves to something else, but the hint says it serves projA.
    other = _project_cwd(tmp_path, "elsewhere")
    _register("alpha", other, status="live")

    from fno.agents.dispatch import resolve_to_project
    res = resolve_to_project("projA")
    assert res.recipient == "alpha"


def test_peers_hint_does_not_hide_peer_from_its_cwd_project(env, tmp_path, monkeypatch):
    # codex P2: a hint must ADD an association, never replace the cwd mapping.
    # alpha's cwd resolves to "elsewhere" and a hint also serves "projA"; alpha
    # must remain a candidate for BOTH, not be hidden from "elsewhere".
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.write_text(
        "schema_version: 1\n"
        f"config:\n  state_dir: {tmp_path}/.fno/\n"
        "  inbox:\n    peers:\n      alpha:\n        project: projA\n",
        encoding="utf-8",
    )
    from fno import config as _cfg
    _cfg.load_settings.cache_clear()
    import fno.paths as _paths
    _paths._settings.cache_clear()
    monkeypatch.chdir(tmp_path)

    other = _project_cwd(tmp_path, "elsewhere")
    _register("alpha", other, status="live")

    from fno.agents.dispatch import resolve_to_project
    # Hinted project still resolves...
    assert resolve_to_project("projA").recipient == "alpha"
    # ...and the actual cwd project is NOT hidden by the hint.
    assert resolve_to_project("elsewhere").recipient == "alpha"


# ---------------------------------------------------------------------------
# dispatch_send_to_project - the delivery wiring
# ---------------------------------------------------------------------------

def test_ac6_hp_dispatch_records_resolved_recipient(env, tmp_path, monkeypatch):
    from fno.agents import dispatch as dmod

    cwd = _project_cwd(tmp_path, "projA")
    _register("alpha", cwd, status="live")

    # Stub the by-name send so we exercise the resolver wiring, not the provider.
    def fake_send(*, name, message, provider, cwd, lock_timeout, from_name):
        return dmod.DispatchSendResult(msg_id="msg-deadbe", delivery="hosted")

    monkeypatch.setattr(dmod, "dispatch_send", fake_send)

    result = dmod.dispatch_send_to_project(
        "projA", "hello there", cwd=tmp_path, from_name="fno"
    )
    assert result.delivery == "hosted"
    assert result.recipient == "alpha"
    assert result.to_project == "projA"


def test_ac6_ui_dispatch_durable_when_no_live(env, tmp_path):
    from fno.agents.dispatch import dispatch_send_to_project
    from fno.bus.log import iter_messages

    _project_cwd(tmp_path, "projA")  # project exists, no live peer registered

    result = dispatch_send_to_project(
        "projA", "queued message body", cwd=tmp_path, from_name="fno"
    )
    assert result.delivery == "durable"
    assert result.to_project == "projA"
    # The durable envelope is addressed to the project and lands on the bus.
    msgs = [m for m in iter_messages() if m.to == "projA"]
    assert len(msgs) == 1
    assert msgs[0].body == "queued message body"


def test_ac6_err_dispatch_raises_on_ambiguous(env, tmp_path):
    from fno.agents.dispatch import dispatch_send_to_project, DispatchAskError

    cwd = _project_cwd(tmp_path, "projA")
    _register("alpha", cwd, status="live")
    _register("bravo", cwd, status="live")

    with pytest.raises(DispatchAskError) as ei:
        dispatch_send_to_project("projA", "msg", cwd=tmp_path, from_name="fno")
    assert ei.value.exit_code == 17
    assert "alpha" in str(ei.value) and "bravo" in str(ei.value)


# ---------------------------------------------------------------------------
# CLI surface
# ---------------------------------------------------------------------------

@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def test_cli_send_to_project_durable_stdout(env, tmp_path, runner):
    from fno.mail.cli import mail_app

    _project_cwd(tmp_path, "projA")
    res = runner.invoke(
        mail_app, ["send", "--to-project", "projA", "hi there project"]
    )
    assert res.exit_code == 0, res.output
    out = res.stdout.strip()
    assert out.startswith("msg-")
    assert "queued (durable) for project projA" in out


def test_cli_send_to_project_demoted_peer_reports_peer_not_project(env, tmp_path, runner, monkeypatch):
    # One live peer resolved, but injection demotes to durable: the envelope is
    # addressed to the peer, so the line must say "for <peer>", not "for project".
    from fno.agents import dispatch as dmod
    from fno.mail.cli import mail_app

    cwd = _project_cwd(tmp_path, "projA")
    _register("alpha", cwd, status="live")

    def fake_send(*, name, message, provider, cwd, lock_timeout, from_name):
        return dmod.DispatchSendResult(msg_id="msg-dem01", delivery="durable")

    monkeypatch.setattr(dmod, "dispatch_send", fake_send)

    res = runner.invoke(mail_app, ["send", "--to-project", "projA", "hi"])
    assert res.exit_code == 0, res.output
    out = res.stdout.strip()
    assert "queued (durable) for alpha" in out
    assert "for project projA" not in out  # would be the misleading mismatch


def test_cli_send_to_project_ambiguous_errors(env, tmp_path, runner):
    from fno.mail.cli import mail_app

    cwd = _project_cwd(tmp_path, "projA")
    _register("alpha", cwd, status="live")
    _register("bravo", cwd, status="live")

    res = runner.invoke(
        mail_app, ["send", "--to-project", "projA", "hi"]
    )
    assert res.exit_code == 17, res.output
    assert "alpha" in (res.stdout + (res.stderr or ""))


# ---------------------------------------------------------------------------
# settings reader
# ---------------------------------------------------------------------------

def test_project_resolution_rejects_illegal_state():
    from fno.agents.dispatch import ProjectResolution

    # recipient AND ambiguous is incoherent - exactly one outcome must hold.
    with pytest.raises(ValueError):
        ProjectResolution(
            recipient="alice", live_candidates=["alice", "bob"],
            durable=False, ambiguous=True,
        )
    # zero outcomes is also illegal.
    with pytest.raises(ValueError):
        ProjectResolution(
            recipient=None, live_candidates=[], durable=False, ambiguous=False,
        )


def test_read_peer_projects_reads_hint(tmp_path):
    from fno.inbox.settings import read_peer_projects

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "settings.yaml").write_text(
        "config:\n  inbox:\n    peers:\n"
        "      foo:\n        project: projX\n"
        "      bar:\n        surfaces: [a, b]\n",  # no project key -> dropped
        encoding="utf-8",
    )
    out = read_peer_projects(tmp_path)
    assert out == {"foo": "projX"}


def test_read_peer_projects_malformed_returns_empty(tmp_path):
    from fno.inbox.settings import read_peer_projects

    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "settings.yaml").write_text(
        "config:\n  inbox:\n    peers: not-a-mapping\n", encoding="utf-8"
    )
    assert read_peer_projects(tmp_path) == {}

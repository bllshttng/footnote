"""Tests for Group 3 / Task 4.1: `/agents chat A B "seed"` live escalation.

`chat` is a thin Python orchestrator over the shipped stream-json switchboard
substrate (epic ab-d3a1ae3e). It adopts BOTH peers onto the stream-json lane
under FRESH host names (the daemon refuses adopting under a name already in the
registry, and claude has no fresh stream host, so the resume UUID is required),
then drives the seed B<-A and the bounded A2A relay synchronously so the
terminal state is reportable.

Covers AC3-HP (both adopted under fresh hosts, reply mirrored, relay to ceiling)
and AC3-UI (always shows the exact command + the plan-credit caveat before the
gate).
"""
from __future__ import annotations

import pytest
from typer.testing import CliRunner

from fno.paths_testing import use_tmpdir


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _register_claude_peer(name: str, *, short_id: str, uuid: str | None) -> None:
    from fno.agents.registry import AgentEntry, load_registry, write_registry

    try:
        existing = list(load_registry())
    except Exception:
        existing = []
    existing.append(
        AgentEntry(
            name=name,
            provider="claude",
            cwd="/tmp",
            log_path=f"/tmp/{name}.log",
            claude_short_id=short_id,
            claude_session_uuid=uuid,
            status="live",
        )
    )
    write_registry(existing)


class _Daemon:
    """Faithful-enough stub of the daemon over `_daemon_rpc`.

    `agent.spawn` REJECTS a name already in the registry (models the daemon's
    `AgentExists` guard) so a test cannot pass — as the real daemon would refuse
    — if chat wrongly adopts under a peer's existing name. A fresh name registers
    and returns a live stream-lane receipt. `agent.switchboard` answers from a
    scripted reply queue; `agent.stop` confirms.
    """

    def __init__(self, registered, *, switchboard_replies=None):
        self.calls: list[tuple[str, dict]] = []
        self.registered = set(registered)
        self._replies = list(switchboard_replies or [])

    def __call__(self, method, params, **kwargs):
        self.calls.append((method, params))
        if method == "agent.spawn":
            name = params.get("name")
            if name in self.registered:  # AgentExists
                return None
            self.registered.add(name)
            return {"short_id": name[:8], "provider": "claude", "status": "live", "lane": "stream"}
        if method == "agent.switchboard":
            return self._replies.pop(0) if self._replies else {"delivered": False, "reason": "exhausted"}
        if method == "agent.stop":
            return {"stopped": True}
        return None

    def spawn_calls(self):
        return [p for m, p in self.calls if m == "agent.spawn"]

    def switchboard_calls(self):
        return [p for m, p in self.calls if m == "agent.switchboard"]


def _wire_two_live_peers(monkeypatch, tmp_path):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))
    _register_claude_peer(
        "agent-a", short_id="aaaa1111", uuid="11111111-1111-4111-8111-111111111111"
    )
    _register_claude_peer(
        "agent-b", short_id="bbbb2222", uuid="22222222-2222-4222-8222-222222222222"
    )
    from fno.agents.providers import claude as claude_mod

    monkeypatch.setattr(claude_mod, "session_is_live", lambda short_id: False)


# ---------------------------------------------------------------------------
# AC3-HP — both adopted under fresh hosts, reply mirrored, relay to the ceiling
# ---------------------------------------------------------------------------

def test_chat_adopts_both_under_fresh_hosts_and_relays_to_ceiling(monkeypatch, tmp_path):
    from fno.agents import dispatch as dispatch_mod

    _wire_two_live_peers(monkeypatch, tmp_path)
    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (True, 3))
    daemon = _Daemon(
        registered={"agent-a", "agent-b"},  # the peers already exist
        switchboard_replies=[
            {"delivered": True, "reply": "B-says-1", "is_error": False},
            {"delivered": True, "reply": "A-says-2", "is_error": False},
            {"delivered": True, "reply": "B-says-3", "is_error": False},
        ],
    )
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat(
        "agent-a", "agent-b", "compare approaches", cwd=tmp_path
    )

    assert res.status == "ok", res
    # Adopted under FRESH host names, never the peers' own (existing) names.
    spawn = daemon.spawn_calls()
    adopted_names = {p["name"] for p in spawn}
    assert adopted_names == {"agent-a-chat", "agent-b-chat"}
    assert "agent-a" not in adopted_names and "agent-b" not in adopted_names
    for p in spawn:
        assert p["host_mode"] == "interactive"
        assert p["provider"] == "claude"
        assert p["resume_id"]  # the full UUID, never the short-id
    # result.adopted carries the host (watch) names.
    assert set(res.adopted) == {"agent-a-chat", "agent-b-chat"}
    # First hop drives B's host from A's host with the seed.
    sb = daemon.switchboard_calls()
    assert sb[0]["to"] == "agent-b-chat"
    assert sb[0]["from"] == "agent-a-chat"
    assert sb[0]["body"] == "compare approaches"
    # Relay alternated up to the ceiling (3 total turns).
    assert res.turns == 3
    assert res.ceiling == 3


def test_chat_observed_mode_single_hop_no_relay(monkeypatch, tmp_path):
    """config.agents.a2a.auto=False -> a single mirrored hop, no autonomous relay."""
    from fno.agents import dispatch as dispatch_mod

    _wire_two_live_peers(monkeypatch, tmp_path)
    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (False, 6))
    daemon = _Daemon(
        registered={"agent-a", "agent-b"},
        switchboard_replies=[{"delivered": True, "reply": "B-reply", "is_error": False}],
    )
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat("agent-a", "agent-b", "hi", cwd=tmp_path)

    assert res.status == "ok", res
    assert res.turns == 1
    assert daemon.switchboard_calls()[0]["mirror"] is True  # observed mirrors into A
    assert len(daemon.switchboard_calls()) == 1


# ---------------------------------------------------------------------------
# AC3-UI — chat ALWAYS shows the exact command + the plan-credit caveat
# ---------------------------------------------------------------------------

def test_chat_cli_always_shows_command_and_plan_credit_caveat(
    monkeypatch, tmp_path, runner
):
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.cli import agents_app

    _wire_two_live_peers(monkeypatch, tmp_path)
    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (False, 6))
    daemon = _Daemon(
        registered={"agent-a", "agent-b"},
        switchboard_replies=[{"delivered": True, "reply": "ok", "is_error": False}],
    )
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    # --yes is the auto-skip path; the caveat MUST still print (AC3-UI).
    result = runner.invoke(
        agents_app, ["chat", "agent-a", "agent-b", "seed text", "--yes"]
    )
    assert result.exit_code == 0, result.output
    out = result.output
    assert "agent-a" in out and "agent-b" in out
    assert "fno agents chat" in out  # the exact command is echoed
    assert "plan credit" in out.lower()  # the billed-launch caveat

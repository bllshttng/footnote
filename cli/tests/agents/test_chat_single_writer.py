"""Tests for Group 3 / Task 4.2: chat single-writer refusal, UUID-unresolved
refusal, --resume-child-death unwind, and honest teardown reporting.

Covers:
- AC3-ERR: a peer that is a live running --bg /target loop -> the single-writer
  guard refuses ("X is busy (running loop), cannot open a live channel") and
  adopts nothing.
- AC3-EDGE: a peer whose full resume UUID is unresolved CANNOT be live-escalated
  (claude has no fresh stream host, and we never adopt a guessed UUID) -> a
  visible failure with an actionable reason, never a silent/guessed thread; the
  turn ceiling ends the relay with a visible note.
- AC3-FR: the --resume adopt child dies -> chat fails with the specific reason
  and best-effort unwinds the host it already adopted; an unconfirmed unwind
  (daemon down) is reported as "may still be live", never asserted torn down.
"""
from __future__ import annotations

from fno.paths_testing import use_tmpdir


def _register_claude_peer(
    name: str, *, short_id: str, uuid: str | None, host_mode: str | None = None
) -> None:
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
            short_id=short_id,
            claude_session_uuid=uuid,
            status="live",
            host_mode=host_mode,
        )
    )
    write_registry(existing)


class _Daemon:
    """Faithful daemon stub. `agent.spawn` rejects an already-registered name
    (AgentExists) and any name in `spawn_dead` (the --resume child died); a fresh
    name registers + returns a live receipt. `agent.stop` confirms unless the
    host is in `stop_unconfirmed` (models a down daemon)."""

    def __init__(
        self, registered, *, switchboard_replies=None, spawn_dead=(), stop_unconfirmed=()
    ):
        self.calls: list[tuple[str, dict]] = []
        self.registered = set(registered)
        self._replies = list(switchboard_replies or [])
        self.spawn_dead = set(spawn_dead)
        self.stop_unconfirmed = set(stop_unconfirmed)

    def __call__(self, method, params, **kwargs):
        self.calls.append((method, params))
        if method == "agent.spawn":
            name = params.get("name")
            if name in self.registered or name in self.spawn_dead:
                return None
            self.registered.add(name)
            return {"short_id": name[:8], "provider": "claude", "status": "live", "lane": "stream"}
        if method == "agent.switchboard":
            return self._replies.pop(0) if self._replies else {"delivered": False, "reason": "exhausted"}
        if method == "agent.stop":
            if params.get("name") in self.stop_unconfirmed:
                return None
            return {"stopped": True}
        return None

    def spawn_calls(self):
        return [p for m, p in self.calls if m == "agent.spawn"]

    def stop_calls(self):
        return [p for m, p in self.calls if m == "agent.stop"]


def _base(monkeypatch, tmp_path):
    use_tmpdir(monkeypatch, tmp_path)
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path / "claims"))


# ---------------------------------------------------------------------------
# AC3-ERR — a busy running loop is refused; nothing is adopted
# ---------------------------------------------------------------------------

def test_chat_refuses_busy_running_loop_and_adopts_nothing(monkeypatch, tmp_path):
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    _base(monkeypatch, tmp_path)
    _register_claude_peer("agent-a", short_id="aaaa1111", uuid="11111111-1111-4111-8111-111111111111")
    _register_claude_peer("agent-b", short_id="bbbb2222", uuid="22222222-2222-4222-8222-222222222222")
    # A is a live running --bg loop (its supervisor socket is reachable).
    monkeypatch.setattr(
        claude_mod, "session_is_live", lambda short_id: short_id == "aaaa1111"
    )
    daemon = _Daemon(registered={"agent-a", "agent-b"})
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat("agent-a", "agent-b", "seed", cwd=tmp_path)

    assert res.status == "refused", res
    assert "agent-a" in res.reason and "busy" in res.reason.lower()
    assert "running loop" in res.reason.lower()
    assert res.adopted == []
    # The "nothing is adopted" guarantee: no adopt RPC was ever issued.
    assert daemon.spawn_calls() == []


# ---------------------------------------------------------------------------
# AC3-EDGE — an unresolved full UUID cannot be live-escalated (visible refusal)
# ---------------------------------------------------------------------------

def test_chat_unresolved_uuid_fails_visibly_and_adopts_nothing(monkeypatch, tmp_path):
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    _base(monkeypatch, tmp_path)
    # agent-a (the FIRST peer adopted) has NO resolved full UUID.
    _register_claude_peer("agent-a", short_id="aaaa1111", uuid=None)
    _register_claude_peer("agent-b", short_id="bbbb2222", uuid="22222222-2222-4222-8222-222222222222")
    monkeypatch.setattr(claude_mod, "session_is_live", lambda short_id: False)
    daemon = _Daemon(registered={"agent-a", "agent-b"})
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat("agent-a", "agent-b", "seed", cwd=tmp_path)

    assert res.status == "failed", res
    low = res.reason.lower()
    assert "agent-a" in res.reason and "uuid" in low
    # Actionable: never guesses a UUID; points at re-spawn or the async bus.
    assert "send" in low or "re-spawn" in low or "respawn" in low
    # Nothing was adopted (we refuse before issuing any adopt RPC for the null peer).
    assert daemon.spawn_calls() == []
    assert res.adopted == []


def test_chat_ceiling_reached_emits_visible_note(monkeypatch, tmp_path):
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    _base(monkeypatch, tmp_path)
    _register_claude_peer("agent-a", short_id="aaaa1111", uuid="11111111-1111-4111-8111-111111111111")
    _register_claude_peer("agent-b", short_id="bbbb2222", uuid="22222222-2222-4222-8222-222222222222")
    monkeypatch.setattr(claude_mod, "session_is_live", lambda short_id: False)
    monkeypatch.setattr(dispatch_mod, "_load_a2a_settings", lambda: (True, 2))
    daemon = _Daemon(
        registered={"agent-a", "agent-b"},
        switchboard_replies=[
            {"delivered": True, "reply": "r1", "is_error": False},
            {"delivered": True, "reply": "r2", "is_error": False},
        ],
    )
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat("agent-a", "agent-b", "seed", cwd=tmp_path)

    assert res.status == "ok", res
    assert res.turns == 2 and res.ceiling == 2
    assert any("ceiling" in n.lower() for n in res.notes), res.notes


# ---------------------------------------------------------------------------
# AC3-FR — the --resume adopt child dies on startup
# ---------------------------------------------------------------------------

def test_chat_resume_child_death_reports_and_unwinds(monkeypatch, tmp_path):
    """B's --resume adopt child dies -> chat fails with the specific reason and
    unwinds the host (agent-a-chat) it already adopted (no stranded half-chat)."""
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    _base(monkeypatch, tmp_path)
    _register_claude_peer("agent-a", short_id="aaaa1111", uuid="11111111-1111-4111-8111-111111111111")
    _register_claude_peer("agent-b", short_id="bbbb2222", uuid="22222222-2222-4222-8222-222222222222")
    monkeypatch.setattr(claude_mod, "session_is_live", lambda short_id: False)
    # agent-b's host adopt returns None (the --resume child died on startup).
    daemon = _Daemon(registered={"agent-a", "agent-b"}, spawn_dead={"agent-b-chat"})
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat("agent-a", "agent-b", "seed", cwd=tmp_path)

    assert res.status == "failed", res
    assert res.reason and "agent-b" in res.reason
    # The already-adopted host A was torn down (a confirmed stop), so it is no
    # longer reported as live.
    assert {p["name"] for p in daemon.stop_calls()} == {"agent-a-chat"}
    assert res.adopted == []
    assert any("unwound" in n.lower() and "agent-a-chat" in n for n in res.notes)


def test_chat_unwind_unconfirmed_warns_may_still_be_live(monkeypatch, tmp_path):
    """When the daemon cannot confirm the teardown stop, chat must NOT claim the
    host was unwound — it warns the host may still be a live billed channel
    (honesty invariant on the teardown side)."""
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    _base(monkeypatch, tmp_path)
    _register_claude_peer("agent-a", short_id="aaaa1111", uuid="11111111-1111-4111-8111-111111111111")
    _register_claude_peer("agent-b", short_id="bbbb2222", uuid="22222222-2222-4222-8222-222222222222")
    monkeypatch.setattr(claude_mod, "session_is_live", lambda short_id: False)
    daemon = _Daemon(
        registered={"agent-a", "agent-b"},
        spawn_dead={"agent-b-chat"},
        stop_unconfirmed={"agent-a-chat"},  # the stop RPC no-ops (daemon down)
    )
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat("agent-a", "agent-b", "seed", cwd=tmp_path)

    assert res.status == "failed", res
    joined = " ".join(res.notes).lower()
    assert "may still be a live" in joined and "agent-a-chat" in " ".join(res.notes)
    # The unconfirmed host stays in adopted (honestly still-possibly-live).
    assert "agent-a-chat" in res.adopted


# ---------------------------------------------------------------------------
# self-chat is refused (gemini review): A and B must differ
# ---------------------------------------------------------------------------

def test_chat_refuses_self_chat(monkeypatch, tmp_path):
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    _base(monkeypatch, tmp_path)
    _register_claude_peer("agent-a", short_id="aaaa1111", uuid="11111111-1111-4111-8111-111111111111")
    monkeypatch.setattr(claude_mod, "session_is_live", lambda short_id: False)
    daemon = _Daemon(registered={"agent-a"})
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat("agent-a", "agent-a", "seed", cwd=tmp_path)

    assert res.status == "failed", res
    assert "itself" in res.reason.lower()
    # No adopt, no switchboard with a null `to`.
    assert daemon.calls == []
    assert res.adopted == []


# ---------------------------------------------------------------------------
# a reused pre-existing channel is NOT torn down on a later abort (codex P2)
# ---------------------------------------------------------------------------

def test_chat_failed_setup_does_not_unwind_reused_host(monkeypatch, tmp_path):
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    _base(monkeypatch, tmp_path)
    a_uuid = "11111111-1111-4111-8111-111111111111"
    _register_claude_peer("agent-a", short_id="aaaa1111", uuid=a_uuid)
    _register_claude_peer("agent-b", short_id="bbbb2222", uuid="22222222-2222-4222-8222-222222222222")
    # agent-a already HAS a live channel from a prior chat -> agent-a-chat is reused.
    _register_claude_peer(
        "agent-a-chat", short_id="ac111111", uuid=a_uuid, host_mode="interactive"
    )
    monkeypatch.setattr(claude_mod, "session_is_live", lambda short_id: False)
    # agent-b's adopt fails; the abort must NOT stop the reused agent-a-chat.
    daemon = _Daemon(
        registered={"agent-a", "agent-b", "agent-a-chat"}, spawn_dead={"agent-b-chat"}
    )
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat("agent-a", "agent-b", "seed", cwd=tmp_path)

    assert res.status == "failed", res
    # The reused host was never torn down (this call did not create it).
    assert daemon.stop_calls() == []
    assert "agent-a-chat" in res.adopted
    assert any("reusing" in n.lower() for n in res.notes)


def test_chat_adopt_does_not_reuse_a_dead_host_row(monkeypatch, tmp_path):
    # A stale interactive host row with a matching uuid must NOT be reported as a
    # reusable live channel. Release-on-idle can leave a host row dead (or, when
    # it removes the row, absent) so the guard requires status=live; a dead row
    # falls through to a fresh spawn resuming the same uuid instead of handing
    # back a dead switchboard target (codex P2 on PR#237).
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.registry import AgentEntry

    _base(monkeypatch, tmp_path)
    a_uuid = "11111111-1111-4111-8111-111111111111"
    peer = AgentEntry(
        name="agent-a",
        provider="claude",
        cwd="/tmp",
        log_path="/tmp/agent-a.log",
        short_id="aaaa1111",
        claude_session_uuid=a_uuid,
        status="live",
    )
    dead_host = AgentEntry(
        name="agent-a-chat",
        provider="claude",
        cwd="/tmp",
        log_path="/tmp/agent-a-chat.log",
        short_id="ac111111",
        claude_session_uuid=a_uuid,
        status="exited",
        host_mode="interactive",
    )
    spawn_calls: list = []

    def _rpc(method, params, **kw):
        spawn_calls.append((method, params.get("name")))
        return {"ok": True}  # a fresh spawn succeeds

    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", _rpc)

    adopted, host, note, reused = dispatch_mod._chat_adopt(
        peer, tmp_path, existing_by_name={"agent-a-chat": dead_host}
    )

    assert reused is False, "a dead interactive host row must NOT be reused"
    assert ("agent.spawn", "agent-a-chat") in spawn_calls, (
        "a dead row must fall through to a fresh spawn resuming the same uuid"
    )
    assert adopted is True  # the fresh spawn succeeded


# ---------------------------------------------------------------------------
# unknown agent -> failed, nothing adopted
# ---------------------------------------------------------------------------

def test_chat_unknown_peer_fails(monkeypatch, tmp_path):
    from fno.agents import dispatch as dispatch_mod
    from fno.agents.providers import claude as claude_mod

    _base(monkeypatch, tmp_path)
    _register_claude_peer("agent-a", short_id="aaaa1111", uuid="11111111-1111-4111-8111-111111111111")
    monkeypatch.setattr(claude_mod, "session_is_live", lambda short_id: False)
    daemon = _Daemon(registered={"agent-a"})
    monkeypatch.setattr(dispatch_mod, "_daemon_rpc", daemon)

    res = dispatch_mod.dispatch_chat("agent-a", "ghost", "seed", cwd=tmp_path)

    assert res.status == "failed", res
    assert "ghost" in res.reason
    assert daemon.spawn_calls() == []

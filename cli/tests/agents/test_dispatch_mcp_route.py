"""Phase 5 (US6) — dispatch_ask MCP route-selection tests.

Covers the routing decision tree at the dispatcher boundary:

- AC1-HP: MCP-backed agent with reachable probe -> ask_followup_via_mcp.
- AC1-ERR: probe True but send raises MCPChannelSendError -> demote.
- AC1-UI: agent_followup_done event carries `backend` field.
- AC3-HP: probe raises ReachabilityProbeError -> demote, socket fallback.
- AC3-EDGE/CHANNEL_NOT_REGISTERED: probe returns False -> demote.
- Socket-only agent (mcp_channel_id=None) skips probe entirely.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def _seed_mcp_agent(name: str = "mcp-worker", *, mcp_channel_id: str = "abc12345") -> None:
    """Seed registry with one MCP-backed claude agent."""
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [
            AgentEntry(
                name=name,
                provider="claude",
                cwd="/tmp",
                log_path=f"/tmp/{name}.log",
                claude_short_id="abc12345",
                status="live",
                mcp_channel_id=mcp_channel_id,
            )
        ]
    )


def _seed_socket_only_agent(name: str = "socket-worker") -> None:
    """Seed registry with one socket-only (US2-style) claude agent."""
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [
            AgentEntry(
                name=name,
                provider="claude",
                cwd="/tmp",
                log_path=f"/tmp/{name}.log",
                claude_short_id="abc12345",
                status="live",
            )
        ]
    )


def _install_fake_provider_layer(
    monkeypatch,
    *,
    probe_result,
    mcp_send_result,
    socket_send_result,
    capture: dict,
):
    """Wire fake provider implementations.

    ``probe_result`` is either ``True`` / ``False`` / a callable that
    raises (used to simulate ReachabilityProbeError).
    ``mcp_send_result`` / ``socket_send_result`` are reply strings or
    callables that raise.
    """
    from fno.agents.providers import claude as claude_mod

    def fake_probe(channel_id, *, timeout=0.25):
        capture.setdefault("probes", []).append({"channel_id": channel_id, "timeout": timeout})
        if callable(probe_result):
            return probe_result()
        return probe_result

    def fake_mcp_send(*, claude_short_id, message, cwd, from_name, timeout,
                     poll_interval=0.5, jobs_dir=None, mcp_channel_id=None):
        capture.setdefault("mcp_sends", []).append(
            {
                "claude_short_id": claude_short_id,
                "message": message,
                "from_name": from_name,
                "mcp_channel_id": mcp_channel_id,
            }
        )
        if callable(mcp_send_result):
            return mcp_send_result()
        return mcp_send_result

    def fake_socket_send(*, claude_short_id, message, cwd, from_name, timeout,
                        poll_interval=0.5, jobs_dir=None):
        capture.setdefault("socket_sends", []).append(
            {
                "claude_short_id": claude_short_id,
                "message": message,
                "from_name": from_name,
            }
        )
        if callable(socket_send_result):
            return socket_send_result()
        return socket_send_result

    monkeypatch.setattr(claude_mod, "mcp_channel_reachable", fake_probe)
    monkeypatch.setattr(claude_mod, "ask_followup_via_mcp", fake_mcp_send)
    monkeypatch.setattr(claude_mod, "ask_followup", fake_socket_send)


def _events_for(name: str, *, paths_mod) -> list[dict]:
    """Read events.jsonl for entries about ``name``."""
    p = paths_mod.state_dir() / "events.jsonl"
    if not p.exists():
        return []
    out: list[dict] = []
    for line in p.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        if rec.get("name") == name:
            out.append(rec)
    return out


# ---------------------------------------------------------------------
# AC1-HP — MCP-backed follow-up succeeds end-to-end
# ---------------------------------------------------------------------


def test_ac1_hp_mcp_send_succeeds_when_probe_true(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _seed_mcp_agent()
    capture: dict = {}
    _install_fake_provider_layer(
        monkeypatch,
        probe_result=True,
        mcp_send_result="mcp reply",
        socket_send_result="socket reply (should NOT be invoked)",
        capture=capture,
    )

    from fno import paths
    from fno.agents.dispatch import dispatch_ask

    result = dispatch_ask(
        name="mcp-worker",
        message="hi via mcp",
        provider="claude",
        cwd=tmp_path,
        timeout=30,
    )
    assert result.reply == "mcp reply"
    # MCP send invoked; socket send NOT invoked.
    assert len(capture["mcp_sends"]) == 1
    assert "socket_sends" not in capture
    # AC1-UI: backend stamp on done event.
    done_events = [
        e for e in _events_for("mcp-worker", paths_mod=paths)
        if e.get("kind") == "agent_followup_done"
    ]
    assert len(done_events) == 1
    assert done_events[0]["backend"] == "mcp"


# ---------------------------------------------------------------------
# AC1-ERR — probe True but MCP send fails -> socket fallback
# ---------------------------------------------------------------------


def test_ac1_err_mcp_send_failure_demotes_to_socket(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _seed_mcp_agent()

    from fno.agents.providers import claude as claude_mod

    def raise_send_failed():
        raise claude_mod.MCPChannelSendError("channel_write_failed")

    capture: dict = {}
    _install_fake_provider_layer(
        monkeypatch,
        probe_result=True,
        mcp_send_result=raise_send_failed,
        socket_send_result="socket fallback reply",
        capture=capture,
    )

    from fno import paths
    from fno.agents.dispatch import dispatch_ask

    result = dispatch_ask(
        name="mcp-worker",
        message="hi",
        provider="claude",
        cwd=tmp_path,
        timeout=30,
    )
    assert result.reply == "socket fallback reply"
    assert len(capture["mcp_sends"]) == 1
    assert len(capture["socket_sends"]) == 1

    # mcp_channel_demoted_to_socket event with send_failed_post_probe.
    demote = [
        e for e in _events_for("mcp-worker", paths_mod=paths)
        if e.get("kind") == "mcp_channel_demoted_to_socket"
    ]
    assert len(demote) == 1
    assert demote[0]["reason"].startswith("send_failed_post_probe:")

    # Final done event carries socket_after_mcp_demote backend.
    done_events = [
        e for e in _events_for("mcp-worker", paths_mod=paths)
        if e.get("kind") == "agent_followup_done"
    ]
    assert done_events[0]["backend"] == "socket_after_mcp_demote"


# ---------------------------------------------------------------------
# AC3-HP — probe inconclusive -> demote with mcp_channel_disconnected
# ---------------------------------------------------------------------


def test_ac3_hp_probe_raises_demotes_to_socket(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _seed_mcp_agent()

    from fno.agents.providers.base import ReachabilityProbeError

    def raise_probe():
        raise ReachabilityProbeError(
            provider="claude", reason="mcp_channel_disconnected"
        )

    capture: dict = {}
    _install_fake_provider_layer(
        monkeypatch,
        probe_result=raise_probe,
        mcp_send_result="should-not-run",
        socket_send_result="socket reply via fallback",
        capture=capture,
    )

    from fno import paths
    from fno.agents.dispatch import dispatch_ask

    result = dispatch_ask(
        name="mcp-worker",
        message="hi",
        provider="claude",
        cwd=tmp_path,
        timeout=30,
    )
    assert result.reply == "socket reply via fallback"
    # Probe attempted, MCP send NOT attempted.
    assert len(capture["probes"]) == 1
    assert "mcp_sends" not in capture
    assert len(capture["socket_sends"]) == 1

    # Spec routing decision tree §4d: probe-raise -> mcp_channel_unreachable.
    unreachable = [
        e for e in _events_for("mcp-worker", paths_mod=paths)
        if e.get("kind") == "mcp_channel_unreachable"
    ]
    assert len(unreachable) == 1
    assert unreachable[0]["reason"] == "mcp_channel_disconnected"
    # And NOT the demoted_to_socket kind (which is §4c, probe-False).
    demoted = [
        e for e in _events_for("mcp-worker", paths_mod=paths)
        if e.get("kind") == "mcp_channel_demoted_to_socket"
    ]
    assert demoted == []


# ---------------------------------------------------------------------
# AC3-CHANNEL_NOT_REGISTERED — probe returns False -> demote
# ---------------------------------------------------------------------


def test_probe_returns_false_demotes_with_not_registered(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _seed_mcp_agent()

    capture: dict = {}
    _install_fake_provider_layer(
        monkeypatch,
        probe_result=False,
        mcp_send_result="should-not-run",
        socket_send_result="socket reply",
        capture=capture,
    )

    from fno import paths
    from fno.agents.dispatch import dispatch_ask

    result = dispatch_ask(
        name="mcp-worker",
        message="hi",
        provider="claude",
        cwd=tmp_path,
        timeout=30,
    )
    assert result.reply == "socket reply"
    demote = [
        e for e in _events_for("mcp-worker", paths_mod=paths)
        if e.get("kind") == "mcp_channel_demoted_to_socket"
    ]
    assert len(demote) == 1
    assert demote[0]["reason"] == "channel_not_registered"


# ---------------------------------------------------------------------
# Socket-only agent skips MCP probe entirely
# ---------------------------------------------------------------------


def test_socket_only_agent_never_probes_mcp(tmp_path: Path, monkeypatch) -> None:
    use_tmpdir(monkeypatch, tmp_path)
    _seed_socket_only_agent()

    capture: dict = {}
    _install_fake_provider_layer(
        monkeypatch,
        probe_result=True,  # would route to MCP if probe ran
        mcp_send_result="mcp reply",
        socket_send_result="socket reply",
        capture=capture,
    )

    from fno import paths
    from fno.agents.dispatch import dispatch_ask

    result = dispatch_ask(
        name="socket-worker",
        message="hi",
        provider="claude",
        cwd=tmp_path,
        timeout=30,
    )
    assert result.reply == "socket reply"
    # No probe should have run because the registry entry has no
    # mcp_channel_id.
    assert "probes" not in capture
    assert "mcp_sends" not in capture
    assert len(capture["socket_sends"]) == 1

    done_events = [
        e for e in _events_for("socket-worker", paths_mod=paths)
        if e.get("kind") == "agent_followup_done"
    ]
    assert done_events[0]["backend"] == "socket"

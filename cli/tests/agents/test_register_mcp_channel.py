"""Tests for ``register_mcp_channel`` write verb (Wave 1.3) and the
MCP-aware reconcile slot (Wave 3.1)."""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.paths_testing import use_tmpdir


def _seed_claude_agent(name: str = "claude-bot", *, short_id: str = "abc12345") -> None:
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [
            AgentEntry(
                name=name,
                provider="claude",
                cwd="/tmp",
                log_path=f"/tmp/{name}.log",
                short_id=short_id,
                status="live",
            )
        ]
    )


def _seed_codex_agent(name: str = "codex-bot") -> None:
    from fno.agents.registry import AgentEntry, write_registry

    write_registry(
        [
            AgentEntry(
                name=name,
                provider="codex",
                cwd="/tmp",
                log_path=f"/tmp/{name}.log",
                codex_session_id="codex-sess-1",
                status="live",
            )
        ]
    )


class TestRegisterMCPChannel:
    def test_assigns_mcp_channel_id_to_existing_agent(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        use_tmpdir(monkeypatch, tmp_path)
        _seed_claude_agent()

        from fno.agents.dispatch import register_mcp_channel
        from fno.agents.registry import load_registry

        result = register_mcp_channel("claude-bot")
        assert result == "abc12345"  # equals short_id (1:1 today)

        loaded = load_registry()
        target = next(e for e in loaded if e.name == "claude-bot")
        assert target.mcp_channel_id == "abc12345"

    def test_idempotent_returns_existing_id(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        use_tmpdir(monkeypatch, tmp_path)
        _seed_claude_agent()

        from fno.agents.dispatch import register_mcp_channel

        first = register_mcp_channel("claude-bot")
        second = register_mcp_channel("claude-bot")
        assert first == second == "abc12345"

    def test_emits_mcp_channel_registered_event(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        use_tmpdir(monkeypatch, tmp_path)
        _seed_claude_agent()

        from fno import paths
        from fno.agents.dispatch import register_mcp_channel

        register_mcp_channel("claude-bot")

        events_text = (paths.state_dir() / "events.jsonl").read_text("utf-8")
        records = [json.loads(l) for l in events_text.splitlines() if l.strip()]
        registered = [r for r in records if r["kind"] == "mcp_channel_registered"]
        assert len(registered) == 1
        assert registered[0]["name"] == "claude-bot"
        assert registered[0]["mcp_channel_id"] == "abc12345"
        assert registered[0]["idempotent"] is False

    def test_idempotent_emit_flag_true_on_second_call(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        use_tmpdir(monkeypatch, tmp_path)
        _seed_claude_agent()

        from fno import paths
        from fno.agents.dispatch import register_mcp_channel

        register_mcp_channel("claude-bot")
        register_mcp_channel("claude-bot")

        events_text = (paths.state_dir() / "events.jsonl").read_text("utf-8")
        records = [json.loads(l) for l in events_text.splitlines() if l.strip()]
        registered = [r for r in records if r["kind"] == "mcp_channel_registered"]
        assert len(registered) == 2
        assert registered[0]["idempotent"] is False
        assert registered[1]["idempotent"] is True

    def test_rejects_non_claude_provider(self, tmp_path: Path, monkeypatch) -> None:
        use_tmpdir(monkeypatch, tmp_path)
        _seed_codex_agent()

        from fno.agents.dispatch import DispatchAskError, register_mcp_channel

        with pytest.raises(DispatchAskError) as exc:
            register_mcp_channel("codex-bot")
        assert "Claude-only" in str(exc.value)

    def test_rejects_missing_agent(self, tmp_path: Path, monkeypatch) -> None:
        use_tmpdir(monkeypatch, tmp_path)
        # No agents seeded.

        from fno.agents.dispatch import DispatchAskError, register_mcp_channel

        with pytest.raises(DispatchAskError):
            register_mcp_channel("never-existed")


class TestReconcileMCPProbeSlot:
    """AC5-HP / AC5-UI / AC5-EDGE — mcp-backed agents probe via the
    sidecar instead of `claude logs`."""

    def test_mcp_backed_claude_agent_probes_via_mcp(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        use_tmpdir(monkeypatch, tmp_path)
        from fno.agents.registry import AgentEntry, write_registry

        write_registry(
            [
                AgentEntry(
                    name="mcp-bot",
                    provider="claude",
                    cwd="/tmp",
                    log_path="/tmp/mcp-bot.log",
                    short_id="ch-xyz12345",
                    status="orphaned",  # start orphaned to detect flip
                    mcp_channel_id="ch-xyz12345",
                )
            ]
        )

        from fno.agents import dispatch
        from fno.agents.providers import claude as claude_mod

        # Track which probe got called.
        probe_calls: dict[str, list] = {"mcp": [], "logs": []}

        def fake_mcp_probe(channel_id, *, timeout=0.25):
            probe_calls["mcp"].append({"channel_id": channel_id, "timeout": timeout})
            return True

        def fake_logs_probe(short_id, *, timeout=10.0):
            probe_calls["logs"].append({"short_id": short_id, "timeout": timeout})
            return True

        from fno.agents.providers import codex as codex_mod

        monkeypatch.setattr(claude_mod, "mcp_channel_reachable", fake_mcp_probe)
        monkeypatch.setattr(claude_mod, "claude_logs_reachable", fake_logs_probe)
        # No codex entries in this test, but session_index_exists is
        # called early in reconcile_agents; stub to "fresh install".
        monkeypatch.setattr(codex_mod, "session_index_exists", lambda: False)
        # Pretend claude is on PATH so the reconcile reaches the probe.
        import shutil

        monkeypatch.setattr(
            shutil,
            "which",
            lambda b: "/bin/" + b if b == "claude" else None,
        )

        result = dispatch.reconcile_agents()
        # MCP probe ran; logs probe DID NOT.
        assert len(probe_calls["mcp"]) == 1
        assert probe_calls["mcp"][0]["channel_id"] == "ch-xyz12345"
        assert len(probe_calls["logs"]) == 0
        # Status flipped from orphaned -> live.
        assert {c["name"] for c in result.recovered} == {"mcp-bot"}

    def test_socket_only_claude_agent_still_uses_logs_probe(
        self, tmp_path: Path, monkeypatch
    ) -> None:
        use_tmpdir(monkeypatch, tmp_path)
        _seed_claude_agent()

        from fno.agents import dispatch
        from fno.agents.providers import claude as claude_mod

        probe_calls: dict[str, list] = {"mcp": [], "logs": []}

        def fake_mcp_probe(channel_id, *, timeout=0.25):
            probe_calls["mcp"].append({"channel_id": channel_id})
            return True

        def fake_logs_probe(short_id, *, timeout=10.0):
            probe_calls["logs"].append({"short_id": short_id})
            return True

        from fno.agents.providers import codex as codex_mod

        monkeypatch.setattr(claude_mod, "mcp_channel_reachable", fake_mcp_probe)
        monkeypatch.setattr(claude_mod, "claude_logs_reachable", fake_logs_probe)
        monkeypatch.setattr(codex_mod, "session_index_exists", lambda: False)
        import shutil

        monkeypatch.setattr(
            shutil,
            "which",
            lambda b: "/bin/" + b if b == "claude" else None,
        )

        dispatch.reconcile_agents()
        # Socket-only entry MUST take the logs path; the MCP probe is
        # never invoked.
        assert len(probe_calls["mcp"]) == 0
        assert len(probe_calls["logs"]) == 1

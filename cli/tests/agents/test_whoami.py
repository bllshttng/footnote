"""Tests for `fno agents whoami` — a mesh worker learns its own registered name.

Backlog node x-301a. Covers the pure-logic resolver (`fno.agents.whoami`),
the CLI wiring (`cmd_whoami` via the agents app), and the read-only invariant
(paired-state md5, mirroring `fno whoami` / `fno status`). The `cli/tests/agents`
conftest auto-forces `FNO_AGENTS_RUNTIME=python` so `runner.invoke` stays on the
Python dispatch instead of exec-ing an installed binary.
"""
from __future__ import annotations

import hashlib
import json

import pytest
from typer.testing import CliRunner

from fno.agents.cli import agents_app
from fno.agents.registry import AgentEntry, write_registry
from fno.agents import whoami as whoami_mod
from fno.paths_testing import use_tmpdir


def _claude(**kw) -> AgentEntry:
    base = dict(
        name="spawn-x-301a-whoami",
        provider="claude",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/spawn-x-301a-whoami/output.jsonl",
        claude_short_id="4a1f9c2b",
        created_at="2026-06-16T17:00:00Z",
        status="live",
    )
    base.update(kw)
    return AgentEntry(**base)


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


# --- pure-logic resolver -------------------------------------------------


class TestResolveSelf:
    def test_env_tier_resolves_name_and_enriches(self):
        reg = [_claude()]
        result = whoami_mod.resolve_self(
            env={"FNO_AGENT_SELF": "spawn-x-301a-whoami", "FNO_AGENT_PROVIDER": "claude"},
            registry=reg,
        )
        assert result.registered is True
        assert result.name == "spawn-x-301a-whoami"
        assert result.provider == "claude"
        assert result.short_id == "4a1f9c2b"
        assert result.status == "live"
        assert result.resolved_via == "env"
        assert result.exit_code == 0

    def test_empty_env_self_treated_as_unset(self):
        # Boundaries: FNO_AGENT_SELF="" reads as unset, not a zero-length name.
        result = whoami_mod.resolve_self(env={"FNO_AGENT_SELF": "   "}, registry=[])
        assert result.registered is False
        assert result.exit_code == whoami_mod.EXIT_NOT_REGISTERED

    def test_env_tier_without_registry_row_still_resolves(self):
        # Name in env but no matching row: tier 1 wins, enrichment is null.
        result = whoami_mod.resolve_self(
            env={"FNO_AGENT_SELF": "ghost-worker"}, registry=[]
        )
        assert result.registered is True
        assert result.name == "ghost-worker"
        assert result.provider is None
        assert result.short_id is None
        assert result.exit_code == 0

    def test_session_fallback_matches_by_uuid(self):
        reg = [_claude(claude_session_uuid="11111111-2222-3333-4444-555555555555")]
        result = whoami_mod.resolve_self(
            env={},  # no FNO_AGENT_SELF
            registry=reg,
            session_uuid="11111111-2222-3333-4444-555555555555",
        )
        assert result.registered is True
        assert result.name == "spawn-x-301a-whoami"
        assert result.resolved_via == "session-fallback"
        assert result.exit_code == 0

    def test_session_fallback_matches_short_id_prefix_of_uuid(self):
        # codex P2: an older claude row may carry ONLY the 8-hex short id (a
        # 32-bit prefix of the full session UUID). The full CLAUDE_CODE_SESSION_ID
        # must still resolve it on the fallback path.
        reg = [_claude(claude_short_id="3410f056", claude_session_uuid=None)]
        result = whoami_mod.resolve_self(
            env={},
            registry=reg,
            session_uuid="3410f056-d832-480c-9b55-09d1842a39b1",
        )
        assert result.registered is True
        assert result.name == "spawn-x-301a-whoami"
        assert result.resolved_via == "session-fallback"

    def test_session_fallback_prefers_exact_full_id_over_prefix(self):
        # The exact full-id pass runs across every row first, so a shared 8-hex
        # prefix never steals a real full-UUID match.
        prefix_row = _claude(name="prefix-collision", claude_short_id="3410f056",
                             claude_session_uuid=None)
        exact_row = _claude(name="exact-match", claude_short_id="ffffffff",
                            claude_session_uuid="3410f056-d832-480c-9b55-09d1842a39b1")
        result = whoami_mod.resolve_self(
            env={},
            registry=[prefix_row, exact_row],
            session_uuid="3410f056-d832-480c-9b55-09d1842a39b1",
        )
        assert result.name == "exact-match"

    def test_no_identity_returns_exit_3(self):
        result = whoami_mod.resolve_self(env={}, registry=[], session_uuid="no-match")
        assert result.registered is False
        assert result.resolved_via is None
        assert result.exit_code == whoami_mod.EXIT_NOT_REGISTERED

    def test_corrupt_registry_still_answers_from_env(self):
        # Errors: registry unreadable but FNO_AGENT_SELF set -> resolve + WARN, exit 0.
        result = whoami_mod.resolve_self(
            env={"FNO_AGENT_SELF": "spawn-x-301a-whoami"},
            registry=[],
            registry_error="registry at /x is malformed JSON",
        )
        assert result.registered is True
        assert result.name == "spawn-x-301a-whoami"
        assert result.exit_code == 0
        assert any("registry unreadable" in w for w in result.warnings)

    def test_live_status_enricher_failure_is_swallowed(self):
        def _boom(_short_id):
            raise RuntimeError("claude shellout failed")

        result = whoami_mod.resolve_self(
            env={"FNO_AGENT_SELF": "spawn-x-301a-whoami", "FNO_AGENT_PROVIDER": "claude"},
            registry=[_claude()],
            live_status_fn=_boom,
        )
        assert result.registered is True
        assert result.live_status is None
        assert result.exit_code == 0  # enrichment failure never changes exit
        assert any("live_status enrichment skipped" in w for w in result.warnings)


# --- CLI wiring ----------------------------------------------------------


class TestWhoamiCLI:
    def test_ac1_hp_registered_worker_learns_name(self, tmp_path, runner, monkeypatch):
        use_tmpdir(monkeypatch, tmp_path)
        write_registry([_claude()])
        monkeypatch.setenv("FNO_AGENT_SELF", "spawn-x-301a-whoami")
        monkeypatch.setenv("FNO_AGENT_PROVIDER", "claude")
        result = runner.invoke(agents_app, ["whoami"])
        assert result.exit_code == 0, result.stdout + result.stderr
        # JSON is emitted under CliRunner (non-TTY); assert on the parsed shape.
        payload = json.loads(result.stdout)
        assert payload["name"] == "spawn-x-301a-whoami"
        assert payload["provider"] == "claude"
        assert payload["short_id"] == "4a1f9c2b"

    def test_ac1_err_corrupt_registry_no_traceback(self, tmp_path, runner, monkeypatch):
        use_tmpdir(monkeypatch, tmp_path)
        from fno import paths

        reg_path = paths.agents_registry_path()
        reg_path.parent.mkdir(parents=True, exist_ok=True)
        reg_path.write_text("{ this is not valid json", encoding="utf-8")
        monkeypatch.setenv("FNO_AGENT_SELF", "spawn-x-301a-whoami")
        result = runner.invoke(agents_app, ["whoami"])
        assert result.exit_code == 0, result.stdout + result.stderr
        assert "spawn-x-301a-whoami" in result.stdout
        assert "Traceback" not in (result.stdout + result.stderr)
        assert "WARN" in result.stderr

    def test_ac1_ui_json_shape_complete(self, tmp_path, runner, monkeypatch):
        use_tmpdir(monkeypatch, tmp_path)
        write_registry([_claude()])
        monkeypatch.setenv("FNO_AGENT_SELF", "spawn-x-301a-whoami")
        result = runner.invoke(agents_app, ["whoami", "--json"])
        assert result.exit_code == 0, result.stdout + result.stderr
        payload = json.loads(result.stdout)
        for key in (
            "registered", "name", "provider", "session", "short_id",
            "status", "live_status", "node", "resolved_via",
        ):
            assert key in payload, f"missing key {key}"
        assert payload["registered"] is True
        assert payload["resolved_via"] == "env"

    def test_ac1_edge_no_identity_exit_3(self, tmp_path, runner, monkeypatch):
        use_tmpdir(monkeypatch, tmp_path)
        monkeypatch.delenv("FNO_AGENT_SELF", raising=False)
        monkeypatch.delenv("CLAUDE_CODE_SESSION_ID", raising=False)
        result = runner.invoke(agents_app, ["whoami", "--json"])
        assert result.exit_code == 3
        payload = json.loads(result.stdout)
        assert payload["registered"] is False

    def test_live_status_shellout_warning_surfaced(self, tmp_path, runner, monkeypatch):
        # codex finding 2: claude_agents_json returns ({}, [warns]) WITHOUT
        # raising on a shellout failure, so the warning must be forwarded to
        # stderr (not silently dropped) even though live_status degrades to null.
        use_tmpdir(monkeypatch, tmp_path)
        write_registry([_claude()])
        monkeypatch.setenv("FNO_AGENT_SELF", "spawn-x-301a-whoami")
        monkeypatch.setenv("FNO_AGENT_PROVIDER", "claude")
        from fno.agents.providers import claude as claude_mod

        monkeypatch.setattr(
            claude_mod, "claude_agents_json",
            lambda *a, **k: ({}, ["claude agents --json timed out"]),
        )
        result = runner.invoke(agents_app, ["whoami", "--json"])
        assert result.exit_code == 0, result.stdout + result.stderr
        assert json.loads(result.stdout)["live_status"] is None
        assert "claude agents --json timed out" in result.stderr

    def test_ac1_fr_read_only_paired_state_hash(self, tmp_path, runner, monkeypatch):
        use_tmpdir(monkeypatch, tmp_path)
        write_registry([_claude()])
        monkeypatch.setenv("FNO_AGENT_SELF", "spawn-x-301a-whoami")
        from fno import paths

        reg_path = paths.agents_registry_path()

        def _hash() -> str:
            return hashlib.md5(reg_path.read_bytes()).hexdigest()

        before = _hash()
        for _ in range(3):
            res = runner.invoke(agents_app, ["whoami"])
            assert res.exit_code == 0, res.stdout + res.stderr
        assert _hash() == before, "whoami must not rewrite the registry"


# --- find_held_node ------------------------------------------------------


SID = "3410f056-d832-480c-9b55-09d1842a39b1"


def _write_manifest(tmp_path, body: str, *, transcript: str = SID):
    fno = tmp_path / ".fno"
    fno.mkdir()
    (fno / "target-state.md").write_text(
        f"---\nclaude_transcript_id: {transcript}\n---\n{body}\n", encoding="utf-8"
    )


class TestFindHeldNode:
    def test_reads_node_when_transcript_id_matches_session(self, tmp_path):
        _write_manifest(tmp_path, "graph_node_id: x-301a")
        assert whoami_mod.find_held_node(str(tmp_path), session_uuid=SID) == "node:x-301a"

    def test_transcript_mismatch_returns_none(self, tmp_path):
        # codex finding 1: the manifest belongs to a DIFFERENT session — never
        # attribute its node to this worker.
        _write_manifest(tmp_path, "graph_node_id: x-999a", transcript="other-session-uuid")
        assert whoami_mod.find_held_node(str(tmp_path), session_uuid=SID) is None

    def test_no_session_uuid_returns_none(self, tmp_path):
        # A codex/gemini worker has no CLAUDE_CODE_SESSION_ID -> never guess.
        _write_manifest(tmp_path, "graph_node_id: x-301a")
        assert whoami_mod.find_held_node(str(tmp_path), session_uuid=None) is None

    def test_null_sentinel_returns_none(self, tmp_path):
        _write_manifest(tmp_path, "graph_node_id: null")
        assert whoami_mod.find_held_node(str(tmp_path), session_uuid=SID) is None

    def test_matched_quote_pair_stripped(self, tmp_path):
        _write_manifest(tmp_path, 'graph_node_id: "x-301a"')
        assert whoami_mod.find_held_node(str(tmp_path), session_uuid=SID) == "node:x-301a"

    def test_unbalanced_quote_not_mangled(self, tmp_path):
        # A lone leading quote is a matched-pair miss, so the value is preserved.
        _write_manifest(tmp_path, 'graph_node_id: "x-301a')
        assert whoami_mod.find_held_node(str(tmp_path), session_uuid=SID) == 'node:"x-301a'

    def test_missing_file_returns_none(self, tmp_path):
        assert whoami_mod.find_held_node(str(tmp_path), session_uuid=SID) is None

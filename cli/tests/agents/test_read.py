"""Tests for fno.agents.read — list_agents() core logic.

Covers AC1-HP (populated table), AC1-EDGE (empty), AC1-UI (--json forces JSON),
AC3-HP (cross-provider shape), AC3-EDGE (filter intersection).

The claude_agents_json() shellout is mocked via monkeypatch of the
``fno.agents.providers.claude.claude_agents_json`` function symbol.
Live shellout is exercised in providers test + integration smoke.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.agents.read import ListResult, list_agents
from fno.agents.registry import AgentEntry, write_registry
from fno.paths_testing import use_tmpdir


def _claude(**kw) -> AgentEntry:
    base = dict(
        name="worker-frontend",
        provider="claude",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-frontend/output.jsonl",
        claude_short_id="abc12345",
        created_at="2026-05-20T17:00:00Z",
        status="live",
        last_message_at="2026-05-20T17:30:12Z",
    )
    base.update(kw)
    return AgentEntry(**base)


def _codex(**kw) -> AgentEntry:
    base = dict(
        name="worker-migration",
        provider="codex",
        cwd="/Users/foo/code/proj",
        log_path="/Users/foo/.fno/agents/worker-migration/output.jsonl",
        codex_session_id="codex-sess-xyz",
        created_at="2026-05-20T17:15:00Z",
        status="live",
        last_message_at="2026-05-20T17:15:43Z",
    )
    base.update(kw)
    return AgentEntry(**base)


@pytest.fixture
def _patch_claude_agents_json(monkeypatch):
    """Return a function that installs a fake claude_agents_json result."""
    def _install(result, warnings=None):
        from fno.agents.providers import claude as claude_mod

        def _fake(timeout=3.0):  # noqa: ARG001
            return result, list(warnings or [])

        monkeypatch.setattr(claude_mod, "claude_agents_json", _fake)
    return _install


def test_list_agents_populated_table(tmp_path, monkeypatch, _patch_claude_agents_json):
    """AC1-HP — three entries render with their respective LIVE columns."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="alpha"),
            _codex(name="bravo"),
            _claude(name="charlie", status="orphaned"),
        ]
    )
    _patch_claude_agents_json(
        {"abc12345": {"live_status": "Working"}}
    )

    result = list_agents(json_out=False, tty=True)

    assert result.exit_code == 0
    assert "alpha" in result.output
    assert "bravo" in result.output
    assert "charlie" in result.output
    assert "Working" in result.output  # claude live entry's LIVE column
    assert result.warnings == []


def test_list_agents_empty_registry_json_shape(tmp_path, monkeypatch, _patch_claude_agents_json):
    """AC1-EDGE — empty registry returns valid empty shape."""
    use_tmpdir(monkeypatch, tmp_path)
    _patch_claude_agents_json({})  # noqa: F841

    result = list_agents(json_out=True, tty=True)

    parsed = json.loads(result.output)
    assert parsed == {
        "agents": [],
        "count": 0,
        "discovered_sessions": [],
        "discovered_count": 0,
        "filters_applied": {"cwd": None, "provider": None, "status": None},
        "schema_version": 2,
    }
    assert result.exit_code == 0


def test_list_agents_json_flag_forces_json_in_tty(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC1-UI — --json forces JSON regardless of TTY."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])
    _patch_claude_agents_json({"abc12345": {"live_status": "Idle"}})

    result = list_agents(json_out=True, tty=True)

    parsed = json.loads(result.output)
    assert parsed["count"] == 1
    assert parsed["agents"][0]["name"] == "alpha"
    assert parsed["agents"][0]["live_status"] == "Idle"


def test_list_agents_non_tty_default_emits_json(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """Locked Decision 4 — non-TTY stdout defaults to JSON."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="alpha")])
    _patch_claude_agents_json({"abc12345": {"live_status": "Idle"}})

    result = list_agents(json_out=False, tty=False)

    # JSON renderer output begins with `{`.
    assert result.output.lstrip().startswith("{")
    json.loads(result.output)  # parseable


def test_list_agents_cross_provider_shape_stable(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC3-HP — JSON shape stable across providers."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(), _codex()])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    result = list_agents(json_out=True, tty=True)
    parsed = json.loads(result.output)

    keysets = [frozenset(a.keys()) for a in parsed["agents"]]
    assert len(set(keysets)) == 1  # identical key set


def test_list_agents_filter_by_provider(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="a"), _codex(name="b")])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    result = list_agents(provider="codex", json_out=True, tty=True)
    parsed = json.loads(result.output)

    assert parsed["count"] == 1
    assert parsed["agents"][0]["name"] == "b"
    assert parsed["filters_applied"]["provider"] == "codex"


def test_list_agents_filter_by_status(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="alive", status="live"),
            _claude(
                name="dead",
                status="orphaned",
                claude_short_id="def67890",
            ),
        ]
    )
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    result = list_agents(status="orphaned", json_out=True, tty=True)
    parsed = json.loads(result.output)

    assert parsed["count"] == 1
    assert parsed["agents"][0]["name"] == "dead"


def test_list_agents_filter_by_cwd_resolves_relative(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC3-EDGE — cwd filter resolves relative paths to absolute first."""
    use_tmpdir(monkeypatch, tmp_path)
    target_cwd = (tmp_path / "subdir").resolve()
    target_cwd.mkdir()
    write_registry([_claude(name="a", cwd=str(target_cwd))])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    monkeypatch.chdir(tmp_path)
    # Relative filter "./subdir" should resolve to target_cwd absolute.
    result = list_agents(cwd="./subdir", json_out=True, tty=True)
    parsed = json.loads(result.output)

    assert parsed["count"] == 1
    # filters_applied preserves the resolved absolute path.
    assert parsed["filters_applied"]["cwd"] == str(target_cwd)


def test_list_agents_filter_intersection_zero_matches(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC3-EDGE — filters with no matches return empty agents + filters_applied."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(), _codex()])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    result = list_agents(
        cwd="/nonexistent", provider="gemini", json_out=True, tty=True
    )
    parsed = json.loads(result.output)

    assert parsed["count"] == 0
    assert parsed["agents"] == []
    assert parsed["filters_applied"]["cwd"] == "/nonexistent"
    assert parsed["filters_applied"]["provider"] == "gemini"


def test_list_agents_claude_shellout_failure_falls_back(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """AC1-FR — claude shellout fail → live_status=null, list still 0."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="a"),
            _claude(name="b", claude_short_id="def67890"),
        ]
    )
    # Empty result + warning simulates the fallback path.
    _patch_claude_agents_json(
        {}, warnings=["claude agents --json failed: command not found"]
    )

    result = list_agents(json_out=True, tty=True)
    parsed = json.loads(result.output)

    assert parsed["count"] == 2
    assert all(a["live_status"] is None for a in parsed["agents"])
    assert any("claude agents" in w for w in result.warnings)
    assert result.exit_code == 0


def test_list_agents_corrupt_registry_exits_1(tmp_path, monkeypatch, _patch_claude_agents_json):
    """AC1-ERR — corrupt JSON exits 1 with file path + parser error in warnings."""
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths

    target = paths.agents_registry_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text("{not valid json", encoding="utf-8")
    _patch_claude_agents_json({})

    result = list_agents(json_out=True, tty=True)

    assert result.exit_code == 1
    assert any(str(target) in w for w in result.warnings)
    # The parser error message should mention what went wrong somewhere.
    assert result.output == ""


def test_list_agents_claude_shellout_only_called_once_per_invocation(
    tmp_path, monkeypatch
):
    """Locked Decision 5b — shell out once per call, no cross-call cache."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry(
        [
            _claude(name="a"),
            _claude(name="b", claude_short_id="def67890"),
        ]
    )

    calls = {"count": 0}
    from fno.agents.providers import claude as claude_mod

    def _fake(timeout=3.0):  # noqa: ARG001
        calls["count"] += 1
        return {}, []

    monkeypatch.setattr(claude_mod, "claude_agents_json", _fake)

    list_agents(json_out=True, tty=True)
    list_agents(json_out=True, tty=True)

    # Each invocation produces exactly one shellout (per spec).
    assert calls["count"] == 2


def test_list_agents_does_not_mutate_registry(
    tmp_path, monkeypatch, _patch_claude_agents_json
):
    """Locked Decision 5 — list is pure-read, registry file unchanged."""
    use_tmpdir(monkeypatch, tmp_path)
    write_registry([_claude(name="a")])
    _patch_claude_agents_json({"abc12345": {"live_status": "Working"}})

    from fno import paths

    before = paths.agents_registry_path().read_text(encoding="utf-8")
    list_agents(json_out=True, tty=True)
    after = paths.agents_registry_path().read_text(encoding="utf-8")

    assert before == after

"""Precedence coverage for resolve_dispatch_target (CG8, Plan B).

Carried out of the deleted sigma_dispatch test module: only the resolver is
live, so only the resolver's precedence chain is retained. The emitter tests
went with the emitter.
"""
from __future__ import annotations

from pathlib import Path

import pytest


def _write_settings_combo_test(
    tmp_path: Path,
    *,
    active_provider: str | None = "a",
    active_combo: str | None = None,
    combos: dict | None = None,
    agents: dict | None = None,
    block_key: str = "providers",
):
    """Helper: write a settings.yaml with three accounts + optional combos/agents.

    ``block_key`` defaults to the pre-rename ``providers`` so the existing cases
    keep exercising the back-compat path; pass ``accounts`` for the canonical one.
    """
    import yaml
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.parent.mkdir(parents=True, exist_ok=True)
    providers_block: dict = {
        "active": active_provider,
        "records": [
            {"id": "a", "name": "A", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"},
            {"id": "b", "name": "B", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"},
            {"id": "c", "name": "C", "cli": "claude", "auth": "oauth_dir", "credentials_source": "~/.claude"},
        ],
    }
    if active_combo is not None:
        providers_block["active_combo"] = active_combo
    if combos:
        providers_block["combos"] = combos
    payload: dict = {"config": {block_key: providers_block}}
    if agents:
        payload["config"]["agents"] = agents
    settings.write_text(yaml.safe_dump(payload, sort_keys=False), encoding="utf-8")
    return tmp_path


class TestResolveDispatchTargetPrecedence:
    def test_per_agent_pin_wins_over_env_combo(self, tmp_path: Path):
        """AC8.2-EDGE: per-agent pin used even when TARGET_COMBO is set."""
        from fno.agents.dispatch_target import resolve_dispatch_target

        _write_settings_combo_test(
            tmp_path,
            combos={"my-stack": {"providers": ["a", "b"]}},
            agents={"reviewer": {"provider": "c"}},
        )
        target = resolve_dispatch_target(
            "reviewer",
            repo_root=tmp_path,
            env={"TARGET_COMBO": "my-stack"},
        )
        assert target.provider_id == "c"
        assert target.combo_name is None
        assert target.source == "per_agent_pin"

    def test_env_combo_used_when_no_per_agent_pin(self, tmp_path: Path):
        """AC8.1-HP: combo wins over fall-through when no per-agent pin."""
        from fno.agents.dispatch_target import resolve_dispatch_target

        _write_settings_combo_test(
            tmp_path,
            combos={"my-stack": {"providers": ["a", "b"]}},
        )
        target = resolve_dispatch_target(
            "any-agent",
            repo_root=tmp_path,
            env={"TARGET_COMBO": "my-stack"},
        )
        assert target.combo_name == "my-stack"
        assert target.provider_id is None
        assert target.source == "env_combo"

    def test_unknown_env_combo_falls_through_with_warning(self, tmp_path: Path):
        """AC8.3-FR: bad TARGET_COMBO logs warning + falls to active provider."""
        from fno.agents.dispatch_target import resolve_dispatch_target

        _write_settings_combo_test(tmp_path, combos={})
        target = resolve_dispatch_target(
            "any-agent",
            repo_root=tmp_path,
            env={"TARGET_COMBO": "deleted-stack"},
        )
        assert target.provider_id == "a"
        assert target.combo_name is None
        assert target.source == "active_provider"

    def test_settings_active_combo_used_when_no_env(self, tmp_path: Path):
        from fno.agents.dispatch_target import resolve_dispatch_target

        _write_settings_combo_test(
            tmp_path,
            active_combo="my-stack",
            combos={"my-stack": {"providers": ["a", "b"]}},
        )
        target = resolve_dispatch_target(
            "any-agent",
            repo_root=tmp_path,
            env={},
        )
        assert target.combo_name == "my-stack"
        assert target.source == "settings_combo"

    def test_settings_active_combo_read_from_canonical_accounts_block(self, tmp_path: Path):
        """The same case under `config.accounts`. This is a bootstrap reader that
        never reaches the providers loader, and a miss returns active_combo=None,
        which silently degrades routing instead of erroring - so both spellings
        need a test rather than one standing in for the other."""
        from fno.agents.dispatch_target import resolve_dispatch_target

        _write_settings_combo_test(
            tmp_path,
            active_combo="my-stack",
            combos={"my-stack": {"providers": ["a", "b"]}},
            block_key="accounts",
        )
        target = resolve_dispatch_target("any-agent", repo_root=tmp_path, env={})
        assert target.combo_name == "my-stack"
        assert target.source == "settings_combo"

    def test_no_combo_no_pin_returns_active_provider(self, tmp_path: Path):
        from fno.agents.dispatch_target import resolve_dispatch_target

        _write_settings_combo_test(tmp_path)
        target = resolve_dispatch_target(
            "any-agent", repo_root=tmp_path, env={},
        )
        assert target.provider_id == "a"
        assert target.source == "active_provider"

    def test_no_settings_file_returns_unresolved(self, tmp_path: Path):
        from fno.agents.dispatch_target import resolve_dispatch_target

        target = resolve_dispatch_target(
            "any-agent", repo_root=tmp_path, env={},
        )
        assert target.provider_id is None
        assert target.combo_name is None
        assert target.source == "unresolved"

    def test_global_active_combo_falls_back_when_no_project_override(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """PR #230 Gemini MEDIUM #2: active_combo from ~/.fno/ should
        be read when project-local settings.yaml lacks the field."""
        import yaml as _yaml
        from fno.agents.dispatch_target import resolve_dispatch_target

        # The autouse conftest pins FNO_GLOBAL_SETTINGS_PATH=/dev/null for
        # test isolation. This test specifically exercises the HOME-based
        # global fallback path, so opt out of the pin to restore the default
        # Path.home() resolution behavior.
        monkeypatch.delenv("FNO_GLOBAL_SETTINGS_PATH", raising=False)

        # Project-local settings: providers + combos defined, but no active_combo.
        project = tmp_path / "project"
        _write_settings_combo_test(
            project,
            combos={"global-stack": {"providers": ["a", "b"]}},
        )

        # Global home: declares active_combo: global-stack only.
        home = tmp_path / "home"
        global_settings = home / ".fno" / "settings.yaml"
        global_settings.parent.mkdir(parents=True, exist_ok=True)
        global_settings.write_text(
            _yaml.safe_dump({
                "config": {
                    "providers": {
                        "active_combo": "global-stack",
                        "records": [
                            {"id": "a", "name": "A", "cli": "claude",
                             "auth": "oauth_dir", "credentials_source": "~/.claude"},
                        ],
                    }
                }
            }, sort_keys=False),
            encoding="utf-8",
        )
        monkeypatch.setenv("HOME", str(home))

        target = resolve_dispatch_target(
            "any-agent", repo_root=project, env={},
        )
        assert target.combo_name == "global-stack"
        assert target.source == "settings_combo"

"""Tests for load_providers and save_providers.

Run: cd cli && uv run pytest src/fno/adapters/providers/test_loader.py -v
"""
from __future__ import annotations

from pathlib import Path

import pytest
import tomli_w
import tomllib


def _flatten(d: dict) -> dict:
    cfg = d.get("config")
    if not isinstance(cfg, dict):
        return d
    out = {k: v for k, v in d.items() if k != "config"}
    out.update(cfg)
    return out


def _strip_none(x):
    if isinstance(x, dict):
        return {k: _strip_none(v) for k, v in x.items() if v is not None}
    if isinstance(x, list):
        return [_strip_none(v) for v in x]
    return x


def _write_settings(path: Path, content: dict) -> None:
    """Write a flat config.toml at path (lifts any legacy config: wrapper)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(tomli_w.dumps(_strip_none(_flatten(content))), encoding="utf-8")


# ---------------------------------------------------------------------------
# Fixture helpers
# ---------------------------------------------------------------------------

def _valid_providers_block(active: str = "claude-primary") -> dict:
    return {
        "config": {
            "providers": {
                "active": active,
                "records": [
                    {
                        "id": "claude-primary",
                        "name": "Claude Primary",
                        "cli": "claude",
                        "auth": "oauth_dir",
                        "credentials_source": "~/.claude",
                        "priority": 10,
                    },
                    {
                        "id": "gemini-backup",
                        "name": "Gemini Backup",
                        "cli": "gemini",
                        "auth": "api_key",
                        "env": {"GEMINI_API_KEY": "key"},
                        "priority": 20,
                    },
                ],
            }
        }
    }


# ---------------------------------------------------------------------------
# AC01.1-HP: Valid config loads cleanly
# ---------------------------------------------------------------------------

class TestLoadProvidersValid:
    def test_valid_settings_returns_providers_config(self, tmp_path: Path):
        """AC01.1-HP: load_providers returns ProvidersConfig with correct records + active."""
        from fno.adapters.providers.loader import load_providers
        from fno.adapters.providers.model import ProvidersConfig

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, _valid_providers_block())

        result = load_providers(repo_root=tmp_path)

        assert isinstance(result, ProvidersConfig)
        assert len(result.records) == 2
        assert result.active == "claude-primary"

    def test_by_id_contains_both_ids(self, tmp_path: Path):
        """AC01.1-HP: config.by_id contains all record ids."""
        from fno.adapters.providers.loader import load_providers

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, _valid_providers_block())

        result = load_providers(repo_root=tmp_path)
        by_id = result.by_id
        assert "claude-primary" in by_id
        assert "gemini-backup" in by_id

    def test_record_fields_preserved(self, tmp_path: Path):
        """AC01.1-HP: ProviderRecord fields match the YAML source."""
        from fno.adapters.providers.loader import load_providers

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, _valid_providers_block())

        result = load_providers(repo_root=tmp_path)
        primary = result.by_id["claude-primary"]
        assert primary.cli == "claude"
        assert primary.auth == "oauth_dir"
        assert primary.priority == 10

    def test_auto_switch_defaults_false(self, tmp_path: Path):
        """US3: config.providers.auto_switch defaults False when the key is absent."""
        from fno.adapters.providers.loader import load_providers

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, _valid_providers_block())
        assert load_providers(repo_root=tmp_path).auto_switch is False

    def test_auto_switch_parsed_when_set(self, tmp_path: Path):
        """US3: an operator-set config.providers.auto_switch = true is read through."""
        from fno.adapters.providers.loader import load_providers

        block = _valid_providers_block()
        block["config"]["providers"]["auto_switch"] = True
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, block)
        assert load_providers(repo_root=tmp_path).auto_switch is True

    def test_auto_switch_quoted_false_is_false(self, tmp_path: Path):
        """Peer review (PR#366): a mistyped quoted `"false"` must parse False, not
        True — pydantic coerces the raw value; a local bool() would arm it."""
        from fno.adapters.providers.loader import load_providers

        block = _valid_providers_block()
        block["config"]["providers"]["auto_switch"] = "false"  # quoted string, not a bool
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, block)
        assert load_providers(repo_root=tmp_path).auto_switch is False

    def test_auto_switch_round_trips_through_save(self, tmp_path: Path, monkeypatch):
        """Peer review (PR#366): save_providers serializes auto_switch=True off the
        object so a write-back never silently disarms the feature.

        save_providers(scope="project") targets ``$PWD/.fno/config.toml``, and
        monkeypatch.chdir does NOT update $PWD - so PWD is pinned to tmp_path here
        or the write escapes the sandbox and clobbers the real config."""
        from fno.adapters.providers.loader import load_providers, save_providers
        from fno.adapters.providers.model import ProvidersConfig

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, _valid_providers_block())
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.chdir(tmp_path)
        cfg = load_providers(repo_root=tmp_path)
        armed = ProvidersConfig(records=cfg.records, active=cfg.active, auto_switch=True)
        save_providers(armed, scope="project")
        assert settings.read_text().count("auto_switch") == 1   # written to the sandbox file
        assert load_providers(repo_root=tmp_path).auto_switch is True

    def test_auto_switch_survives_agents_reconstruction(self, tmp_path: Path):
        """US3 review (PR#366): the agents-path ProvidersConfig rebuild must keep
        auto_switch, or it reverts to False for anyone using per-agent routing."""
        from fno.adapters.providers.loader import load_providers

        block = _valid_providers_block()
        block["config"]["providers"]["auto_switch"] = True
        block["config"]["agents"] = {"reviewer": {"provider": "claude-primary"}}
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, block)
        result = load_providers(repo_root=tmp_path)
        assert result.auto_switch is True
        assert "reviewer" in result.agents


# ---------------------------------------------------------------------------
# AC01.3-EDGE: Empty list / missing section is not an error
# ---------------------------------------------------------------------------

class TestLoadProvidersEmpty:
    def test_no_providers_section_returns_empty(self, tmp_path: Path):
        """AC01.3-EDGE: missing config.providers returns empty config without error."""
        from fno.adapters.providers.loader import load_providers

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, {"config": {"v2_enabled": True}})

        result = load_providers(repo_root=tmp_path)
        assert result.records == []
        assert result.active is None

    def test_empty_records_list_returns_empty(self, tmp_path: Path):
        """AC01.3-EDGE: config.providers.records=[] returns empty config without error."""
        from fno.adapters.providers.loader import load_providers

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, {"config": {"providers": {"active": None, "records": []}}})

        result = load_providers(repo_root=tmp_path)
        assert result.records == []
        assert result.active is None

    def test_no_files_at_all_returns_empty(self, tmp_path: Path):
        """AC01.3-EDGE: when no settings.yaml exists anywhere, returns empty config."""
        from fno.adapters.providers.loader import load_providers

        # tmp_path has no .fno dir
        result = load_providers(repo_root=tmp_path)
        assert result.records == []
        assert result.active is None

    def test_global_config_pollution_does_not_leak(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ):
        """AC2-FR (ab-a1118224): a globally-configured ~/.fno/settings.yaml
        must not leak into ``repo_root=tmp_path`` test isolation.

        Simulates the failure mode the developer was seeing locally: a user
        with their own provider config sees this test class fail with stray
        records because the loader falls back to ``Path.home() /
        .fno / settings.yaml``. The ``FNO_GLOBAL_SETTINGS_PATH`` env
        var override (set to a fake home with a populated providers block
        here) demonstrates that the override is the seam tests use to stay
        clean. The autouse conftest's ``/dev/null`` pin handles the default
        case; this test exercises the seam directly.
        """
        from fno.adapters.providers.loader import load_providers

        # Fake "global" config with a populated providers block.
        fake_global = tmp_path / "fake_home" / ".fno" / "config.toml"
        _write_settings(fake_global, _valid_providers_block(active="claude-primary"))

        # Point the loader's "global" candidate at the fake. Project-local
        # (tmp_path / .fno / settings.yaml) is absent.
        monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(fake_global))

        # The loader sees the global config because project-local is absent.
        result = load_providers(repo_root=tmp_path)
        assert result.active == "claude-primary"
        assert len(result.records) == 2  # claude-primary, gemini-backup

        # Now flip the override to /dev/null (mirrors the conftest default).
        # The loader returns empty because both candidates are non-existent
        # (project-local absent, global == /dev/null which parses to nothing).
        monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
        result = load_providers(repo_root=tmp_path)
        assert result.records == []
        assert result.active is None


# ---------------------------------------------------------------------------
# AC01.2-ERR: Invalid record raises ProviderConfigError naming the id
# ---------------------------------------------------------------------------

class TestLoadProvidersInvalidRecord:
    def test_invalid_record_raises_provider_config_error(self, tmp_path: Path):
        """AC01.2-ERR: record with auth=oauth_dir but no credentials_source raises ProviderConfigError."""
        from fno.adapters.providers.loader import load_providers
        from fno.adapters.providers.model import ProviderConfigError

        bad_settings = {
            "config": {
                "providers": {
                    "active": None,
                    "records": [
                        {
                            "id": "bad-record",
                            "name": "Bad",
                            "cli": "claude",
                            "auth": "oauth_dir",
                            # missing credentials_source
                        }
                    ],
                }
            }
        }
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, bad_settings)

        with pytest.raises(ProviderConfigError) as exc_info:
            load_providers(repo_root=tmp_path)

        error_msg = str(exc_info.value)
        assert "bad-record" in error_msg
        assert "auth_strategy_mismatch" in error_msg

    def test_bad_api_key_env_raises_provider_config_error(self, tmp_path: Path):
        """AC01.4-EDGE: api_key with no recognized env key raises ProviderConfigError."""
        from fno.adapters.providers.loader import load_providers
        from fno.adapters.providers.model import ProviderConfigError

        bad_settings = {
            "config": {
                "providers": {
                    "active": None,
                    "records": [
                        {
                            "id": "bad-api-key",
                            "name": "Bad API",
                            "cli": "openclaw",
                            "auth": "api_key",
                            "env": {"SOME_RANDOM_KEY": "value"},
                        }
                    ],
                }
            }
        }
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, bad_settings)

        with pytest.raises(ProviderConfigError) as exc_info:
            load_providers(repo_root=tmp_path)

        error_msg = str(exc_info.value)
        assert "bad-api-key" in error_msg
        assert "auth_strategy_mismatch" in error_msg


# ---------------------------------------------------------------------------
# Precedence: project-local overrides global
# ---------------------------------------------------------------------------

class TestLoadProvidersProjectLocalOverride:
    def test_project_local_overrides_global(self, tmp_path: Path, monkeypatch):
        """Project-local config.providers block entirely replaces global block."""
        from fno.adapters.providers.loader import load_providers

        # Global: two records
        global_home = tmp_path / "fake-home"
        global_settings = global_home / ".fno" / "config.toml"
        _write_settings(
            global_settings,
            {
                "config": {
                    "providers": {
                        "active": "global-record",
                        "records": [
                            {
                                "id": "global-record",
                                "name": "Global",
                                "cli": "gemini",
                                "auth": "api_key",
                                "env": {"GEMINI_API_KEY": "global-key"},
                            }
                        ],
                    }
                }
            },
        )

        # Project-local: different single record
        project_root = tmp_path / "repo"
        local_settings = project_root / ".fno" / "config.toml"
        _write_settings(
            local_settings,
            {
                "config": {
                    "providers": {
                        "active": "local-record",
                        "records": [
                            {
                                "id": "local-record",
                                "name": "Local",
                                "cli": "claude",
                                "auth": "oauth_dir",
                                "credentials_source": "~/.claude",
                            }
                        ],
                    }
                }
            },
        )

        # Patch Path.home() to point to fake home
        monkeypatch.setattr(Path, "home", staticmethod(lambda: global_home))

        result = load_providers(repo_root=project_root)

        # Project-local should win: only local-record present
        assert len(result.records) == 1
        assert result.records[0].id == "local-record"
        assert result.active == "local-record"

    def test_global_used_when_no_local(self, tmp_path: Path, monkeypatch):
        """Global settings.yaml is used when no project-local file exists."""
        from fno.adapters.providers.loader import load_providers

        # The autouse conftest pins FNO_GLOBAL_SETTINGS_PATH=/dev/null for
        # test isolation; this test specifically exercises the
        # Path.home()-based global fallback, so opt out of the pin to
        # restore default resolution.
        monkeypatch.delenv("FNO_GLOBAL_SETTINGS_PATH", raising=False)

        global_home = tmp_path / "fake-home"
        global_settings = global_home / ".fno" / "config.toml"
        _write_settings(
            global_settings,
            {
                "config": {
                    "providers": {
                        "active": "global-only",
                        "records": [
                            {
                                "id": "global-only",
                                "name": "Global Only",
                                "cli": "claude",
                                "auth": "oauth_dir",
                                "credentials_source": "~/.claude",
                            }
                        ],
                    }
                }
            },
        )

        # repo has no .fno dir
        project_root = tmp_path / "repo"
        project_root.mkdir()

        monkeypatch.setattr(Path, "home", staticmethod(lambda: global_home))

        result = load_providers(repo_root=project_root)
        assert len(result.records) == 1
        assert result.records[0].id == "global-only"


# ---------------------------------------------------------------------------
# AC01.5-EDGE: active references non-existent id
# ---------------------------------------------------------------------------

class TestLoadProvidersActiveNotFound:
    def test_active_id_not_in_records_raises(self, tmp_path: Path):
        """AC01.5-EDGE: active id that doesn't match any record raises ProviderConfigError."""
        from fno.adapters.providers.loader import load_providers
        from fno.adapters.providers.model import ProviderConfigError

        settings_data = {
            "config": {
                "providers": {
                    "active": "nonexistent-id",
                    "records": [
                        {
                            "id": "claude-primary",
                            "name": "Claude Primary",
                            "cli": "claude",
                            "auth": "oauth_dir",
                            "credentials_source": "~/.claude",
                        }
                    ],
                }
            }
        }
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, settings_data)

        with pytest.raises(ProviderConfigError) as exc_info:
            load_providers(repo_root=tmp_path)

        error_msg = str(exc_info.value)
        assert "active_record_not_found" in error_msg
        assert "nonexistent-id" in error_msg


# ---------------------------------------------------------------------------
# save_providers: atomic write + key preservation
# ---------------------------------------------------------------------------

class TestSaveProviders:
    def test_save_project_scope_writes_settings(self, tmp_path: Path, monkeypatch):
        """save_providers with scope='project' writes to cwd/.fno/settings.yaml."""
        from fno.adapters.providers.loader import save_providers
        from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

        # Work in tmp_path as cwd; set PWD too so loader uses the same path.
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.chdir(tmp_path)

        record = ProviderRecord(
            id="claude-saved",
            name="Saved",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path("~/.claude"),
        )
        cfg = ProvidersConfig(records=[record], active="claude-saved")

        save_providers(cfg, scope="project")

        output = tmp_path / ".fno" / "config.toml"
        assert output.exists()
        data = tomllib.loads(output.read_text())
        providers = data["providers"]
        assert providers["active"] == "claude-saved"
        assert len(providers["records"]) == 1
        assert providers["records"][0]["id"] == "claude-saved"

    def test_save_preserves_other_keys(self, tmp_path: Path, monkeypatch):
        """save_providers preserves existing non-providers keys in settings.yaml."""
        from fno.adapters.providers.loader import save_providers
        from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.chdir(tmp_path)

        # Write existing settings with other keys
        existing = tmp_path / ".fno" / "config.toml"
        _write_settings(
            existing,
            {
                "config": {
                    "v2_enabled": True,
                    "some_other_setting": "preserved",
                }
            },
        )

        record = ProviderRecord(
            id="new-record",
            name="New",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path("~/.claude"),
        )
        cfg = ProvidersConfig(records=[record], active=None)

        save_providers(cfg, scope="project")

        data = tomllib.loads(existing.read_text())
        # Other config keys preserved
        assert data["v2_enabled"] is True
        assert data["some_other_setting"] == "preserved"
        # providers block updated
        assert "providers" in data
        assert len(data["providers"]["records"]) == 1

    def test_save_preserves_provider_subkeys(self, tmp_path: Path, monkeypatch):
        """save_providers keeps provider subkeys it does not rebuild (x-5d3e).

        A normal provider edit (use/add/remove) must not silently drop
        config.providers.quota / combos / failover - rebuilding the block from
        only records+active would turn an operator's defer_dispatch back off.
        """
        from fno.adapters.providers.loader import save_providers
        from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.chdir(tmp_path)

        existing = tmp_path / ".fno" / "config.toml"
        _write_settings(
            existing,
            {
                "config": {
                    "providers": {
                        "records": [],
                        "quota": {"defer_dispatch": True, "defer_threshold_pct": 80},
                        "combos": {"c1": {"providers": ["a"]}},
                    }
                }
            },
        )

        record = ProviderRecord(
            id="a", name="A", cli="claude", auth="oauth_dir",
            credentials_source=Path("~/.claude"),
        )
        save_providers(ProvidersConfig(records=[record], active="a"), scope="project")

        data = tomllib.loads(existing.read_text())
        assert data["providers"]["quota"] == {"defer_dispatch": True, "defer_threshold_pct": 80}
        assert data["providers"]["combos"] == {"c1": {"providers": ["a"]}}
        assert data["providers"]["active"] == "a"

    def test_save_global_scope(self, tmp_path: Path, monkeypatch):
        """save_providers with scope='global' writes to home/.fno/settings.yaml."""
        from fno.adapters.providers.loader import save_providers
        from fno.adapters.providers.model import ProviderRecord, ProvidersConfig

        fake_home = tmp_path / "home"
        monkeypatch.setattr(Path, "home", staticmethod(lambda: fake_home))

        record = ProviderRecord(
            id="global-write",
            name="Global Write",
            cli="gemini",
            auth="api_key",
            env={"GEMINI_API_KEY": "k"},
        )
        cfg = ProvidersConfig(records=[record], active="global-write")

        save_providers(cfg, scope="global")

        output = fake_home / ".fno" / "config.toml"
        assert output.exists()
        data = tomllib.loads(output.read_text())
        assert data["providers"]["active"] == "global-write"


# ---------------------------------------------------------------------------
# Fix 1 regression: save_providers refuses to overwrite a corrupt settings.yaml
# ---------------------------------------------------------------------------

class TestSaveProvidersCorruptFile:
    def test_save_providers_refuses_to_overwrite_corrupt_settings(
        self, tmp_path: Path, monkeypatch
    ):
        """save_providers raises ProviderConfigError when settings.yaml has a YAML syntax
        error, and leaves the original (corrupt) file unchanged on disk."""
        from fno.adapters.providers.loader import save_providers
        from fno.adapters.providers.model import (
            ProviderConfigError,
            ProviderRecord,
            ProvidersConfig,
        )

        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.chdir(tmp_path)

        # Write a settings.yaml that has valid keys AND a syntactically broken line.
        corrupt_content = (
            "config:\n"
            "  budget:\n"
            "    daily_limit: 5.00\n"
            "  : this_is_broken_yaml: [unclosed\n"  # deliberately malformed
        )
        abilities_dir = tmp_path / ".fno"
        abilities_dir.mkdir(parents=True, exist_ok=True)
        settings_path = abilities_dir / "config.toml"
        settings_path.write_text(corrupt_content, encoding="utf-8")

        original_bytes = settings_path.read_bytes()

        record = ProviderRecord(
            id="new-provider",
            name="New Provider",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path("~/.claude"),
        )
        cfg = ProvidersConfig(records=[record], active=None)

        with pytest.raises(ProviderConfigError) as exc_info:
            save_providers(cfg, scope="project")

        # Error message must mention the broken file
        assert settings_path.name in str(exc_info.value) or str(settings_path) in str(exc_info.value)

        # Original file must be untouched
        assert settings_path.read_bytes() == original_bytes

        # No temp file leaked under .fno/
        leaked = [f for f in abilities_dir.iterdir() if f != settings_path]
        assert leaked == [], f"Temp file(s) leaked: {leaked}"


# ---------------------------------------------------------------------------
# Fix 2 regression: load_providers expands tilde in credentials_source
# ---------------------------------------------------------------------------

class TestLoadProvidersTildeExpansion:
    def test_load_providers_expands_tilde_in_credentials_source(
        self, tmp_path: Path
    ):
        """credentials_source: ~/.claude in YAML resolves to Path.home()/'.claude'."""
        from fno.adapters.providers.loader import load_providers

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(
            settings,
            {
                "config": {
                    "providers": {
                        "active": None,
                        "records": [
                            {
                                "id": "claude-tilde",
                                "name": "Claude Tilde",
                                "cli": "claude",
                                "auth": "oauth_dir",
                                "credentials_source": "~/.claude",
                            }
                        ],
                    }
                }
            },
        )

        result = load_providers(repo_root=tmp_path)
        record = result.by_id["claude-tilde"]
        assert record.credentials_source == Path.home() / ".claude"
        assert record.credentials_source != Path("~/.claude")


# ---------------------------------------------------------------------------
# Fix 4 regression: load_providers rejects duplicate ids in YAML
# ---------------------------------------------------------------------------

class TestLoadProvidersDuplicateIds:
    def test_load_providers_rejects_duplicate_ids_in_yaml(
        self, tmp_path: Path
    ):
        """load_providers raises ProviderConfigError when two records share the same id."""
        from fno.adapters.providers.loader import load_providers
        from fno.adapters.providers.model import ProviderConfigError

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(
            settings,
            {
                "config": {
                    "providers": {
                        "active": None,
                        "records": [
                            {
                                "id": "claude-dupe",
                                "name": "Claude Dupe A",
                                "cli": "claude",
                                "auth": "oauth_dir",
                                "credentials_source": "~/.claude",
                            },
                            {
                                "id": "claude-dupe",  # same id - should be rejected
                                "name": "Claude Dupe B",
                                "cli": "claude",
                                "auth": "oauth_dir",
                                "credentials_source": "~/.claude",
                            },
                        ],
                    }
                }
            },
        )

        with pytest.raises(ProviderConfigError) as exc_info:
            load_providers(repo_root=tmp_path)


# ---------------------------------------------------------------------------
# PWD env var respected (Gemini Code Assist MEDIUM finding PR #199)
# ---------------------------------------------------------------------------

class TestPWDRespected:
    """load_providers and save_providers must use PWD env var when repo_root is None,
    matching the pattern used by _resolve_cwd in cli.py for test isolation."""

    def test_load_providers_uses_pwd_env_var(self, tmp_path: Path, monkeypatch):
        """load_providers(repo_root=None) must use PWD over os.getcwd() so test
        isolation via PWD override is consistent with cli.py's _resolve_cwd."""
        from fno.adapters.providers.loader import load_providers

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(
            settings,
            {
                "config": {
                    "providers": {
                        "active": "claude-pwd-test",
                        "records": [
                            {
                                "id": "claude-pwd-test",
                                "name": "Claude PWD Test",
                                "cli": "claude",
                                "auth": "oauth_dir",
                                "credentials_source": "~/.claude",
                            },
                        ],
                    }
                }
            },
        )

        # Point PWD at tmp_path (settings lives there); keep real cwd elsewhere
        # so we can confirm which one is used.
        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.chdir(tmp_path.parent)  # cwd != PWD

        config = load_providers()  # repo_root=None - must fall back to PWD
        assert "claude-pwd-test" in config.by_id, (
            "load_providers with repo_root=None must discover settings via PWD, not cwd"
        )

    def test_save_providers_uses_pwd_env_var(self, tmp_path: Path, monkeypatch):
        """save_providers(scope='project') must write to PWD/.fno/settings.yaml,
        not to cwd/.fno/settings.yaml."""
        from fno.adapters.providers.loader import save_providers
        from fno.adapters.providers.model import ProvidersConfig, ProviderRecord

        monkeypatch.setenv("PWD", str(tmp_path))
        monkeypatch.chdir(tmp_path.parent)  # cwd != PWD

        record = ProviderRecord(
            id="save-pwd-test",
            name="Save PWD Test",
            cli="claude",
            auth="oauth_dir",
            credentials_source=Path.home() / ".claude",
        )
        config = ProvidersConfig(records=[record], active=None)
        save_providers(config, scope="project")

        expected_path = tmp_path / ".fno" / "config.toml"
        assert expected_path.exists(), (
            f"save_providers must write to PWD/.fno/settings.yaml, not cwd. "
            f"Expected {expected_path}"
        )


# ---------------------------------------------------------------------------
# Task 1.2: atomic_mutate_settings - read+mutate+write under exclusive lock.
# Phase 01 of provider rotation failover (ab-9728b70b).
# ---------------------------------------------------------------------------

class TestAtomicMutate:
    def test_hp1_single_writer_mutation_persists(self, tmp_path: Path):
        from fno.adapters.providers.loader import atomic_mutate_settings

        settings_path = tmp_path / "config.toml"
        _write_settings(settings_path, {
            "config": {"providers": {"active": "foo", "records": []}},
        })

        def mutator(d: dict) -> dict:
            d["providers"]["active"] = "bar"
            return d

        atomic_mutate_settings(mutator, settings_path=settings_path)

        loaded = tomllib.loads(settings_path.read_text())
        assert loaded["providers"]["active"] == "bar"

    def test_hp1b_other_keys_preserved(self, tmp_path: Path):
        from fno.adapters.providers.loader import atomic_mutate_settings

        settings_path = tmp_path / "config.toml"
        _write_settings(settings_path, {
            "config": {
                "providers": {"active": "foo", "records": []},
                "other_section": {"keep": "me"},
            },
            "top_level_other": "alive",
        })

        def mutator(d: dict) -> dict:
            d["providers"]["active"] = "bar"
            return d

        atomic_mutate_settings(mutator, settings_path=settings_path)

        loaded = tomllib.loads(settings_path.read_text())
        assert loaded["other_section"]["keep"] == "me"
        assert loaded["top_level_other"] == "alive"

    def test_hp2_read_during_mutation_sees_consistent_state(self, tmp_path: Path):
        """While a writer is mid-mutation, a non-locking reader sees either the
        pre-write or post-write state, never a half-written file. Verified by
        spawning a writer in a thread that holds the lock, reading from the
        main thread, and asserting the file is parseable YAML at every read.
        """
        import threading
        import time

        from fno.adapters.providers.loader import atomic_mutate_settings

        settings_path = tmp_path / "config.toml"
        _write_settings(settings_path, {
            "config": {"providers": {"active": "foo", "records": []}},
        })

        # Writer holds the lock for ~80ms inside mutator; reader polls during.
        def slow_mutator(d: dict) -> dict:
            time.sleep(0.08)
            d["providers"]["active"] = "bar"
            return d

        observed_states = []

        def reader_loop():
            for _ in range(20):
                try:
                    text = settings_path.read_text()
                    parsed = tomllib.loads(text)
                    observed_states.append(parsed["providers"]["active"])
                except (tomllib.TOMLDecodeError, KeyError, TypeError):
                    observed_states.append("CORRUPT")
                time.sleep(0.005)

        reader_thread = threading.Thread(target=reader_loop)
        reader_thread.start()

        atomic_mutate_settings(slow_mutator, settings_path=settings_path)
        reader_thread.join()

        # Every observation must be either "foo" (pre) or "bar" (post),
        # never CORRUPT.
        assert "CORRUPT" not in observed_states
        assert set(observed_states).issubset({"foo", "bar"})
        assert "bar" in observed_states  # final read should see the new value

    def test_err1_mutator_exception_leaves_file_unchanged(self, tmp_path: Path):
        from fno.adapters.providers.loader import atomic_mutate_settings

        settings_path = tmp_path / "config.toml"
        _write_settings(settings_path, {
            "config": {"providers": {"active": "foo", "records": []}},
        })
        original_text = settings_path.read_text()

        def boom(d: dict) -> dict:
            d["providers"]["active"] = "wont-stick"
            raise RuntimeError("mutator boom")

        with pytest.raises(RuntimeError, match="mutator boom"):
            atomic_mutate_settings(boom, settings_path=settings_path)

        assert settings_path.read_text() == original_text

    def test_edge1_concurrent_mutators_are_serialized(self, tmp_path: Path):
        """Two parallel sessions racing on settings.yaml swap. Cites what-if
        finding #4: 'Two parallel sessions racing on settings.yaml swap.'

        GIVEN sessions A and B both call atomic_mutate_settings to swap the
        active provider WHEN both run concurrently THEN exactly one wins
        the lock first, settings.yaml is parseable YAML at every observable
        instant, and the final file reflects exactly one of the contributed
        states.
        """
        import threading
        import time

        from fno.adapters.providers.loader import atomic_mutate_settings

        settings_path = tmp_path / "config.toml"
        _write_settings(settings_path, {
            "config": {"providers": {"active": "foo", "records": []}},
        })

        def mutator_factory(target: str):
            def m(d: dict) -> dict:
                # Hold the lock briefly so the threads actually contend.
                time.sleep(0.01)
                d["providers"]["active"] = target
                return d
            return m

        threads = []
        for target in ("bar", "baz"):
            t = threading.Thread(
                target=lambda tg=target: atomic_mutate_settings(
                    mutator_factory(tg), settings_path=settings_path,
                ),
            )
            threads.append(t)
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        loaded = tomllib.loads(settings_path.read_text())
        assert loaded["providers"]["active"] in {"bar", "baz"}

    def test_edge1b_high_contention_race(self, tmp_path: Path):
        """Many threads × many mutations: file must always parse, final value
        must come from one of the contributors, no lost-update corruption."""
        import threading
        from fno.adapters.providers.loader import atomic_mutate_settings

        settings_path = tmp_path / "config.toml"
        _write_settings(settings_path, {
            "config": {"providers": {"active": "init", "counter": 0, "records": []}},
        })

        def increment(d: dict) -> dict:
            d["providers"]["counter"] += 1
            return d

        def worker():
            for _ in range(50):
                atomic_mutate_settings(increment, settings_path=settings_path)

        threads = [threading.Thread(target=worker) for _ in range(10)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        loaded = tomllib.loads(settings_path.read_text())
        # Every thread did 50 increments, all under the exclusive lock.
        # No lost updates: counter == 10*50 = 500.
        assert loaded["providers"]["counter"] == 500

    def test_edge2_orphan_tempfiles_ignored(self, tmp_path: Path):
        """A writer that crashed before os.replace leaves a `.tmp.*` orphan
        in the parent directory. The next reader/writer must ignore it and
        operate on the canonical settings.yaml only."""
        from fno.adapters.providers.loader import atomic_mutate_settings

        settings_path = tmp_path / "config.toml"
        _write_settings(settings_path, {
            "config": {"providers": {"active": "foo", "records": []}},
        })

        # Plant an orphan tempfile
        orphan = tmp_path / ".config.toml.tmp.99999.0"
        orphan.write_text("garbage: not_yaml: ::: invalid")

        def mutator(d: dict) -> dict:
            d["providers"]["active"] = "bar"
            return d

        atomic_mutate_settings(mutator, settings_path=settings_path)

        loaded = tomllib.loads(settings_path.read_text())
        assert loaded["providers"]["active"] == "bar"
        # Orphan still on disk (no auto-cleanup is part of this task) but the
        # canonical file is intact.
        assert settings_path.exists()


# ---------------------------------------------------------------------------
# Task 1.3: read_active_provider_atomic - shared-lock atomic snapshot read.
# Phase 01 of provider rotation failover (ab-9728b70b).
# ---------------------------------------------------------------------------

class TestAtomicRead:
    def _seed(self, settings_path: Path, *, active: str = "claude-primary") -> None:
        _write_settings(settings_path, {
            "config": {
                "providers": {
                    "active": active,
                    "records": [
                        {
                            "id": "claude-primary",
                            "name": "Claude Primary",
                            "cli": "claude",
                            "auth": "oauth_dir",
                            "credentials_source": "~/.claude",
                            "priority": 10,
                            "base_url": "https://api.anthropic.com",
                        },
                        {
                            "id": "claude-secondary",
                            "name": "Claude Secondary",
                            "cli": "claude",
                            "auth": "api_key",
                            "env": {"ANTHROPIC_API_KEY": "key-B"},
                            "priority": 20,
                        },
                    ],
                }
            }
        })

    def test_hp1_returns_frozen_snapshot_for_active(self, tmp_path: Path):
        from fno.adapters.providers.loader import (
            ActiveProviderSnapshot,
            read_active_provider_atomic,
        )

        settings_path = tmp_path / "config.toml"
        self._seed(settings_path, active="claude-primary")

        snap = read_active_provider_atomic(settings_path=settings_path)

        assert isinstance(snap, ActiveProviderSnapshot)
        assert snap.id == "claude-primary"
        assert snap.cli == "claude"
        assert snap.auth == "oauth_dir"
        assert snap.base_url == "https://api.anthropic.com"
        # Snapshot is frozen
        with pytest.raises((AttributeError, Exception)):
            snap.id = "tampered"  # type: ignore[misc]

    def test_hp2_concurrent_readers_do_not_serialize(self, tmp_path: Path):
        """Multiple shared-lock readers proceed without blocking each other.

        Sanity bound: 10 concurrent reads should complete much faster than
        10 sequential reads at the same artificial latency. We verify the
        ratio is well under 10×.
        """
        import threading
        import time

        from fno.adapters.providers.loader import read_active_provider_atomic

        settings_path = tmp_path / "config.toml"
        self._seed(settings_path)

        # Warm up file cache
        read_active_provider_atomic(settings_path=settings_path)

        # Sequential baseline: 10 reads
        sequential_start = time.perf_counter()
        for _ in range(10):
            read_active_provider_atomic(settings_path=settings_path)
        sequential_dur = time.perf_counter() - sequential_start

        # Concurrent: 10 readers in parallel
        barrier = threading.Barrier(10)

        def reader():
            barrier.wait()
            read_active_provider_atomic(settings_path=settings_path)

        concurrent_start = time.perf_counter()
        threads = [threading.Thread(target=reader) for _ in range(10)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        concurrent_dur = time.perf_counter() - concurrent_start

        # Concurrent must NOT be massively slower than sequential. If the
        # shared lock degenerated to exclusive (forcing serialization),
        # 10 threads would take ~10x sequential time. Allow up to 3x to
        # absorb scheduler noise on a busy CI machine while still catching
        # an actual serialization regression. The structural invariant
        # (LOCK_SH not LOCK_EX) is the load-bearing test in
        # test_edge1_writer_blocks_reader_then_reader_sees_post_state.
        assert concurrent_dur < max(sequential_dur * 3.0, 0.5), (
            f"Shared-lock readers appear serialized: "
            f"sequential={sequential_dur:.3f}s, concurrent={concurrent_dur:.3f}s"
        )

    def test_err1_lock_file_missing_is_created_on_demand(self, tmp_path: Path):
        from fno.adapters.providers.loader import (
            _settings_lock_path,
            read_active_provider_atomic,
        )

        settings_path = tmp_path / "config.toml"
        self._seed(settings_path)

        lock_path = _settings_lock_path(settings_path)
        assert not lock_path.exists()

        snap = read_active_provider_atomic(settings_path=settings_path)
        assert snap.id == "claude-primary"
        assert lock_path.exists()  # created on demand

    def test_edge1_writer_blocks_reader_then_reader_sees_post_state(
        self, tmp_path: Path,
    ):
        """Cites what-if finding #6: 'Auth-mismatch cascade from non-atomic
        credential swap.' GIVEN session A is mid-mutate (LOCK_EX) WHEN
        session B calls read_active_provider_atomic THEN B blocks until A
        finishes, then reads either pre- or post-swap state, NEVER a
        mismatched (active_id, auth) pair.
        """
        import threading
        import time

        from fno.adapters.providers.loader import (
            atomic_mutate_settings,
            read_active_provider_atomic,
        )

        settings_path = tmp_path / "config.toml"
        self._seed(settings_path, active="claude-primary")

        # Writer holds the lock for ~80ms while it changes both active and
        # the records.  A reader during that window must block until the
        # writer releases, then see the post-swap state.
        writer_started = threading.Event()

        def slow_swap(d: dict) -> dict:
            writer_started.set()
            time.sleep(0.08)
            d["providers"]["active"] = "claude-secondary"
            return d

        observed = []

        def reader_after_writer():
            # Wait for the writer to be inside the lock before reading.
            writer_started.wait(timeout=1.0)
            time.sleep(0.005)  # ensure writer has actually called flock
            snap = read_active_provider_atomic(settings_path=settings_path)
            observed.append(snap)

        reader_t = threading.Thread(target=reader_after_writer)
        reader_t.start()
        atomic_mutate_settings(slow_swap, settings_path=settings_path)
        reader_t.join()

        assert len(observed) == 1
        snap = observed[0]
        # Reader saw post-swap state (writer finished, then reader unblocked).
        assert snap.id == "claude-secondary"
        # Critical invariant: id and auth come from the SAME record. Never
        # (claude-secondary, oauth_dir) - that would be the auth-mismatch.
        assert snap.auth == "api_key"

    def test_edge2_active_id_missing_record_raises_structured_error(
        self, tmp_path: Path,
    ):
        from fno.adapters.providers.loader import (
            MissingActiveProvider,
            read_active_provider_atomic,
        )

        settings_path = tmp_path / "config.toml"
        # active points to a record id that isn't in records
        _write_settings(settings_path, {
            "config": {
                "providers": {
                    "active": "ghost",
                    "records": [
                        {
                            "id": "claude-primary",
                            "name": "Claude Primary",
                            "cli": "claude",
                            "auth": "oauth_dir",
                            "credentials_source": "~/.claude",
                        },
                    ],
                }
            }
        })

        with pytest.raises(MissingActiveProvider) as exc_info:
            read_active_provider_atomic(settings_path=settings_path)
        # Bad id is named in the exception
        assert "ghost" in str(exc_info.value)

    def test_edge2b_no_active_set_raises_structured_error(self, tmp_path: Path):
        from fno.adapters.providers.loader import (
            MissingActiveProvider,
            read_active_provider_atomic,
        )

        settings_path = tmp_path / "config.toml"
        _write_settings(settings_path, {
            "config": {"providers": {"active": None, "records": []}},
        })

        with pytest.raises(MissingActiveProvider):
            read_active_provider_atomic(settings_path=settings_path)

    def test_pricing_slot_returns_when_present(self, tmp_path: Path):
        """The snapshot exposes a `pricing` field that is None when the
        record has no pricing entry, and a dict when it does. The schema
        for pricing lands in task 2.2; the slot must already exist on the
        snapshot type so subprocess env snapshots in task 2b.1 can carry
        it forward without a follow-up shape change."""
        from fno.adapters.providers.loader import read_active_provider_atomic

        settings_path = tmp_path / "config.toml"
        _write_settings(settings_path, {
            "config": {
                "providers": {
                    "active": "claude-primary",
                    "records": [
                        {
                            "id": "claude-primary",
                            "name": "Claude Primary",
                            "cli": "claude",
                            "auth": "oauth_dir",
                            "credentials_source": "~/.claude",
                            "pricing": {"input_per_million": 3.0, "output_per_million": 15.0},
                        }
                    ],
                }
            }
        })

        snap = read_active_provider_atomic(settings_path=settings_path)
        assert snap.pricing == {"input_per_million": 3.0, "output_per_million": 15.0}

    def test_pricing_slot_is_none_when_absent(self, tmp_path: Path):
        from fno.adapters.providers.loader import read_active_provider_atomic

        settings_path = tmp_path / "config.toml"
        self._seed(settings_path)
        snap = read_active_provider_atomic(settings_path=settings_path)
        assert snap.pricing is None


# ---------------------------------------------------------------------------
# Task 1.1: config.agents.<name>.provider — per-agent provider binding
# Part of: ab-978e93ed (per-agent sigma-review routing, Spec 3)
# ---------------------------------------------------------------------------

class TestLoadAgents:
    def test_no_agents_block_returns_empty_map(self, tmp_path: Path):
        """AC1-HP: settings.yaml with providers block but no config.agents returns empty dict."""
        from fno.adapters.providers.loader import load_providers

        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, _valid_providers_block())

        result = load_providers(repo_root=tmp_path)

        assert result.agents == {}

    def test_one_agent_resolves(self, tmp_path: Path):
        """AC2-HP: one agent entry with valid provider id exposes the binding."""
        from fno.adapters.providers.loader import load_providers

        base = _valid_providers_block()
        base["agents"] = {
            "code-reviewer": {"provider": "claude-primary"},
        }
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, base)

        result = load_providers(repo_root=tmp_path)

        assert "code-reviewer" in result.agents
        assert result.agents["code-reviewer"].provider == "claude-primary"

    def test_multiple_agents_all_resolve(self, tmp_path: Path):
        """AC3-HP: three agents with valid provider ids all appear in the map."""
        from fno.adapters.providers.loader import load_providers

        base = _valid_providers_block()
        base["agents"] = {
            "code-reviewer": {"provider": "claude-primary"},
            "silent-failure-hunter": {"provider": "gemini-backup"},
            "type-coverage-checker": {"provider": "claude-primary"},
        }
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, base)

        result = load_providers(repo_root=tmp_path)

        assert len(result.agents) == 3
        assert result.agents["code-reviewer"].provider == "claude-primary"
        assert result.agents["silent-failure-hunter"].provider == "gemini-backup"
        assert result.agents["type-coverage-checker"].provider == "claude-primary"

    def test_unknown_provider_id_rejected(self, tmp_path: Path):
        """AC4-ERR: agent referencing an id not in records raises ProviderConfigError
        naming both the agent name and the missing provider id."""
        from fno.adapters.providers.loader import load_providers
        from fno.adapters.providers.model import ProviderConfigError

        base = _valid_providers_block()
        base["agents"] = {
            "code-reviewer": {"provider": "nonexistent-provider"},
        }
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, base)

        with pytest.raises(ProviderConfigError) as exc_info:
            load_providers(repo_root=tmp_path)

        error_msg = str(exc_info.value)
        assert "code-reviewer" in error_msg
        assert "nonexistent-provider" in error_msg

    def test_extra_field_on_AgentProviderBinding_rejected(self, tmp_path: Path):
        """Fix 4 (panel HIGH): AgentProviderBinding has extra='forbid'; an unknown field
        in config.agents.<name> must raise ProviderConfigError naming the unknown field.

        Before the fix: config_obj.agents = parsed_agents was a post-construction field
        assignment that bypassed Pydantic validation. AgentProviderBinding.model_validate
        would raise for extra fields, but only if called at all. After restructuring the
        constructor call, validation is consistent.

        This test specifically guards AgentProviderBinding(extra='forbid') is enforced
        and the error surfaces via ProviderConfigError (not a raw pydantic.ValidationError).
        """
        from fno.adapters.providers.loader import load_providers
        from fno.adapters.providers.model import ProviderConfigError

        base = _valid_providers_block()
        base["agents"] = {
            "code-reviewer": {"provider": "claude-primary", "unknown_field": "x"},
        }
        settings = tmp_path / ".fno" / "config.toml"
        _write_settings(settings, base)

        with pytest.raises(ProviderConfigError) as exc_info:
            load_providers(repo_root=tmp_path)

        error_msg = str(exc_info.value)
        # The error must name the offending agent
        assert "code-reviewer" in error_msg
        # The error must reference the unknown field (Pydantic surfaces "unknown_field")
        assert "unknown_field" in error_msg

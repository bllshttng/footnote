"""Tests for the `fno route` verb family (ls / set / unset / env) - x-b0b4."""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents import model_routing as mr
from fno.config import ConfigBlock, ModelProvider, ModelRoutingBlock, SettingsModel
from fno.route_cli import route_app

runner = CliRunner()


def _settings(**block_kwargs: object) -> SettingsModel:
    return SettingsModel(
        config=ConfigBlock(model_routing=ModelRoutingBlock(**block_kwargs))
    )


@pytest.fixture
def project_scope(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Isolate --local (project-scope) config writes into a tmp repo root."""
    monkeypatch.setattr("fno.paths.resolve_repo_root", lambda: tmp_path)
    return tmp_path


# ---------------------------------------------------------------------------
# ls / build_route_table (AC1-HP, AC1-UI)
# ---------------------------------------------------------------------------


def test_route_table_shows_builtins_config_build_and_protected() -> None:
    settings = _settings(
        providers={
            "zai-openai": ModelProvider(
                protocol="openai",
                base_url="https://api.z.ai/api/coding/paas/v4",
                api_key_env="ZAI_API_KEY",
            )
        },
        roles={"codex-verify": "zai-openai,glm-4.6"},
    )
    rows = mr.build_route_table(settings=settings, env={"ZAI_API_KEY": "k"})
    by_role = {r["role"]: r for r in rows}

    for r in ("coordinate", "tidy", "orient", "consolidate", "post-merge"):
        assert by_role[r]["target"] == "zai,glm-5.2"
    assert by_role["codex-verify"]["target"] == "zai-openai,glm-4.6"
    assert by_role["codex-verify"]["protocol"] == "openai"
    assert by_role["build"]["target"] == "unconfigured"
    for p in ("implement", "review-verdict"):
        assert "never routed" in by_role[p]["target"]
    # Every row carries the canonical 5-field shape (AC1-UI).
    for r in rows:
        assert set(r) == {"role", "target", "protocol", "key", "assigned_by"}


def test_route_table_key_status_names_source_and_missing() -> None:
    found = mr.build_route_table(settings=_settings(), env={"ZAI_API_KEY": "k"})
    assert any("found via ZAI_API_KEY" in r["key"] for r in found)
    missing = mr.build_route_table(settings=_settings(), env={})
    assert any(r["key"].startswith("MISSING (checked ZAI_API_KEY") for r in missing)


def test_ls_json_matches_table() -> None:
    res = runner.invoke(route_app, ["ls", "-J"])
    assert res.exit_code == 0
    data = json.loads(res.stdout)
    assert isinstance(data, list) and data
    for row in data:
        assert set(row) == {"role", "target", "protocol", "key", "assigned_by"}


def test_ls_text_has_header() -> None:
    res = runner.invoke(route_app, ["ls"])
    assert res.exit_code == 0
    assert "ROLE" in res.stdout and "ASSIGNED-BY" in res.stdout


# ---------------------------------------------------------------------------
# set (AC1-ERR, AC2-ERR, happy) - isolated to a tmp project scope
# ---------------------------------------------------------------------------


def _roles_on_disk(repo_root: Path) -> dict:
    from fno.config.writer import read_scope_value

    return read_scope_value(
        "model_routing.roles", scope="project", repo_root=repo_root
    ) or {}


def test_set_build_writes_roles(project_scope: Path) -> None:
    res = runner.invoke(route_app, ["set", "build", "zai,glm-5.2", "--local"])
    assert res.exit_code == 0, res.stdout
    assert _roles_on_disk(project_scope) == {"build": "zai,glm-5.2"}


def test_set_preserves_existing_roles(project_scope: Path) -> None:
    runner.invoke(route_app, ["set", "tidy", "zai,glm-4.7", "--local"])
    runner.invoke(route_app, ["set", "build", "zai,glm-5.2", "--local"])
    assert _roles_on_disk(project_scope) == {
        "tidy": "zai,glm-4.7",
        "build": "zai,glm-5.2",
    }


def test_set_one_m_suffix_passes(project_scope: Path) -> None:
    res = runner.invoke(route_app, ["set", "build", "zai,glm-5.2[1m]", "--local"])
    assert res.exit_code == 0, res.stdout
    assert _roles_on_disk(project_scope)["build"] == "zai,glm-5.2[1m]"


def test_set_protected_role_refused_no_write(project_scope: Path) -> None:
    res = runner.invoke(route_app, ["set", "implement", "zai,glm-5.2", "--local"])
    assert res.exit_code == 2
    assert "protected" in res.output.lower()
    assert not (project_scope / ".fno" / "config.toml").exists()


def test_set_unknown_provider_refused_no_write(project_scope: Path) -> None:
    res = runner.invoke(route_app, ["set", "build", "zia,glm-5.2", "--local"])
    assert res.exit_code == 2
    assert "unknown provider" in res.output.lower()
    assert not (project_scope / ".fno" / "config.toml").exists()


def test_set_malformed_target_refused(project_scope: Path) -> None:
    res = runner.invoke(route_app, ["set", "build", "zai-only", "--local"])
    assert res.exit_code == 2
    assert not (project_scope / ".fno" / "config.toml").exists()


# ---------------------------------------------------------------------------
# unset (AC1-EDGE, idempotent no-op)
# ---------------------------------------------------------------------------


def test_unset_removes_role(project_scope: Path) -> None:
    runner.invoke(route_app, ["set", "build", "zai,glm-5.2", "--local"])
    runner.invoke(route_app, ["set", "tidy", "zai,glm-4.7", "--local"])
    res = runner.invoke(route_app, ["unset", "build", "--local"])
    assert res.exit_code == 0, res.stdout
    assert _roles_on_disk(project_scope) == {"tidy": "zai,glm-4.7"}


def test_unset_unconfigured_is_noop(project_scope: Path) -> None:
    res = runner.invoke(route_app, ["unset", "build", "--local"])
    assert res.exit_code == 0
    assert "not configured" in res.stdout.lower()


def test_unset_builtin_role_mentions_default(project_scope: Path) -> None:
    res = runner.invoke(route_app, ["unset", "tidy", "--local"])
    assert res.exit_code == 0
    assert "built-in" in res.stdout.lower()


# ---------------------------------------------------------------------------
# env (AC5-HP, AC2-FR) - built-in zai provider, no config needed
# ---------------------------------------------------------------------------


def test_env_explicit_emits_export_block(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    res = runner.invoke(route_app, ["env", "zai,glm-5.2"])
    assert res.exit_code == 0
    out = res.stdout
    assert "export ANTHROPIC_BASE_URL=" in out
    assert "export ANTHROPIC_AUTH_TOKEN=" in out
    assert "glm-5.2" in out


def test_env_unsets_parent_anthropic_creds_before_exports(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # A parent ANTHROPIC_API_KEY / OAuth token in the invoking shell would
    # otherwise win over the routed AUTH_TOKEN and silently bill Anthropic. env
    # must clear them (parity with bg_create), BEFORE the exports.
    monkeypatch.setenv("ZAI_API_KEY", "zk-live")
    res = runner.invoke(route_app, ["env", "zai,glm-5.2"])
    assert res.exit_code == 0
    out = res.stdout
    assert "unset ANTHROPIC_API_KEY" in out
    assert "unset CLAUDE_CODE_OAUTH_TOKEN" in out
    assert out.index("unset ANTHROPIC_API_KEY") < out.index("export ANTHROPIC_AUTH_TOKEN")


def test_env_missing_key_emits_no_unset_or_export(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Fail-closed: nothing on stdout at all, not even the unset lines.
    monkeypatch.delenv("ZAI_API_KEY", raising=False)
    res = runner.invoke(route_app, ["env", "zai,glm-5.2"])
    assert res.exit_code == 1
    assert "unset " not in res.stdout
    assert "export " not in res.stdout


@pytest.mark.parametrize("target", ["zai,glm 5.2", "zai,glm\n5.2", "z ai,glm-5.2"])
def test_set_rejects_whitespace_in_tokens(
    target: str, project_scope: Path
) -> None:
    res = runner.invoke(route_app, ["set", "build", target, "--local"])
    assert res.exit_code == 2
    assert not (project_scope / ".fno" / "config.toml").exists()


def test_unset_surfaces_malformed_config(project_scope: Path) -> None:
    # A malformed scope file must NOT be reported as a clean "not configured"
    # no-op; the read now raises and route unset exits non-zero.
    cfg = project_scope / ".fno" / "config.toml"
    cfg.parent.mkdir(parents=True, exist_ok=True)
    cfg.write_text("this is = = not valid toml [[[\n", encoding="utf-8")
    res = runner.invoke(route_app, ["unset", "build", "--local"])
    assert res.exit_code != 0
    assert "malformed" in res.output.lower() or "error" in res.output.lower()


def test_env_missing_key_fails_closed_no_stdout(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("ZAI_API_KEY", raising=False)
    res = runner.invoke(route_app, ["env", "zai,glm-5.2"])
    assert res.exit_code == 1
    # No export lines emitted (nothing to half-eval).
    assert "export " not in res.stdout
    # The reason names the checked variable on stderr.
    assert "ZAI_API_KEY" in res.output


def test_env_malformed_target(monkeypatch: pytest.MonkeyPatch) -> None:
    res = runner.invoke(route_app, ["env", "zai,"])
    assert res.exit_code == 2

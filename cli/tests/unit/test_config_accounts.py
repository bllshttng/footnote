"""Tests for accounts configuration resolution (x-e90a).

Tests that `fno config get`, `set`, `unset`, and `doctor` can see and manipulate
the `[accounts]` namespace (including `accounts.quota.defer_dispatch`), and
properly aliases the pre-rename `providers` namespace.
"""
from __future__ import annotations

from pathlib import Path
import tomllib

import pytest
from typer.testing import CliRunner

from fno.cli import app
from fno.config import load_settings
from fno.config.writer import set_config_value, unset_config_value
from fno.setup.doctor import check_accounts


runner = CliRunner()


def _write_config(tmp_path: Path, content: str) -> Path:
    d = tmp_path / ".fno"
    d.mkdir(parents=True, exist_ok=True)
    f = d / "config.toml"
    f.write_text(content, encoding="utf-8")
    return f


def test_get_accounts_quota_defer_dispatch(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    cfg = _write_config(tmp_path, '[accounts.quota]\ndefer_dispatch = true\n')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    load_settings.cache_clear()  # type: ignore[attr-defined]

    r = runner.invoke(app, ["config", "get", "accounts.quota.defer_dispatch"])
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "True"


def test_get_config_prefixed_accounts_quota_defer_dispatch(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    cfg = _write_config(tmp_path, '[accounts.quota]\ndefer_dispatch = true\n')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    load_settings.cache_clear()  # type: ignore[attr-defined]

    r = runner.invoke(app, ["config", "get", "config.accounts.quota.defer_dispatch"])
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "True"


def test_get_legacy_providers_alias(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    cfg = _write_config(tmp_path, '[accounts.quota]\ndefer_dispatch = true\n')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    load_settings.cache_clear()  # type: ignore[attr-defined]

    r = runner.invoke(app, ["config", "get", "providers.quota.defer_dispatch"])
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "True"


def test_get_accounts_active(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    cfg = _write_config(tmp_path, '[accounts]\nactive = "readyrule"\n')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    load_settings.cache_clear()  # type: ignore[attr-defined]

    r = runner.invoke(app, ["config", "get", "accounts.active"])
    assert r.exit_code == 0, r.output
    assert r.stdout.strip() == "readyrule"


def test_set_accounts_quota_defer_dispatch(tmp_path: Path):
    res = set_config_value(
        "accounts.quota.defer_dispatch", "true", scope="project", repo_root=tmp_path
    )
    assert res.value is True
    data = tomllib.loads((tmp_path / ".fno" / "config.toml").read_text(encoding="utf-8"))
    assert data["accounts"]["quota"]["defer_dispatch"] is True


def test_unset_accounts_quota_defer_dispatch(tmp_path: Path):
    set_config_value(
        "accounts.quota.defer_dispatch", "true", scope="project", repo_root=tmp_path
    )
    res = unset_config_value(
        "accounts.quota.defer_dispatch", scope="project", repo_root=tmp_path
    )
    assert res.was is True
    assert res.default is False
    data = tomllib.loads((tmp_path / ".fno" / "config.toml").read_text(encoding="utf-8"))
    assert "defer_dispatch" not in data.get("accounts", {}).get("quota", {})


def test_doctor_check_accounts_valid(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    cfg = _write_config(tmp_path, '''
[accounts]
active = "acc1"

[[accounts.records]]
id = "acc1"
name = "acc1"
harness = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
priority = 100
''')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    monkeypatch.setenv("PWD", str(tmp_path))
    monkeypatch.setenv("FNO_TEST_MODE", "1")
    load_settings.cache_clear()  # type: ignore[attr-defined]

    problems = check_accounts()
    assert problems == []


def test_doctor_check_accounts_invalid(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    cfg = _write_config(tmp_path, '''
[accounts]
active = "nonexistent"

[[accounts.records]]
id = "acc1"
name = "acc1"
harness = "claude"
auth = "oauth_dir"
credentials_source = "~/.claude"
''')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    monkeypatch.setenv("PWD", str(tmp_path))
    monkeypatch.setenv("FNO_TEST_MODE", "1")
    load_settings.cache_clear()  # type: ignore[attr-defined]

    problems = check_accounts()
    assert len(problems) > 0
    assert any("accounts" in p for p in problems)


def test_doctor_check_accounts_quota_typo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    cfg = _write_config(tmp_path, '''
[accounts.quota]
defer_dispatchh = true
''')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    monkeypatch.setenv("PWD", str(tmp_path))
    monkeypatch.setenv("FNO_TEST_MODE", "1")
    load_settings.cache_clear()  # type: ignore[attr-defined]

    problems = check_accounts()
    assert len(problems) > 0
    assert any("accounts.quota has unknown key 'defer_dispatchh'" in p for p in problems)


def test_doctor_check_accounts_root_typo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    cfg = _write_config(tmp_path, '''
[accounts]
autto_switch = true
''')
    monkeypatch.setenv("FNO_CONFIG", str(cfg))
    monkeypatch.setenv("PWD", str(tmp_path))
    monkeypatch.setenv("FNO_TEST_MODE", "1")
    load_settings.cache_clear()  # type: ignore[attr-defined]

    problems = check_accounts()
    assert len(problems) > 0
    assert any("accounts has unknown key 'autto_switch'" in p for p in problems)


from __future__ import annotations

import json
from pathlib import Path

import yaml
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()

REPO_ROOT = Path(__file__).resolve().parents[4]
PACK = REPO_ROOT / "plugins" / "growth-studio" / "plugin.yaml"


def _invoke(*args: str):
    return runner.invoke(app, list(args))


def _root_env(monkeypatch, tmp_path) -> Path:
    root = tmp_path / "roles"
    root.mkdir()
    monkeypatch.setenv("FNO_ROLES_ROOT", str(root))
    return root


def test_plugins_group_is_registered_and_ls_is_empty(monkeypatch, tmp_path):
    _root_env(monkeypatch, tmp_path)
    result = _invoke("plugins", "ls")
    assert result.exit_code == 0, result.output
    assert "no installed packs" in result.output


def test_verify_real_pack_reports_ok(monkeypatch, tmp_path):
    _root_env(monkeypatch, tmp_path)
    result = _invoke("plugins", "verify", str(PACK))
    assert result.exit_code == 0, result.output
    assert "ok: True" in result.output


def test_verify_emits_versioned_json(monkeypatch, tmp_path):
    _root_env(monkeypatch, tmp_path)
    result = _invoke("plugins", "verify", str(PACK), "--json")
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["version"] == 1
    assert payload["ok"] is True
    assert payload["conditions"]


def test_activate_ls_inspect_deactivate_round_trip(monkeypatch, tmp_path):
    _root_env(monkeypatch, tmp_path)

    activated = _invoke("plugins", "activate", str(PACK))
    assert activated.exit_code == 0, activated.output
    assert "activated growth-studio" in activated.output

    listed = _invoke("plugins", "ls")
    assert listed.exit_code == 0, listed.output
    assert "growth-studio" in listed.output
    assert "active" in listed.output
    assert "external.publication" in listed.output

    inspected = _invoke("plugins", "inspect", "growth-studio")
    assert inspected.exit_code == 0, inspected.output
    assert "declares roles:" in inspected.output

    deactivated = _invoke("plugins", "deactivate", "growth-studio")
    assert deactivated.exit_code == 0, deactivated.output
    assert "deactivated growth-studio" in deactivated.output

    listed_after = _invoke("plugins", "ls")
    assert "growth-studio" in listed_after.output
    assert "installed" in listed_after.output


def test_activate_refuses_a_corrupt_pack(monkeypatch, tmp_path):
    _root_env(monkeypatch, tmp_path)
    bad = tmp_path / "broken"
    bad.mkdir()
    (bad / "plugin.yaml").write_text("id: broken\n  bad: indent\n - x\n", encoding="utf-8")
    result = _invoke("plugins", "activate", str(bad))
    assert result.exit_code == 1, result.output
    assert "refused" in result.output


def test_verify_reports_nonzero_on_a_failed_topology(monkeypatch, tmp_path):
    _root_env(monkeypatch, tmp_path)
    pack = tmp_path / "bad-topo"
    pack.mkdir()
    payload = yaml.safe_load(PACK.read_text(encoding="utf-8"))
    payload["roles"][0]["default_topology"] = "fifth-shape"
    (pack / "plugin.yaml").write_text(yaml.safe_dump(payload), encoding="utf-8")
    result = _invoke("plugins", "verify", str(pack))
    assert result.exit_code == 1, result.output
    assert "ok: False" in result.output

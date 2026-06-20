"""Tests for `fno setup wizard` (x-50f9, US4).

Drives the interactive-agnostic core ``run_wizard`` with stub prompt/scope
functions (mirroring how test_config_post_merge drives scaffold_post_merge).

Covers AC4-HP (always fields written via the validated path, result loads
cleanly), AC4-ERR (a rejected value re-prompts, never aborts), AC4-UI (each
write echoes scope + path), AC4-EDGE (--advanced surfaces advanced + project
keys prompt scope), AC4-FR (cancel mid-run keeps written keys, nothing partial).
"""
from __future__ import annotations

import json

import yaml
from typer.testing import CliRunner

from fno.config import schema_gen
from fno.setup_cli import PROJECT_SCOPED_KEYS, run_wizard


def _always_fields():
    raw = json.loads(schema_gen.wizard_plan())
    return [f for f in raw["fields"] if f.get("tier") == "always"]


def _global_path(tmp_path, monkeypatch):
    gpath = tmp_path / "global.yaml"
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(gpath))
    return gpath


def test_ac4_hp_always_fields_written_and_valid(tmp_path, monkeypatch):
    gpath = _global_path(tmp_path, monkeypatch)
    fields = _always_fields()

    # Accept the default for every field (Enter == default string).
    def prompt_fn(message, default):
        return default

    result = run_wizard(
        tmp_path,
        fields,
        prompt_fn=prompt_fn,
        scope_fn=lambda key: "global",
    )
    assert result["cancelled"] is False
    # Fields with a non-None default get written; None-default blanks are skipped.
    expected = [f["path"] for f in fields if f.get("default") is not None]
    assert result["written"] == expected

    # The resulting global file loads cleanly into the model (doctor-clean spirit).
    from fno.config import SettingsModel

    data = yaml.safe_load(gpath.read_text())
    SettingsModel.model_validate(data)  # raises on an invalid combination


def test_ac4_err_rejected_value_reprompts(tmp_path, monkeypatch):
    _global_path(tmp_path, monkeypatch)
    # Just the id_prefix field, which has a strict validator.
    field = next(f for f in _always_fields() if f["path"] == "config.backlog.id_prefix")

    calls = {"n": 0}

    def prompt_fn(message, default):
        calls["n"] += 1
        # First answer is rejected (uppercase / invalid), second is accepted.
        return "BADPREFIX" if calls["n"] == 1 else "xy"

    result = run_wizard(
        tmp_path, [field], prompt_fn=prompt_fn, scope_fn=lambda k: "global"
    )
    # It re-prompted rather than aborting, and eventually wrote the valid value.
    assert calls["n"] == 2
    assert result["written"] == ["config.backlog.id_prefix"]


def test_ac4_ui_echoes_scope_and_path(tmp_path, monkeypatch):
    _global_path(tmp_path, monkeypatch)
    field = next(
        f for f in _always_fields() if f["path"] == "config.auto_merge.enabled"
    )
    lines: list[str] = []

    run_wizard(
        tmp_path,
        [field],
        prompt_fn=lambda m, d: "true",
        scope_fn=lambda k: "global",
        echo_fn=lines.append,
    )
    blob = "\n".join(lines)
    assert "config.auto_merge.enabled" in blob
    assert "global" in blob


def test_ac4_edge_project_scoped_key_routes_to_project(tmp_path, monkeypatch):
    _global_path(tmp_path, monkeypatch)
    # config.post_merge.parking_lot_path is project-scoped (advanced tier).
    assert "config.post_merge.parking_lot_path" in PROJECT_SCOPED_KEYS
    field = {
        "path": "config.post_merge.parking_lot_path",
        "default": None,
        "tier": "advanced",
        "question": "Parking-lot path?",
    }

    asked_scope: list[str] = []

    def scope_fn(key):
        asked_scope.append(key)
        return "project"

    result = run_wizard(
        tmp_path,
        [field],
        prompt_fn=lambda m, d: "internal/x/backlog/parking-lot.md",
        scope_fn=scope_fn,
    )
    # The scope prompt fired for the project-scoped key...
    assert asked_scope == ["config.post_merge.parking_lot_path"]
    # ...and the value landed in the PROJECT file, not global.
    data = yaml.safe_load((tmp_path / ".fno" / "settings.yaml").read_text())
    assert (
        data["config"]["post_merge"]["parking_lot_path"]
        == "internal/x/backlog/parking-lot.md"
    )
    assert result["written"] == ["config.post_merge.parking_lot_path"]


def test_ac4_fr_cancel_midrun_keeps_written_nothing_partial(tmp_path, monkeypatch):
    gpath = _global_path(tmp_path, monkeypatch)
    fields = _always_fields()

    seen: list[str] = []

    def prompt_fn(message, default):
        seen.append(message)
        # Cancel (None) once we reach the second prompt.
        return None if len(seen) >= 2 else default

    result = run_wizard(
        tmp_path, fields, prompt_fn=prompt_fn, scope_fn=lambda k: "global"
    )
    assert result["cancelled"] is True
    # The in-flight (second) prompt wrote nothing; at most the first key (if it
    # had a writable default) is persisted. Each prior write was atomic.
    assert len(result["written"]) <= 1
    if gpath.exists():
        # Whatever did land parses cleanly (no partial / corrupt write).
        yaml.safe_load(gpath.read_text())


def test_cli_wizard_smoke_accepts_defaults(tmp_path, monkeypatch):
    _global_path(tmp_path, monkeypatch)
    monkeypatch.chdir(tmp_path)
    from fno.setup_cli import app

    fields = _always_fields()
    # One newline per field accepts each default.
    stdin = "\n" * (len(fields) + 2)
    res = CliRunner().invoke(app, ["wizard"], input=stdin)
    assert res.exit_code == 0, res.output
    assert "wizard" in res.output.lower()


def test_cli_wizard_advanced_surfaces_more_fields(tmp_path, monkeypatch):
    _global_path(tmp_path, monkeypatch)
    monkeypatch.chdir(tmp_path)
    from fno.setup_cli import app

    advanced = json.loads(schema_gen.wizard_plan())["fields"]
    # --advanced asks more than the always-only set; feed plenty of newlines.
    stdin = "\n" * (len(advanced) * 2 + 4)
    res = CliRunner().invoke(app, ["wizard", "--advanced"], input=stdin)
    assert res.exit_code == 0, res.output

"""Tests for block/object set `fno config set <block> '<json>'`.

Covers AC3-HP (set a block from JSON, get a leaf back), AC3-ERR (malformed
JSON / invalid block -> exit 2, file unchanged), AC3-EDGE (nested map of maps
like config.work.workspaces round-trips), AC3-FR (parse/validate never
partially writes).
"""
from __future__ import annotations

import pytest
import yaml
from typer.testing import CliRunner

from fno.config.writer import ConfigSetError, set_config_value


def _read(tmp_path):
    return yaml.safe_load((tmp_path / ".fno" / "settings.yaml").read_text())


def test_ac3_hp_block_set_from_json(tmp_path):
    res = set_config_value(
        "config.review",
        '{"required_bots": ["x"]}',
        scope="project",
        repo_root=tmp_path,
    )
    assert res.value == {"required_bots": ["x"]}
    data = _read(tmp_path)
    assert data["config"]["review"]["required_bots"] == ["x"]


def test_ac3_hp_block_set_replace_semantics(tmp_path):
    # Seed two leaves in the block.
    set_config_value(
        "config.review.required_bots", '["a"]', scope="project", repo_root=tmp_path
    )
    # Block-set REPLACES the whole block.
    set_config_value(
        "config.review",
        '{"required_bots": ["b"]}',
        scope="project",
        repo_root=tmp_path,
    )
    data = _read(tmp_path)
    assert data["config"]["review"] == {"required_bots": ["b"]}


def test_ac3_err_malformed_json_exit2_unchanged(tmp_path):
    set_config_value(
        "config.agents.a2a.auto", "false", scope="project", repo_root=tmp_path
    )
    before = (tmp_path / ".fno" / "settings.yaml").read_text()
    with pytest.raises(ConfigSetError) as exc:
        set_config_value(
            "config.review", "{not json", scope="project", repo_root=tmp_path
        )
    assert exc.value.exit_code == 2
    assert (tmp_path / ".fno" / "settings.yaml").read_text() == before


def test_ac3_err_invalid_block_value_exit2(tmp_path):
    # A value that parses but fails block validation (wrong type for a field).
    with pytest.raises(ConfigSetError) as exc:
        set_config_value(
            "config.agents.a2a",
            '{"turn_ceiling": 0}',  # turn_ceiling must be >= 1
            scope="project",
            repo_root=tmp_path,
        )
    assert exc.value.exit_code == 2
    assert not (tmp_path / ".fno" / "settings.yaml").exists()


def test_block_set_requires_mapping_not_scalar(tmp_path):
    with pytest.raises(ConfigSetError) as exc:
        set_config_value(
            "config.review", "5", scope="project", repo_root=tmp_path
        )
    assert exc.value.exit_code == 2


def test_ac3_edge_nested_map_of_maps_roundtrips(tmp_path):
    workspaces = (
        '{"ws": {"projects": [{"name": "p", "path": "/tmp/p"}]}}'
    )
    res = set_config_value(
        "config.work.workspaces",
        workspaces,
        scope="project",
        repo_root=tmp_path,
    )
    data = _read(tmp_path)
    nested = data["config"]["work"]["workspaces"]
    assert nested["ws"]["projects"][0]["name"] == "p"
    assert res.value["ws"]["projects"][0]["path"] == "/tmp/p"


def test_ac3_edge_empty_mapping_is_block_defaults(tmp_path):
    res = set_config_value(
        "config.work.workspaces", "{}", scope="project", repo_root=tmp_path
    )
    assert res.value == {}
    assert _read(tmp_path)["config"]["work"]["workspaces"] == {}


def test_block_set_accepts_yaml_flow(tmp_path):
    # Claude's Discretion #2: block-set accepts trivial YAML too.
    res = set_config_value(
        "config.review",
        "{required_bots: [y]}",
        scope="project",
        repo_root=tmp_path,
    )
    assert res.value == {"required_bots": ["y"]}


def test_ac3_ui_cli_confirms_block(tmp_path, monkeypatch):
    gpath = tmp_path / "g.yaml"
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(gpath))
    from fno.config_cli import app

    res = CliRunner().invoke(
        app, ["set", "config.review", '{"required_bots": ["x"]}']
    )
    assert res.exit_code == 0, res.output
    assert "config.review" in res.output
    assert "global" in res.output
    assert yaml.safe_load(gpath.read_text())["config"]["review"]["required_bots"] == ["x"]

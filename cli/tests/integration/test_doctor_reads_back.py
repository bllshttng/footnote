"""`fno config doctor` reads the config back the way an operator reads it (x-b052).

Every test here pairs a positive marker with the input that makes the same
check fail. No test asserts only that an error did not appear: an absence has
three explanations and only one of them is the outcome.
"""
from __future__ import annotations

from pathlib import Path
from typing import Generator

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()
_ENV = {
    "COLUMNS": "240",
    "NO_COLOR": "1",
    "TERM": "dumb",
    "FNO_SKIP_MIGRATION": "1",
    "FNO_TEST_MODE": "1",
}


@pytest.fixture(autouse=True)
def _isolate(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Generator[None, None, None]:
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.delenv("FNO_CONFIG", raising=False)
    yield


def _doctor(config: Path, **extra: str):
    env = {**_ENV, "FNO_CONFIG": str(config), **extra}
    return runner.invoke(app, ["config", "doctor"], env=env)


def _write(path: Path, body: str) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    return path


# --- AC1: an unreadable settings file refuses -----------------------------


def test_a_config_toml_holding_yaml_refuses_and_names_the_parse_error(tmp_path: Path) -> None:
    f = _write(tmp_path / "config.toml", "schema_version: 1\nconfig:\n  state_dir: ~/.fno/\n")
    result = _doctor(f)
    assert result.exit_code == 1, result.output
    assert "[doctor] 1 unreadable settings file(s):" in result.output
    assert str(f) in result.output
    assert "Expected '='" in result.output


def test_a_well_formed_config_toml_is_clean(tmp_path: Path) -> None:
    """AC1 negative control."""
    f = _write(tmp_path / "config.toml", 'schema_version = 1\nstate_dir = "%s"\n' % (tmp_path / ".fno"))
    result = _doctor(f)
    assert result.exit_code == 0, result.output
    assert "[doctor] OK; no suspicious paths detected." in result.output
    assert "unreadable settings file" not in result.output


# --- AC2: a non-mapping document refuses and names its type ---------------


def test_a_settings_yaml_holding_a_list_names_the_type_it_parsed_to(tmp_path: Path) -> None:
    f = _write(tmp_path / "settings.yaml", "- just\n- a\n- list\n")
    result = _doctor(f)
    assert result.exit_code == 1, result.output
    assert str(f) in result.output
    assert "parsed to a list, not a table" in result.output


def test_an_empty_config_toml_stays_legal(tmp_path: Path) -> None:
    """AC2 negative control: an empty file contributes nothing and that is fine."""
    f = _write(tmp_path / "config.toml", "")
    result = _doctor(f)
    assert result.exit_code == 0, result.output
    assert "[doctor] OK" in result.output


# --- AC3 / AC4: unknown keys, named with their file ------------------------


def test_a_typod_section_is_reported_with_the_file_that_holds_it(tmp_path: Path) -> None:
    f = _write(tmp_path / "config.toml", "schema_version = 1\n[reveiw]\ncross_model = true\n")
    result = _doctor(f)
    assert result.exit_code == 1, result.output
    line = next(ln for ln in result.output.splitlines() if "reveiw.cross_model" in ln)
    assert "ignored" in line
    assert str(f) in line


def test_a_correctly_spelled_section_reports_nothing(tmp_path: Path) -> None:
    """AC3 negative control."""
    f = _write(
        tmp_path / "config.toml",
        'schema_version = 1\nstate_dir = "%s"\n[review]\nmax_rounds = 2\n' % (tmp_path / ".fno"),
    )
    result = _doctor(f)
    assert result.exit_code == 0, result.output
    assert "ignored" not in result.output


def test_the_wrong_section_pair_names_the_key_the_operator_meant(tmp_path: Path) -> None:
    """AC4: `[agents] max_lanes` reads as a lane cap and sets no lane cap."""
    f = _write(tmp_path / "config.toml", "schema_version = 1\n[agents]\nmax_lanes = 4\n")
    result = _doctor(f)
    assert result.exit_code == 1, result.output
    line = next(ln for ln in result.output.splitlines() if "agents.max_lanes" in ln)
    assert "parallel.max_lanes" in line


def test_the_right_section_resolves_and_is_clean(tmp_path: Path) -> None:
    """AC4 negative control: the key in its real section sets the value."""
    f = _write(
        tmp_path / "config.toml",
        'schema_version = 1\nstate_dir = "%s"\n[parallel]\nmax_lanes = 4\n' % (tmp_path / ".fno"),
    )
    result = _doctor(f)
    assert result.exit_code == 0, result.output

    from fno.config import load_settings, resolve_source

    import os

    os.environ["FNO_CONFIG"] = str(f)
    try:
        assert load_settings().parallel.max_lanes == 4
        decided = resolve_source("parallel.max_lanes")
        assert decided is not None and decided[0].resolve() == f.resolve()
    finally:
        os.environ.pop("FNO_CONFIG", None)


# --- AC5: a switch enabled with an empty population ------------------------


def test_cross_model_enabled_with_no_dispatchable_peer_is_named(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from fno.setup import doctor as doctor_mod

    f = _write(tmp_path / "config.toml", "schema_version = 1\n[review.cross_model]\nenabled = true\n")
    monkeypatch.setenv("FNO_CONFIG", str(f))
    monkeypatch.setattr(
        "fno.review.provider_resolution.available_provider_kinds", lambda **_: ["claude"]
    )
    problems = doctor_mod.check_enabled_with_empty_population()
    assert len(problems) == 1, problems
    assert "review.cross_model.enabled is true" in problems[0]
    assert "available reviewer kinds: claude" in problems[0]


def test_cross_model_with_a_real_peer_is_clean(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC5 negative control, with the resolver itself pinned.

    The second assertion is the positive control on the STUB: a stub that
    returned claude alone would make the first assertion pass for the wrong
    reason.
    """
    from fno.review import provider_resolution as pr
    from fno.setup import doctor as doctor_mod

    f = _write(tmp_path / "config.toml", "schema_version = 1\n[review.cross_model]\nenabled = true\n")
    monkeypatch.setenv("FNO_CONFIG", str(f))
    monkeypatch.setattr(pr, "available_provider_kinds", lambda **_: ["claude", "codex"])
    assert doctor_mod.check_enabled_with_empty_population() == []
    assert len(pr.available_provider_kinds()) > 1


def test_cross_model_disabled_is_clean(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from fno.setup import doctor as doctor_mod

    f = _write(tmp_path / "config.toml", "schema_version = 1\n")
    monkeypatch.setenv("FNO_CONFIG", str(f))
    monkeypatch.setattr(
        "fno.review.provider_resolution.available_provider_kinds", lambda **_: ["claude"]
    )
    assert doctor_mod.check_enabled_with_empty_population() == []


# --- AC6: every printed value names its decider ----------------------------


def test_a_set_path_key_names_the_file_that_decided_it(tmp_path: Path) -> None:
    graph = tmp_path / "graph.json"
    f = _write(
        tmp_path / "config.toml",
        'schema_version = 1\nstate_dir = "%s"\n[paths]\ngraph_json = "%s"\n'
        % (tmp_path / ".fno", graph),
    )
    result = _doctor(f)
    assert result.exit_code == 0, result.output
    line = next(ln for ln in result.output.splitlines() if "  graph_json:" in ln)
    assert f"set in {f}" in line


def test_an_unset_path_key_reads_default(tmp_path: Path) -> None:
    """AC6 negative control."""
    f = _write(tmp_path / "config.toml", 'schema_version = 1\nstate_dir = "%s"\n' % (tmp_path / ".fno"))
    result = _doctor(f)
    assert result.exit_code == 0, result.output
    line = next(ln for ln in result.output.splitlines() if "  graph_json:" in ln)
    assert line.endswith("(config.paths.graph_json default)")
    assert "set in" not in line


# --- AC7: the settings-source line lists contributors, not presences -------


def test_the_source_line_skips_an_unreadable_higher_priority_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    repo = tmp_path / "repo"
    project = _write(repo / ".fno" / "config.toml", "not = = toml\n")
    glob = _write(tmp_path / "home" / ".fno" / "config.toml", 'schema_version = 1\nstate_dir = "%s"\n' % (tmp_path / ".fno"))
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo))
    env = {
        **_ENV,
        "FNO_REPO_ROOT": str(repo),
        "FNO_GLOBAL_SETTINGS_PATH": str(glob.with_name("settings.yaml")),
    }
    result = runner.invoke(app, ["config", "doctor"], env=env)
    source = next(ln for ln in result.output.splitlines() if "settings source:" in ln)
    assert str(glob) in source
    assert str(project) not in source


def test_the_source_line_lists_both_valid_files_highest_first(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """AC7 negative control: two readable files both contribute, in order."""
    repo = tmp_path / "repo"
    project = _write(repo / ".fno" / "config.toml", "schema_version = 1\n")
    glob = _write(tmp_path / "home" / ".fno" / "config.toml", 'schema_version = 1\nstate_dir = "%s"\n' % (tmp_path / ".fno"))
    monkeypatch.setenv("FNO_REPO_ROOT", str(repo))
    env = {
        **_ENV,
        "FNO_REPO_ROOT": str(repo),
        "FNO_GLOBAL_SETTINGS_PATH": str(glob.with_name("settings.yaml")),
    }
    result = runner.invoke(app, ["config", "doctor"], env=env)
    source = next(ln for ln in result.output.splitlines() if "settings source:" in ln)
    assert source.index(str(project)) < source.index(str(glob))


# --- the check that had stopped running ------------------------------------


def test_wip_caps_are_read_from_config_toml_not_only_settings_yaml(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """After the yaml-to-toml migration only the config.toml exists, so a check
    reading `settings.yaml` alone had been a no-op on every migrated machine."""
    from fno.setup.doctor import check_wip_caps

    toml = _write(tmp_path / "config.toml", '[kanban.wip_caps]\nnow = "20"\n')
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(toml.with_name("settings.yaml")))
    problems = check_wip_caps()
    assert len(problems) == 1, problems
    assert "'now'" in problems[0]


def test_wip_caps_in_config_toml_can_be_clean(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Negative control on the same reader: a valid cap in the same file."""
    from fno.setup.doctor import check_wip_caps

    toml = _write(tmp_path / "config.toml", "[kanban.wip_caps]\nnow = 20\n")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(toml.with_name("settings.yaml")))
    assert check_wip_caps() == []


# --- the report stays readable on a real machine ---------------------------


def test_a_dict_keyed_block_is_not_read_as_a_typo(tmp_path: Path) -> None:
    """`agents.profiles.<verb>` keys are operator-chosen names, not field names.

    The walker resolved `dict[str, SpawnProfileBlock]` to its VALUE model and
    then checked the map's own keys against that model's fields, so every real
    profile read as an unknown key. Measured on one machine's live config: 31
    reported keys, 19 of them entries in a `dict[str, Model]` block.
    """
    f = _write(
        tmp_path / "config.toml",
        'schema_version = 1\nstate_dir = "%s"\n'
        '[agents.profiles.blueprint]\nmodel = "opus"\n'
        "[work.workspaces.main]\nprojects = []\n" % (tmp_path / ".fno"),
    )
    result = _doctor(f)
    assert result.exit_code == 0, result.output
    unknown = [ln for ln in result.output.splitlines() if "not a modeled config key" in ln]
    assert unknown == [], unknown


def test_a_typod_leaf_inside_a_dict_keyed_block_is_still_caught(tmp_path: Path) -> None:
    """Negative control on the same walk: the VALUE model is still checked."""
    f = _write(
        tmp_path / "config.toml",
        "schema_version = 1\n[agents.profiles.blueprint]\nmodle = \"opus\"\n",
    )
    result = _doctor(f)
    assert result.exit_code == 1, result.output
    assert "agents.profiles.blueprint.modle" in result.output


def test_a_large_unknown_table_reports_once(tmp_path: Path) -> None:
    """A foreign tool's block sharing the config file is one finding, not many."""
    f = _write(
        tmp_path / "config.toml",
        "schema_version = 1\n[companions.rtk]\n"
        'status = "ok"\nversion = "1"\nchecked_at = "now"\nnote = "x"\n',
    )
    result = _doctor(f)
    assert result.exit_code == 1, result.output
    unknown = [ln for ln in result.output.splitlines() if "companions" in ln]
    assert len(unknown) == 1, unknown
    assert "companions (set in" in unknown[0]


def test_a_leaf_name_shared_by_many_sections_gets_no_hint(tmp_path: Path) -> None:
    """`enabled` lives under 25 sections; listing all of them is not a remedy."""
    from fno.config.readback import _near_miss_keys

    assert _near_miss_keys("nosuchsection.enabled") == []
    # Positive control on the lookup itself, so an empty list above cannot be
    # a broken FIELD_META read.
    assert _near_miss_keys("agents.max_lanes") == ["parallel.max_lanes"]


def test_both_optional_spellings_of_a_dict_block_resolve(tmp_path: Path) -> None:
    """`Optional[dict[str, M]]` and `dict[str, M] | None` must resolve alike.

    The model uses neither spelling today. A future field that does would
    otherwise regress silently to walking the map's keys as field names, which
    is the defect this pins.
    """
    from typing import Optional

    from pydantic import BaseModel

    from fno.config.readback import _mapping_value_model

    class Row(BaseModel):
        name: str = ""

    assert _mapping_value_model(dict[str, Row]) is Row
    assert _mapping_value_model(Optional[dict[str, Row]]) is Row
    assert _mapping_value_model(dict[str, Row] | None) is Row
    # Positive control on the negative case: a plain map has no schema below it.
    assert _mapping_value_model(dict[str, str]) is None

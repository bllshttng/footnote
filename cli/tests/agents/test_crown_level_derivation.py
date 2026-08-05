"""Tests for scope-derived crown altitude (x-7685, US10/AC19/AC20/AC21).

The ladder altitude is a fact about the SCOPE, not an operator input: a project
is a VP (0), an epic a Director (1), any other backlog node an IC (2). Before
this, a human crowning stamped level 0 for ANY scope, so `--scope banana` minted
a VP over a scope nobody could find. Now a scope that resolves to nothing is
REFUSED. An explicit --level still wins, and a superset-king grant still uses
grantor_level+1 without consulting the graph (AC21 - no regression).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.agents.cli import agents_app
from fno.agents.registry import AgentEntry, write_registry
from fno.paths_testing import use_tmpdir
from fno.projects import resolve as proj_resolve


_EPIC = "x-epic1"
_NODE = "x-node1"
_PROJ = "fno-proj"
_TARGET = "crownee"


def _seed_graph(tmp_path: Path) -> None:
    from fno import paths

    # graph.json lives under state_dir (tmp_path/.fno under use_tmpdir), and the
    # crown resolver reads paths.graph_json() dynamically.
    graph_path = paths.graph_json()
    graph_path.parent.mkdir(parents=True, exist_ok=True)
    graph_path.write_text(
        json.dumps(
            {"entries": [
                {"id": _EPIC, "type": "epic"},
                {"id": _NODE, "type": "feature"},
            ]}
        ),
        encoding="utf-8",
    )


def _seed_settings(monkeypatch, tmp_path: Path) -> None:
    cfg = tmp_path / "config.toml"
    cfg.write_text(
        "[work.workspaces.ws1]\nprojects = [{ name = \"%s\", short_name = \"fp\" }]\n" % _PROJ,
        encoding="utf-8",
    )
    monkeypatch.setattr(proj_resolve, "SETTINGS_PATH", cfg)
    proj_resolve._clear_cache()


def _entry(name):
    return AgentEntry(name=name, harness="claude", cwd="/tmp",
                      log_path=f"/tmp/{name}.log", short_id=name)


@pytest.fixture
def human_shell(tmp_path: Path, monkeypatch):
    """Isolated graph + settings + registry; an attended HUMAN shell (no
    FNO_AGENT_SELF) does the crowning, so the level comes from scope derivation."""
    use_tmpdir(monkeypatch, tmp_path)
    _seed_graph(tmp_path)
    _seed_settings(monkeypatch, tmp_path)
    write_registry([_entry(_TARGET)])
    monkeypatch.delenv("FNO_AGENT_SELF", raising=False)
    return tmp_path


def _crown(*args):
    return CliRunner().invoke(agents_app, ["crown", _TARGET, *args])


def _level():
    from fno.agents.registry import load_registry
    return next(e for e in load_registry() if e.name == _TARGET).crown_level


# --- AC19: altitude derives from what the scope IS --------------------------


def test_epic_scope_stamps_director_level_1(human_shell):
    result = _crown("--scope", _EPIC)
    assert result.exit_code == 0, result.stdout + result.stderr
    assert _level() == 1


def test_node_scope_stamps_ic_level_2(human_shell):
    result = _crown("--scope", _NODE)
    assert result.exit_code == 0, result.stdout + result.stderr
    assert _level() == 2


def test_project_name_stamps_vp_level_0(human_shell):
    result = _crown("--scope", _PROJ)
    assert result.exit_code == 0, result.stdout + result.stderr
    assert _level() == 0


# --- AC20: a scope that resolves to nothing is refused ----------------------


def test_unknown_scope_refused_with_no_write(human_shell):
    result = _crown("--scope", "zzz-banana")
    assert result.exit_code == 2
    assert "resolves to neither" in result.stderr
    assert _level() is None  # no registry write on refusal


# --- AC21: no regression on the paths that already work ---------------------


def test_explicit_level_overrides_derivation(human_shell):
    # --scope is a project (would derive 0), but --level 2 wins.
    result = _crown("--scope", _PROJ, "--level", "2")
    assert result.exit_code == 0, result.stdout + result.stderr
    assert _level() == 2


def test_superset_king_grant_uses_grantor_level_plus_one(tmp_path, monkeypatch):
    # A crowned caller grants over a DIFFERENT (narrower) scope: the level is
    # grantor_level + 1, and the graph is NOT consulted. Caller at level 1 over
    # "king-scope" grants over the epic _EPIC (which would derive 1); the target
    # gets 2 (caller+1), proving the graph-derived 1 did not win.
    use_tmpdir(monkeypatch, tmp_path)
    _seed_graph(tmp_path)
    _seed_settings(monkeypatch, tmp_path)
    write_registry([
        AgentEntry(name="king", harness="claude", cwd="/tmp", log_path="/tmp/k.log",
                   short_id="king", crown_level=1, crown_scope="king-scope"),
        _entry(_TARGET),
    ])
    monkeypatch.setenv("FNO_AGENT_SELF", "king")
    result = _crown("--scope", _EPIC)  # no --level
    assert result.exit_code == 0, result.stdout + result.stderr
    assert _level() == 2  # grantor_level(1) + 1, not the epic-derived 1

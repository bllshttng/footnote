"""Runtime dispatch proofs for the Phase 2 lowercase shorts (ab-e893ba6e, US2).

``test_short_flag_convention.py`` is a static AST scan: it proves the
``typer.Option`` declarations exist but structurally cannot catch a Click
registration failure, because every touched sub-app is lazily loaded
(``cli/src/fno/cli.py`` ``LAZY_SUBCOMMANDS``) and the scan never
imports the command tree. These tests drive the REAL root app so each
touched sub-app imports, registers, and parses - the runtime counterpart
the Phase 1 review established with ``test_cmd_ask_short_flags_behave_like_long``.

Three layers, coarsest sufficient grain (one registration probe per surface,
one short-vs-long parity proof per previously-untested risk):

* ``--help`` registration smoke per Phase 2 surface (a malformed flag decl
  fails Click registration before any output).
* ``backlog find`` short-vs-long parity (read-only graph path).
* ``providers add`` short-vs-long parity (the one Phase 2 command with no
  prior CLI test of any kind).
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()

# --------------------------------------------------------------------------- #
# Registration smoke: one invocation per Phase 2 surface.
# --------------------------------------------------------------------------- #

PHASE2_HELP_SURFACES: dict[str, list[str]] = {
    "backlog-add": ["backlog", "add", "--help"],
    "backlog-idea": ["backlog", "idea", "--help"],
    "backlog-intake": ["backlog", "intake", "--help"],
    "backlog-update": ["backlog", "update", "--help"],
    "backlog-next": ["backlog", "next", "--help"],
    "backlog-ready": ["backlog", "ready", "--help"],
    "backlog-find": ["backlog", "find", "--help"],
    "backlog-capture-add": ["backlog", "capture", "add", "--help"],
    "mail-send": ["mail", "send", "--help"],
    "providers-add": ["providers", "add", "--help"],
    # gate-verify / gate-check removed: the `fno gate` sub-app was deleted by
    # the control-plane collapse wedge (ab-d0337fbc).
    "event-emit": ["event", "emit", "--help"],
    "done": ["done", "--help"],
    "carveout-add": ["carveout", "add", "--help"],
}


@pytest.mark.parametrize(
    "argv",
    list(PHASE2_HELP_SURFACES.values()),
    ids=list(PHASE2_HELP_SURFACES.keys()),
)
def test_phase2_surface_registers(argv: list[str]) -> None:
    """Each lazily-loaded sub-app imports and Click accepts its flag decls."""
    result = runner.invoke(app, argv)
    assert result.exit_code == 0, result.output


# --------------------------------------------------------------------------- #
# Parity: backlog find (read-only graph path).
# --------------------------------------------------------------------------- #

@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def test_backlog_find_short_flags_match_long(tmp_graph: Path) -> None:
    """AC4: `backlog find -p X -s Y -J` is byte-identical to the long form."""
    tmp_graph.write_text(json.dumps({"entries": [
        {"id": "ab-sf000001", "title": "Short flag rollout", "status": "done",
         "domain": "code", "project": "fno"},
        {"id": "ab-sf000002", "title": "Unrelated thing", "status": "ready",
         "domain": "code", "project": "other"},
    ]}) + "\n")
    long = runner.invoke(app, [
        "backlog", "find", "rollout",
        "--project", "fno", "--status", "done", "--json",
    ])
    short = runner.invoke(app, [
        "backlog", "find", "rollout", "-p", "fno", "-s", "done", "-J",
    ])
    assert long.exit_code == 0, long.output
    assert short.exit_code == long.exit_code
    assert short.stdout == long.stdout
    assert "ab-sf000001" in short.stdout


# --------------------------------------------------------------------------- #
# Parity: providers add (no prior CLI coverage at all).
# --------------------------------------------------------------------------- #

def _add_provider(monkeypatch, workdir: Path, argv: list[str]):
    """Run `providers add` isolated to workdir (project scope, no global)."""
    workdir.mkdir(parents=True, exist_ok=True)
    # _resolve_cwd() and save_providers(scope="project") both honor $PWD;
    # /dev/null disables the real per-user global settings candidate.
    monkeypatch.setenv("PWD", str(workdir))
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
    return runner.invoke(app, argv)


def test_providers_add_short_flags_match_long(tmp_path, monkeypatch) -> None:
    """AC4: `providers add -c/-a/-s/-p` writes the same record as the longs."""
    creds = tmp_path / "oauth"
    creds.mkdir()
    short_dir = tmp_path / "short"
    long_dir = tmp_path / "long"

    short_res = _add_provider(monkeypatch, short_dir, [
        "providers", "add", "prov-x",
        "-c", "claude", "-a", "oauth_dir",
        "--credentials-source", str(creds),
        "-s", "project", "-p", "50",
    ])
    long_res = _add_provider(monkeypatch, long_dir, [
        "providers", "add", "prov-x",
        "--cli", "claude", "--auth", "oauth_dir",
        "--credentials-source", str(creds),
        "--scope", "project", "--priority", "50",
    ])
    assert short_res.exit_code == 0, short_res.output
    assert long_res.exit_code == 0, long_res.output
    assert short_res.stdout == long_res.stdout

    short_yaml = (short_dir / ".fno" / "config.toml").read_text()
    long_yaml = (long_dir / ".fno" / "config.toml").read_text()
    assert short_yaml == long_yaml
    assert "prov-x" in short_yaml

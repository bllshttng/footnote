"""Runtime dispatch proofs for the Phase 2 lowercase shorts (ab-e893ba6e, US2).

``test_short_flag_convention.py`` is a static AST scan: it proves the
``typer.Option`` declarations exist but structurally cannot catch a Click
registration failure, because every touched sub-app is lazily loaded
(``cli/src/fno/cli.py`` ``LAZY_SUBCOMMANDS``) and the scan never
imports the command tree. These tests drive the Python root app for its remaining
leaves and the native binary for backlog add/idea, whose Python legs were retired.

Three layers, coarsest sufficient grain (one registration probe per surface,
one short-vs-long parity proof per previously-untested risk):

* ``--help`` registration smoke per Phase 2 surface (a malformed flag decl
  fails Click registration before any output).
* ``backlog find`` short-vs-long parity (read-only graph path).
* ``config accounts add`` short-vs-long parity (the one Phase 2 command with no
  prior CLI test of any kind).
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

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
    # backlog-update moved with the update port: the native binary answers
    # --help now (pinned below, next to native-find).
    "backlog-next": ["backlog", "next", "--help"],
    "backlog-ready": ["backlog", "ready", "--help"],
    "backlog-capture-add": ["backlog", "capture", "add", "--help"],
    "mail-send": ["mail", "send", "--help"],
    "config-accounts-add": ["config", "accounts", "add", "--help"],
    # gate-verify / gate-check removed: the `fno gate` sub-app was deleted by
    # the control-plane collapse wedge (ab-d0337fbc).
    "event-emit": ["doctor", "event", "emit", "--help"],
    "done": ["done", "--help"],
    "carveout-add": ["carveout", "add", "--help"],
}


@pytest.mark.parametrize(
    ("surface", "argv"),
    list(PHASE2_HELP_SURFACES.items()),
    ids=list(PHASE2_HELP_SURFACES.keys()),
)
def test_phase2_surface_registers(surface: str, argv: list[str]) -> None:
    """Creation verbs register natively; remaining Phase 2 leaves stay Python."""
    if surface in {"backlog-add", "backlog-idea"}:
        from tests._native_door import run_native

        code, out, err = run_native(*argv)
        assert code == 0, f"{out}\n{err}"
        return

    result = runner.invoke(app, argv)
    assert result.exit_code == 0, result.output


# --------------------------------------------------------------------------- #
# Parity: backlog find (read-only graph path).
# --------------------------------------------------------------------------- #

def test_backlog_find_native_help_registers() -> None:
    """`backlog find --help` is the native binary's now; the flag decl still
    parses and the surface answers."""
    from tests._native_door import run_native

    code, out, err = run_native("backlog", "find", "--help")
    assert code == 0, err
    assert "Usage" in out + err


def test_backlog_update_native_help_registers() -> None:
    """`backlog update --help` moved with the update port: the binary's flag
    decls still parse and the surface answers."""
    from tests._native_door import run_native

    code, out, err = run_native("backlog", "update", "--help")
    assert code == 0, err
    assert "Usage" in out + err


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # find routes through the guarded display reader, which resolves
    # paths.graph_json at call time.
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    # The native binary resolves the store through FNO_CONFIG's state_dir.
    (tmp_path / "config.toml").write_text(f'state_dir = "{tmp_path}"\n')
    monkeypatch.setenv("FNO_CONFIG", str(tmp_path / "config.toml"))
    return g


def _native_find(*args: str) -> tuple[int, str, str]:
    from tests._native_door import run_native

    return run_native("backlog", "find", *args)


def test_backlog_find_short_flags_match_long(tmp_graph: Path) -> None:
    """AC4: `backlog find -p X -s Y -d Z -J` is byte-identical to the long form.

    The Phase 2 lowercase table's find pin moved here with the find port:
    the native binary owns the surface, so the -p/-s/-d decls are readable
    only at this door.
    """
    seed_graph(tmp_graph, json.dumps({"entries": [
        {"id": "ab-sf000001", "title": "Short flag rollout", "status": "done",
         "domain": "code", "project": "fno"},
        {"id": "ab-sf000002", "title": "Unrelated thing", "status": "ready",
         "domain": "docs", "project": "other"},
    ]}) + "\n")
    long_code, long_out, long_err = _native_find(
        "rollout", "--project", "fno", "--status", "done", "--domain", "code", "--json",
    )
    short_code, short_out, _short_err = _native_find(
        "rollout", "-p", "fno", "-s", "done", "-d", "code", "-J",
    )
    assert long_code == 0, long_err
    assert short_code == long_code
    assert short_out == long_out
    assert "ab-sf000001" in short_out


# --------------------------------------------------------------------------- #
# Parity: config accounts add (no prior CLI coverage at all).
# --------------------------------------------------------------------------- #

def _add_provider(monkeypatch, workdir: Path, argv: list[str]):
    """Run `config accounts add` isolated to workdir (project scope, no global)."""
    workdir.mkdir(parents=True, exist_ok=True)
    # _resolve_cwd() and save_providers(scope="project") both honor $PWD;
    # /dev/null disables the real per-user global settings candidate.
    monkeypatch.setenv("PWD", str(workdir))
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", "/dev/null")
    return runner.invoke(app, argv)


def test_accounts_add_short_flags_match_long(tmp_path, monkeypatch) -> None:
    """AC4: `config accounts add -H/-a/-s/-p` writes the same record as the longs."""
    creds = tmp_path / "oauth"
    creds.mkdir()
    short_dir = tmp_path / "short"
    long_dir = tmp_path / "long"

    short_res = _add_provider(monkeypatch, short_dir, [
        "config", "accounts", "add", "prov-x",
        "-H", "claude", "-a", "oauth_dir",
        "--credentials-source", str(creds),
        "-s", "project", "-p", "50",
    ])
    long_res = _add_provider(monkeypatch, long_dir, [
        "config", "accounts", "add", "prov-x",
        "--harness", "claude", "--auth", "oauth_dir",
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

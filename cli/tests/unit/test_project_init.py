"""`fno project init` - the config write, both refusals, and the receipt.

The receipt assertions are not cosmetic. `fno project init` isolates fno's own
data and cannot isolate the machine's harness substrate, so a receipt that says
"isolated environment" without qualification is false about identity. The
wording IS the deliverable; pin it.
"""
from __future__ import annotations

import tomllib

import pytest
from typer.testing import CliRunner

from fno.project import init, project_app, receipt

runner = CliRunner()


@pytest.fixture(autouse=True)
def _isolate_global_settings(tmp_path_factory, monkeypatch):
    """Keep every read off the developer's real ~/.fno config.

    Without this the `paths.agents_registry_path` refusal reads the machine's
    global settings and the test's verdict depends on the developer's box.
    """
    monkeypatch.setenv(
        "FNO_GLOBAL_SETTINGS_PATH",
        str(tmp_path_factory.mktemp("globals") / "config.toml"),
    )


def _read(repo):
    return tomllib.loads((repo / ".fno" / "config.toml").read_text())


def _run(repo, *args):
    return runner.invoke(project_app, ["init", *args, "--repo-root", str(repo)])


def test_ac2_hp_writes_state_dir_and_prefix_and_mints_the_root(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    repo = tmp_path / "repo"
    repo.mkdir()

    res = _run(repo, "demo")

    assert res.exit_code == 0, res.output
    data = _read(repo)
    assert data["state_dir"] == "~/.fno/projects/demo"
    # The prefix keeps foreign node ids out of the demo graph. Stored as
    # typed, like `fno config set`; the schema normalizes the trailing dash on
    # read, which is what minting actually consumes.
    assert data["backlog"]["id_prefix"] == "demo"
    from fno.config import BacklogBlock

    assert BacklogBlock(id_prefix=data["backlog"]["id_prefix"]).id_prefix == "demo-"
    assert (tmp_path / "home" / ".fno" / "projects" / "demo").is_dir()


def test_ac2_err_refuses_a_different_state_dir_and_writes_nothing(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    repo = tmp_path / "repo"
    (repo / ".fno").mkdir(parents=True)
    (repo / ".fno" / "config.toml").write_text('state_dir = "~/.fno/projects/other"\n')
    before = (repo / ".fno" / "config.toml").read_text()

    res = _run(repo, "demo")

    assert res.exit_code == 1
    # The existing value and the file path, so the operator can act on the
    # refusal without going hunting.
    assert "~/.fno/projects/other" in res.output
    assert str(repo / ".fno" / "config.toml") in res.output
    assert (repo / ".fno" / "config.toml").read_text() == before


def test_re_running_with_the_same_id_is_idempotent(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    repo = tmp_path / "repo"
    repo.mkdir()

    assert _run(repo, "demo").exit_code == 0
    second = _run(repo, "demo")

    assert second.exit_code == 0, second.output
    assert _read(repo)["state_dir"] == "~/.fno/projects/demo"


def test_ac2_edge_refuses_an_agents_registry_override(tmp_path, monkeypatch):
    # An explicit `paths.agents_registry_path` is honored AHEAD of the state_dir
    # fallback, so this environment would get its own graph and share the
    # roster. Half-isolation is worse than none: it looks clean and is not.
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    globals_toml = tmp_path / "globals.toml"
    globals_toml.write_text(
        "[paths]\nagents_registry_path = \"~/.fno/agents/registry.json\"\n"
    )
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(globals_toml))
    repo = tmp_path / "repo"
    repo.mkdir()

    res = _run(repo, "demo")

    assert res.exit_code == 1
    assert "paths.agents_registry_path" in res.output
    assert not (repo / ".fno" / "config.toml").exists()


def test_refuses_an_id_the_node_grammar_rejects(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    repo = tmp_path / "repo"
    repo.mkdir()

    res = _run(repo, "Demo Project")

    assert res.exit_code != 0
    assert "id_prefix" in res.output
    # All-or-nothing: a rejected prefix must not leave a half-written state_dir
    # pointing at a root the graph will never use.
    assert not (repo / ".fno" / "config.toml").exists()


def test_ac3_hp_receipt_names_the_layer_it_did_not_isolate():
    text = receipt("demo", "~/.fno/projects/demo")

    assert "~/.fno/projects/demo" in text
    for moved in ("graph", "ledger", "briefs", "agent registry", "mail bus"):
        assert moved in text, moved
    # The half that is NOT isolated, named rather than implied.
    assert "NOT isolated" in text
    assert "claude daemon" in text
    assert "codex app-server" in text
    # LD4: the separate bus is inherited from state_dir, so the receipt states
    # it rather than leaving the operator to discover it when a message to a
    # demo worker goes nowhere.
    assert "OWN bus" in text


def test_the_receipt_is_what_the_verb_prints(tmp_path, monkeypatch):
    # A receipt function nothing calls would pass every assertion above while
    # the verb printed something else.
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    repo = tmp_path / "repo"
    repo.mkdir()

    res = _run(repo, "demo")

    assert res.exit_code == 0, res.output
    assert receipt("demo", "~/.fno/projects/demo") in res.output


def test_init_is_registered_on_the_cli():
    from fno.cli import LAZY_SUBCOMMANDS

    assert LAZY_SUBCOMMANDS["project"][0] == "fno.project:project_app"
    assert callable(init)


def test_the_id_is_normalized_before_anything_derives_from_it(tmp_path, monkeypatch):
    """`WEB` and `web` must name one environment, not two half-agreeing ones.

    `backlog.id_prefix` lowercases at read time, so an un-normalized `WEB`
    wrote `WEB` into the file while the graph minted `web-` nodes, and pointed
    the root at a differently-cased directory. Re-running with the other case
    then hit the already-pinned refusal.
    """
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    repo = tmp_path / "repo"
    repo.mkdir()

    res = _run(repo, "WEB")
    assert res.exit_code == 0, res.output

    cfg = (repo / ".fno" / "config.toml").read_text(encoding="utf-8")
    assert 'state_dir = "~/.fno/projects/web"' in cfg
    assert 'id_prefix = "web"' in cfg
    assert "project web: isolated" in res.output

    # The other case is now the SAME environment, not a refusal.
    again = _run(repo, "web")
    assert again.exit_code == 0, again.output

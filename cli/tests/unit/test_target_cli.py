"""Unit tests for `fno target init` and the `fno state init` redirect.

Change 3 of the worktree-binding plan (backlog ab-02e44aa6): a discoverable
bootstrap verb that records input/plan_path + the owner_cwd binding and
refuses to write a stub, plus a redirect on the substitution-prone
`fno state init` bare bootstrap.
"""
from __future__ import annotations


from typer.testing import CliRunner

from fno.cli import app
from fno import target_cli
from fno.paths import resolve_repo_root

runner = CliRunner()


def _clear_root_cache():
    # resolve_repo_root() caches the FNO_REPO_ROOT value per process; tests
    # that flip the env must clear it first.
    try:
        resolve_repo_root.cache_clear()
    except AttributeError:
        pass


def test_target_init_help_documents_inputs():
    result = runner.invoke(app, ["target", "init", "--help"])
    assert result.exit_code == 0
    assert "--input" in result.stdout
    assert "--plan-path" in result.stdout


def test_target_init_refuses_stub(monkeypatch, tmp_path):
    """AC (refuses stub): no --input/--plan-path -> non-zero, no subprocess."""
    called = {"ran": False}

    def _stub_run(*a, **k):
        called["ran"] = True
        raise AssertionError("init script must not run when args are missing")

    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)
    result = runner.invoke(app, ["target", "init"])
    assert result.exit_code == 2
    assert not called["ran"]
    assert "stub" in result.output.lower()


def test_target_init_shells_through_with_env(monkeypatch, tmp_path):
    """AC (happy path): --input shells to init script with TARGET_START + TARGET_INPUT."""
    captured = {}

    class _Result:
        returncode = 0

    def _stub_run(cmd, check=False, env=None, **kwargs):
        captured["cmd"] = list(cmd)
        captured["env"] = dict(env or {})
        return _Result()

    # Point the resolver at a fake plugin root that DOES contain the script.
    fake_root = tmp_path / "plugin"
    (fake_root / "hooks" / "helpers").mkdir(parents=True)
    (fake_root / "hooks" / "helpers" / "init-target-state.sh").write_text("#!/bin/bash\n")
    # CLAUDE_PLUGIN_ROOT wins over FNO_REPO_ROOT; clear it so the test's
    # FNO_REPO_ROOT is authoritative.
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()
    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)

    result = runner.invoke(app, ["target", "init", "--input", "fix-login", "--plan-path", "p/x.md"])
    assert result.exit_code == 0, result.output
    assert captured["cmd"][0] == "bash"
    assert captured["cmd"][1].endswith("hooks/helpers/init-target-state.sh")
    assert captured["env"].get("TARGET_START") == "1"
    assert captured["env"].get("TARGET_INPUT") == "fix-login"
    assert captured["env"].get("TARGET_PLAN_PATH") == "p/x.md"
    _clear_root_cache()


def _fake_plugin_root(tmp_path):
    fake_root = tmp_path / "plugin"
    (fake_root / "hooks" / "helpers").mkdir(parents=True)
    (fake_root / "hooks" / "helpers" / "init-target-state.sh").write_text("#!/bin/bash\n")
    return fake_root


def test_target_init_size_sets_target_size_env(monkeypatch, tmp_path):
    """--size propagates TARGET_SIZE (normalized to upper) to the init script."""
    captured = {}

    class _Result:
        returncode = 0

    def _stub_run(cmd, check=False, env=None, **kwargs):
        captured["env"] = dict(env or {})
        return _Result()

    fake_root = _fake_plugin_root(tmp_path)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()
    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)

    result = runner.invoke(app, ["target", "init", "--input", "x", "--size", "m"])
    assert result.exit_code == 0, result.output
    assert captured["env"].get("TARGET_SIZE") == "M"  # normalized to upper
    _clear_root_cache()


def test_target_init_rejects_invalid_size(monkeypatch, tmp_path):
    """Invalid --size exits 2 with a clear message (script resolves fine)."""
    fake_root = _fake_plugin_root(tmp_path)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()

    def _no_run(*a, **k):
        raise AssertionError("must not shell out on invalid --size")

    monkeypatch.setattr(target_cli.subprocess, "run", _no_run)
    result = runner.invoke(app, ["target", "init", "--input", "x", "--size", "XL"])
    assert result.exit_code == 2
    assert "invalid --size" in result.output
    _clear_root_cache()


def test_target_init_help_documents_size():
    result = runner.invoke(app, ["target", "init", "--help"])
    assert result.exit_code == 0
    assert "--size" in result.stdout


def test_target_init_missing_script_exits_2(monkeypatch, tmp_path):
    """AC3-ERR: bare-install degrade is actionable - names the footnote plugin
    and an install path, exits 2, no traceback. FNO_REPO_ROOT is authoritative."""
    fake_root = tmp_path / "empty"
    fake_root.mkdir()
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()
    result = runner.invoke(app, ["target", "init", "--input", "x"])
    assert result.exit_code == 2
    # Capability-accurate (not "is the plugin installed correctly?"): the
    # message names the footnote plugin and the bare-install gap + install path.
    assert "footnote plugin" in result.output
    assert "pip install fno" in result.output
    assert "--plugin-dir" in result.output
    _clear_root_cache()


def test_target_init_degrade_writes_no_state(monkeypatch, tmp_path):
    """AC3-FR / AC3-EDGE: when the init script is unresolvable (bare install or
    binary-present-but-skills-absent), the degrade writes no partial
    target-state.md / .fno state and never shells out."""
    fake_root = tmp_path / "empty"
    fake_root.mkdir()
    proj = tmp_path / "proj"
    proj.mkdir()
    monkeypatch.chdir(proj)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()

    def _no_run(*a, **k):
        raise AssertionError("must not shell out when the init script is missing")

    monkeypatch.setattr(target_cli.subprocess, "run", _no_run)
    result = runner.invoke(app, ["target", "init", "--input", "x"])
    assert result.exit_code == 2
    assert "footnote plugin" in result.output
    assert not (proj / ".fno").exists()
    _clear_root_cache()


def test_target_init_resolves_from_plugin_root(monkeypatch, tmp_path):
    """Codex P1: init script resolves from CLAUDE_PLUGIN_ROOT, not the cwd repo.

    Simulates running `fno target init` inside a user project (cwd repo has no
    hooks/) with CLAUDE_PLUGIN_ROOT pointing at the plugin install.
    """
    captured = {}

    class _Result:
        returncode = 0

    def _stub_run(cmd, check=False, env=None, **kwargs):
        captured["cmd"] = list(cmd)
        return _Result()

    plugin_root = tmp_path / "plugin"
    (plugin_root / "hooks" / "helpers").mkdir(parents=True)
    (plugin_root / "hooks" / "helpers" / "init-target-state.sh").write_text("#!/bin/bash\n")
    user_project = tmp_path / "user-proj"  # cwd repo without hooks/
    user_project.mkdir()
    monkeypatch.chdir(user_project)
    monkeypatch.setenv("CLAUDE_PLUGIN_ROOT", str(plugin_root))
    monkeypatch.delenv("FNO_REPO_ROOT", raising=False)
    _clear_root_cache()
    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)

    result = runner.invoke(app, ["target", "init", "--input", "x"])
    assert result.exit_code == 0, result.output
    assert str(plugin_root) in captured["cmd"][1]
    _clear_root_cache()


def test_state_init_redirects_target_bootstrap():
    """AC (redirect): bare `state init` (default type=target) -> non-zero redirect."""
    result = runner.invoke(app, ["state", "init"])
    assert result.exit_code == 2
    assert "fno target init" in result.output


def test_state_init_explicit_output_is_spared(tmp_path):
    """A deliberate --output is NOT a bootstrap; it must still create a file."""
    out = tmp_path / "explicit-state.md"
    result = runner.invoke(app, ["state", "init", "--type", "target", "--output", str(out)])
    assert result.exit_code == 0, result.output
    assert out.exists()


def test_state_init_allow_stub_escape(tmp_path, monkeypatch):
    """--allow-stub bypasses the redirect for internal/test use."""
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["state", "init", "--allow-stub"])
    assert result.exit_code == 0, result.output
    assert (tmp_path / ".fno" / "target-state.md").exists()

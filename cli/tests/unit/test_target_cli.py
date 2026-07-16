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
        # Capture the bash init shell-through specifically. init() also runs git
        # subprocesses both before (script-path resolution) and after (the
        # post-init orientation report, x-a7be) the bash call; neither is the
        # call under test.
        if list(cmd)[:1] == ["bash"]:
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
        # Capture the bash init call; git subprocesses (script resolution, the
        # post-init orientation report) bracket it and are not under test.
        if list(cmd)[:1] == ["bash"]:
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


def test_target_init_model_provider_set_dispatch_env(monkeypatch, tmp_path):
    """--model/--provider persist to the init env so the bash writer stamps them."""
    captured = {}

    class _Result:
        returncode = 0

    def _stub_run(cmd, check=False, env=None, **kwargs):
        if list(cmd)[:1] == ["bash"]:
            captured["env"] = dict(env or {})
        return _Result()

    fake_root = _fake_plugin_root(tmp_path)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()
    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)

    result = runner.invoke(
        app, ["target", "init", "--input", "x", "--model", "glm-4.7", "--provider", "codex"]
    )
    assert result.exit_code == 0, result.output
    assert captured["env"].get("TARGET_DISPATCH_MODEL") == "glm-4.7"
    assert captured["env"].get("TARGET_DISPATCH_PROVIDER") == "codex"
    _clear_root_cache()


def test_target_init_no_pins_no_dispatch_env(monkeypatch, tmp_path):
    """Byte-for-byte: without pins the dispatch env vars are absent."""
    captured = {}

    class _Result:
        returncode = 0

    def _stub_run(cmd, check=False, env=None, **kwargs):
        if list(cmd)[:1] == ["bash"]:
            captured["env"] = dict(env or {})
        return _Result()

    fake_root = _fake_plugin_root(tmp_path)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()
    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)

    result = runner.invoke(app, ["target", "init", "--input", "x"])
    assert result.exit_code == 0, result.output
    assert "TARGET_DISPATCH_MODEL" not in captured["env"]
    assert "TARGET_DISPATCH_PROVIDER" not in captured["env"]
    _clear_root_cache()


def test_target_init_empty_model_rejected(monkeypatch, tmp_path):
    """AC2-ERR: an empty --model exits 2 with a usage error, no shell-out."""
    fake_root = _fake_plugin_root(tmp_path)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()

    def _no_run(*a, **k):
        raise AssertionError("must not shell out on an empty --model")

    monkeypatch.setattr(target_cli.subprocess, "run", _no_run)
    result = runner.invoke(app, ["target", "init", "--input", "x", "--model", "  "])
    assert result.exit_code == 2
    assert "--model must not be empty" in result.output
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
        # Capture the init invocation specifically; the post-init work-start
        # dispatch (x-122a) may shell `git rev-parse` afterward, which must not
        # clobber the assertion target.
        if cmd and cmd[0] == "bash":
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


# ---------------------------------------------------------------------------
# A2 work-start lifecycle dispatch (x-122a)
# ---------------------------------------------------------------------------


def _write_manifest(repo_root, node_id):
    fno_dir = repo_root / ".fno"
    fno_dir.mkdir(parents=True, exist_ok=True)
    (fno_dir / "target-state.md").write_text(
        f"session_id: s1\ngraph_node_id: {node_id}\nattended: false\n", encoding="utf-8"
    )


def _arm_work_start(monkeypatch):
    """Force config.think_spawn.on_work_start True past the gate-first check."""
    import types
    from fno import config as _config

    fake = types.SimpleNamespace(
        think_spawn=types.SimpleNamespace(on_work_start=True)
    )
    monkeypatch.setattr(_config, "load_settings", lambda *a, **k: fake)


def test_work_start_dispatch_gated_off_does_nothing(tmp_path, monkeypatch):
    """Default-OFF: no settings arm -> the helper returns before any repo/graph I/O."""
    _write_manifest(tmp_path, "x-122a")
    from fno.provenance import spawn_think as _st

    seen = []
    monkeypatch.setattr(_st, "on_node_work_start", lambda n, **k: seen.append(n))
    target_cli._maybe_dispatch_work_start()
    assert seen == []


def test_work_start_dispatch_reads_claimed_node(tmp_path, monkeypatch):
    """AC2-HP wiring: a real graph_node_id routes the durable node to on_node_work_start."""
    _arm_work_start(monkeypatch)
    _write_manifest(tmp_path, "x-122a")
    import json as _json
    from fno import paths as _paths
    from fno.provenance import spawn_think as _st

    node = {"id": "x-122a", "title": "lifecycle"}
    g = tmp_path / "graph.json"
    g.write_text(_json.dumps({"entries": [node]}), encoding="utf-8")

    monkeypatch.setattr(_paths, "resolve_repo_root", lambda: tmp_path)
    monkeypatch.setattr(_paths, "graph_json", lambda: g)
    seen = []
    monkeypatch.setattr(_st, "on_node_work_start", lambda n, **k: seen.append(n["id"]))

    target_cli._maybe_dispatch_work_start()
    assert seen == ["x-122a"]


def test_work_start_dispatch_overlays_dispatch_pins(tmp_path, monkeypatch):
    """AC1-HP wiring: manifest dispatch_model/provider ride onto the node so the
    work-start /think spawn carries `fno target start --model X`'s choice."""
    _arm_work_start(monkeypatch)
    fno_dir = tmp_path / ".fno"
    fno_dir.mkdir(parents=True, exist_ok=True)
    (fno_dir / "target-state.md").write_text(
        "session_id: s1\ngraph_node_id: x-122a\nattended: false\n"
        "dispatch_model: glm-4.7\ndispatch_provider: codex\n",
        encoding="utf-8",
    )
    import json as _json
    from fno import paths as _paths
    from fno.provenance import spawn_think as _st

    node = {"id": "x-122a", "title": "lifecycle"}
    g = tmp_path / "graph.json"
    g.write_text(_json.dumps({"entries": [node]}), encoding="utf-8")

    monkeypatch.setattr(_paths, "resolve_repo_root", lambda: tmp_path)
    monkeypatch.setattr(_paths, "graph_json", lambda: g)
    seen = []
    monkeypatch.setattr(_st, "on_node_work_start", lambda n, **k: seen.append(n))

    target_cli._maybe_dispatch_work_start()
    assert len(seen) == 1
    assert seen[0]["model"] == "glm-4.7"
    assert seen[0]["provider"] == "codex"


def test_work_start_dispatch_skips_null_node(tmp_path, monkeypatch):
    """graph_node_id: null means no node was claimed -> nothing dispatched."""
    _arm_work_start(monkeypatch)
    _write_manifest(tmp_path, "null")
    from fno import paths as _paths
    from fno.provenance import spawn_think as _st

    monkeypatch.setattr(_paths, "resolve_repo_root", lambda: tmp_path)
    seen = []
    monkeypatch.setattr(_st, "on_node_work_start", lambda n, **k: seen.append(n))
    target_cli._maybe_dispatch_work_start()
    assert seen == []


def test_work_start_dispatch_non_fatal_on_missing_manifest(tmp_path, monkeypatch):
    """No manifest -> the helper swallows the read error and never raises."""
    _arm_work_start(monkeypatch)
    from fno import paths as _paths

    monkeypatch.setattr(_paths, "resolve_repo_root", lambda: tmp_path)
    target_cli._maybe_dispatch_work_start()  # must not raise


def test_target_start_forwards_harness_and_never_launches_in_place(tmp_path, monkeypatch):
    """A `never` project: ensure returns the repo root, so `fno target start`
    launches in place, forwards --harness claude, and does NOT run the setup hook
    on the canonical checkout (Locked Decision 4: no worktree-only side effect on
    path == repo root, which would corrupt canonical .fno)."""
    import subprocess as _real_subprocess
    from fno.harness_identity import HarnessIdentity

    repo = tmp_path / "vault"
    repo.mkdir()
    _real_subprocess.run(["git", "-C", str(repo), "init", "-q", "-b", "main"], check=True)
    _real_subprocess.run(
        ["git", "-C", str(repo), "-c", "user.email=t@t", "-c", "user.name=t",
         "commit", "-q", "--allow-empty", "-m", "init"], check=True
    )
    monkeypatch.chdir(repo)

    seen = {"ensure": None, "setup_called": False, "init": False}
    real_run = _real_subprocess.run

    def _dispatch(cmd, *a, **k):
        cmd = list(cmd)
        if cmd and cmd[0] == "git":
            return real_run(cmd, *a, **k)
        if "ensure" in cmd:  # simulate policy=never: repo root on stdout, exit 0
            seen["ensure"] = cmd
            return _real_subprocess.CompletedProcess(cmd, 0, stdout=f"{repo.resolve()}\n", stderr="")
        seen["init"] = True  # target init
        return _real_subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", _dispatch)
    monkeypatch.setattr(
        "fno.harness_identity.resolve_harness_identity",
        lambda *a, **k: HarnessIdentity(session_id="s", harness="claude"),
    )

    def _setup_hook(*a, **k):
        seen["setup_called"] = True
        return (0, "")

    monkeypatch.setattr("fno.worktree._run_setup_worktree_hook", _setup_hook)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_resolve_node_model", lambda *a, **k: (None, "none"))

    result = runner.invoke(app, ["target", "start", "x-nev"])
    assert result.exit_code == 0, result.output
    assert seen["ensure"] is not None
    assert "--harness" in seen["ensure"] and "claude" in seen["ensure"]
    assert seen["init"] is True                     # init still runs, in place
    assert seen["setup_called"] is False            # canonical .fno never touched
    assert "base=in-place" in result.output

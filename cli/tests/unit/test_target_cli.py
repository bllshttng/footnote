"""Unit tests for `fno do target init` and the `fno do state init` redirect.

Change 3 of the worktree-binding plan (backlog ab-02e44aa6): a discoverable
bootstrap verb that records input/plan_path + the owner_cwd binding and
refuses to write a stub, plus a redirect on the substitution-prone
`fno do state init` bare bootstrap.
"""
from __future__ import annotations

import json
import os
from types import SimpleNamespace

import pytest
import typer
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
    result = runner.invoke(app, ["do", "target", "init", "--help"])
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
    result = runner.invoke(app, ["do", "target", "init"])
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
    plan = tmp_path / "p" / "x.md"
    plan.parent.mkdir()
    plan.write_text("# Plan\n", encoding="utf-8")

    result = runner.invoke(
        app,
        ["do", "target", "init", "--input", "fix-login", "--plan-path", str(plan)],
    )
    assert result.exit_code == 0, result.output
    assert captured["cmd"][0] == "bash"
    assert captured["cmd"][1].endswith("hooks/helpers/init-target-state.sh")
    assert captured["env"].get("TARGET_START") == "1"
    assert captured["env"].get("TARGET_INPUT") == "fix-login"
    assert captured["env"].get("TARGET_PLAN_PATH") == str(plan)
    _clear_root_cache()


def _fake_plugin_root(tmp_path):
    fake_root = tmp_path / "plugin"
    (fake_root / "hooks" / "helpers").mkdir(parents=True)
    (fake_root / "hooks" / "helpers" / "init-target-state.sh").write_text("#!/bin/bash\n")
    return fake_root


def _retro_dispatch_node(tmp_path, *, source_pr=555, evidence=None):
    details = (
        f"Source: PR #{source_pr}, https://github.com/o/r/pull/{source_pr}#discussion_r123\n"
        f"<!-- retro-triage source_pr={source_pr} finding_hash=deadbeef -->"
    )
    node = {"id": "x-retro", "details": details, "cwd": str(tmp_path)}
    if evidence is not None:
        node["evidence"] = evidence
    return node


def test_retro_dispatch_preflight_non_retro_makes_no_probe(monkeypatch):
    calls = []
    target_cli._retro_dispatch_preflight(
        {"id": "x-plain", "details": "ordinary target"},
        gh_runner=lambda args: calls.append(args),
    )
    assert calls == []


def test_retro_dispatch_preflight_closed_node_is_a_noop(tmp_path):
    node = _retro_dispatch_node(tmp_path)
    node["completed_at"] = "2026-07-23T00:00:00Z"
    calls = []
    target_cli._retro_dispatch_preflight(
        node, gh_runner=lambda args: calls.append(args)
    )
    assert calls == []


def test_retro_dispatch_preflight_source_pr_none_warns_each_skipped_tier(
    tmp_path, capsys
):
    node = {
        "id": "x-postmortem",
        "details": "<!-- retro-triage source_pr=None finding_hash=deadbeef -->",
        "cwd": str(tmp_path),
    }
    calls = []
    target_cli._retro_dispatch_preflight(
        node, gh_runner=lambda args: calls.append(args)
    )
    output = capsys.readouterr().err
    assert output.count("WARN target init:") == 2
    assert "Tier 1 skipped" in output and "Tier 2 skipped" in output
    assert calls == []


def test_retro_dispatch_preflight_tier1_refuses_and_supersedes(tmp_path, capsys):
    node = _retro_dispatch_node(tmp_path)
    closed = []

    def scan(entries, **kwargs):
        assert kwargs["include_planned"] is True
        return [SimpleNamespace(signal="resolved/outdated thread")]

    with pytest.raises(typer.Exit) as exc:
        target_cli._retro_dispatch_preflight(
            node,
            scan_fn=scan,
            supersede_fn=lambda node_id, reason: closed.append((node_id, reason)) or True,
        )
    assert exc.value.exit_code == 3
    assert closed and closed[0][0] == "x-retro"
    assert "source PR #555" in capsys.readouterr().err


def test_retro_dispatch_preflight_warns_on_tier1_uncertainty(tmp_path, capsys):
    node = _retro_dispatch_node(tmp_path)

    def scan(entries, *, warnings, **kwargs):
        warnings.append("thread state unavailable")
        return []

    target_cli._retro_dispatch_preflight(node, scan_fn=scan)
    output = capsys.readouterr().err
    assert "Tier 1 skipped" in output
    assert "Tier 2 skipped" in output


def test_retro_dispatch_preflight_tier2_scopes_and_warns_on_overlap(tmp_path, capsys):
    node = _retro_dispatch_node(
        tmp_path, evidence={"items": {"git:merged-region:cli/src/fno/target_cli.py": "region"}}
    )
    calls = []

    def gh(args):
        calls.append(args)
        if args[:2] == ["pr", "view"]:
            return 0, json.dumps({"mergedAt": "2026-07-01T00:00:00Z"}), ""
        return 0, json.dumps([
            {
                "number": 568,
                "title": "selector fix",
                "mergedAt": "2026-07-02T00:00:00Z",
                "files": [{"path": "cli/src/fno/target_cli.py"}],
            }
        ]), ""

    target_cli._retro_dispatch_preflight(
        node,
        scan_fn=lambda entries, **kwargs: [],
        gh_runner=gh,
        unattended=True,
    )
    output = capsys.readouterr().err
    assert "Tier 2 sibling PR #568" in output
    assert "cli/src/fno/target_cli.py" in output
    assert all(args[args.index("--repo") + 1] == "o/r" for args in calls)


def test_retro_dispatch_preflight_attended_overlap_is_advisory_only(tmp_path):
    node = _retro_dispatch_node(
        tmp_path, evidence={"git:merged-region:cli/src/fno/target_cli.py": "region"}
    )
    closed = []

    def gh(args):
        if args[:2] == ["pr", "view"]:
            return 0, json.dumps({"mergedAt": "2026-07-01T00:00:00Z"}), ""
        return 0, json.dumps([{
            "number": 568,
            "title": "selector fix",
            "mergedAt": "2026-07-02T00:00:00Z",
            "files": [{"path": "cli/src/fno/target_cli.py"}],
        }]), ""

    target_cli._retro_dispatch_preflight(
        node,
        scan_fn=lambda entries, **kwargs: [],
        gh_runner=gh,
        supersede_fn=lambda node_id, reason: closed.append(node_id) or True,
    )
    assert closed == []


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

    result = runner.invoke(app, ["do", "target", "init", "--input", "x", "--size", "m"])
    assert result.exit_code == 0, result.output
    assert captured["env"].get("TARGET_SIZE") == "M"  # normalized to upper
    _clear_root_cache()


def test_target_init_model_provider_set_dispatch_env(monkeypatch, tmp_path):
    """--model/--harness persist to the init env so the bash writer stamps them."""
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
        app, ["do", "target", "init", "--input", "x", "--model", "glm-4.7", "--harness", "codex"]
    )
    assert result.exit_code == 0, result.output
    assert captured["env"].get("TARGET_DISPATCH_MODEL") == "glm-4.7"
    assert captured["env"].get("TARGET_DISPATCH_PROVIDER") == "codex"
    _clear_root_cache()


def test_target_init_beastmode_sets_authority_env(monkeypatch, tmp_path):
    """x-6390: --beastmode plumbs TARGET_BEASTMODE=1 so the bash writer stamps the grant."""
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

    result = runner.invoke(app, ["do", "target", "init", "--input", "x", "--beastmode"])
    assert result.exit_code == 0, result.output
    assert captured["env"].get("TARGET_BEASTMODE") == "1"
    _clear_root_cache()


def test_target_init_beast_alias_grants_too(monkeypatch, tmp_path):
    """x-6390: `--beast` is an accepted alias of `--beastmode`.

    Mobile autocorrect splits `beastmode` into `beast mode`, so the short
    spelling has to work or an operator on a phone silently loses the grant -
    and a dropped grant looks exactly like never asking for one.
    """
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

    result = runner.invoke(app, ["do", "target", "init", "--input", "x", "--beast"])
    assert result.exit_code == 0, result.output
    assert captured["env"].get("TARGET_BEASTMODE") == "1"
    _clear_root_cache()


def test_target_init_clears_ambient_beastmode_without_flag(monkeypatch, tmp_path):
    """x-6390: an inherited TARGET_BEASTMODE must NEVER grant authority.

    The flag is the sole authority. A worker spawned under an exported
    TARGET_BEASTMODE (codex/gemini spawn with env=None and inherit wholesale) would
    otherwise self-grant walk-away autonomy nobody asked for.
    """
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
    monkeypatch.setenv("TARGET_BEASTMODE", "1")  # the ambient grant
    _clear_root_cache()
    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)

    result = runner.invoke(app, ["do", "target", "init", "--input", "x"])
    assert result.exit_code == 0, result.output
    assert captured["env"].get("TARGET_BEASTMODE") == "", (
        "ambient TARGET_BEASTMODE must be cleared, not forwarded: "
        f"got {captured['env'].get('TARGET_BEASTMODE')!r}"
    )
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

    result = runner.invoke(app, ["do", "target", "init", "--input", "x"])
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
    result = runner.invoke(app, ["do", "target", "init", "--input", "x", "--model", "  "])
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
    result = runner.invoke(app, ["do", "target", "init", "--input", "x", "--size", "XL"])
    assert result.exit_code == 2
    assert "invalid --size" in result.output
    _clear_root_cache()


def test_target_init_help_documents_size():
    result = runner.invoke(app, ["do", "target", "init", "--help"])
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
    result = runner.invoke(app, ["do", "target", "init", "--input", "x"])
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
    target-state.md / .fno do state and never shells out."""
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
    result = runner.invoke(app, ["do", "target", "init", "--input", "x"])
    assert result.exit_code == 2
    assert "footnote plugin" in result.output
    assert not (proj / ".fno").exists()
    _clear_root_cache()


def test_target_init_resolves_from_plugin_root(monkeypatch, tmp_path):
    """Codex P1: init script resolves from CLAUDE_PLUGIN_ROOT, not the cwd repo.

    Simulates running `fno do target init` inside a user project (cwd repo has no
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

    result = runner.invoke(app, ["do", "target", "init", "--input", "x"])
    assert result.exit_code == 0, result.output
    assert str(plugin_root) in captured["cmd"][1]
    _clear_root_cache()


def test_state_init_redirects_target_bootstrap():
    """AC (redirect): bare `state init` (default type=target) -> non-zero redirect."""
    result = runner.invoke(app, ["do", "state", "init"])
    assert result.exit_code == 2
    assert "fno do target init" in result.output


def test_state_init_explicit_output_is_spared(tmp_path):
    """A deliberate --output is NOT a bootstrap; it must still create a file."""
    out = tmp_path / "explicit-state.md"
    result = runner.invoke(app, ["do", "state", "init", "--type", "target", "--output", str(out)])
    assert result.exit_code == 0, result.output
    assert out.exists()


def test_state_init_allow_stub_escape(tmp_path, monkeypatch):
    """--allow-stub bypasses the redirect for internal/test use."""
    monkeypatch.chdir(tmp_path)
    result = runner.invoke(app, ["do", "state", "init", "--allow-stub"])
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
    work-start /think spawn carries `fno do target start --model X`'s choice."""
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
    """A `never` project: ensure returns the repo root, so `fno do target start`
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

    result = runner.invoke(app, ["do", "target", "start", "x-nev"])
    assert result.exit_code == 0, result.output
    assert seen["ensure"] is not None
    assert "--harness" in seen["ensure"] and "claude" in seen["ensure"]
    assert seen["init"] is True                     # init still runs, in place
    assert seen["setup_called"] is False            # canonical .fno never touched
    assert "base=in-place" in result.output


def test_target_start_forwards_beastmode_to_init(tmp_path, monkeypatch):
    """x-6390: `/target beastmode` cold-starts through `start`, not `init`, so the
    forward is the link that actually carries the grant in real use. Without it
    the feature is inert while every init-level test stays green."""
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

    seen = {"init_cmd": None}
    real_run = _real_subprocess.run

    def _dispatch(cmd, *a, **k):
        cmd = list(cmd)
        if cmd and cmd[0] == "git":
            return real_run(cmd, *a, **k)
        if "ensure" in cmd:
            return _real_subprocess.CompletedProcess(cmd, 0, stdout=f"{repo.resolve()}\n", stderr="")
        seen["init_cmd"] = cmd
        return _real_subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", _dispatch)
    monkeypatch.setattr(
        "fno.harness_identity.resolve_harness_identity",
        lambda *a, **k: HarnessIdentity(session_id="s", harness="claude"),
    )
    monkeypatch.setattr("fno.worktree._run_setup_worktree_hook", lambda *a, **k: (0, ""))
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_resolve_node_model", lambda *a, **k: (None, "none"))

    result = runner.invoke(app, ["do", "target", "start", "x-yol", "--beastmode"])
    assert result.exit_code == 0, result.output
    assert "--beastmode" in (seen["init_cmd"] or []), f"start did not forward --beastmode: {seen['init_cmd']}"

    seen["init_cmd"] = None
    result = runner.invoke(app, ["do", "target", "start", "x-yol"])
    assert result.exit_code == 0, result.output
    assert "--beastmode" not in (seen["init_cmd"] or []), "start forwarded --beastmode without the flag"


def test_target_start_beastmode_noop_when_already_isolated_is_named(tmp_path, monkeypatch):
    """x-6390: `start` no-ops inside a linked worktree and returns before it can
    forward --beastmode, so the grant is dropped. Same silent-drop class as the init
    path; it must say so rather than print a normal-looking receipt."""
    fake_root = _fake_plugin_root(tmp_path)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()
    manifest = fake_root / ".fno" / "target-state.md"
    manifest.parent.mkdir(parents=True, exist_ok=True)
    manifest.write_text("---\nattended: true\n---\n")

    monkeypatch.chdir(fake_root)
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda _p: True)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_foreign_live_holder", lambda _n: None)

    result = runner.invoke(app, ["do", "target", "start", "x-yol", "--beastmode"])
    assert result.exit_code == 0, result.output
    assert "already isolated" in result.output
    assert "did NOT take" in result.output, result.output

    result = runner.invoke(app, ["do", "target", "start", "x-yol"])
    assert result.exit_code == 0, result.output
    assert "did NOT take" not in result.output, "warned without the flag"
    _clear_root_cache()


def test_target_init_beastmode_noop_on_existing_manifest_is_named(tmp_path, monkeypatch):
    """x-6390: the manifest is write-once, so --beastmode against an initialized
    session is a no-op - and a dropped grant looks exactly like no grant. Say it."""
    class _Result:
        returncode = 0

    def _stub_run(cmd, check=False, env=None, **kwargs):
        return _Result()

    fake_root = _fake_plugin_root(tmp_path)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()
    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)

    manifest = fake_root / ".fno" / "target-state.md"
    manifest.parent.mkdir(parents=True, exist_ok=True)

    # A pre-existing manifest with no grant: the flag was dropped -> warn.
    manifest.write_text("---\nattended: true\n---\n")
    result = runner.invoke(app, ["do", "target", "init", "--input", "x", "--beastmode"])
    assert result.exit_code == 0, result.output
    assert "did NOT take" in result.output, result.output

    # The grant IS present AND anchored to a LIVE CLAIM: nothing to warn about.
    # A live owner_pid alone would NOT qualify - it is alive for every session
    # at init time, so it cannot distinguish a durable grant from a doomed one.
    manifest.write_text(
        '---\nattended: true\nauthority: full\n---\ntarget_claim_key: "node:x-1"\n'
    )
    monkeypatch.setattr(
        "fno.target.orient._claim_state", lambda _k: "live", raising=False
    )
    result = runner.invoke(app, ["do", "target", "init", "--input", "x", "--beastmode"])
    assert result.exit_code == 0, result.output
    assert "did NOT take" not in result.output, result.output
    assert "ANCHOR IT" not in result.output, result.output
    _clear_root_cache()


def test_target_init_beastmode_unanchored_grant_is_named(tmp_path, monkeypatch):
    """x-6390: a free-text run claims no node, so `authority: full` has nothing
    to anchor it and the orienter refuses it (fail closed). The stamp alone would
    otherwise look like a working grant."""
    class _Result:
        returncode = 0

    def _stub_run(cmd, check=False, env=None, **kwargs):
        return _Result()

    fake_root = _fake_plugin_root(tmp_path)
    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(fake_root))
    _clear_root_cache()
    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)

    manifest = fake_root / ".fno" / "target-state.md"
    manifest.parent.mkdir(parents=True, exist_ok=True)
    # A REAL claimless init: no claim key, but owner_pid is ALIVE (it always is
    # at init). A liveness-only check would suppress the warning here and let the
    # grant evaporate silently minutes later.
    manifest.write_text(
        f"---\nattended: true\nauthority: full\nowner_pid: {os.getpid()}\n---\n"
    )

    result = runner.invoke(app, ["do", "target", "init", "--input", "some idea", "--beastmode"])
    assert result.exit_code == 0, result.output
    assert "NOTHING LIVE TO ANCHOR IT" in result.output, result.output

    # The message is an operator-facing CONTRACT, not decoration: it must name
    # the real grant condition and must not offer owner_pid as an alternative.
    # A message that contradicts the rule tells someone their claimless session
    # might be fine, which is worse than saying nothing.
    # Scope to the warning itself; the orientation report that follows legitimately
    # mentions owner_pid as a liveness *reason*, which is a different claim.
    warning = result.output.split("node:", 1)[0]
    assert "LIVE CLAIM" in warning, warning
    assert "owner_pid" not in warning, (
        "the warning must not suggest a pid can anchor a grant: " + warning
    )
    _clear_root_cache()


def test_target_start_never_refuses_mismatched_inplace_manifest(tmp_path, monkeypatch):
    """In-place (policy=never) uses the SHARED canonical .fno, so a manifest for a
    DIFFERENT node must refuse rather than report already-claimed and run under
    another node's session state."""
    import subprocess as _real_subprocess
    from fno.harness_identity import HarnessIdentity

    repo = tmp_path / "vault"
    repo.mkdir()
    _real_subprocess.run(["git", "-C", str(repo), "init", "-q", "-b", "main"], check=True)
    _real_subprocess.run(
        ["git", "-C", str(repo), "-c", "user.email=t@t", "-c", "user.name=t",
         "commit", "-q", "--allow-empty", "-m", "init"], check=True
    )
    # a manifest belonging to a DIFFERENT node already in the canonical .fno
    (repo / ".fno").mkdir()
    (repo / ".fno" / "target-state.md").write_text("---\ngraph_node_id: x-other\n---\n")
    monkeypatch.chdir(repo)

    seen = {"init": False}
    real_run = _real_subprocess.run

    def _dispatch(cmd, *a, **k):
        cmd = list(cmd)
        if cmd and cmd[0] == "git":
            return real_run(cmd, *a, **k)
        if "ensure" in cmd:  # never -> repo root
            return _real_subprocess.CompletedProcess(cmd, 0, stdout=f"{repo.resolve()}\n", stderr="")
        seen["init"] = True
        return _real_subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", _dispatch)
    monkeypatch.setattr(
        "fno.harness_identity.resolve_harness_identity",
        lambda *a, **k: HarnessIdentity(session_id="s", harness="claude"),
    )
    monkeypatch.setattr("fno.worktree._run_setup_worktree_hook", lambda *a, **k: (0, ""))
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_foreign_live_holder", lambda n: None)

    result = runner.invoke(app, ["do", "target", "start", "x-nev"])
    assert result.exit_code == 1                    # refused, not already-claimed
    assert seen["init"] is False                    # never ran init under x-other
    combined = result.output + (getattr(result, "stderr", "") or "")
    assert "x-other" in combined


# ---------------------------------------------------------------------------
# x-e957 task 1.3b: a NAMED contained node is redirected, not claimed
# ---------------------------------------------------------------------------


def _contained_graph(tmp_path, monkeypatch, *, owner="x-6320"):
    """A graph with one delivery unit and one node contained in it.

    The owner binds a real plan file. A bound, resolving plan is one of the two
    scope denominators, so the denominator gate at init must see it resolve; a
    placeholder path gets emptied at back-fill and the node reads as plan-less.
    """
    import json

    plan = tmp_path / "one.md"
    plan.write_text("# plan\n", encoding="utf-8")
    plan_path = str(plan)
    gp = tmp_path / "graph.json"
    gp.write_text(json.dumps({"entries": [
        {"id": owner, "plan_path": plan_path, "status": "ready"},
        {"id": "x-261c", "plan_path": plan_path, "status": "ready",
         "contained_in": owner},
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)
    return gp


def _init_env(tmp_path, monkeypatch):
    """Wire target init so the bash bootstrap is stubbed and observable."""
    ran = []

    class _Result:
        returncode = 0

    def _stub_run(cmd, check=False, env=None, **kwargs):
        if list(cmd)[:1] == ["bash"]:
            ran.append(dict(env or {}))
        return _Result()

    monkeypatch.delenv("CLAUDE_PLUGIN_ROOT", raising=False)
    monkeypatch.setenv("FNO_REPO_ROOT", str(_fake_plugin_root(tmp_path)))
    _clear_root_cache()
    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)
    return ran


def _held_graph(tmp_path, monkeypatch):
    import json

    plan = tmp_path / "held.md"
    plan.write_text(
        "---\nstatus: ready\ndispatch_hold:\n"
        "  reason: Blocking review finding is unresolved\n"
        "  release_when: The finding is fixed and re-reviewed\n"
        "  review_on: 2099-08-20\n"
        "  set_by: king:119e3c52\n---\n",
        encoding="utf-8",
    )
    gp = tmp_path / "graph-held.json"
    gp.write_text(
        json.dumps(
            {
                "entries": [
                    {"id": "x-5a5c", "status": "ready", "plan_path": str(plan)},
                    {
                        "id": "x-1a2b",
                        "status": "ready",
                        "contained_in": "x-5a5c",
                    },
                ]
            }
        ),
        encoding="utf-8",
    )
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)
    return gp


def test_target_init_refuses_held_node_before_bootstrap(tmp_path, monkeypatch):
    _held_graph(tmp_path, monkeypatch)
    ran = _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--input", "x-5a5c"])
    assert result.exit_code == 2, result.output
    assert "Blocking review finding is unresolved" in result.output
    assert "king:119e3c52" in result.output
    assert "The finding is fixed and re-reviewed" in result.output
    assert ran == []
    _clear_root_cache()


def test_target_init_refuses_child_held_by_delivery_owner(tmp_path, monkeypatch):
    _held_graph(tmp_path, monkeypatch)
    ran = _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--input", "x-1a2b"])
    assert result.exit_code == 2, result.output
    assert "held by plan x-5a5c" in result.output
    assert ran == []
    _clear_root_cache()


def test_check_dispatch_hold_is_wired_for_direct_shell_bootstrap(tmp_path, monkeypatch):
    _held_graph(tmp_path, monkeypatch)
    monkeypatch.setenv("TARGET_INPUT", "x-5a5c")
    result = runner.invoke(app, ["do", "target", "check-dispatch-hold"])
    assert result.exit_code == 9, result.output
    assert "king:119e3c52" in result.output

    from pathlib import Path as _P
    from fno.paths import resolve_plugin_script

    text = _P(resolve_plugin_script("hooks/helpers/init-target-state.sh")).read_text()
    assert "fno do target check-dispatch-hold" in text
    assert '"$_DH_RC" -eq 9' in text


def test_target_init_redirects_a_named_contained_node(tmp_path, monkeypatch):
    """AC4: report the delivery unit's id and claim nothing.

    Naming a node is consent and selection_guards honors that (it is autonomous-
    only by its own docstring), so without this the operator walks straight past
    the guard and opens a second PR for one plan.
    """
    _contained_graph(tmp_path, monkeypatch)
    ran = _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--input", "x-261c"])
    assert result.exit_code == 2, result.output
    # Nothing was claimed: the bash bootstrap - which acquires the node claim
    # and writes the immutable manifest - never ran.
    assert ran == []
    _clear_root_cache()


def test_target_init_redirect_names_the_delivery_unit_it_routes_to(tmp_path, monkeypatch):
    """The DESTINATION is the payload, not the fact that something was refused.

    Two nodes contained in different units must route to different places; an
    assertion that only checks "it was refused" agrees on the tag and says
    nothing about where the operator should go.
    """
    _contained_graph(tmp_path, monkeypatch, owner="x-8a4f")
    _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--input", "x-261c"])
    assert result.exit_code == 2
    assert "x-8a4f" in result.output
    assert "/fno:target x-8a4f" in result.output
    _clear_root_cache()


def test_target_init_still_dispatches_the_delivery_unit_itself(tmp_path, monkeypatch):
    """The unit carries the PR, so naming IT is the whole point of the redirect."""
    _contained_graph(tmp_path, monkeypatch)
    ran = _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--input", "x-6320"])
    assert result.exit_code == 0, result.output
    assert len(ran) == 1
    assert ran[0].get("TARGET_INPUT") == "x-6320"
    _clear_root_cache()


def test_target_init_free_text_is_untouched_by_the_containment_read(tmp_path, monkeypatch):
    """An idea-first run resolves no node; the gate must not invent one."""
    _contained_graph(tmp_path, monkeypatch)
    ran = _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--input", "fix the login redirect"])
    assert result.exit_code == 0, result.output
    assert len(ran) == 1
    _clear_root_cache()


def test_redirect_helper_ignores_non_contained_and_malformed_input():
    """Fail-open on anything that is not an affirmative owner id.

    Raising on a missing/odd node would turn a fail-open resolver into a
    dispatch-blocker, and the resolver returns None for every free-text input.
    """
    target_cli._redirect_if_contained(None)
    target_cli._redirect_if_contained({"id": "x-a"})
    target_cli._redirect_if_contained({"id": "x-a", "contained_in": None})
    target_cli._redirect_if_contained({"id": "x-a", "contained_in": ""})
    target_cli._redirect_if_contained({"id": "x-a", "contained_in": 0})


# ---------------------------------------------------------------------------
# codex P1: containment must hold on EVERY bootstrap path, not just `init`
# ---------------------------------------------------------------------------


def test_shared_plan_path_resolves_to_the_delivery_unit(tmp_path, monkeypatch):
    """A plan held by a unit and its contained children is legal, not ambiguous.

    Returning None for the normal contained shape let `--plan-path <shared>`
    sail past the containment redirect AND the retro dedup gate: both read this
    one resolver, so the miss was doubled.
    """
    gp = _contained_graph(tmp_path, monkeypatch)
    plan_path = json.loads(gp.read_text(encoding="utf-8"))["entries"][0]["plan_path"]
    node = target_cli._resolve_dispatch_node(None, plan_path)
    assert node is not None and node["id"] == "x-6320"


def test_two_uncontained_holders_stay_ambiguous(tmp_path, monkeypatch):
    """Narrowing to the unit must not start guessing between real rivals.

    Change 1.1 refuses to CREATE this state; a graph that predates it must not
    have one of the two picked silently.
    """
    import json

    gp = tmp_path / "graph.json"
    gp.write_text(json.dumps({"entries": [
        {"id": "x-6320", "plan_path": "/p/one.md", "status": "ready"},
        {"id": "x-8a4f", "plan_path": "/p/one.md", "status": "ready"},
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)
    assert target_cli._resolve_dispatch_node(None, "/p/one.md") is None


def test_plan_path_naming_only_contained_nodes_is_redirected(tmp_path, monkeypatch):
    """No delivery unit on the plan at all -> still a contained node."""
    import json

    plan = tmp_path / "one.md"
    plan.write_text("---\nstatus: ready\n---\n")
    gp = tmp_path / "graph.json"
    gp.write_text(json.dumps({"entries": [
        {"id": "x-261c", "plan_path": str(plan), "contained_in": "x-6320"},
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)
    ran = _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--plan-path", str(plan)])
    assert result.exit_code == 2, result.output
    assert ran == []
    _clear_root_cache()


def test_check_contained_refuses_with_the_shell_gates_own_code(tmp_path, monkeypatch):
    """rc 9, not 2: a stale fno exits 2 as a Click "No such command".

    Treating 2 as a refusal would hard-refuse every direct bootstrap the moment
    the installed CLI fell behind source - the same reasoning check-review-gate
    documents for its own code.
    """
    _contained_graph(tmp_path, monkeypatch)
    monkeypatch.setenv("TARGET_INPUT", "x-261c")
    result = runner.invoke(app, ["do", "target", "check-contained"])
    assert result.exit_code == 9, result.output
    assert "x-6320" in result.output


def test_check_contained_passes_a_delivery_unit_and_a_bare_idea(tmp_path, monkeypatch):
    """Exit 0 for anything that is not an affirmative contained node."""
    _contained_graph(tmp_path, monkeypatch)

    monkeypatch.setenv("TARGET_INPUT", "x-6320")
    assert runner.invoke(app, ["do", "target", "check-contained"]).exit_code == 0

    monkeypatch.setenv("TARGET_INPUT", "fix the login redirect")
    assert runner.invoke(app, ["do", "target", "check-contained"]).exit_code == 0

    monkeypatch.delenv("TARGET_INPUT", raising=False)
    assert runner.invoke(app, ["do", "target", "check-contained"]).exit_code == 0


def test_init_script_wires_the_containment_gate():
    """The verb is only a guard if the documented direct path actually calls it.

    A refusal nothing invokes is the decorative-guard failure this whole task is
    about, so assert the wiring, not just the verb.
    """
    from pathlib import Path as _P

    from fno.paths import resolve_plugin_script

    script = _P(resolve_plugin_script("hooks/helpers/init-target-state.sh"))
    text = script.read_text(encoding="utf-8")
    assert "fno do target check-contained" in text
    # rc 9 refuses; anything else must fall through rather than brick bootstrap.
    assert '"$_CT_RC" -eq 9' in text


def test_plan_held_only_by_contained_nodes_still_redirects(tmp_path, monkeypatch):
    """The first narrowing covered len(units)==1 and missed len(units)==0.

    Owner superseded, deleted, or its plan_path edited away leaves a plan whose
    every holder is contained. That fell back to the len==1 rule, returned None,
    and skipped the redirect - the exact second-PR state the guard exists for.
    They all name one owner, so the destination is unambiguous.
    """
    import json

    plan = tmp_path / "one.md"
    plan.write_text("---\nstatus: ready\n---\n")
    gp = tmp_path / "graph.json"
    gp.write_text(json.dumps({"entries": [
        {"id": "x-261c", "plan_path": str(plan), "contained_in": "x-6320"},
        {"id": "x-3f8d", "plan_path": str(plan), "contained_in": "x-6320"},
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)
    ran = _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--plan-path", str(plan)])
    assert result.exit_code == 2, result.output
    assert "x-6320" in result.output
    assert ran == []
    _clear_root_cache()


def test_contained_nodes_naming_different_owners_stay_ambiguous(tmp_path, monkeypatch):
    """Two owners means no single destination; do not pick one."""
    import json

    gp = tmp_path / "graph.json"
    gp.write_text(json.dumps({"entries": [
        {"id": "x-261c", "plan_path": "/p/one.md", "contained_in": "x-6320"},
        {"id": "x-3f8d", "plan_path": "/p/one.md", "contained_in": "x-8a4f"},
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)
    assert target_cli._resolve_dispatch_node(None, "/p/one.md") is None


def test_redirect_to_an_already_merged_owner_says_so(tmp_path, monkeypatch):
    """After the cascade the owner is usually done; routing there is a dead end.

    "run /fno:target <done node>" reads as a broken redirect rather than as
    "this already shipped".
    """
    import json

    plan = tmp_path / "one.md"
    plan.write_text("---\nstatus: ready\n---\n")
    gp = tmp_path / "graph.json"
    gp.write_text(json.dumps({"entries": [
        {"id": "x-6320", "plan_path": str(plan), "pr_number": 700,
         "completed_at": "2026-07-29T00:00:00+00:00"},
        {"id": "x-261c", "plan_path": str(plan), "contained_in": "x-6320"},
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)
    _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--input", "x-261c"])
    assert result.exit_code == 2
    assert "already shipped" in result.output
    assert "700" in result.output
    assert "run `/fno:target x-6320`" not in result.output
    _clear_root_cache()


def test_target_start_redirects_before_creating_a_worktree(tmp_path, monkeypatch):
    """sigma: `init` caught it, but only after `start` allocated the worktree.

    The operator got a refusal saying "Nothing was claimed" sitting next to an
    orphan directory and branch they had to remove by hand.
    """
    _contained_graph(tmp_path, monkeypatch)
    ensured = []

    def _stub_run(cmd, *a, **k):
        if "worktree" in list(cmd) and "ensure" in list(cmd):
            ensured.append(list(cmd))

        class _R:
            returncode = 0
            stdout = str(tmp_path / "wt")
            stderr = ""
        return _R()

    monkeypatch.setattr(target_cli.subprocess, "run", _stub_run)
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_git_out", lambda *a, **k: str(tmp_path))
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_codex_desktop_handoff_policy", lambda r: None)

    result = runner.invoke(app, ["do", "target", "start", "x-261c"])
    assert result.exit_code == 2, result.output
    assert ensured == [], "worktree was allocated before the redirect fired"
    _clear_root_cache()


def test_check_contained_reads_under_the_graph_lock(tmp_path, monkeypatch):
    """codex P1: `read_graph` takes no lock, so the re-check was still raceable.

    decompose holds the graph flock and sees no claim, the bootstrap acquires
    the claim, this read returns the PRE-adoption graph because decompose has
    not done its atomic replace yet, and decompose then commits `contained_in`
    while the worker proceeds. Taking the same flock totalizes the ordering.

    Asserts the lock is actually held ACROSS the resolve - a lock acquired and
    dropped before the read would pass a "did we lock" assertion and serialize
    nothing.
    """
    import fno.graph.store as gs

    _contained_graph(tmp_path, monkeypatch)
    held = {"during_resolve": False, "acquired": 0}

    real_acquire, real_release = gs._acquire_flock, gs._release_flock
    state = {"open": False}

    def acq(p):
        state["open"] = True
        held["acquired"] += 1
        return real_acquire(p)

    def rel(fd):
        state["open"] = False
        return real_release(fd)

    real_resolve = target_cli._resolve_dispatch_node

    def resolve(*a, **k):
        held["during_resolve"] = state["open"]
        return real_resolve(*a, **k)

    monkeypatch.setattr(gs, "_acquire_flock", acq)
    monkeypatch.setattr(gs, "_release_flock", rel)
    monkeypatch.setattr(target_cli, "_resolve_dispatch_node", resolve)
    monkeypatch.setenv("TARGET_INPUT", "x-261c")

    result = runner.invoke(app, ["do", "target", "check-contained"])
    assert result.exit_code == 9, result.output
    assert held["acquired"] == 1, "the graph lock was never taken"
    assert held["during_resolve"], "the lock was not held across the graph read"
    assert not state["open"], "the graph lock was leaked"


def test_check_contained_proceeds_when_the_graph_lock_is_unavailable(tmp_path,
                                                                     monkeypatch):
    """An unlockable graph must not block every dispatch.

    Fail-open matches the rest of this gate: a broken lock is a broken gate, and
    the pre-claim check plus decompose's own live-claim refusal still stand.
    """
    import fno.graph.store as gs

    _contained_graph(tmp_path, monkeypatch)

    def boom(p):
        raise OSError("no lock for you")

    monkeypatch.setattr(gs, "_acquire_flock", boom)
    monkeypatch.setenv("TARGET_INPUT", "x-261c")
    # Still resolves and still refuses - the read just was not serialized.
    assert runner.invoke(app, ["do", "target", "check-contained"]).exit_code == 9


def test_redirect_names_a_dead_owner_instead_of_routing_to_it(tmp_path, monkeypatch):
    """sigma: a superseded owner will never ship, so routing there is useless.

    Named separately from the shipped case because the remedy differs: this is
    stale containment that wants clearing, not work that already landed.
    """
    import json

    owner_plan = tmp_path / "one.md"
    child_plan = tmp_path / "two.md"
    owner_plan.write_text("---\nstatus: ready\n---\n")
    child_plan.write_text("---\nstatus: ready\n---\n")
    gp = tmp_path / "graph.json"
    gp.write_text(json.dumps({"entries": [
        {"id": "x-6320", "plan_path": str(owner_plan), "superseded_by": "x-9999",
         "deferred_at": "2026-07-29T00:00:00+00:00"},
        {"id": "x-261c", "plan_path": str(child_plan), "contained_in": "x-6320"},
    ]}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: gp)
    ran = _init_env(tmp_path, monkeypatch)

    result = runner.invoke(app, ["do", "target", "init", "--input", "x-261c"])
    assert result.exit_code == 2, result.output
    assert "superseded" in result.output
    assert "--parent null" in result.output
    assert "run `/fno:target x-6320`" not in result.output
    assert ran == []
    _clear_root_cache()


def test_check_contained_says_so_when_the_graph_lock_is_unavailable(tmp_path,
                                                                    monkeypatch,
                                                                    capsys):
    """A lost serialization that still exits 0 reads as "checked, and fine".

    The flock is what makes this check unraceable, so swallowing its failure
    silently is the same degrade this gate has already been fixed for twice.
    Still fail-open - it just says what it lost.
    """
    import fno.graph.store as gs

    _contained_graph(tmp_path, monkeypatch)

    def boom(p):
        raise OSError("no lock for you")

    monkeypatch.setattr(gs, "_acquire_flock", boom)
    monkeypatch.setenv("TARGET_INPUT", "x-261c")
    result = runner.invoke(app, ["do", "target", "check-contained"])
    assert result.exit_code == 9, result.output
    assert "UNSERIALIZED" in result.output
    assert "no lock for you" in result.output


def test_resolve_owned_identity_verb_refuses_collision_resolves_claude(
    tmp_path, monkeypatch
) -> None:
    """AC1-HP + AC3-ERR at the verb: a CODEX_THREAD_ID a live row already owns
    is refused and the proven claude marker wins.

    Proven in Python so it does not depend on the PATH-resolved `fno` carrying
    the verb - the bash hook test's CI limitation, since the PR's own CI runs a
    `fno` that predates the verb. The prover is pinned to claude (a real claude
    process's view) so the claude marker is proven and wins; the foreign codex
    marker is refused as another live row's. CI-robust: no real process tree.
    """
    from fno.agents.registry import register_existing_session
    from fno.paths_testing import use_tmpdir

    use_tmpdir(monkeypatch, tmp_path)
    foreign = "019fc87d-ddff-7c90-926a-6bdd7ebb186c"
    owner = register_existing_session(
        provider="codex", session_id=foreign, cwd="/x"
    ).name
    # Pin the prover to claude (what a real claude session's process tree sees),
    # so the test does not depend on the runner's actual harness.
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_harness",
        lambda from_pid=None: "claude",
    )
    monkeypatch.setenv("CODEX_THREAD_ID", foreign)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "aaaa1111-mine")

    result = runner.invoke(app, ["do", "target", "resolve-owned-identity"])
    assert result.exit_code == 0, result.output
    fields = {
        line.split("=", 1)[0]: line.split("=", 1)[1]
        for line in result.stdout.splitlines()
        if "=" in line
    }
    assert fields["HARNESS"] == "claude"
    assert fields["SESSION_ID"] == "aaaa1111-mine"
    assert fields["DISPOSITION"] == "proven"
    assert fields["COLLISION"] == owner
    assert fields["COLLISION_ID"] == foreign


def test_holder_is_ours_recognizes_own_pid_unavailable_claim(monkeypatch):
    """The transcript-identity arm: init minted ``target-session:<sid>`` from the
    proven harness session when no env id existed, and the pid walk failed, so
    the v2 claim carries no pid for the pid arm to compare. A rerun in the same
    session must still read it as ours instead of parking on its own claim."""
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda env=None: SimpleNamespace(session_id="aaaa1111-mine"),
    )
    info = {"pid_unavailable": True, "host": "other-box", "machine_id": "m2"}
    assert target_cli._holder_is_ours("target-session:aaaa1111-mine", info)
    assert not target_cli._holder_is_ours("target-session:zzz999-not-mine", info)


def test_holder_is_ours_identity_arm_fails_closed(monkeypatch):
    """No provable identity -> no recognition: the claim stays foreign to this
    rerun (park / re-acquire), never assumed ours."""
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda env=None: SimpleNamespace(session_id=""),
    )
    info = {"pid_unavailable": True, "host": "h", "machine_id": "m"}
    assert not target_cli._holder_is_ours("target-session:anything", info)

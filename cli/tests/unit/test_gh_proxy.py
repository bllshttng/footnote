from __future__ import annotations

import os
import sys

import pytest

from fno.pr import gh_proxy
from fno.pr._proc import Result
from fno.pr.gh_proxy import Action, classify, command_args, delegate
from fno.setup.github_cli import InstallResult, ensure_proxy, worker_environment


def test_proxy_classifies_every_graphql_gh_surface():
    assert classify(["pr", "view", "930", "--json", "headRefOid"]) is Action.BROKER
    assert classify(["pr", "checks", "930"]) is Action.BROKER
    assert classify(["pr", "view", "930"]) is Action.BROKER
    assert classify(["pr", "view", "930", "--json=state"]) is Action.BROKER
    assert classify(["pr", "list", "--state", "open"]) is Action.BROKER
    assert classify(["pr", "status"]) is Action.BROKER
    assert classify(["api", "graphql", "-f", "query={viewer{login}}"]) is Action.BROKER
    assert classify(["api", "-X", "POST", "graphql", "-f", "query=x"]) is Action.BROKER
    assert classify(["api", "--hostname", "github.com", "graphql"]) is Action.BROKER
    assert classify(["api", "repos/o/r/pulls/930"]) is Action.DELEGATE


def test_proxy_normalizes_repo_global_options_before_classifying():
    assert command_args(["-R", "o/r", "pr", "view", "930"]) == ["pr", "view", "930"]
    assert classify(["--repo=o/r", "pr", "checks", "930"]) is Action.BROKER
    assert classify(["-Ro/r", "pr", "list"]) is Action.BROKER
    assert classify(["pr", "-R", "o/r", "view", "930"]) is Action.BROKER
    assert classify(["pr", "--repo=o/r", "checks", "930"]) is Action.BROKER


def test_install_backs_up_an_unrelated_existing_wrapper(tmp_path):
    proxy = tmp_path / "gh"
    proxy.write_text("#!/bin/sh\necho existing\n")
    proxy.chmod(0o755)
    real = tmp_path / "real-gh"
    real.write_text("real")
    result = ensure_proxy(directory=tmp_path, real_gh=real)
    assert result.changed is True
    assert result.backup is not None
    assert result.backup.read_text() == "#!/bin/sh\necho existing\n"
    assert "fno-gh-proxy" in proxy.read_text()
    assert os.access(proxy, os.X_OK)


def test_missing_gh_fails_before_loading_configured_proxy_path(monkeypatch):
    def path_must_not_load():
        raise AssertionError("configured paths loaded before gh existence check")

    monkeypatch.setattr(
        "fno.setup.github_cli.github_cli_proxy_dir", path_must_not_load
    )
    with pytest.raises(FileNotFoundError, match="real gh executable not found"):
        ensure_proxy(which=lambda _: None)


def test_worker_environment_prepends_proxy_and_pins_delegate(tmp_path, monkeypatch):
    real = tmp_path / "real-gh"
    real.write_text("real")
    proxy = tmp_path / "proxy" / "gh"
    monkeypatch.setattr(
        "fno.setup.github_cli.ensure_proxy",
        lambda **_: InstallResult(proxy=proxy, delegate=real.resolve(), changed=False),
    )
    env = worker_environment({"PATH": "/usr/bin", "KEEP": "yes"})
    assert env["PATH"].split(os.pathsep)[0] == str(tmp_path / "proxy")
    assert "FNO_REAL_GH" not in env
    assert env["KEEP"] == "yes"


def test_nested_worker_reuses_the_pinned_real_delegate(tmp_path, monkeypatch):
    proxy_dir = tmp_path / "proxy"
    real = tmp_path / "real-gh"
    real.write_text("real")
    monkeypatch.setattr("fno.setup.github_cli.github_cli_proxy_dir", lambda: proxy_dir)
    nested = worker_environment(
        {"PATH": f"{proxy_dir}:/usr/bin", "FNO_REAL_GH": str(real)}
    )
    assert "FNO_REAL_GH" not in nested
    assert nested["PATH"].split(os.pathsep)[0] == str(proxy_dir)


def test_worker_environment_surfaces_proxy_install_io_failure(monkeypatch):
    def fail(**kwargs):
        raise PermissionError("read-only state root")

    monkeypatch.setattr("fno.setup.github_cli.ensure_proxy", fail)
    with __import__("pytest").raises(PermissionError, match="read-only state root"):
        worker_environment({"PATH": "/usr/bin"})


def test_worker_environment_inherits_env_when_config_layer_is_unimportable(monkeypatch):
    """A bare interpreter without the venv-only config deps (the codex ask
    parity harness drives exactly that, with gh on PATH as CI has it) must
    inherit the parent env, not crash every ask-path spawn at import time."""
    def fail(**kwargs):
        raise ModuleNotFoundError("No module named 'tomli_w'")

    monkeypatch.setattr("fno.setup.github_cli.ensure_proxy", fail)
    env = worker_environment({"PATH": "/usr/bin", "KEEP": "yes"})
    assert env == {"PATH": "/usr/bin", "KEEP": "yes"}


def test_delegate_replaces_proxy_to_preserve_tty(monkeypatch):
    seen = {}

    def execve(path, argv, env):
        seen["path"] = path
        seen["argv"] = argv
        seen["path_env"] = env["PATH"]
        raise RuntimeError("exec sentinel")

    monkeypatch.setattr("fno.pr.gh_proxy.os.execve", execve)
    monkeypatch.setattr(
        "fno.pr.gh_proxy._quota.delegate_environment", lambda: {"PATH": "/real/bin"}
    )
    with __import__("pytest").raises(RuntimeError, match="exec sentinel"):
        delegate("/real/gh", ["auth", "status"])
    assert seen == {
        "path": "/real/gh",
        "argv": ["/real/gh", "auth", "status"],
        "path_env": "/real/bin",
    }


def test_direct_pr_view_reaches_the_shared_floor_and_diagnostic(monkeypatch, capsys):
    monkeypatch.setattr(sys, "argv", ["gh", "pr", "view", "930", "--json", "reviews"])
    monkeypatch.setattr(gh_proxy._quota, "resolve_real_gh", lambda: "/real/gh")
    monkeypatch.setattr(
        gh_proxy._quota,
        "execute_graphql",
        lambda *args, **kwargs: Result(
            75,
            "",
            "GraphQL discretionary read refused: reset at 2026-08-19T00:00:00Z. "
            "Use `fno pr info 930` for state/head/mergeability and `fno pr status 930` "
            "for CI; stop retrying GraphQL until reset. `fno pr status` still contains "
            "optional review-thread and coverage reads that are GraphQL; those reads "
            "preserve the reserved coverage budget.",
        ),
    )
    with pytest.raises(SystemExit, match="75"):
        gh_proxy.main()
    err = capsys.readouterr().err
    assert "Use `fno pr info 930`" in err
    assert "stop retrying GraphQL until reset" in err
    assert "coverage reads that are GraphQL" in err

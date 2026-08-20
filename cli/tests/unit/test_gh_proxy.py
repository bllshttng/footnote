from __future__ import annotations

import os
import sys

import pytest
from typer.testing import CliRunner

from fno.cli import _protect_process_path as protect_process_path
from fno.pr import gh_proxy
from fno.pr._proc import Result
from fno.pr.gh_proxy import Action, classify, command_args, delegate
from fno.setup.github_cli import InstallResult, ensure_proxy, worker_environment


def test_cli_root_callback_enters_process_proxy_boundary(monkeypatch):
    from fno.cli import app

    calls = []
    monkeypatch.setattr("fno.cli._protect_process_path", lambda ctx: calls.append(ctx))
    result = CliRunner().invoke(app, ["--version"])
    assert result.exit_code == 0
    assert len(calls) == 1


def test_process_proxy_boundary_restores_path_on_close(monkeypatch, tmp_path):
    original = "/original/bin"
    proxy = str(tmp_path / "proxy")
    monkeypatch.setenv("PATH", original)
    monkeypatch.setattr(
        "fno.setup.github_cli.worker_environment",
        lambda base: {**dict(base), "PATH": f"{proxy}{os.pathsep}{original}"},
    )
    closed = []

    class Context:
        def call_on_close(self, callback):
            closed.append(callback)

    protect_process_path(Context())
    assert os.environ["PATH"].split(os.pathsep)[0] == proxy
    assert os.environ["FNO_GH_PROXY_DIR"]
    assert len(closed) == 1
    closed[0]()
    assert os.environ["PATH"] == original
    assert "FNO_GH_PROXY_DIR" not in os.environ


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
    env = worker_environment(
        {"PATH": "/usr/bin", "KEEP": "yes", "FNO_REAL_GH": str(real)}
    )
    assert env["PATH"].split(os.pathsep)[0] == str(tmp_path / "proxy")
    assert env["FNO_GH_PROXY_DIR"] == str(tmp_path / "proxy")
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


def test_worker_environment_surfaces_proxy_install_io_failure(monkeypatch, tmp_path):
    real = tmp_path / "real-gh"
    real.write_text("real")

    def fail(**kwargs):
        raise PermissionError("read-only state root")

    monkeypatch.setattr("fno.setup.github_cli.ensure_proxy", fail)
    with __import__("pytest").raises(PermissionError, match="read-only state root"):
        worker_environment({"PATH": "/usr/bin", "FNO_REAL_GH": str(real)})


def test_worker_environment_survives_a_binary_delegate(tmp_path, monkeypatch):
    real = tmp_path / "gh"
    real.write_bytes(b"\x7fELF\x02" + b"\x90" * 120)
    proxy = tmp_path / "proxy" / "gh"
    monkeypatch.setattr(
        "fno.setup.github_cli.ensure_proxy",
        lambda **_: InstallResult(proxy=proxy, delegate=real, changed=True),
    )
    env = worker_environment({"PATH": "/usr/bin", "FNO_REAL_GH": str(real)})
    assert env["PATH"].split(os.pathsep)[0] == str(tmp_path / "proxy")


def test_worker_environment_uses_config_free_fallback(monkeypatch, tmp_path):
    real = tmp_path / "real-gh"
    real.write_text("real")
    fallback = tmp_path / "fallback"
    calls = []

    def install(**kwargs):
        calls.append(kwargs.get("directory"))
        if kwargs.get("directory") is None:
            raise AttributeError("settings stub has no state_dir")
        return InstallResult(proxy=fallback / "gh", delegate=real, changed=True)

    monkeypatch.setattr("fno.setup.github_cli.ensure_proxy", install)
    monkeypatch.setattr("fno.setup.github_cli.fallback_proxy_dir", lambda: fallback)
    env = worker_environment({"PATH": "/usr/bin", "FNO_REAL_GH": str(real)})
    assert calls == [None, fallback]
    assert env["PATH"].split(os.pathsep)[0] == str(fallback)
    assert env["FNO_GH_PROXY_DIR"] == str(fallback)


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


def test_resolve_real_gh_returns_a_path_even_with_no_proxy_dir_to_skip(
    tmp_path, monkeypatch
):
    """os.execve needs a path with a slash, never a bare command name.

    The real gh being the FIRST match on PATH (no proxy entry ahead of it to
    skip past) used to return the literal string "gh" instead of a resolved
    path, and delegate()'s os.execve("gh", ...) fails with FileNotFoundError:
    execve does not search PATH the way execvp does.
    """
    from fno.pr import _quota

    real = tmp_path / "gh"
    real.write_text("#!/bin/sh\necho real\n")
    real.chmod(0o755)
    monkeypatch.delenv("FNO_REAL_GH", raising=False)
    monkeypatch.delenv("FNO_GH_PROXY_DIR", raising=False)
    monkeypatch.setattr(_quota, "_proxy_dirs", lambda: set())
    monkeypatch.setenv("PATH", str(tmp_path))

    resolved = _quota.resolve_real_gh()

    assert resolved == str(real.resolve())
    assert os.sep in resolved


def test_main_refuses_to_run_when_the_reentry_marker_is_set(monkeypatch, capsys):
    # os.execve leaves no child, so the marker is the only evidence of a repeat.
    monkeypatch.setattr(sys, "argv", ["gh", "api", "rate_limit"])
    monkeypatch.setenv("FNO_GH_PROXY_DEPTH", "1")
    with pytest.raises(SystemExit, match="1"):
        gh_proxy.main()
    assert "re-entered itself" in capsys.readouterr().err


def test_delegate_stamps_the_reentry_marker_into_the_exec_environment(monkeypatch):
    seen = {}

    def execve(path, argv, env):
        seen["env"] = env
        raise RuntimeError("exec sentinel")

    monkeypatch.setattr("fno.pr.gh_proxy.os.execve", execve)
    monkeypatch.setattr(
        "fno.pr.gh_proxy._quota.delegate_environment", lambda: {"PATH": "/real/bin"}
    )
    with pytest.raises(RuntimeError, match="exec sentinel"):
        delegate("/real/gh", ["auth", "status"])
    assert seen["env"]["FNO_GH_PROXY_DEPTH"] == "1"


def test_main_turns_a_proxy_identity_failure_into_a_clean_refusal(monkeypatch, capsys):
    monkeypatch.setattr(sys, "argv", ["gh", "auth", "status"])
    monkeypatch.delenv("FNO_GH_PROXY_DEPTH", raising=False)

    def broken():
        raise gh_proxy._quota.ProxyIdentityError("config load failed")

    monkeypatch.setattr(gh_proxy._quota, "resolve_real_gh", broken)
    with pytest.raises(SystemExit, match="2"):
        gh_proxy.main()
    assert "cannot identify its own install directory" in capsys.readouterr().err


def test_process_proxy_boundary_drops_an_inherited_reentry_marker(monkeypatch, tmp_path):
    # The boundary prepends the proxy to PATH. Every bare-`gh` shell-out from
    # this process then hits it, and an inherited marker would refuse them all.
    original = "/original/bin"
    proxy = str(tmp_path / "proxy")
    monkeypatch.setenv("PATH", original)
    monkeypatch.setenv("FNO_GH_PROXY_DEPTH", "1")
    monkeypatch.setattr(
        "fno.setup.github_cli.worker_environment",
        lambda base: {
            key: value
            for key, value in {**dict(base), "PATH": f"{proxy}{os.pathsep}{original}"}.items()
            if key != "FNO_GH_PROXY_DEPTH"
        },
    )
    closed = []

    class Context:
        def call_on_close(self, callback):
            closed.append(callback)

    protect_process_path(Context())
    assert "FNO_GH_PROXY_DEPTH" not in os.environ
    closed[0]()
    assert os.environ["FNO_GH_PROXY_DEPTH"] == "1"


def test_worker_environment_drops_the_marker_even_with_no_gh_to_proxy(monkeypatch):
    # The early return is one of three exits, and a guard on one of N paths is
    # decorative: a PATH that gains gh later would meet a marker it never earned.
    env = worker_environment({"PATH": "/nowhere", "FNO_GH_PROXY_DEPTH": "1"})
    assert "FNO_GH_PROXY_DEPTH" not in env


def test_worker_environment_drops_an_inherited_reentry_marker(monkeypatch, tmp_path):
    # A delegated gh runs git, a git hook runs fno, and fno builds a worker env
    # from os.environ. The marker rides along and the worker's first, only gh
    # call would refuse. Same reason FNO_REAL_GH is dropped one line above.
    real = tmp_path / "real"
    real.mkdir()
    gh = real / "gh"
    gh.write_text("#!/bin/sh\necho real\n")
    gh.chmod(0o755)
    env = worker_environment(
        {"PATH": str(real), "FNO_GH_PROXY_DEPTH": "1", "FNO_GH_PROXY_DIR": str(tmp_path / "proxy")}
    )
    assert "FNO_GH_PROXY_DEPTH" not in env


def test_delegate_environment_drops_the_marker_before_running_the_real_gh(monkeypatch):
    monkeypatch.setenv("FNO_GH_PROXY_DEPTH", "1")
    monkeypatch.setenv("PATH", "/usr/bin")
    assert "FNO_GH_PROXY_DEPTH" not in gh_proxy._quota.delegate_environment()

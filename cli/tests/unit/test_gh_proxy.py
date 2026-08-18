from __future__ import annotations

import os
import sys

import pytest

from fno.pr import gh_proxy
from fno.pr._proc import Result
from fno.pr.gh_proxy import Action, classify, delegate
from fno.setup.github_cli import InstallResult, ensure_proxy, worker_environment


def test_proxy_classifies_every_graphql_gh_surface():
    assert classify(["pr", "view", "930", "--json", "headRefOid"]) is Action.BROKER
    assert classify(["pr", "checks", "930"]) is Action.BROKER
    assert classify(["api", "graphql", "-f", "query={viewer{login}}"]) is Action.BROKER
    assert classify(["api", "repos/o/r/pulls/930"]) is Action.DELEGATE


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


def test_delegate_replaces_proxy_to_preserve_tty(monkeypatch):
    seen = {}

    def execv(path, argv):
        seen["path"] = path
        seen["argv"] = argv
        raise RuntimeError("exec sentinel")

    monkeypatch.setattr("fno.pr.gh_proxy.os.execv", execv)
    with __import__("pytest").raises(RuntimeError, match="exec sentinel"):
        delegate("/real/gh", ["auth", "status"])
    assert seen == {"path": "/real/gh", "argv": ["/real/gh", "auth", "status"]}


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

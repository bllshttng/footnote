from __future__ import annotations

import json
import os

import pytest

from fno.pr import _quota
from fno.pr._proc import Result


def _runner(remaining: int | None, calls: list[list[str]], command_result: Result | None = None):
    def run(cmd, **kwargs):
        calls.append(list(cmd))
        if cmd[-2:] == ["api", "rate_limit"]:
            if remaining is None:
                return Result(1, "", "instrument unavailable")
            return Result(
                0,
                json.dumps({"resources": {"graphql": {"remaining": remaining, "reset": 1787072400}}}),
                "",
            )
        return command_result or Result(0, '{"data":{"ok":true}}', "")

    return run


def test_discretionary_read_is_refused_at_floor_before_graphql(tmp_path):
    calls: list[list[str]] = []
    result = _quota.execute_graphql(
        "discretionary",
        ["pr", "view", "930", "--json", "headRefOid"],
        runner=_runner(_quota.GRAPHQL_RESERVE, calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == _quota.REFUSED
    assert calls == [["/real/gh", "api", "rate_limit"]]
    assert "fno pr info 930" in result.stderr
    assert "stop retrying GraphQL until" in result.stderr
    assert "still contains optional review-thread and coverage reads" in result.stderr


def test_coverage_can_consume_reserved_points(tmp_path):
    calls: list[list[str]] = []
    result = _quota.execute_graphql(
        "coverage",
        [
            "pr", "view", "930", "--json",
            "reviews,comments,headRefOid,baseRefName",
        ],
        runner=_runner(_quota.GRAPHQL_RESERVE, calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 0
    assert calls == [
        ["/real/gh", "api", "rate_limit"],
        [
            "/real/gh", "pr", "view", "930", "--json",
            "reviews,comments,headRefOid,baseRefName",
        ],
    ]


def test_coverage_publisher_label_read_can_consume_reserved_points(tmp_path):
    calls: list[list[str]] = []
    result = _quota.execute_graphql(
        "coverage",
        ["pr", "view", "930", "--json", "labels"],
        runner=_runner(_quota.GRAPHQL_RESERVE, calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 0
    assert calls[-1] == ["/real/gh", "pr", "view", "930", "--json", "labels"]


def test_unreadable_instrument_fails_closed_only_for_discretionary(tmp_path):
    discretionary_calls: list[list[str]] = []
    refused = _quota.execute_graphql(
        "discretionary",
        ["pr", "view", "930", "--json", "commits"],
        runner=_runner(None, discretionary_calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert refused.returncode == _quota.REFUSED
    assert "instrument unavailable" in refused.stderr
    assert len(discretionary_calls) == 1

    coverage_calls: list[list[str]] = []
    allowed = _quota.execute_graphql(
        "coverage",
        ["pr", "view", "930", "--json", "commits"],
        runner=_runner(None, coverage_calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert allowed.returncode == 0
    assert len(coverage_calls) == 2


def test_coverage_purpose_rejects_arbitrary_graphql(tmp_path):
    calls: list[list[str]] = []
    result = _quota.execute_graphql(
        "coverage",
        ["api", "graphql", "-f", "query={viewer{login}}"],
        runner=_runner(5000, calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 2
    assert "review-coverage reads only" in result.stderr
    assert calls == []


def test_only_coverage_spelling_can_claim_the_reserve(tmp_path):
    calls: list[list[str]] = []
    result = _quota.execute_graphql(
        "priority",
        ["api", "graphql", "-f", "query={viewer{login}}"],
        runner=_runner(5000, calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 2
    assert calls == []


def test_bare_gh_cannot_reenter_the_worker_proxy(tmp_path, monkeypatch):
    calls: list[list[str]] = []
    real = tmp_path / "real-gh"
    real.write_text("#!/bin/sh\nexit 0\n")
    real.chmod(0o755)
    monkeypatch.setenv("FNO_REAL_GH", str(real))
    result = _quota.execute_graphql(
        "discretionary",
        ["pr", "view", "930", "--json", "reviews"],
        runner=_runner(5000, calls),
        real_gh="gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 0
    assert calls == [
        [str(real), "api", "rate_limit"],
        [str(real), "pr", "view", "930", "--json", "reviews"],
    ]


def test_preserved_wrapper_cannot_resolve_back_to_quota_proxy(tmp_path, monkeypatch):
    proxy_dir = tmp_path / "proxy"
    wrapper_dir = tmp_path / "wrapper"
    real_dir = tmp_path / "real"
    for directory in (proxy_dir, wrapper_dir, real_dir):
        directory.mkdir()
    proxy = proxy_dir / "gh"
    proxy.write_text("#!/bin/sh\necho proxy-recursion >&2\nexit 99\n")
    wrapper = wrapper_dir / "gh"
    wrapper.write_text(
        "#!/bin/sh\n"
        "self=$(dirname \"$0\")\n"
        "old=$IFS; IFS=:\n"
        "for d in $PATH; do [ \"$d\" = \"$self\" ] && continue; "
        "[ -x \"$d/gh\" ] && exec \"$d/gh\" \"$@\"; done\n"
        "IFS=$old\nexit 127\n"
    )
    real = real_dir / "gh"
    real.write_text(
        "#!/bin/sh\n"
        "if [ \"$*\" = \"api rate_limit\" ]; then "
        "echo '{\"resources\":{\"graphql\":{\"remaining\":5000,\"reset\":1787072400}}}'; "
        "else echo '{\"data\":{\"viewer\":{\"login\":\"ok\"}}}'; fi\n"
    )
    for executable in (proxy, wrapper, real):
        executable.chmod(0o755)
    monkeypatch.setattr(_quota, "github_cli_proxy_dir", lambda: proxy_dir)
    monkeypatch.delenv("FNO_REAL_GH", raising=False)
    monkeypatch.setenv("PATH", f"{proxy_dir}:{wrapper_dir}:{real_dir}:/usr/bin:/bin")
    result = _quota.execute_graphql(
        "discretionary",
        ["api", "graphql", "-f", "query={viewer{login}}"],
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 0
    assert '"login":"ok"' in result.stdout


def test_delegate_environment_strips_fallback_proxy_from_env(monkeypatch, tmp_path):
    fallback = tmp_path / "fallback"
    real = tmp_path / "real"
    monkeypatch.setenv("FNO_GH_PROXY_DIR", str(fallback))
    monkeypatch.setenv("PATH", f"{fallback}:{real}:/usr/bin")
    env = _quota.delegate_environment()
    assert env["PATH"].split(os.pathsep) == [str(real), "/usr/bin"]
    assert "FNO_GH_PROXY_DIR" not in env


def _shim(directory, name="gh"):
    """A proxy shim as `fno setup` writes it: a two-line script calling the broker."""
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / name
    path.write_text('#!/bin/sh\nexec fno-gh-proxy "$@"\n')
    path.chmod(0o755)
    return path


def _real(directory, name="gh"):
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / name
    path.write_text("#!/bin/sh\necho real\n")
    path.chmod(0o755)
    return path


def test_resolve_real_gh_never_returns_a_bare_name(monkeypatch, tmp_path):
    """A bare "gh" is unusable by both callers, so it must never be returned.

    `delegate` execve's the result, and execve does no PATH lookup, so a bare
    name raises FileNotFoundError. `execute_graphql` runs the result while
    holding the flock, and a bare name re-enters the shim and blocks forever.
    """
    shim_dir = tmp_path / "unrecognized-shim"
    real_dir = tmp_path / "real"
    _shim(shim_dir)
    real = _real(real_dir)
    # The shim's own directory is TMPDIR-derived. A caller whose TMPDIR differs
    # (a background job, a launchd agent) computes a different one and so does
    # not recognize the shim sitting first on its inherited PATH.
    monkeypatch.setattr(_quota, "github_cli_proxy_dir", lambda: tmp_path / "elsewhere")
    monkeypatch.delenv("FNO_GH_PROXY_DIR", raising=False)
    monkeypatch.delenv("FNO_REAL_GH", raising=False)
    monkeypatch.setenv("PATH", f"{shim_dir}:{real_dir}")

    resolved = _quota.resolve_real_gh()

    assert resolved == str(real.resolve())
    assert os.path.isabs(resolved)


def test_resolve_real_gh_refuses_a_configured_shim(monkeypatch, tmp_path):
    """FNO_REAL_GH pointing at a shim would loop the broker into itself."""
    shim_dir = tmp_path / "shim"
    real_dir = tmp_path / "real"
    shim = _shim(shim_dir)
    real = _real(real_dir)
    monkeypatch.setattr(_quota, "github_cli_proxy_dir", lambda: tmp_path / "elsewhere")
    monkeypatch.delenv("FNO_GH_PROXY_DIR", raising=False)
    monkeypatch.setenv("FNO_REAL_GH", str(shim))
    monkeypatch.setenv("PATH", f"{real_dir}")

    assert _quota.resolve_real_gh() == str(real.resolve())


def test_delegate_environment_strips_an_unrecognized_proxy_shim(monkeypatch, tmp_path):
    """The delegate's own PATH walk must not find a shim we failed to name."""
    shim_dir = tmp_path / "unrecognized-shim"
    real_dir = tmp_path / "real"
    _shim(shim_dir)
    _real(real_dir)
    monkeypatch.setattr(_quota, "github_cli_proxy_dir", lambda: tmp_path / "elsewhere")
    monkeypatch.delenv("FNO_GH_PROXY_DIR", raising=False)
    monkeypatch.setenv("PATH", f"{shim_dir}:{real_dir}:/usr/bin")

    assert _quota.delegate_environment()["PATH"].split(os.pathsep) == [
        str(real_dir),
        "/usr/bin",
    ]


def test_is_proxy_shim_detects_a_padded_shim_over_the_old_size_gate(tmp_path):
    """A shim padded past 4096 bytes must still be recognized as the shim.

    Padding can land on either side of the exec line, so this checks both:
    trailing padding (the common case) and leading padding, which pushed the
    exec line past a naive head-bounded read in an earlier version of this fix.
    """
    trailing = tmp_path / "gh-trailing"
    padding = "# padding\n" * 1000
    trailing.write_text('#!/bin/sh\nexec fno-gh-proxy "$@"\n' + padding)
    trailing.chmod(0o755)
    assert trailing.stat().st_size > 4096
    assert _quota._is_proxy_shim(trailing) is True

    leading = tmp_path / "gh-leading"
    leading.write_text(("# padding\n" * 500) + '#!/bin/sh\nexec fno-gh-proxy "$@"\n')
    leading.chmod(0o755)
    assert leading.stat().st_size > 4096
    assert _quota._is_proxy_shim(leading) is True


def test_is_proxy_shim_does_not_match_a_real_gh_that_mentions_the_name(tmp_path):
    """A comment merely naming the proxy must not be classified as the shim.

    A bare substring match would misclassify this as the shim, and
    ``resolve_real_gh``/``delegate_environment`` would then skip a working
    real ``gh``, turning it into "gh not found".
    """
    path = tmp_path / "gh"
    path.write_text("#!/bin/sh\n# not the fno-gh-proxy wrapper, just mentions it\necho real\n")
    path.chmod(0o755)

    assert _quota._is_proxy_shim(path) is False


def test_proxy_dirs_fails_closed_instead_of_reporting_no_proxy(monkeypatch):
    # An empty set reads as "there is no proxy", and resolve_real_gh then hands
    # back the proxy's own shim. The lookup failure must surface, not vanish.
    def broken():
        raise RuntimeError("config load failed")

    monkeypatch.setattr(_quota, "github_cli_proxy_dir", broken)
    monkeypatch.delenv("FNO_GH_PROXY_DIR", raising=False)
    with pytest.raises(_quota.ProxyIdentityError, match="config load failed"):
        _quota._proxy_dirs()


def test_proxy_dirs_keeps_the_inherited_dir_when_the_config_load_fails(monkeypatch, tmp_path):
    # An inherited FNO_GH_PROXY_DIR already answers "where do my shims live?".
    # Refusing there would fail every gh command over an unrelated config error.
    def broken():
        raise RuntimeError("config load failed")

    monkeypatch.setattr(_quota, "github_cli_proxy_dir", broken)
    monkeypatch.setenv("FNO_GH_PROXY_DIR", str(tmp_path))
    assert _quota._proxy_dirs() == {str(tmp_path.resolve())}


def test_execute_graphql_refuses_when_the_proxy_cannot_identify_itself(monkeypatch):
    def broken():
        raise RuntimeError("config load failed")

    monkeypatch.setattr(_quota, "github_cli_proxy_dir", broken)
    monkeypatch.delenv("FNO_GH_PROXY_DIR", raising=False)
    monkeypatch.delenv("FNO_REAL_GH", raising=False)
    result = _quota.execute_graphql("discretionary", ["api", "graphql", "-f", "query=x"])
    assert result.returncode == 2
    assert "cannot identify its own install directory" in result.stderr

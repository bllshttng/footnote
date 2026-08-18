from __future__ import annotations

import json

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
        ["pr", "view", "930", "--json", "reviews,comments"],
        runner=_runner(_quota.GRAPHQL_RESERVE, calls),
        real_gh="/real/gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 0
    assert calls == [
        ["/real/gh", "api", "rate_limit"],
        ["/real/gh", "pr", "view", "930", "--json", "reviews,comments"],
    ]


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
        ["api", "graphql", "-f", "query={viewer{login}}"],
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
    monkeypatch.setenv("FNO_REAL_GH", "/real/gh")
    result = _quota.execute_graphql(
        "discretionary",
        ["pr", "view", "930", "--json", "reviews"],
        runner=_runner(5000, calls),
        real_gh="gh",
        lock_path=tmp_path / "quota.lock",
    )
    assert result.returncode == 0
    assert calls == [
        ["/real/gh", "api", "rate_limit"],
        ["/real/gh", "pr", "view", "930", "--json", "reviews"],
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

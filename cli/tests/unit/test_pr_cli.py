"""CLI-layer tests for `fno do pr merge` after the in-package port (ab-d4c98550).

The merge logic itself is characterized in test_pr_merge.py; here we only
assert the Typer verb dispatches into the in-package _merge module and
propagates its exit code (the old "forwards to pr-merge.sh" assertions are
retired - the bash is gone).
"""
from __future__ import annotations

import json
import re
import shlex
from pathlib import Path

from typer.testing import CliRunner

from fno.cli import app
from fno.pr import _merge, _quota

runner = CliRunner()


def test_pr_info_prints_rest_metadata(monkeypatch):
    from fno.pr import _rest

    monkeypatch.setattr(
        _rest,
        "fetch_pr_info_rest",
        lambda pr, cwd=None, repo=None: (
            {
                "pr": 930,
                "url": "https://github.com/Owner/Repo/pull/930",
                "state": "OPEN",
                "head_sha": "abc123",
                "head_ref": "feature/x",
                "base_ref": "main",
                "mergeable": "MERGEABLE",
                "merged_at": None,
            },
            "",
        ),
    )
    result = runner.invoke(app, ["do", "pr", "info", "930", "--repo", "Owner/Repo"])
    assert result.exit_code == 0
    assert json.loads(result.stdout) == {
        "pr": 930,
        "url": "https://github.com/Owner/Repo/pull/930",
        "state": "OPEN",
        "head_sha": "abc123",
        "head_ref": "feature/x",
        "base_ref": "main",
        "mergeable": "MERGEABLE",
        "merged_at": None,
    }


def test_pr_list_prints_rest_summaries(monkeypatch):
    from fno.pr import _rest

    monkeypatch.setattr(
        _rest,
        "list_prs_rest",
        lambda slug, **kwargs: (
            [
                {
                    "number": 930,
                    "state": "OPEN",
                    "title": "Quota reserve",
                    "headRefName": "feature/x",
                    "url": "https://github.com/o/r/pull/930",
                }
            ],
            "",
        ),
    )
    result = runner.invoke(app, ["do", "pr", "list", "--repo", "o/r"])
    assert result.exit_code == 0
    assert json.loads(result.stdout)[0]["headRefName"] == "feature/x"


def test_graphql_exec_rejects_public_coverage_purpose():
    result = runner.invoke(
        app,
        ["do", "pr", "graphql-exec", "--purpose", "coverage", "--", "pr", "view", "930"],
    )
    assert result.exit_code == 2


def test_fno_process_routes_subprocesses_through_proxy(monkeypatch):
    monkeypatch.setenv("PATH", "/real/bin")
    monkeypatch.setattr(
        "fno.setup.github_cli.worker_environment",
        lambda base: {**base, "PATH": "/quota-proxy:/real/bin"},
    )

    def _fake(argv, cwd=None):
        assert __import__("os").environ["PATH"] == "/quota-proxy:/real/bin"
        return 2

    monkeypatch.setattr(_merge, "run_merge", _fake)
    result = runner.invoke(app, ["do", "pr", "merge", "930"])
    assert result.exit_code == 2
    assert __import__("os").environ["PATH"] == "/real/bin"


def test_pr_help_renders():
    result = runner.invoke(app, ["do", "pr", "--help"])
    assert result.exit_code == 0
    assert "merge" in result.stdout


def test_pr_merge_help_renders():
    result = runner.invoke(app, ["do", "pr", "merge", "--help"])
    assert result.exit_code == 0


_HOOK_SOURCE = (
    Path(__file__).resolve().parents[3] / "hooks" / "git-protection.py"
)
_ROUTE_RE = re.compile(r"fno do pr ([a-z][a-z0-9-]*)")


def _refusal_named_pr_verbs() -> set[str]:
    """Every `fno do pr <verb>` a refusal surface names, read from source.

    Scans the PreToolUse hook text, the quota broker's floor refusal, and the
    broker's malformed-argv refusal, so a future refusal naming a missing or
    shape-broken verb fails the guard test instead of advertising a remedy
    nobody executed. One hop through each surface's rendered help covers help
    text that names further routes (the graphql-exec epilog teaches its own
    argv); deeper text like an unrelated verb's skill reference stays out.
    """
    texts = [
        _HOOK_SOURCE.read_text(),
        _quota._refusal(["pr", "view", "930"], reset=None),
        _quota._malformed_argv_refusal("-f"),
    ]
    frontier: set[str] = set()
    for text in texts:
        frontier.update(_ROUTE_RE.findall(text))
    verbs: set[str] = set()
    while frontier:
        verb = frontier.pop()
        if verb in verbs:
            continue
        result = runner.invoke(app, ["do", "pr", verb, "--help"])
        assert result.exit_code == 0, (
            f"refusal names `fno do pr {verb}` but --help fails"
        )
        verbs.add(verb)
        frontier.update(_ROUTE_RE.findall(result.stdout))
    assert verbs, "route scan found nothing; the guard went blind"
    return verbs


def test_every_refusal_named_route_is_a_registered_command():
    for verb in sorted(_refusal_named_pr_verbs()):
        result = runner.invoke(app, ["do", "pr", verb, "--help"])
        assert result.exit_code == 0, f"refusal names `fno do pr {verb}` but --help fails"


def test_refusal_named_routes_advertise_the_shape_they_imply():
    helps = {
        verb: runner.invoke(app, ["do", "pr", verb, "--help"]).stdout
        for verb in _refusal_named_pr_verbs()
    }
    assert "api graphql" in helps["graphql-exec"], (
        "graphql-exec help must teach the argv the reserve refusal routes to"
    )
    assert "PR_NUMBER" in helps["status"], "status help must advertise the PR argument"
    assert "PR_NUMBER" in helps["info"], "info help must advertise the PR argument"


def _graphql_route_argv() -> list[str]:
    """Tokenize the exact backtick-quoted route the hook refusal prints.

    The refusal literal spans source lines, so the captured span can carry a
    newline, indentation, and the adjacent string-literal quotes; strip those
    before tokenizing. What remains is the route exactly as it prints.
    """
    route = re.search(r"`fno do pr graphql-exec[^`]*`", _HOOK_SOURCE.read_text())
    assert route, "hook refusal no longer names a graphql-exec route"
    raw = re.sub(r"\s+", " ", route.group(0).strip("`").replace('"', ""))
    tokens = shlex.split(raw)
    tokens = [
        "query={ viewer { login } }" if tok == "query=..." else tok for tok in tokens
    ]
    tokens += ["--jq", ".data.viewer.login"]
    assert tokens[:2] == ["fno", "do"]
    return tokens[1:]


def _fake_gh(tmp_path: Path, monkeypatch) -> Path:
    """A real executable standing in for gh, so the full exec path runs."""
    gh = tmp_path / "gh"
    rec = tmp_path / "gh-calls.txt"
    rate_limit_json = '{"resources":{"graphql":{"remaining":5000,"reset":1787072400}}}'
    viewer_json = '{"data":{"viewer":{"login":"bllshttng"}}}'
    gh.write_text(
        "#!/bin/sh\n"
        f'printf \'%s\\n\' "$*" >> {shlex.quote(str(rec))}\n'
        'if [ "$1 $2" = "api rate_limit" ]; then\n'
        f"  printf '{rate_limit_json}'\n"
        'elif [ "$1 $2" = "api graphql" ]; then\n'
        f"  printf '{viewer_json}'\n"
        "fi\n"
    )
    gh.chmod(0o755)
    monkeypatch.setattr(_quota, "resolve_real_gh", lambda: str(gh))
    monkeypatch.setattr(_quota, "quota_lock_path", lambda: tmp_path / "quota.lock")
    return rec


def test_the_exact_refusal_printed_command_returns_a_query_result(tmp_path, monkeypatch):
    """Positive marker: the command the refusal prints yields a query payload."""
    rec = _fake_gh(tmp_path, monkeypatch)
    result = runner.invoke(app, _graphql_route_argv())
    assert result.exit_code == 0, result.output
    assert "bllshttng" in result.stdout
    recorded = rec.read_text()
    assert "api rate_limit" in recorded
    assert "api graphql" in recorded and "query={ viewer { login } }" in recorded, (
        f"the exec argv lost the query; recorded: {recorded!r}"
    )


def test_flag_first_route_argv_is_refused_at_the_cli(tmp_path, monkeypatch):
    """The king's shape: flags after -- with no command word must refuse."""
    rec = _fake_gh(tmp_path, monkeypatch)
    result = runner.invoke(
        app,
        ["do", "pr", "graphql-exec", "--purpose", "discretionary",
         "--", "-f", "query=x", "--jq", "y"],
    )
    assert result.exit_code == 2
    assert "api graphql" in result.output
    assert "command words first" in result.output
    assert not rec.exists(), "no gh call may run for a refused argv"


def test_pr_merge_dispatches_in_package(monkeypatch):
    """The verb calls _merge.run_merge with the forwarded args and exits its rc."""
    captured = {}

    def _fake(argv, cwd=None):
        captured["argv"] = list(argv)
        return 2

    monkeypatch.setattr(_merge, "run_merge", _fake)
    result = runner.invoke(app, ["do", "pr", "merge", "999999"])
    assert result.exit_code == 2
    assert captured["argv"] == ["999999"]


def test_pr_merge_forwards_legacy_invoker_flag(monkeypatch):
    """x-04ab: a legacy --invoker=... is forwarded verbatim (run_merge ignores it)."""
    captured = {}

    def _fake(argv, cwd=None):
        captured["argv"] = list(argv)
        return 2

    monkeypatch.setattr(_merge, "run_merge", _fake)
    result = runner.invoke(app, ["do", "pr", "merge", "--invoker=target", "42"])
    assert result.exit_code == 2
    assert captured["argv"] == ["--invoker=target", "42"]


def test_closure_trailer_warns_on_a_dropped_malformed_extra_id(monkeypatch, tmp_path):
    """Round-7 review fix: render_closure_trailer silently drops a malformed
    --extra id with no other signal - the CLI layer must warn rather than
    ship the trailer one node short with no operator visibility."""
    from fno import paths

    graph_path = tmp_path / "graph.json"
    graph_path.write_text(json.dumps({"entries": [{"id": "x-1111", "status": "ready"}]}))
    monkeypatch.setattr(paths, "graph_json", lambda: graph_path)

    result = runner.invoke(
        app, ["do", "pr", "closure-trailer", "x-1111", "--extra", "not-an-id"],
    )
    assert result.exit_code == 0
    assert "warning: dropping malformed --extra id(s)" in result.output
    assert "not-an-id" in result.output
    assert "Backlog-Closure: x-1111" in result.output


def test_global_receipt_path_uses_pinned_accessor(monkeypatch, tmp_path):
    from fno import paths

    expected = tmp_path / "events.jsonl"
    monkeypatch.setattr(paths, "global_events_json", lambda: expected)

    result = runner.invoke(app, ["do", "pr", "global-receipt-events-path"])

    assert result.exit_code == 0
    assert result.stdout.strip() == str(expected)

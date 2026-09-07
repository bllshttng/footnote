"""The per-round disposition comment (x-82ac change 5): one machine-parseable
comment per (pr, head), posted over REST, findings-free rounds post none."""
from __future__ import annotations

import json

from typer.testing import CliRunner

from fno.review import cli as review_cli
from fno.review.cli import review_app

HEAD = "a" * 40

runner = CliRunner()


def _payload(dispositions: list[dict] | None = None) -> dict:
    payload = {
        "findings": [
            {
                "file": "cli/src/a.py",
                "line": 10,
                "category": "correctness",
                "verdict": "CONFIRMED",
                "blocking": True,
                "summary": "wrong",
            },
            {
                "file": "docs/b.md",
                "line": 2,
                "category": "docs",
                "blocking": False,
                "summary": "stale",
            },
        ],
    }
    if dispositions is not None:
        payload["dispositions"] = dispositions
    return payload


def test_renders_marker_and_per_finding_outcomes():
    record = review_cli.build_emit_record(
        _payload(
            [
                {
                    "finding_key": "cli/src/a.py:10:correctness",
                    "disposition": "fixed",
                    "reason": "fixed at def5678",
                }
            ]
        )
    )
    keys = [f.get("finding_key") for f in record["findings"]]
    assert keys == ["cli/src/a.py:10:correctness", "docs/b.md:2:docs"]
    body = review_cli._render_round_comment(record, HEAD, 2, "code-review")
    assert f"<!-- fno-review-round head={HEAD} round=2 reviewer=code-review -->" in body
    assert "2 findings, 1 disposed" in body
    assert "- cli/src/a.py:10:correctness: fixed (fixed at def5678)" in body
    assert "- docs/b.md:2:docs: no disposition" in body


def test_no_dispositions_posts_nothing(tmp_path, monkeypatch):
    findings = tmp_path / "f.json"
    findings.write_text(json.dumps(_payload(None)), encoding="utf-8")
    calls: list = []
    monkeypatch.setattr("fno.pr._proc.run", lambda cmd, **kw: calls.append(cmd))
    result = runner.invoke(
        review_app,
        ["post-dispositions", "--findings-file", str(findings), "--head", HEAD, "--pr", "7"],
    )
    assert result.exit_code == 0
    assert "nothing posted" in result.output
    assert calls == [], "a disposition-free round must not touch the network"


def test_direct_call_treats_the_typer_default_as_no_pr(tmp_path, monkeypatch, capsys):
    findings = tmp_path / "f.json"
    findings.write_text(json.dumps(_payload(_DISPOSED)), encoding="utf-8")
    monkeypatch.setattr(
        "fno.pr._rest._slug_or_reason", lambda *args, **kwargs: ("own/repo", "")
    )
    monkeypatch.setattr(
        "fno.pr._rest.resolve_current_pr_number_rest",
        lambda: (None, "no PR for branch"),
    )
    monkeypatch.setattr(
        "fno.pr._rest.fetch_pr_info_rest",
        lambda *_: (_ for _ in ()).throw(AssertionError("must resolve omitted PR")),
    )

    review_cli.post_dispositions(
        findings_file=findings,
        head=HEAD,
        reviewer="code-review",
    )

    assert "no PR for branch" in capsys.readouterr().out


class _FakeResult:
    def __init__(self, stdout: str = "", returncode: int = 0) -> None:
        self.returncode = returncode
        self.stdout = stdout
        self.stderr = ""

    @property
    def ok(self) -> bool:
        return self.returncode == 0


def _stub_rest(monkeypatch, posts: list, comments_stdout: str = "[]") -> None:
    """Slug and PR-head resolve from stubs; gh calls ride a fake run that
    records POSTs and answers the comments read with `comments_stdout`."""

    def fake_run(cmd, **kwargs):
        if cmd[:2] == ["gh", "api"] and "POST" in cmd:
            posts.append(cmd)
            return _FakeResult("{}\n")
        return _FakeResult(comments_stdout)

    monkeypatch.setattr("fno.pr._proc.run", fake_run)
    monkeypatch.setattr(
        "fno.pr._rest._slug_or_reason", lambda cwd=None, runner=None, repo=None: ("own/repo", "")
    )
    monkeypatch.setattr(
        "fno.pr._rest.fetch_pr_info_rest",
        lambda pr, cwd=None, runner=None, repo=None: ({"head_sha": HEAD}, ""),
    )


_DISPOSED = [
    {
        "finding_key": "cli/src/a.py:10:correctness",
        "disposition": "fixed",
        "reason": "fixed at def5678",
    }
]


def test_posts_once_per_head(tmp_path, monkeypatch):
    findings = tmp_path / "f.json"
    findings.write_text(json.dumps(_payload(_DISPOSED)), encoding="utf-8")
    posts: list = []
    _stub_rest(monkeypatch, posts)
    result = runner.invoke(
        review_app,
        ["post-dispositions", "--findings-file", str(findings), "--head", HEAD, "--pr", "7"],
    )
    assert result.exit_code == 0, result.output
    assert "posted round comment on PR 7" in result.output
    assert len(posts) == 1
    assert any("repos/own/repo/issues/7/comments" in arg for arg in posts[0])
    assert any(f"head={HEAD}" in arg for arg in posts[0]), "the marker rides the body"

    # A second post at the same head sees the marker and writes nothing.
    marker = review_cli._ROUND_MARKER.format(head=HEAD, round=1, reviewer="code-review")
    posts.clear()
    _stub_rest(monkeypatch, posts, comments_stdout=json.dumps([{"body": marker}]))
    result2 = runner.invoke(
        review_app,
        ["post-dispositions", "--findings-file", str(findings), "--head", HEAD, "--pr", "7"],
    )
    assert result2.exit_code == 0
    assert "already posted" in result2.output
    assert posts == [], "the second run must not post"


def test_head_mismatch_refuses(tmp_path, monkeypatch):
    findings = tmp_path / "f.json"
    findings.write_text(json.dumps(_payload(_DISPOSED)), encoding="utf-8")
    posts: list = []
    _stub_rest(monkeypatch, posts)
    monkeypatch.setattr(
        "fno.pr._rest.fetch_pr_info_rest",
        lambda pr, cwd=None, runner=None, repo=None: ({"head_sha": "b" * 40}, ""),
    )
    result = runner.invoke(
        review_app,
        ["post-dispositions", "--findings-file", str(findings), "--head", HEAD, "--pr", "7"],
    )
    assert result.exit_code == 3
    assert "is not the attested head" in result.output
    assert posts == [], "a refused post must not write"


def test_called_as_a_function_with_pr_none_resolves_the_branch_pr(tmp_path, monkeypatch):
    """The attest path calls this as a plain function. Omitting ``pr`` there
    hands the Typer OptionInfo default to the REST reader as a PR number, so
    the caller passes ``pr=None`` and the branch resolver runs instead."""
    findings = tmp_path / "f.json"
    findings.write_text(json.dumps(_payload(_DISPOSED)), encoding="utf-8")
    posts: list = []
    _stub_rest(monkeypatch, posts)
    resolved: list = []

    def fake_resolve(**kwargs):
        resolved.append(True)
        return 7, ""

    monkeypatch.setattr("fno.pr._rest.resolve_current_pr_number_rest", fake_resolve)
    review_cli.post_dispositions(
        findings_file=findings, head=HEAD, reviewer="code-review", pr=None
    )
    assert resolved == [True], "pr=None must resolve the branch's PR"
    assert len(posts) == 1, "the resolved PR takes the round comment"

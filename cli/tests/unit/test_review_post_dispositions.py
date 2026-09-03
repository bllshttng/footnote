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
    posted: list = []
    monkeypatch.setattr(
        review_cli, "_gh_api", lambda args, timeout=30.0: posted.append(args) or (0, "", "")
    )
    result = runner.invoke(
        review_app,
        ["post-dispositions", "--findings-file", str(findings), "--head", HEAD, "--pr", "7"],
    )
    assert result.exit_code == 0
    assert "nothing posted" in result.output
    assert posted == [], "a disposition-free round must not touch the network"


class _FakeProc:
    def __init__(self, stdout: str = "") -> None:
        self.returncode = 0
        self.stdout = stdout
        self.stderr = ""


def _stub_remote(monkeypatch, posts: list) -> None:
    """The local remote read answers a GitHub URL; gh POSTs are recorded."""

    def fake_run(cmd, **kwargs):
        if cmd[:3] == ["git", "remote", "get-url"]:
            return _FakeProc("https://github.com/own/repo.git\n")
        if cmd[:2] == ["gh", "api"] and "POST" in cmd:
            posts.append(cmd)
            return _FakeProc("{}\n")
        return _FakeProc()

    monkeypatch.setattr("subprocess.run", fake_run)


def test_posts_once_per_head(tmp_path, monkeypatch):
    findings = tmp_path / "f.json"
    findings.write_text(
        json.dumps(
            _payload(
                [
                    {
                        "finding_key": "cli/src/a.py:10:correctness",
                        "disposition": "fixed",
                        "reason": "fixed at def5678",
                    }
                ]
            )
        ),
        encoding="utf-8",
    )
    posts: list = []
    _stub_remote(monkeypatch, posts)

    def fake_gh_api(args, timeout=30.0):
        joined = " ".join(args)
        if "pulls/7" in joined and "GET" in joined:
            return 0, json.dumps({"head": {"sha": HEAD}}), ""
        return 0, json.dumps([]), ""  # empty comments list

    monkeypatch.setattr(review_cli, "_gh_api", fake_gh_api)
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

    def fake_gh_api_again(args, timeout=30.0):
        joined = " ".join(args)
        if "issues/7/comments" in joined:
            return 0, json.dumps([{"body": marker}]), ""
        return 0, json.dumps({"head": {"sha": HEAD}}), ""

    monkeypatch.setattr(review_cli, "_gh_api", fake_gh_api_again)
    result2 = runner.invoke(
        review_app,
        ["post-dispositions", "--findings-file", str(findings), "--head", HEAD, "--pr", "7"],
    )
    assert result2.exit_code == 0
    assert "already posted" in result2.output
    assert len(posts) == 1, "the second run must not post"


def test_head_mismatch_refuses(tmp_path, monkeypatch):
    findings = tmp_path / "f.json"
    findings.write_text(
        json.dumps(
            _payload(
                [
                    {
                        "finding_key": "cli/src/a.py:10:correctness",
                        "disposition": "fixed",
                        "reason": "fixed",
                    }
                ]
            )
        ),
        encoding="utf-8",
    )
    posts: list = []
    _stub_remote(monkeypatch, posts)

    def fake_gh_api(args, timeout=30.0):
        return 0, json.dumps({"head": {"sha": "b" * 40}}), ""

    monkeypatch.setattr(review_cli, "_gh_api", fake_gh_api)
    result = runner.invoke(
        review_app,
        ["post-dispositions", "--findings-file", str(findings), "--head", HEAD, "--pr", "7"],
    )
    assert result.exit_code == 3
    assert "is not the attested head" in result.output
    assert posts == [], "a refused post must not write"

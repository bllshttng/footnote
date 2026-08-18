"""publish_review (x-93ea): the reviewer lane's GitHub posting producer.

Refusal matrix + posted-state assertions. Every test fakes the subprocess
layer (``fno.pr._publish_review.run``) so no test touches the network; the
assertions that matter are which calls fire (a refusal must never reach the
POST) and what the result reports (the reviewDecision readback, never the
POST receipt).
"""
from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Optional

import pytest

from fno.pr import _publish_review as pr_mod
from fno.pr._proc import Result

REPO = "bllshttng/footnote"
IDENTITY = "fno-review-bot"
TOKEN_ENV = "GH_REVIEW_BOT_TOKEN"
HEAD = "a" * 40


class FakeGh:
    """Answers git/gh argv from a script, recording every POST."""

    def __init__(
        self,
        *,
        author: str = "bllshttng",
        pr_head: Optional[str] = HEAD,
        slug: str = REPO,
        post_rc: int = 0,
        review_decision: str = "APPROVED",
    ) -> None:
        self.author = author
        self.pr_head = pr_head
        self.slug = slug
        self.post_rc = post_rc
        self.review_decision = review_decision
        self.posts: list[dict] = []
        self.saw_envs: list[Optional[dict]] = []

    def __call__(self, cmd, *, cwd=None, env=None, input_text=None, timeout=None):
        argv = list(cmd)
        self.saw_envs.append(env)
        if argv[:3] == ["git", "rev-parse", "--show-toplevel"]:
            return _ok(str(cwd))
        if argv[:2] == ["gh", "pr"] and "--json" in argv:
            field = argv[argv.index("--json") + 1]
            if "author" in field.split(","):
                return _ok(self.author)
            if "headRefOid" in field.split(","):
                return _ok(self.pr_head if self.pr_head is not None else "")
            if "reviewDecision" in field.split(","):
                return _ok(self.review_decision)
        if argv[:2] == ["gh", "repo"] and "nameWithOwner" in argv:
            return _ok(self.slug)
        if argv[:3] == ["gh", "api", "-X"] and "POST" in argv:
            self.posts.append({"argv": argv, "env": env})
            if self.post_rc != 0:
                return _rc(self.post_rc, "", "gh: Not Found")
            return _ok(json.dumps({"state": "APPROVED"}))
        return _rc(1, "", f"fake: unhandled argv {argv}")


def _ok(stdout: str):
    return Result(returncode=0, stdout=stdout, stderr="")


def _rc(rc: int, stdout: str, stderr: str):
    return Result(returncode=rc, stdout=stdout, stderr=stderr)


@pytest.fixture
def repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A directory whose .fno/settings.yaml configures the bot lane."""
    settings = tmp_path / ".fno"
    settings.mkdir()
    (settings / "settings.yaml").write_text(
        "schema_version: 1\nconfig:\n  review:\n"
        f"    bot_identity: {IDENTITY}\n    bot_token_env: {TOKEN_ENV}\n",
        encoding="utf-8",
    )
    monkeypatch.setenv(TOKEN_ENV, "tok")
    return tmp_path


def _publish(repo: Path, fake: FakeGh, monkeypatch: pytest.MonkeyPatch, **kw):
    monkeypatch.setattr(pr_mod, "run", fake)
    return pr_mod.publish_review(
        pr_number=931,
        head_sha=kw.pop("head_sha", HEAD),
        verdict=kw.pop("verdict", "pass"),
        reviewer=kw.pop("reviewer", "sigma"),
        cwd=str(repo),
        **kw,
    )


def test_posts_approve_and_reports_readback(repo, monkeypatch):
    fake = FakeGh()
    result = _publish(repo, fake, monkeypatch)
    assert result.status == "posted"
    assert result.review_decision == "APPROVED"
    assert result.receipt == (
        f"bot-review: posted APPROVE as {IDENTITY} on #931 (reviewDecision=APPROVED)"
    )
    assert len(fake.posts) == 1
    argv = fake.posts[0]["argv"]
    assert "event=APPROVE" in argv
    assert f"commit_id={HEAD}" in argv
    assert any(a.startswith("body=fno review mirror: reviewer=sigma") for a in argv)
    assert f"/repos/{REPO}/pulls/931/reviews" in argv


def test_post_auth_is_subprocess_env_only(repo, monkeypatch):
    monkeypatch.delenv("GH_TOKEN", raising=False)
    fake = FakeGh()
    _publish(repo, fake, monkeypatch)
    # The POST env carries the bot token; the caller's os.environ survives.
    assert fake.posts[0]["env"]["GH_TOKEN"] == "tok"
    assert os.environ.get("GH_TOKEN") is None


def test_identity_collision_refuses_without_posting(repo, monkeypatch):
    fake = FakeGh(author=IDENTITY)
    result = _publish(repo, fake, monkeypatch)
    assert result.status == "refused"
    assert IDENTITY in result.reason
    assert fake.posts == []


def test_identity_collision_strips_bot_suffix(repo, monkeypatch):
    fake = FakeGh(author=f"{IDENTITY}[bot]")
    result = _publish(repo, fake, monkeypatch)
    assert result.status == "refused"
    assert fake.posts == []


def test_unset_identity_skips(repo, monkeypatch):
    (repo / ".fno" / "settings.yaml").write_text(
        "schema_version: 1\nconfig:\n  review:\n    peers: []\n", encoding="utf-8"
    )
    fake = FakeGh()
    result = _publish(repo, fake, monkeypatch)
    assert result.status == "skipped"
    assert result.reason == "review.bot_identity unset"
    assert fake.posts == []


def test_unset_token_env_skips(repo, monkeypatch):
    monkeypatch.delenv(TOKEN_ENV, raising=False)
    fake = FakeGh()
    result = _publish(repo, fake, monkeypatch)
    assert result.status == "skipped"
    assert TOKEN_ENV in result.reason
    assert fake.posts == []


def test_stale_head_refuses(repo, monkeypatch):
    fake = FakeGh()
    result = _publish(repo, fake, monkeypatch, head_sha="b" * 40)
    assert result.status == "refused"
    assert "stale" in result.reason
    assert fake.posts == []


def test_fail_verdict_posts_request_changes(repo, monkeypatch):
    fake = FakeGh(review_decision="CHANGES_REQUESTED")
    result = _publish(repo, fake, monkeypatch, verdict="fail")
    assert result.status == "posted"
    assert result.review_decision == "CHANGES_REQUESTED"
    assert "event=REQUEST_CHANGES" in fake.posts[0]["argv"]


def test_unmappable_verdict_refuses(repo, monkeypatch):
    fake = FakeGh()
    result = _publish(repo, fake, monkeypatch, verdict="meh")
    assert result.status == "refused"
    assert fake.posts == []


def test_gh_post_failure_fails_closed_without_raising(repo, monkeypatch):
    fake = FakeGh(post_rc=422)
    result = _publish(repo, fake, monkeypatch)
    assert result.status == "failed"
    assert result.stderr and "Not Found" in result.stderr
    assert result.review_decision is None


def test_dry_run_names_event_without_posting(repo, monkeypatch):
    fake = FakeGh()
    result = _publish(repo, fake, monkeypatch, dry_run=True)
    assert result.status == "skipped"
    assert "APPROVE" in result.reason
    assert fake.posts == []


def test_no_open_pr_author_unreadable_skips(repo, monkeypatch):
    fake = FakeGh(author="")
    result = _publish(repo, fake, monkeypatch)
    assert result.status == "skipped"
    assert fake.posts == []

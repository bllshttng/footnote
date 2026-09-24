"""The target ship review is the in-session fno lane; the mail round trip is gone.

The review step is exactly `/fno:review <size> --comment` (Codex
`$fno:review <size> --comment`), invoked by the model in its own context on
the final local HEAD before `/fno:pr create`. No mail, no paste, no turn
boundary. These tests pin that direction across every reachable target
surface and guard against the prose regrowing the retired
`request-self-review` round trip or the deleted `preship_review_plan`
decision.
"""
from __future__ import annotations

from pathlib import Path
import json

REPO_ROOT = Path(__file__).resolve().parents[3]


def test_request_self_review_pins_pr_head_and_uses_the_raw_self_route(
    monkeypatch,
):
    import fno.target_cli as target_cli
    from typer.testing import CliRunner

    calls = []
    monkeypatch.setattr(
        target_cli,
        "_git_out",
        lambda _cwd, *args: "abc1234" if args == ("rev-parse", "HEAD") else None,
    )
    monkeypatch.setattr(
        target_cli,
        "_read_pr_metadata",
        lambda _pr, _cwd: {
            "number": 123,
            "headRefOid": "abc1234",
            "baseRefName": "main",
        },
    )
    monkeypatch.setattr(
        target_cli,
        "_resolve_self_review_identity",
        lambda: ("codex", "codex-session"),
    )
    monkeypatch.setattr(
        target_cli,
        "_send_self_review_payload",
        lambda **kwargs: calls.append(kwargs) or {"outcome": "started", "transport": "codex-daemon"},
    )

    result = CliRunner().invoke(target_cli.target_app, ["request-self-review", "--pr", "123"])

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output)
    assert receipt["outcome"] == "started"
    # The inline fno lane is rendered in the active harness's spelling.
    assert calls[0]["payload"].startswith("$fno:review ")
    # The bare PR number leads the target slot; HEAD and base are trailing
    # context a strict reader never mistakes for the target.
    assert calls[0]["payload"].endswith(
        "123 HEAD abc1234 against origin/main"
    )
    assert calls[0]["harness"] == "codex"


def test_send_self_review_payload_maps_started_and_addresses_the_full_session(
    monkeypatch,
):
    import fno.target_cli as target_cli
    from fno.mail import cli as mail_cli

    sends = []

    def fake_raw_send(recipient, payload, **kwargs):
        sends.append((recipient, payload, kwargs))
        print("started")
        import typer

        raise typer.Exit(code=0)

    monkeypatch.setattr(mail_cli, "_raw_send", fake_raw_send)
    receipt = target_cli._send_self_review_payload(
        payload="/review HEAD abc1234 of PR 123 against origin/main",
        harness="codex",
        session_id="0199abcdef0123456789abcdef012345",
    )

    assert receipt == {"outcome": "started", "transport": "mux-pane"}
    # The self lane addresses the FULL session id: a codex head-8 is a
    # timestamp bucket shared by every same-minute sibling, so resolution
    # fails closed exactly when the fleet is busiest.
    assert sends[0][0] == "0199abcdef0123456789abcdef012345"
    assert sends[0][2]["review_request"] is True


def test_request_self_review_refuses_when_local_head_does_not_match_pr(
    monkeypatch,
):
    import fno.target_cli as target_cli
    from typer.testing import CliRunner

    monkeypatch.setattr(
        target_cli,
        "_git_out",
        lambda _cwd, *args: "local999" if args == ("rev-parse", "HEAD") else None,
    )
    monkeypatch.setattr(
        target_cli,
        "_read_pr_metadata",
        lambda _pr, _cwd: {
            "number": 123,
            "headRefOid": "abc1234",
            "baseRefName": "main",
        },
    )
    result = CliRunner().invoke(target_cli.target_app, ["request-self-review", "--pr", "123"])

    assert result.exit_code != 0
    receipt = json.loads(result.output)
    assert receipt["outcome"] == "refused"
    assert "HEAD" in receipt["reason"]


def test_request_self_review_without_pr_pins_local_head_and_branch(
    monkeypatch,
):
    import fno.target_cli as target_cli
    from typer.testing import CliRunner

    calls = []

    def fake_git_out(_cwd, *args):
        if args == ("rev-parse", "HEAD"):
            return "abc1234"
        if args == ("rev-parse", "--abbrev-ref", "HEAD"):
            return "feature/x-98ac"
        if args == ("symbolic-ref", "--short", "refs/remotes/origin/HEAD"):
            return "origin/main"
        return None

    monkeypatch.setattr(target_cli, "_git_out", fake_git_out)
    monkeypatch.setattr(
        target_cli,
        "_read_pr_metadata",
        lambda _pr, _cwd: (_ for _ in ()).throw(AssertionError("no-PR form read a PR")),
    )
    monkeypatch.setattr(
        target_cli,
        "_resolve_self_review_identity",
        lambda: ("codex", "codex-session"),
    )
    monkeypatch.setattr(
        target_cli,
        "_send_self_review_payload",
        lambda **kwargs: calls.append(kwargs) or {"outcome": "started", "transport": "codex-daemon"},
    )

    result = CliRunner().invoke(target_cli.target_app, ["request-self-review"])

    assert result.exit_code == 0, result.output
    receipt = json.loads(result.output)
    assert receipt["outcome"] == "started"
    assert receipt["pr"] is None
    assert receipt["branch"] == "feature/x-98ac"
    # The branch name leads the target slot, HEAD and base trail: the same
    # strict-reader contract as the --pr form, pre-push. No --comment: the
    # PR the flag writes to does not exist yet.
    assert calls[0]["payload"].endswith(
        "feature/x-98ac HEAD abc1234 against origin/main"
    )
    assert "--comment" not in calls[0]["payload"]


def test_request_self_review_without_pr_refuses_detached_head(
    monkeypatch,
):
    import fno.target_cli as target_cli
    from typer.testing import CliRunner

    monkeypatch.setattr(
        target_cli,
        "_git_out",
        lambda _cwd, *args: "abc1234" if args == ("rev-parse", "HEAD") else "HEAD",
    )

    result = CliRunner().invoke(target_cli.target_app, ["request-self-review"])

    assert result.exit_code == 2
    receipt = json.loads(result.output)
    assert receipt["outcome"] == "refused"
    assert "no branch" in receipt["reason"]


def test_render_self_review_invocation_refuses_pr_and_branch_together():
    from fno.review_capability import render_self_review_invocation

    try:
        render_self_review_invocation(
            pr_number=123,
            branch="feature/x-98ac",
            head_sha="abc1234",
            base_branch="main",
        )
    except ValueError:
        pass
    else:
        raise AssertionError("pr_number and branch together must raise ValueError")


def test_skill_prose_describes_the_same_direction_as_the_decision():
    # The contract the review step encodes must hold across every reachable
    # target surface, or a guard on one path is decorative (repo pitfall #1).
    skill = (REPO_ROOT / "skills" / "target" / "SKILL.md").read_text()
    phase = (REPO_ROOT / "skills" / "target" / "references" / "phase-bodies.md").read_text()
    ship = (REPO_ROOT / "skills" / "target" / "references" / "ship-and-promise.md").read_text()
    routing = (REPO_ROOT / "skills" / "target" / "references" / "phase-invocations.md").read_text()

    # The review step is the in-session fno lane with --comment, run BEFORE
    # /pr create, sized by the diff. The mail round trip is gone from every
    # surface, and the deleted decision helper must not regrow in prose.
    assert "internal sigma panel (cheap insurance)" not in skill
    assert "internal sigma panel (cheap insurance)" not in phase
    for text in (skill, phase, ship, routing):
        assert "request-self-review" not in text
        assert "preship_review_plan" not in text
        assert "/fno:review" in text
        assert "--comment" in text
    spine_start = skill.index("```")
    spine = skill[spine_start : skill.index("```", spine_start + 3)]
    assert "/fno:review" in spine
    assert spine.index("/fno:review") < spine.index("/pr create")
    assert "medium" in skill and "300" in skill and "xhigh" in skill

    # Findings hold when no PR exists and post when it opens.
    assert "holds" in phase and "posts" in phase

    # AC12, retired: sigma no longer defers anything (a config naming it is
    # refused at init). The lane is the producer named on every reachable
    # surface, and cleanup-class material never buys a review round.
    assert "sigma runs once, post-ship" not in ship.lower()
    assert "RETIRED" in ship
    assert "the fno lane" in ship.lower() or "fno review lane" in ship.lower()
    assert "/fno:review cleanup" in ship

    # The phase-routing layer names the in-session lane, not a decision helper.
    assert "/fno:review" in routing
    assert "default: `fno:review`" not in routing and "default: fno:review" not in routing


def test_ship_text_stops_at_the_round_cap():
    retired = (
        "the old attestation is stale",
        "stales a pre-push attestation",
        "stales the attestation",
        "re-run the reviewer and re-emit",
        "invalidated by any later fix or rebase",
        "clean head-pinned",
        "non-author GitHub approval",
        "IMPOSSIBLE",
    )
    paths = (
        Path("skills/target/SKILL.md"),
        Path("skills/target/references/ship-and-promise.md"),
        Path("skills/target/references/phase-bodies.md"),
        Path("skills/target/references/pipeline-and-philosophy.md"),
    )
    contents = {path: (REPO_ROOT / path).read_text(encoding="utf-8") for path in paths}
    for path, text in contents.items():
        for phrase in retired:
            assert phrase not in text, f"{path} contains retired phrase {phrase!r}"
    for path in paths[:2]:
        text = contents[path]
        assert "rounds_exhausted" in text, f"{path} omits rounds_exhausted"
        assert "review.max_rounds" in text, f"{path} omits review.max_rounds"

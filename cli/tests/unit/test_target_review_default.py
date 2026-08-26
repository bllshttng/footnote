"""Target pre-ship review defaults to a native final-head review; sigma is opt-in.

AC11-HP: no `config.review.reviewers` -> the ship step requests the harness-native
review verb for the pushed final HEAD through the explicit-target self-send
router, and no six-agent sigma panel is dispatched. The request is a producer,
not advisory prose: it names the PR, HEAD SHA, and origin base before it can
reach the transport.
AC12-CON: `reviewers` includes `sigma` -> sigma runs exactly once (post-ship, on
the final HEAD) and the skip logic reads in the same direction as its docs.

The decision lives in `preship_review_plan` so every target skill surface answers
to one codified direction. These tests pin that direction and the config default
that makes the self-review the default, then guard the prose from reverting to
the old inverted framing where the bare default spawned the sigma panel.
"""
from __future__ import annotations

from pathlib import Path
import json

from fno.config import ReviewBlock
from fno.review_capability import preship_review_plan

REPO_ROOT = Path(__file__).resolve().parents[3]


def test_default_reviewers_is_empty_so_native_review_is_the_default():
    # This default is what makes native review the ordinary path; pin it so a
    # future edit cannot quietly re-introduce the inverted skip.
    assert ReviewBlock().reviewers == []


def test_no_reviewers_runs_native_review_and_dispatches_no_sigma():
    plan = preship_review_plan([])
    assert plan.kind == "native"
    assert "request-self-review" in plan.reason


def test_sigma_configured_skips_preship_and_runs_once_post_ship():
    plan = preship_review_plan(["sigma"])
    assert plan.kind == "skip"
    assert "once" in plan.reason
    assert "post-ship" in plan.reason


def test_sigma_presence_not_other_reviewers_decides_the_skip():
    # `/code-review` is a different local-attestation reviewer; sigma's presence
    # is what defers the pre-ship step, and a leading slash must not hide it.
    assert preship_review_plan(["/code-review"]).kind == "native"
    assert preship_review_plan(["/code-review", "sigma"]).kind == "skip"
    assert preship_review_plan(["sigma", "declare"]).kind == "skip"


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
    assert calls[0]["payload"] == (
        "/review HEAD abc1234 of PR 123 against origin/main"
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


def test_skill_prose_describes_the_same_direction_as_the_decision():
    # The contract the decision encodes must hold across every reachable target
    # surface, or a guard on one path is decorative (repo pitfall #1).
    skill = (REPO_ROOT / "skills" / "target" / "SKILL.md").read_text()
    phase = (REPO_ROOT / "skills" / "target" / "references" / "phase-bodies.md").read_text()
    ship = (REPO_ROOT / "skills" / "target" / "references" / "ship-and-promise.md").read_text()
    routing = (REPO_ROOT / "skills" / "target" / "references" / "phase-invocations.md").read_text()

    # AC11: the default ship step requests a native review with an explicit
    # final-head target. The old advisory default is gone.
    assert "internal sigma panel (cheap insurance)" not in skill
    assert "internal sigma panel (cheap insurance)" not in phase
    assert "request-self-review --pr" in skill
    assert "HEAD" in skill and "origin/main" in skill
    assert "queued" in skill.lower() and "turn boundary" in skill.lower()
    assert "advisory self-review by default" not in skill
    assert "optional escalation" not in skill

    # AC12: configured sigma runs once post-ship and the skip reads the same
    # direction as preship_review_plan (sigma configured -> pre-ship skipped).
    assert "sigma runs once, post-ship" in ship.lower()
    assert "skip" in phase.lower() and "sigma" in phase.lower()

    # The phase-routing layer must not short-circuit the decision by routing the
    # review phase to `fno:review` (sigma) unconditionally; it defers to the plan.
    assert "preship_review_plan" in routing
    assert "default: `fno:review`" not in routing and "default: fno:review" not in routing

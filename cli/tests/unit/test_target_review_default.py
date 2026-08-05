"""Target pre-ship review defaults to the harness's native review; sigma is opt-in.

AC11-HP: no `config.review.reviewers` -> the invoking harness's own native review
runs and no six-agent sigma panel is dispatched.
AC12-CON: `reviewers` includes `sigma` -> sigma runs exactly once (post-ship, on
the final HEAD) and the skip logic reads in the same direction as its docs.

The decision lives in `preship_review_plan` so every target skill surface answers
to one codified direction. These tests pin that direction and the config default
that makes native review the default, then guard the prose from reverting to the
old inverted framing where the bare default spawned the sigma panel.
"""
from __future__ import annotations

from pathlib import Path

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
    assert "do not dispatch" in plan.reason


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


def test_skill_prose_describes_the_same_direction_as_the_decision():
    # The contract the decision encodes must hold across every reachable target
    # surface, or a guard on one path is decorative (repo pitfall #1).
    skill = (REPO_ROOT / "skills" / "target" / "SKILL.md").read_text()
    phase = (REPO_ROOT / "skills" / "target" / "references" / "phase-bodies.md").read_text()
    ship = (REPO_ROOT / "skills" / "target" / "references" / "ship-and-promise.md").read_text()
    routing = (REPO_ROOT / "skills" / "target" / "references" / "phase-invocations.md").read_text()

    # AC11: the default pre-ship step is a native review, never the sigma panel.
    # The old inverted spine advertised the panel as cheap insurance; it is gone.
    assert "internal sigma panel (cheap insurance)" not in skill
    assert "internal sigma panel (cheap insurance)" not in phase
    assert "native review" in skill.lower()

    # AC12: configured sigma runs once post-ship and the skip reads the same
    # direction as preship_review_plan (sigma configured -> pre-ship skipped).
    assert "sigma runs once, post-ship" in ship.lower()
    assert "skip" in phase.lower() and "sigma" in phase.lower()

    # The phase-routing layer must not short-circuit the decision by routing the
    # review phase to `fno:review` (sigma) unconditionally; it defers to the plan.
    assert "preship_review_plan" in routing
    assert "default: `fno:review`" not in routing and "default: fno:review" not in routing

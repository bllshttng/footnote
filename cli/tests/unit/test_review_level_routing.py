"""Level routing: level -> (band, effort, model), one seam, no inheritance.

AC4-MARKER is the load-bearing assertion: `max` and `high` on one provider
resolve to the SAME model and DIFFERENT effort - the one test that
distinguishes a four-band table from a relabeled three-band one. AC4-HP pins
`level_source` in all three of its legal values and asserts the vocabulary has
no `last_used` at all. AC4-ERR pins the degraded-max record.
"""
from __future__ import annotations

import json

import fno.review_level as rl
from fno.review_level import (
    FALLBACK_LEVEL,
    LEVEL_TO_BAND_EFFORT,
    resolve_review_level,
)


def test_max_and_high_share_a_model_and_differ_in_effort():
    """AC4-MARKER: both halves asserted - the shared model id AND the
    differing effort token. Both resolving successfully proves nothing about
    the axis that separates them."""
    hi = resolve_review_level("high", provider="openai")
    mx = resolve_review_level("max", provider="openai")
    assert mx.model is not None and hi.model is not None
    assert mx.model == hi.model == "gpt-5.6-sol"
    assert hi.effort == "high"
    assert mx.effort == "max"
    assert hi.effort != mx.effort


def test_claude_keeps_distinct_models_per_band_and_max_is_not_degraded():
    mx = resolve_review_level("max", provider="anthropic")
    hi = resolve_review_level("high", provider="anthropic")
    assert mx.model == "claude-fable-5"
    assert hi.model == "claude-opus-5"
    assert mx.model != hi.model
    assert mx.degraded_max is False


def test_the_zai_lane_resolves_its_medium_model():
    med = resolve_review_level("medium", provider="zai")
    assert med.model == "glm-5.3[1m]"
    assert med.band == "medium"
    assert med.effort == "high"


def test_the_zai_lane_max_degrades_to_its_strongest_family_model():
    """zai rides the claude harness but serves the GLM family, so a max
    request it cannot staff falls through to its strongest family model at
    max effort - recorded as degraded, never presented as a staffed max."""
    mx = resolve_review_level("max", provider="zai")
    assert mx.model == "glm-5.3[1m]"
    assert mx.effort == "max"
    assert mx.degraded_max is True
    assert any("provider family(zai)" in step for step in mx.chain)


def test_xhigh_shares_the_high_band_at_max_effort():
    xh = resolve_review_level("xhigh", provider="openai")
    assert xh.band == "high"
    assert xh.effort == "max"
    assert xh.model == "gpt-5.6-sol"


def test_level_source_explicit_diff_sized_and_fallback(monkeypatch, tmp_path):
    """AC4-HP: the recorded source per case, and no `last_used` anywhere."""
    explicit = resolve_review_level("low", provider="openai")
    assert explicit.level_source == "explicit"
    assert explicit.level == "low"

    monkeypatch.setattr(rl, "diff_review_level", lambda root: "high")
    sized = resolve_review_level(None, provider="openai", project_root=tmp_path)
    assert sized.level_source == "diff-sized"
    assert sized.level == "high"

    monkeypatch.setattr(rl, "diff_review_level", lambda root: None)
    fell = resolve_review_level(None, provider="openai", project_root=tmp_path)
    assert fell.level_source == "fallback"
    assert fell.level == FALLBACK_LEVEL  # the default is named, not silent


def test_no_level_is_ever_inherited():
    """The upstream hazard this refuses: a bare verb reusing the last typed
    level. The vocabulary has no such source and nothing persists a level."""
    assert set(s for s, _ in LEVEL_TO_BAND_EFFORT.items()) == {
        "low",
        "medium",
        "high",
        "xhigh",
        "max",
    }
    assert "last_used" not in rl.__dict__
    assert "last_used" not in dir(rl)


def test_a_degraded_max_is_recorded_not_presented(monkeypatch):
    """AC4-ERR: a provider with no distinct max model serves the request from
    the high band at max effort, and the record says degraded - the failure
    the record exists to prevent is a silent degraded max."""
    monkeypatch.setattr(
        rl,
        "resolve_tier",
        lambda band, provider=None: ("gpt-5.6-sol", ["static high -> gpt-5.6-sol"]),
    )
    mx = resolve_review_level("max", provider="openai")
    assert mx.model == "gpt-5.6-sol"
    assert mx.effort == "max"
    assert mx.degraded_max is True

    monkeypatch.setattr(
        rl,
        "resolve_tier",
        lambda band, provider=None: ("gpt-5.6-sol", ["static max -> gpt-5.6-sol"]),
    )
    assert resolve_review_level("max", provider="openai").degraded_max is False


def test_the_snapshot_degrade_chain_also_records_a_degraded_max(monkeypatch):
    monkeypatch.setattr(
        rl,
        "resolve_tier",
        lambda band, provider=None: (
            "claude-opus-5",
            ["snapshot band(>=95) degrade -> claude-opus-5 (best 92 < 95)"],
        ),
    )
    assert resolve_review_level("max", provider="anthropic").degraded_max is True


def test_an_unknown_provider_resolves_unscoped_and_never_refuses():
    out = resolve_review_level("high", provider="bogus")
    assert out.model is not None  # unscoped: the resolver's any-harness pick
    assert out.provider == "bogus"


def test_the_cli_seam_prints_the_resolved_record(tmp_path, monkeypatch):
    """`fno do review resolve-level` is the seam the skill and the invocation
    event call; its stdout is one JSON record."""
    from typer.testing import CliRunner

    monkeypatch.chdir(tmp_path)
    from fno.review.cli import review_app

    out = CliRunner().invoke(
        review_app, ["resolve-level", "max", "--provider", "openai"]
    )
    assert out.exit_code == 0, out.output
    record = json.loads(out.output)
    assert record["level"] == "max"
    assert record["level_source"] == "explicit"
    assert record["band"] == "max"
    assert record["effort"] == "max"
    assert record["model"] == "gpt-5.6-sol"
    assert record["provider"] == "openai"
    assert record["degraded_max"] is False

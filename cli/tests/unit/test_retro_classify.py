"""Unit tests for retro classify (Wave 3.2, US3)."""
from __future__ import annotations

from fno.retro.classify import (
    BODY_CAP,
    classify,
    classify_item,
    derive_title,
    severity_to_priority,
    severity_to_tier,
)
from fno.retro.types import (
    KIND_CARVEOUT,
    KIND_DEFERRED,
    KIND_REVIEW,
    TIER_INBOX,
    TIER_NODE,
    RawItem,
)


def test_severity_to_tier_and_priority():
    assert severity_to_tier("critical") == TIER_NODE
    assert severity_to_tier("high") == TIER_NODE
    assert severity_to_tier("medium") == TIER_NODE
    assert severity_to_tier("low") == TIER_INBOX
    assert severity_to_tier(None) == TIER_NODE  # deliberate work
    assert severity_to_priority("critical") == "p0"
    assert severity_to_priority("high") == "p1"
    assert severity_to_priority("medium") == "p2"
    assert severity_to_priority("low") == "p3"
    assert severity_to_priority(None) == "p3"


def test_ac3_hp_verbatim_body_and_real_title():
    """AC3-HP: body contains the reviewer's comment verbatim + source cite; title is a real summary."""
    item = RawItem(
        kind=KIND_REVIEW,
        text="![high] The retry loop never resets `attempts`, so it spins forever on a transient error.",
        source_pr=343,
        source_id="556677",
        severity="high",
        url="https://github.com/o/r/pull/343#discussion_r556677",
        reviewer="gemini[bot]",
    )
    c = classify_item(item)
    assert c.uncited is False
    assert c.tier == TIER_NODE
    assert c.priority == "p1"
    # Verbatim reasoning preserved in the body.
    assert "retry loop never resets `attempts`" in c.body
    # Source cite present.
    assert "PR #343" in c.body
    assert "556677" in c.body or "discussion_r556677" in c.body
    # Title is a real one-line summary, not a generic stub.
    assert c.title and "address review feedback" not in c.title.lower()
    assert "retry loop never resets" in c.title


def test_codex_badge_subscript_stripped_from_title():
    """The codex review bot wraps its priority badge in <sub><sub>..</sub></sub>;
    the tags must not survive into the title (which slugified to sub-sub-sub-sub)."""
    item = RawItem(
        kind=KIND_REVIEW,
        text=(
            "**<sub><sub>![P2 Badge](https://img.shields.io/badge/P2-yellow?style=flat)"
            "</sub></sub>  Reject duplicate pane bindings before committing**"
        ),
        source_pr=555,
        source_id="3629390387",
        severity="medium",
        url="https://github.com/o/r/pull/555#discussion_r3629390387",
        reviewer="chatgpt-codex-connector[bot]",
    )
    title = derive_title(item)
    assert "sub" not in title.lower().split()  # no bare "sub" token to slugify
    assert "<" not in title and ">" not in title
    assert not title.endswith("*")
    assert title == "Reject duplicate pane bindings before committing"


def test_ac3_err_uncited_candidate_rejected():
    """AC3-ERR: a candidate with no source_pr/source id is marked uncited (no node)."""
    item = RawItem(kind=KIND_REVIEW, text="some floating finding", source_pr=None, source_id="")
    c = classify_item(item)
    assert c.uncited is True
    cited, uncited = classify([item])
    assert cited == []
    assert len(uncited) == 1


def test_ac3_fr_oversize_body_truncated_with_marker():
    """AC3-FR: a body exceeding the cap is truncated with a marker + link, never dropped."""
    huge = "x" * 20_000
    item = RawItem(
        kind=KIND_REVIEW,
        text=huge,
        source_pr=10,
        source_id="1",
        severity="medium",
        url="http://c/1",
    )
    c = classify_item(item)
    assert len(c.body) <= BODY_CAP
    assert "truncated" in c.body
    assert "http://c/1" in c.body  # link back to source preserved


def test_carveout_title_from_need_and_priority_inherited():
    item = RawItem(
        kind=KIND_CARVEOUT,
        text="skipped SSO wiring pending provider choice",
        source_pr=5,
        source_id="cv-abc",
        priority="p1",
        title_hint="which auth provider",
        subkind="deferred",
    )
    c = classify_item(item)
    assert c.title == "which auth provider"
    assert c.priority == "p1"  # carve-out --priority inherited
    assert c.tier == TIER_NODE


def test_oos_bug_title_prefixed():
    item = RawItem(
        kind=KIND_CARVEOUT,
        text="null deref in the export path when list is empty",
        source_pr=5,
        source_id="cv-xyz",
        subkind="oos-bug",
    )
    c = classify_item(item)
    assert c.title.startswith("bug:")
    assert c.priority == "p3"  # no priority, no severity -> default


def test_ac3_edge_deferred_finding_first_line_is_title():
    item = RawItem(
        kind=KIND_DEFERRED,
        text="harden the lock timeout path\nmore detail on the next line",
        source_pr=8,
        source_id="deferred:0",
    )
    c = classify_item(item)
    assert c.title == "harden the lock timeout path"
    assert c.tier == TIER_NODE

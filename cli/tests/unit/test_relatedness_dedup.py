"""Filing-time dedup scorer: relatedness.similar_nodes (plan x-6ac7).

The dedup twin of ``epic_candidates``. Fixtures use controlled token sets so a
score is an exact inter/union ratio, not natural-language guesswork; ids are
obviously synthetic (no live node ids).
"""
from __future__ import annotations

from fno.graph.relatedness import _score, _tokens, similar_nodes


def node(nid, **kw):
    base = {"id": nid, "type": "feature", "title": nid, "domain": "code", "details": ""}
    base.update(kw)
    return base


# -- the specimen classes (AC7 + the caught dups) --


def test_similar_nodes_catches_divergent_phrasing_dup():
    # Same topic, divergent title phrasing: high token overlap, caught.
    existing = node("dup", title="alpha bravo charlie delta", domain="code")
    new = node("new", title="alpha bravo charlie echo", domain="code")
    res = similar_nodes(new, [existing])
    assert res and res[0][0] == "dup"
    assert res[0][1] >= 0.30


def test_similar_nodes_excludes_epic_bonus_so_siblings_stay_silent():
    # Same epic + domain, moderate overlap. WITH the epic bonus this pair scores
    # ~0.52 (would warn); the dedup path excludes the bonus, so it stays ~0.27
    # and is silent. This is the x-6ac7/x-a7ab sibling class.
    sib_a = node("aaa", title="alpha bravo charlie delta", domain="code", parent="epic1")
    sib_b = node("bbb", title="alpha echo golf", domain="code", parent="epic1")
    assert similar_nodes(sib_b, [sib_a]) == []
    with_epic = _score(sib_a, sib_b, _tokens(sib_a), _tokens(sib_b), include_epic=True)[0]
    assert with_epic >= 0.30  # proves the exclusion is load-bearing, not a no-op


def test_similar_nodes_unrelated_pair_absent():
    a = node("aaa", title="alpha bravo charlie", domain="code")
    b = node("bbb", title="delta echo foxtrot", domain="infra")
    assert similar_nodes(a, [b]) == []


# -- exclusion rules --


def test_similar_nodes_excludes_self_but_keeps_twin():
    a = node("aaa", title="alpha bravo charlie", domain="code")
    twin = node("bbb", title="alpha bravo charlie delta", domain="code")
    assert [r[0] for r in similar_nodes(a, [a, twin])] == ["bbb"]


def test_similar_nodes_excludes_superseded():
    sup = node("sup", title="alpha bravo charlie delta", domain="code", status="superseded")
    new = node("new", title="alpha bravo charlie", domain="code")
    assert similar_nodes(new, [sup]) == []


def test_similar_nodes_includes_done_and_in_review_states():
    # A shipped `done` node is the answer to a duplicate filing; in_review is
    # in-flight work. Both are candidates (the old idea-state-only net missed them).
    done = node("don", title="alpha bravo charlie delta", domain="code", status="done")
    rev = node("rev", title="bravo charlie echo foxtrot", domain="code", status="in_review")
    new = node("new", title="alpha bravo charlie", domain="code")
    assert {r[0] for r in similar_nodes(new, [done, rev])} == {"don", "rev"}


# -- threshold boundary --


def test_similar_nodes_threshold_floor_inclusive_drops_below():
    # inter=1, union=5, +domain => exactly 0.30 (the floor): included.
    at_floor = node("ccc", title="alpha bravo charlie", domain="code")
    new_floor = node("ddd", title="alpha delta echo", domain="code")
    assert [r[0] for r in similar_nodes(new_floor, [at_floor])] == ["ccc"]

    # inter=1, union=6, +domain => 0.267: below the floor but above _MIN_SCORE,
    # so _score returns it and similar_nodes drops it (the 0.30 gate bites).
    below = node("eee", title="alpha bravo charlie delta", domain="code")
    new_below = node("fff", title="alpha echo golf", domain="code")
    assert similar_nodes(new_below, [below]) == []


# -- ordering / caps --


def test_similar_nodes_caps_at_k():
    new = node("new", title="alpha bravo charlie", domain="code")
    entries = [node(f"t{i}", title="alpha bravo charlie delta", domain="code") for i in range(5)]
    assert len(similar_nodes(new, entries)) == 3


def test_similar_nodes_ties_break_on_id():
    new = node("new", title="alpha bravo charlie", domain="code")
    zzz = node("zzz", title="alpha bravo charlie delta", domain="code")
    aaa = node("aaa", title="alpha bravo charlie delta", domain="code")
    assert [r[0] for r in similar_nodes(new, [zzz, aaa])] == ["aaa", "zzz"]


def test_similar_nodes_on_empty_and_self_only():
    new = node("new", title="alpha bravo", domain="code")
    assert similar_nodes(new, []) == []
    assert similar_nodes(new, [new]) == []


def test_similar_nodes_handles_title_only_node():
    # No details field at all: tokens come from title alone, no crash.
    bare = {"id": "bare", "type": "feature", "title": "alpha bravo charlie", "domain": "code"}
    new = {"id": "new", "type": "feature", "title": "alpha bravo charlie delta", "domain": "code"}
    assert [r[0] for r in similar_nodes(new, [bare])] == ["bare"]

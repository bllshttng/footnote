"""PR-independent carve-out sweep: dedup dispositions and the apply contract.

The load-bearing assertion here is that the fuzzy matcher FIRES (a positive
control). A dedup step that silently never matches looks identical to a working
one on a clean ledger, and would file a duplicate for every already-tracked
carve-out the first time it met one.
"""
from __future__ import annotations

from pathlib import Path

from fno.retro.dedup import content_hash, trailer
from fno.retro.land import MODE_INTERACTIVE
from fno.retro.sweep import (
    DISPOSITION_FILE,
    DISPOSITION_RESOLVE,
    DISPOSITION_REVIEW,
    plan_sweep,
    sweep_carveouts,
)


def _cv(cid: str, description: str, kind: str = "deferred", **extra) -> dict:
    return {"id": cid, "kind": kind, "description": description, **extra}


def test_cv_id_quoted_in_a_node_resolves_instead_of_filing():
    cv = _cv("cv-11112222", "migrate the spawn adapters")
    nodes = [{"id": "x-1234", "title": "spawn seam", "details": "tracks cv-11112222"}]

    (item,) = plan_sweep([cv], nodes)

    assert item.disposition == DISPOSITION_RESOLVE
    assert item.node_id == "x-1234"


def test_existing_retro_trailer_hash_resolves_across_a_different_source_pr():
    """A PR-scoped harvest already filed this one; the sweep has no PR of its
    own, so the match must ignore the trailer's source_pr."""
    description = "the mux pane send exits 0 without a submit key"
    cv = _cv("cv-33334444", description, kind="oos-bug")
    nodes = [
        {
            "id": "x-5678",
            "title": "pane send",
            "details": trailer(604, content_hash(description)),
        }
    ]

    (item,) = plan_sweep([cv], nodes)

    assert item.disposition == DISPOSITION_RESOLVE
    assert item.node_id == "x-5678"


def test_fuzzy_subject_match_parks_for_review_and_never_files():
    """Positive control for the fuzzy matcher. A near-identical title must NOT
    file (that would duplicate) and must NOT resolve (a guess that consumes the
    row would lose the work): it parks."""
    cv = _cv("cv-55556666", "consolidate the duplicated stub_exec test fixture")
    nodes = [{"id": "x-9999", "title": "consolidate the duplicated stub_exec test fixtures"}]

    (item,) = plan_sweep([cv], nodes)

    assert item.disposition == DISPOSITION_REVIEW
    assert item.node_id == "x-9999"


def test_an_unrelated_graph_does_not_match():
    cv = _cv("cv-77778888", "wire clippy into CI and clear the two pre-existing sites")
    nodes = [{"id": "x-0001", "title": "rewrite the onboarding docs for new contributors"}]

    (item,) = plan_sweep([cv], nodes)

    assert item.disposition == DISPOSITION_FILE
    assert item.candidate is not None
    # A carve-out's cite is its own ledger id; requiring a PR rejected every
    # swept row as uncited, which is what made the ledger unharvestable.
    assert not item.candidate.uncited


def test_backfill_is_never_swept():
    rows = [_cv("cv-aaaa1111", "backfill the ranks", kind="backfill")]

    assert plan_sweep(rows, []) == []


def test_deferred_and_oos_bug_are_both_swept_and_stay_distinguishable():
    rows = [
        _cv("cv-bbbb1111", "declared scope that did not ship", kind="deferred"),
        _cv("cv-bbbb2222", "an unrelated crash found on the way", kind="oos-bug"),
    ]

    items = plan_sweep(rows, [])

    assert [i.disposition for i in items] == [DISPOSITION_FILE, DISPOSITION_FILE]
    assert [i.kind for i in items] == ["deferred", "oos-bug"]


def test_dry_run_writes_nothing():
    rows = [_cv("cv-cccc1111", "something nothing tracks")]
    consumed: list = []

    report = sweep_carveouts(
        repo_root=Path("/nonexistent"),
        carveout_root=Path("/nonexistent"),
        nodes=[],
        apply=False,
        read_fn=lambda root, kind=None: rows,
        create_fn=lambda **kw: (_ for _ in ()).throw(AssertionError("dry run created a node")),
        consume_fn=lambda root, ids: consumed.extend(ids) or len(ids),
    )

    assert report.by_disposition(DISPOSITION_FILE)
    assert consumed == []
    assert report.consumed == 0


def test_apply_consumes_a_filed_row_but_never_a_review_row():
    rows = [
        _cv("cv-dddd1111", "nothing tracks this one"),
        _cv("cv-dddd2222", "consolidate the duplicated stub_exec test fixture"),
    ]
    nodes = [{"id": "x-9999", "title": "consolidate the duplicated stub_exec test fixtures"}]
    consumed: list = []

    report = sweep_carveouts(
        repo_root=Path("/nonexistent"),
        carveout_root=Path("/nonexistent"),
        nodes=nodes,
        apply=True,
        mode=MODE_INTERACTIVE,
        read_fn=lambda root, kind=None: rows,
        create_fn=lambda **kw: "x-new1",
        consume_fn=lambda root, ids: consumed.extend(ids) or len(ids),
    )

    assert consumed == ["cv-dddd1111"]
    assert report.by_disposition(DISPOSITION_FILE)[0].node_id == "x-new1"
    assert report.by_disposition(DISPOSITION_REVIEW)[0].node_id == "x-9999"


def test_a_failed_mint_leaves_the_row_in_the_ledger():
    """Consuming a row whose node never landed is the exact failure the close
    gate exists to prevent: the ledger goes green and the work is tracked
    nowhere."""
    rows = [_cv("cv-eeee1111", "nothing tracks this one either")]
    consumed: list = []

    def _boom(**kwargs):
        raise RuntimeError("graph lock timeout")

    report = sweep_carveouts(
        repo_root=Path("/nonexistent"),
        carveout_root=Path("/nonexistent"),
        nodes=[],
        apply=True,
        read_fn=lambda root, kind=None: rows,
        create_fn=_boom,
        consume_fn=lambda root, ids: consumed.extend(ids) or len(ids),
    )

    assert consumed == []
    assert report.failed
    assert report.by_disposition(DISPOSITION_FILE)[0].error


def test_an_unreadable_ledger_is_not_an_empty_one():
    def _boom(root, kind=None):
        raise OSError("ledger is unreadable")

    report = sweep_carveouts(
        repo_root=Path("/nonexistent"),
        carveout_root=Path("/nonexistent"),
        nodes=[],
        apply=True,
        read_fn=_boom,
    )

    assert report.items == []
    assert report.warnings

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


def test_body_stays_within_the_cap_with_a_long_need():
    """`Blocked on:` is appended after build_body reserves its overhead, so a
    long --need used to push the body past BODY_CAP."""
    from fno.retro.classify import BODY_CAP, classify_item
    from fno.retro.types import KIND_CARVEOUT, RawItem

    c = classify_item(
        RawItem(
            kind=KIND_CARVEOUT,
            text="x" * (BODY_CAP * 2),
            source_id="cv-long0001",
            title_hint="y" * 900,
            subkind="deferred",
        )
    )

    assert len(c.body) <= BODY_CAP
    assert "Blocked on:" in c.body


def test_cv_id_quoted_in_a_node_resolves_instead_of_filing():
    cv = _cv("cv-11112222", "migrate the spawn adapters")
    nodes = [{"id": "x-1234", "title": "spawn seam", "details": "tracks cv-11112222"}]

    (item,) = plan_sweep([cv], nodes)

    assert item.disposition == DISPOSITION_RESOLVE
    assert item.node_id == "x-1234"


def test_a_bare_description_hash_parks_rather_than_consuming():
    """The hash covers the description ONLY. Two carve-outs can share a generic
    description while differing in kind, need, and scope, so resolving on it
    would consume the later row without filing its distinct work. Provable
    identity is the cv-id, which a PR-harvested node cites too."""
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

    assert item.disposition == DISPOSITION_REVIEW
    assert item.node_id == "x-5678"


def test_a_pr_harvested_node_still_resolves_by_its_cited_cv_id():
    """The reason downgrading the hash matcher costs nothing: a carve-out node
    cites its cv-id whichever harvest filed it."""
    cv = _cv("cv-33335555", "the mux pane send exits 0 without a submit key")
    nodes = [
        {
            "id": "x-5679",
            "title": "pane send",
            "details": "...\n\nSource: PR #604, source `cv-33335555`",
        }
    ]

    (item,) = plan_sweep([cv], nodes)

    assert item.disposition == DISPOSITION_RESOLVE
    assert item.node_id == "x-5679"


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


def test_a_filed_candidate_carries_a_real_dedup_hash():
    """The trailer land writes is the ONLY key a later harvest can dedup on.
    Landing without assign_hashes wrote `finding_hash=` empty, which matches no
    trailer pattern, so every node the sweep filed was invisible to dedup."""
    cv = _cv("cv-99990000", "some work nothing tracks")

    (item,) = plan_sweep([cv], [])

    assert item.candidate.content_hash
    assert item.candidate.content_hash == content_hash("some work nothing tracks")
    # And the trailer built from it round-trips through the reader.
    from fno.retro.dedup import existing_keys_from_nodes

    node = {"id": "x-1", "details": trailer(None, item.candidate.content_hash)}
    assert existing_keys_from_nodes([node])


def test_two_rows_with_the_same_text_do_not_both_file():
    """One blocker carved out from two sessions. Filing both mints two
    permanent nodes for one piece of work, and there is no delete verb."""
    rows = [
        _cv("cv-dup00001", "the same blocker, written twice"),
        _cv("cv-dup00002", "the same blocker, written twice"),
    ]

    first, second = plan_sweep(rows, [])

    assert first.disposition == DISPOSITION_FILE
    assert second.disposition == DISPOSITION_REVIEW
    assert "cv-dup00001" in second.match_reason


def test_a_row_with_no_id_parks_instead_of_failing_the_sweep():
    """read_carveouts does not require an id. Such a row cannot be cited, so it
    can never land; treating that as an error made the verb exit 1 forever."""
    rows = [{"kind": "deferred", "description": "no id on this row"}]

    (item,) = plan_sweep(rows, [])

    assert item.disposition == DISPOSITION_REVIEW
    assert item.error is None


def test_the_fuzzy_matcher_compares_the_title_the_sweep_would_file():
    """Matching on `need` left the sweep blind to nodes it filed itself, since
    the filed title comes from the description."""
    cv = _cv(
        "cv-aaaa9999",
        "consolidate the duplicated stub_exec test fixture",
        need="which conftest should own it",
    )
    nodes = [{"id": "x-7777", "title": "consolidate the duplicated stub_exec test fixtures"}]

    (item,) = plan_sweep([cv], nodes)

    assert item.disposition == DISPOSITION_REVIEW
    assert item.node_id == "x-7777"


def test_an_unreadable_ledger_reports_failed_so_the_verb_can_exit_nonzero():
    """A present-but-unreadable ledger printing '0 unharvested' and exiting 0
    is the silent success read_carveouts raises specifically to prevent."""

    def _boom(root, kind=None):
        raise OSError("ledger is unreadable")

    report = sweep_carveouts(
        repo_root=Path("/nonexistent"),
        carveout_root=Path("/nonexistent"),
        nodes=[],
        apply=False,
        read_fn=_boom,
    )

    assert report.read_failed
    assert report.failed  # drives the CLI exit code


def test_the_pr_scoped_path_skips_a_carveout_the_sweep_already_filed():
    """The hash key carries source_pr, so a sweep-filed node (None) and the same
    carve-out under a PR (123) key differently and both file. Reachable whenever
    a sweep's consume fell short, which is a documented best-effort failure."""
    from fno.retro.dedup import cv_ids_cited_in_nodes

    swept_node = {
        "id": "x-swept",
        "title": "some work",
        "details": "body\n\nSource: source `cv-cross001`\n\n"
        + trailer(None, content_hash("some work")),
    }

    # The PR-scoped path keys on "123:<hash>" and finds nothing...
    from fno.retro.dedup import existing_keys_from_nodes

    assert f"123:{content_hash('some work')}" not in existing_keys_from_nodes([swept_node])
    # ...so the cv-id is what has to catch it, and does.
    assert cv_ids_cited_in_nodes([swept_node], ["cv-cross001"]) == {"cv-cross001"}


def test_cv_id_scan_does_not_match_an_unrelated_node():
    """Positive control for the helper above: it really discriminates."""
    from fno.retro.dedup import cv_ids_cited_in_nodes

    node = {"id": "x-other", "title": "unrelated", "details": "cites cv-99999999"}

    assert cv_ids_cited_in_nodes([node], ["cv-cross001"]) == set()


def test_render_never_prints_a_count_for_an_unreadable_ledger():
    """'0 unharvested' on stdout is the masquerade read_failed exists to stop;
    a caller with stderr redirected away would read a clean ledger."""
    from fno.retro.sweep import SweepReport, render_sweep

    out = "\n".join(render_sweep(SweepReport(read_failed=True)))

    assert "unharvested" not in out or "FAILED" in out
    assert "0 file" not in out
    assert "FAILED to read the ledger" in out


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

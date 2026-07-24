"""Filing-time anchor check at retro-triage minting (x-a7ab 1.1).

A finding already addressed on its source PR (fixed-on-main) is never minted;
an unresolvable scan fails toward filing with an anchor-unverified note. Reuses
the x-7624 dispatch-time scan via a transient trailer-bearing pseudo-entry built
from the candidate.
"""
from types import SimpleNamespace

from fno.retro.dedup import anchor_verdict
from fno.retro.land import MODE_AUTONOMOUS, land_candidates
from fno.retro.types import TIER_NODE, Candidate


def _cand(*, source_pr=42, body="finding http://github.com/o/r/pull/42#discussion-rev-x"):
    return Candidate(
        title="t",
        body=body,
        tier=TIER_NODE,
        priority="p2",
        source_pr=source_pr,
        source_id="c1",
        content_hash="ab12cd34",
    )


# anchor_verdict matrix ------------------------------------------------------ #
def test_anchor_verdict_dead_when_addressed():
    # AC1-HP: the source PR now shows the finding addressed -> dead (skip).
    assert anchor_verdict(_cand(), lambda e, **k: [SimpleNamespace()]) == "dead"


def test_anchor_verdict_present_when_not_addressed():
    assert anchor_verdict(_cand(), lambda e, **k: []) == "present"


def test_anchor_verdict_unresolvable_on_scan_error():
    # AC5-EDGE: a scan failure -> unresolvable (fail toward filing).
    def boom(e, **k):
        raise RuntimeError("gh down")

    assert anchor_verdict(_cand(), boom) == "unresolvable"


def test_anchor_verdict_present_when_no_source_pr():
    seen = []
    assert anchor_verdict(_cand(source_pr=None), lambda e, **k: seen.append(e) or []) == "present"
    assert seen == []  # scan never called without a source PR


def test_anchor_verdict_present_when_no_scan_fn():
    assert anchor_verdict(_cand(), None) == "present"


# land_candidates routing ---------------------------------------------------- #
def test_land_candidates_skips_fixed_on_main(tmp_path):
    # AC1-HP: addressed -> never minted, LandResult skipped.
    created = []
    results = land_candidates(
        [_cand()],
        mode=MODE_AUTONOMOUS,
        repo_root=tmp_path,
        create_fn=lambda **k: created.append(k) or "n1",
        anchor_scan_fn=lambda e, **k: [SimpleNamespace()],
    )
    assert created == []  # never minted
    assert results and results[0].outcome == "skipped"
    assert results[0].reason == "fixed-on-main"


def test_land_candidates_mints_with_note_when_unresolvable(tmp_path):
    # AC5-EDGE: scan error -> mint, details carry the anchor-unverified note.
    created = {}

    def create(**k):
        created.update(k)
        return "n1"

    def boom(e, **k):
        raise RuntimeError("gh down")

    land_candidates(
        [_cand()],
        mode=MODE_AUTONOMOUS,
        repo_root=tmp_path,
        create_fn=create,
        anchor_scan_fn=boom,
    )
    assert "anchor-unverified" in created["details"]


def test_land_candidates_mints_normally_when_present(tmp_path):
    created = {}

    def create(**k):
        created.update(k)
        return "n1"

    land_candidates(
        [_cand()],
        mode=MODE_AUTONOMOUS,
        repo_root=tmp_path,
        create_fn=create,
        anchor_scan_fn=lambda e, **k: [],
    )
    assert created  # minted
    assert "anchor-unverified" not in created["details"]

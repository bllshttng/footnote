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


def _cand(*, source_pr=42, source_id="c1", body="finding http://github.com/o/r/pull/42#discussion-rev-x"):
    return Candidate(
        title="t",
        body=body,
        tier=TIER_NODE,
        priority="p2",
        source_pr=source_pr,
        source_id=source_id,
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


def test_anchor_verdict_unresolvable_on_outage_warning():
    # F8: scan_addressed_findings signals a GitHub outage via warnings + an empty
    # result (not an exception). anchor_verdict must read it as unresolvable, not
    # present, so the finding mints with the anchor-unverified marker.
    def outage(entries, *, include_planned=False, warnings=None):
        if warnings is not None:
            warnings.append("reconcile-findings: PR #42 thread state unavailable")
        return []  # the PR was skipped (unavailable), not determined "not addressed"

    assert anchor_verdict(_cand(), outage) == "unresolvable"


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
        anchor_scan_fn=lambda e, **k: [SimpleNamespace(node_id="candidate:c1")],
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


# F10: batched anchor scan (one scan per harvest, not per candidate) ----------- #
def test_anchor_verdicts_one_scan_for_many_candidates():
    # F10: N candidates -> ONE scan call (each PR fetched once), not N.
    from fno.retro.dedup import anchor_verdicts

    cands = [_cand(), _cand(source_id="c2")]
    calls = []

    def scan(entries, *, include_planned=False, warnings=None):
        calls.append(len(entries))
        return []

    out = anchor_verdicts(cands, scan)
    assert len(calls) == 1  # batched
    assert calls[0] == 2  # both candidates in one batch
    assert out == {"c1": "present", "c2": "present"}


def test_anchor_verdicts_dead_only_for_the_addressed_candidate():
    # F10: a candidate is dead only if ITS finding was addressed (membership, not
    # a non-empty list), so one addressed finding does not mark a sibling dead.
    from fno.retro.dedup import anchor_verdicts

    cands = [_cand(), _cand(source_id="c2")]

    def scan(entries, *, include_planned=False, warnings=None):
        return [SimpleNamespace(node_id="candidate:c1")]  # only c1 addressed

    out = anchor_verdicts(cands, scan)
    assert out == {"c1": "dead", "c2": "present"}

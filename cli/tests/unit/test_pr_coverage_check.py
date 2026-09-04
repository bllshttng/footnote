"""The coverage predicate's two surfaces refuse with ONE sentence.

`fno do pr merge` (through run_merge, recompute=True) and `fno do pr
coverage-check` (through the verb, recompute=False) must print the same
refusal for the same row. Two denials for different reasons is precisely the
defect the shared predicate exists to remove, so these assert the TEXT
character for character, not merely that both refused.

The absence pins are the temptation these tests exist to outlive: a future
author will want to soften a missing or head-mismatched row into a fail-open.
Exit 3 on absence is load-bearing - `fno do pr merge` refuses those rows, so a
hook that waved them through would recreate the divergence on the exact
input where nothing reviewed the PR. Exit 4 stays reserved for a named
instrument failure, reachable here only by forcing the head fetch to fail or
the read to raise, never by an empty read.
"""
import json
import re
from pathlib import Path

import pytest

from fno.pr import _coverage_gate, _merge, _reviews
from fno.pr._proc import Result

from .test_pr_merge import FakeRun, _last_json, enabled  # noqa: F401

HEAD = "aaaa1111bbbb2222"


def _seed_row(
    tmp_path, *, coverage, count, head, verdicts=None, pr=42, self_attested=None,
    review_state=None,
):
    """One review_coverage event in the project log the gate reads."""
    (tmp_path / ".fno").mkdir(exist_ok=True)
    data = {"pr": pr, "coverage": coverage, "head_sha": head}
    if review_state is not None:
        data["review_state"] = review_state
    if coverage == "covered":
        # A posture-capable producer resolves a rung on every covered row;
        # without the key the gate treats the row as pre-posture evidence and
        # demands a recompute, which is not what these tests are about.
        data["review_posture"] = {
            "posture": "self_review",
            "rank": 3,
            "source": "legacy",
            "posture_satisfied": True,
            "posture_gaps": [],
        }
    if coverage in ("covered", "uncovered"):
        data["reviewed_count"] = count
    if self_attested is not None:
        data["self_attested_count"] = self_attested
    if verdicts is not None:
        data["verdicts"] = verdicts
    (tmp_path / ".fno" / "events.jsonl").write_text(
        json.dumps({"ts": "2026-08-16T03:00:00Z", "type": "review_coverage", "data": data})
        + "\n",
        encoding="utf-8",
    )


@pytest.fixture
def live_head(monkeypatch):
    """Both surfaces see the same live PR head and the real events read."""
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    # Route the merge path through the REAL read (the `enabled` fixture's
    # covered stub would bypass it): the only seams are the head and the verb.
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: _reviews.review_coverage_for_gate(pr, repo, head),
    )
    # No 3am valve: these tests pin the REFUSAL sentences, and the override
    # would answer COVERED before any of them is built.
    monkeypatch.setattr(_reviews, "_override_label_actor", lambda pr, repo, r: (False, None))
    # The disposition pass scopes the attestation chain by the PR's head
    # branch; a missing probe is an instrument failure (UNANSWERED), so the
    # hermetic fixture answers it.
    monkeypatch.setattr(_merge, "_pr_base_head_refs", lambda pr, cwd: ("main", "feature/x"))


def _merge_refusal(capsys, tmp_path, fake):
    """The refusal sentence run_merge emits for this row, prefix stripped."""
    monkeypatch_run = pytest.MonkeyPatch()
    monkeypatch_run.setattr(_merge, "run", fake)
    try:
        assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
        reason = _last_json(capsys, stream="err")["reason"]
    finally:
        monkeypatch_run.undo()
    assert reason.startswith("unreviewed merge refused: ")
    return reason[len("unreviewed merge refused: "):]


def _verb_refusal(capsys, tmp_path):
    """The first stderr line the verb prints, and its exit state."""
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    cap = capsys.readouterr()
    return rc, (cap.err.strip().splitlines() or [""])[0]


# ---- the two refusal branches, asserted on the TEXT ----


def test_uncovered_row_refuses_with_one_sentence(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    fake = FakeRun(toplevel=str(tmp_path))
    merge_line = _merge_refusal(capsys, tmp_path, fake)
    assert "0 reviewed" in merge_line
    rc, verb_line = _verb_refusal(capsys, tmp_path)
    assert rc == 3
    assert verb_line == merge_line, "both surfaces must refuse with one sentence"


def test_required_code_review_attestation_refuses_with_one_sentence(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """The attestation refusal comes from a different branch and is the one
    most likely to drift; pin it to the same one-sentence contract."""
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: True)
    _seed_row(
        tmp_path,
        coverage="covered",
        count=2,
        head=HEAD,
        verdicts=[{"name": "some-bot", "producer": "github_app", "verdict": "reviewed"}],
    )
    fake = FakeRun(toplevel=str(tmp_path))
    merge_line = _merge_refusal(capsys, tmp_path, fake)
    assert "required code-review has no head-pinned local pass attestation" in merge_line
    rc, verb_line = _verb_refusal(capsys, tmp_path)
    assert rc == 3
    assert verb_line == merge_line, "both surfaces must refuse with one sentence"


# ---- the positive side, at the same head ----


def test_covered_row_passes_both_surfaces(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(
        tmp_path,
        coverage="covered",
        count=1,
        head=HEAD,
        verdicts=[{
            "name": "code-review",
            "producer": "local_attestation",
            "verdict": "reviewed",
            "reviewed_sha": HEAD,
            "freshness": "fresh",
        }],
    )
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    capsys.readouterr()
    assert rc == 0
    fake = FakeRun(gh_merge=Result(0, "Merged pull request", ""), toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    assert _last_json(capsys)["outcome"] == "merged"


def test_rebased_out_verdict_refuses_both_surfaces(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    # A producer that read a rewritten-out sha emits `stale` (the Rust
    # predicate's verdict); the shaper reads the stored label and refuses.
    _seed_row(
        tmp_path,
        coverage="covered",
        count=1,
        head=HEAD,
        verdicts=[
            {
                "name": "code-review",
                "producer": "local_attestation",
                "verdict": "reviewed",
                "reviewed_sha": "rewritten-out",
                "freshness": "stale",
            }
        ],
    )
    fake = FakeRun(toplevel=str(tmp_path))
    merge_line = _merge_refusal(capsys, tmp_path, fake)
    assert "uncovered" in merge_line
    rc, verb_line = _verb_refusal(capsys, tmp_path)
    assert rc == 3
    assert verb_line == merge_line


def test_covered_row_without_verdict_proof_refuses_both_surfaces(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(tmp_path, coverage="covered", count=1, head=HEAD, verdicts=[])
    fake = FakeRun(toplevel=str(tmp_path))
    merge_line = _merge_refusal(capsys, tmp_path, fake)
    assert "uncovered" in merge_line
    rc, verb_line = _verb_refusal(capsys, tmp_path)
    assert rc == 3
    assert verb_line == merge_line


# ---- the absence pins: refusal, never a shrug ----


def test_no_row_refuses_and_spawns_no_subprocess(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """Absence is an answer: nothing attested this head. The pure read must
    not fire the Rust producer, which is budgeted far past a hook's reach."""
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    fired: list = []
    monkeypatch.setattr(
        _reviews, "_fire_review_coverage_verb", lambda *a, **k: fired.append(a) or (False, "")
    )
    rc, verb_line = _verb_refusal(capsys, tmp_path)
    assert rc == 3
    assert "no review_coverage event for this PR" in verb_line
    assert "searched:" in verb_line, "the refusal names the logs it read"
    assert not fired, "recompute=False spawns no fno-agents subprocess"


def test_row_pinned_to_another_head_refuses(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(tmp_path, coverage="covered", count=2, head="cccc3333dddd4444")
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "eeee5555ffff6666")
    rc, verb_line = _verb_refusal(capsys, tmp_path)
    assert rc == 3
    assert "cccc3333" in verb_line and "eeee5555" in verb_line
    assert "attestations are head-pinned by design" in verb_line


# ---- exit 4: a named instrument failure, and nothing else ----


def test_exit_four_is_reached_only_by_a_named_instrument_failure(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """Force the head fetch to die: the verdict is UNANSWERED with a note
    naming the probe. Then, same PR with a live head and an empty log, the
    read returns 3 - no absence route can reach 4."""
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: None)
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    note = capsys.readouterr().err.strip()
    assert rc == 4
    assert "head fetch" in note, "the note names which probe died"

    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    capsys.readouterr()
    assert rc == 3, "an empty read is absence, and absence refuses"


def test_raised_read_is_unanswered_not_absence(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """A read that raises is an instrument failure, not a finding of nothing."""
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)

    def boom(*a, **k):
        raise OSError("log unreadable")

    # Patch the reader ACTUALLY on the no-recompute path. `latest_review_coverage`
    # is a thin wrapper the gate no longer calls, so patching it raises nothing
    # and the assertion below would pass on a read that quietly succeeded.
    monkeypatch.setattr(_reviews, "latest_review_coverage_row", boom)
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    note = capsys.readouterr().err.strip()
    assert rc == 4
    assert "events read raised" in note


def test_merge_blocks_when_the_head_fetch_fails(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """The merge surface fails closed on UNANSWERED: a covered row cannot
    rescue a merge whose head probe died (the recompute needs the head to pin
    the row it would emit). The receipt names the dead probe instead of
    calling the PR unreviewed, so a worker's recovery is a retry, not a hunt
    for reviewers."""
    _seed_row(tmp_path, coverage="covered", count=2, head=HEAD)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: None)
    fake = FakeRun(toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
    obj = _last_json(capsys, stream="err")
    assert obj["outcome"] == "blocked"
    assert obj["reason"] == "coverage probe failed, merge refused: pr head fetch failed"


# ---- the 3am release valve on the verb the docs name ----
#
# `docs/best-practices.md` and `docs/troubleshooting.md` both route merges
# through `fno do pr merge` and name the `coverage-override` label as the only
# way past an uncovered head. A valve read anywhere but the shared predicate
# is a valve on one of N reachable paths: the gate refuses first and the
# publisher's override branch is never reached, leaving the raw `gh` path the
# same docs forbid as the only way through.


def test_the_override_label_opens_the_gate_on_an_uncovered_pr(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """The label answers COVERED where the same row would otherwise refuse."""
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(
        _reviews, "_override_label_actor", lambda pr, repo, r: (True, "jane")
    )
    monkeypatch.setattr(_coverage_gate, "_pr_author_login", lambda pr, repo: "op")
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED
    assert refusal == ""
    # The waiver drops the review, never the TOCTOU pin: `--match-head-commit`
    # must still refuse a push that races the merge.
    assert covered_head == HEAD
    assert note.startswith(_coverage_gate.OVERRIDE_NOTE_PREFIX)
    assert "jane" in note


def test_an_override_applied_by_the_pr_author_is_refused(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """The author check the approval path always had, on the one bypass
    beside it. An agent that cannot merge can label its own PR; that is
    reject-and-attest wearing a label. The refusal NAMES the author, so an
    operator whose label was refused sees exactly why the valve stayed
    shut."""
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(
        _merge, "_code_review_attestation_required", lambda repo, pr_number=0: False
    )
    monkeypatch.setattr(
        _reviews, "_override_label_actor", lambda pr, repo, r: (True, "worker-login")
    )
    monkeypatch.setattr(
        _coverage_gate, "_pr_author_login", lambda pr, repo: "worker-login"
    )
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "worker-login" in refusal, "the refusal must name the author: " + refusal
    assert "the PR author" in refusal
    assert "cannot override its own review gate" in refusal


def test_an_override_by_the_author_in_different_casing_is_refused(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """A GitHub login is case-insensitive. An exact-case compare would open
    the valve for the author the moment the events feed and /pulls/{n}
    disagree on casing, which is the one shape this check exists to stop."""
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(
        _merge, "_code_review_attestation_required", lambda repo, pr_number=0: False
    )
    monkeypatch.setattr(
        _reviews, "_override_label_actor", lambda pr, repo, r: (True, "BllsHttng")
    )
    monkeypatch.setattr(_coverage_gate, "_pr_author_login", lambda pr, repo: "bllshttng")
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "cannot override its own review gate" in refusal, refusal


def test_an_override_with_an_unreadable_actor_fails_closed(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """Label held, actor unreadable: the valve stays shut. An unreadable
    actor is exactly the state a forger produces, and the approval path
    already fails closed on the same ambiguity."""
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(
        _merge, "_code_review_attestation_required", lambda repo, pr_number=0: False
    )
    monkeypatch.setattr(
        _reviews, "_override_label_actor", lambda pr, repo, r: (True, None)
    )
    # The author read must never even matter: the actor alone is unreadable.
    monkeypatch.setattr(
        _coverage_gate,
        "_pr_author_login",
        lambda pr, repo: pytest.fail("no author read on an unreadable actor"),
    )
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "labelling actor is unreadable" in refusal


def test_an_override_with_an_unreadable_author_fails_closed(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """Actor readable, author unreadable: the labeller cannot be proven to
    differ from the author, so the valve refuses rather than guess."""
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(
        _merge, "_code_review_attestation_required", lambda repo, pr_number=0: False
    )
    monkeypatch.setattr(
        _reviews, "_override_label_actor", lambda pr, repo, r: (True, "jane")
    )
    monkeypatch.setattr(_coverage_gate, "_pr_author_login", lambda pr, repo: None)
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "PR author is unreadable" in refusal


def test_the_override_reaches_the_merge_verb_and_says_so(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """`fno do pr merge` proceeds under the label, and the receipt names the waiver.

    Exit 2 here would mean the documented valve does not open on the verb the
    docs tell the operator to use. A silent exit 0 would mean a waived merge
    is indistinguishable from a reviewed one in the log.
    """
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: ({"coverage": "uncovered", "reviewed_count": 0}, ""),
    )
    monkeypatch.setattr(
        _reviews, "_override_label_actor", lambda pr, repo, r: (True, "jane")
    )
    # The valve now needs a labeller distinct from the author; name one.
    monkeypatch.setattr(_coverage_gate, "_pr_author_login", lambda pr, repo: "op")
    fake = FakeRun(toplevel=str(tmp_path))
    monkeypatch.setattr(_merge, "run", fake)
    rc = _merge.run_merge(["42"], cwd=str(tmp_path))
    err = capsys.readouterr().err
    assert rc != 2, "the documented valve did not open on `fno do pr merge`"
    assert "coverage waived: coverage-override label applied by jane" in err


def test_an_unreadable_label_never_opens_the_valve(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """Fail closed: a label read that dies is not an override.

    The recovery from a wrong refusal is one command. The recovery from a
    merge nobody reviewed is a revert.
    """
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(
        _merge, "_code_review_attestation_required", lambda repo, pr_number=0: False
    )

    def boom(*_a, **_k):
        raise OSError("gh died")

    monkeypatch.setattr(_reviews, "_override_label_actor", boom)
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    capsys.readouterr()
    assert rc == 3


# ---- config.review.require_corroboration (x-7f7b) ----


@pytest.fixture
def corroboration_on(monkeypatch):
    """config.review.require_corroboration = true, the operator's explicit
    flip. Default (unset) stays covered by the pins above."""
    from fno.config import ReviewBlock

    class _Settings:
        review = ReviewBlock(require_corroboration=True)

    import fno.config as config_mod

    monkeypatch.setattr(
        config_mod, "load_settings_for_repo", lambda *a, **k: _Settings()
    )


_SELF_ONLY_VERDICTS = [
    {
        "name": "code-review",
        "producer": "local_attestation",
        "verdict": "reviewed",
        "attestation_origin": "self_attested",
        "reviewed_sha": HEAD,
        "freshness": "fresh",
    }
]


def test_corroboration_on_holds_a_self_attested_only_pr(
    enabled, live_head, corroboration_on, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """The policy: a PR whose only coverage is the author's own attestation
    reads as uncovered, and the refusal names BOTH satisfying paths."""
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(
        tmp_path,
        coverage="covered",
        count=1,
        head=HEAD,
        verdicts=_SELF_ONLY_VERDICTS,
        self_attested=1,
        review_state="reviewed",
    )
    fake = FakeRun(toplevel=str(tmp_path))
    merge_line = _merge_refusal(capsys, tmp_path, fake)
    assert "rests on the author's own attestation alone" in merge_line
    assert "second session's head-pinned attestation" in merge_line
    assert "GitHub App review" in merge_line
    rc, verb_line = _verb_refusal(capsys, tmp_path)
    assert rc == 3
    assert verb_line == merge_line, "both surfaces must refuse with one sentence"


def test_corroboration_on_holds_a_policy_rewritten_row(
    enabled, live_head, corroboration_on, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """The reachable shape: the loop-side policy already rewrote the row to
    covered=uncovered / reviewed_count 0 while PRESERVING the self-attestation
    count. The gate must answer with the corroboration refusal naming both
    remedies - the generic uncovered remedy ("re-run your own review") can
    never satisfy the policy and would burn a worker to budget."""
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(
        tmp_path,
        coverage="uncovered",
        count=0,
        head=HEAD,
        verdicts=_SELF_ONLY_VERDICTS,
        self_attested=1,
        review_state="reviewed",
    )
    rc, verb_line = _verb_refusal(capsys, tmp_path)
    assert rc == 3
    assert "rests on the author's own attestation alone" in verb_line
    assert "second session's head-pinned attestation" in verb_line


def test_corroboration_never_masks_a_stale_head(
    enabled, live_head, corroboration_on, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """A self-attested-only row pinned to a MOVED head: the stale-head remedy
    (re-attest at the live head) is the truer one and outranks the policy
    sentence, which prescribes corroboration for a row that is stale anyway."""
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(
        tmp_path,
        coverage="covered",
        count=1,
        head="cccc3333dddd4444",
        verdicts=_SELF_ONLY_VERDICTS,
        self_attested=1,
        review_state="reviewed",
    )
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "eeee5555ffff6666")
    rc, verb_line = _verb_refusal(capsys, tmp_path)
    assert rc == 3
    assert "attestations are head-pinned by design" in verb_line
    assert "rests on the author's own attestation alone" not in verb_line


def test_corroboration_off_by_default_covers_the_same_row(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """Unchanged, pinned: with the key unset the same row is covered. No
    existing install changes behavior when this ships."""
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(
        tmp_path,
        coverage="covered",
        count=1,
        head=HEAD,
        verdicts=_SELF_ONLY_VERDICTS,
        self_attested=1,
        review_state="reviewed",
    )
    fake = FakeRun(gh_merge=Result(0, "Merged", ""), toplevel=str(tmp_path))
    monkeypatch_run = pytest.MonkeyPatch()
    monkeypatch_run.setattr(_merge, "run", fake)
    try:
        assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    finally:
        monkeypatch_run.undo()


def test_corroboration_on_covers_a_corroborated_pr(
    enabled, live_head, corroboration_on, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """One self-attested pass plus a second session's attestation is covered:
    the policy demands corroboration, and it has it."""
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(
        tmp_path,
        coverage="covered",
        count=2,
        head=HEAD,
        verdicts=_SELF_ONLY_VERDICTS
        + [
            {
                "name": "code-review",
                "producer": "local_attestation",
                "verdict": "reviewed",
                "attestation_origin": "other_session",
                "reviewed_sha": HEAD,
                "freshness": "fresh",
            }
        ],
        self_attested=1,
        review_state="reviewed",
    )
    fake = FakeRun(gh_merge=Result(0, "Merged", ""), toplevel=str(tmp_path))
    monkeypatch_run = pytest.MonkeyPatch()
    monkeypatch_run.setattr(_merge, "run", fake)
    try:
        assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
    finally:
        monkeypatch_run.undo()


def test_status_ready_names_the_corroboration_conjunct(
    corroboration_on, monkeypatch, tmp_path  # noqa: F811
):
    """`fno do pr status` reports the same refusal merge enforces: a
    self-attested-only row blocks `ready` as review_coverage_corroboration,
    not as a bare uncovered count a worker would retry into budget. The
    policy-rewritten shape (0 counted, self-attestation preserved) names the
    policy too - the truer blocker, exactly as the merge verb ranks it."""
    from fno.pr import _status

    monkeypatch.setattr(_merge, "_repo_state_dir", lambda repo: str(tmp_path / ".fno"))
    base = dict(
        head_sha=HEAD,
        verdicts=_SELF_ONLY_VERDICTS,
        self_attested_count=1,
        review_state="reviewed",
    )
    covered_row = dict(coverage="covered", reviewed_count=1, **base)
    rewritten_row = dict(coverage="uncovered", reviewed_count=0, **base)
    for row in (covered_row, rewritten_row):
        blockers = _status._ready_blockers(
            True,
            "green",
            0,
            row,
            True,
            head=HEAD,
            code_review_required=False,
            counts={},
            repo=str(tmp_path),
        )
        assert blockers == ["review_coverage_corroboration"], (row, blockers)


# ---- the disposition-complete pass condition (AC5) ----
#
# The specimen is BYTE-PINNED to head 46695fffd (ruling d-fc3b3837): five
# codex findings, five fix commits, five dispositions recorded on the round
# that reviewed the fix delta, a chain tiling base..head. The record that did
# everything right and still could not merge under the clean-only rule MUST
# pass under disposition-complete. The fixture carries its own review
# payloads and never reads the live PR: PR 1179 as it stands today is a
# different record (14 open threads), and a fixture built from it would
# assert COVERED over the exploit itself.

FIXTURE_HEAD = "46695fffd00000000000000000000000000000000"
DISPOSITIONS_FIXTURE = (
    Path(__file__).resolve().parents[1]
    / "fixtures"
    / "review_dispositions"
    / "pr1179-46695fffd.jsonl"
)


def _seed_specimen(tmp_path, *, extra_lines=()):
    (tmp_path / ".fno").mkdir(exist_ok=True)
    text = DISPOSITIONS_FIXTURE.read_text(encoding="utf-8")
    for line in extra_lines:
        text += json.dumps(line) + "\n"
    (tmp_path / ".fno" / "events.jsonl").write_text(text, encoding="utf-8")


def _specimen_gates(monkeypatch):
    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo, pr_number=0: True)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: FIXTURE_HEAD)
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    monkeypatch.setattr(_merge, "_pr_base_head_refs", lambda pr, cwd: ("main", "feature/x-8439"))
    monkeypatch.setattr(_reviews, "_override_label_actor", lambda pr, repo, r: (False, None))


def _ac5b_finding():
    return {
        "ts": "2026-08-25T22:28:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": "46695fffd00000000000000000000000000000000",
            "verdict": "fail",
            "session_id": "s-1179",
            "branch": "feature/x-8439",
            "reviewed_base_sha": "a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3",
            "reviewed_head_sha": "46695fffd00000000000000000000000000000000",
            "reviewed_file_count": 43,
            "reviewed_line_count": 2452,
            "findings_blocking": 1,
            "findings_nonblocking": 0,
            "findings": [
                {
                    "category": "correctness",
                    "verdict": None,
                    "blocking": True,
                    "has_required_fields": True,
                    "finding_key": "cli/src/fno/pr/_coverage_gate.py:999:correctness",
                }
            ],
        },
    }


def test_ac5_marker_specimen_is_covered(monkeypatch, tmp_path):
    """AC5-MARKER: the disposition-complete specimen returns literal COVERED."""
    _specimen_gates(monkeypatch)
    _seed_specimen(tmp_path)
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        1179, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED
    assert refusal == ""


def test_ac5b_marker_specimen_plus_one_open_finding_refuses(monkeypatch, tmp_path):
    """AC5b: the same fixture plus one open correctness finding REFUSES, by key.

    The budget is raised here: the specimen's chain carries three attested
    rounds (fail, pass, this finding's fail) and the x-2219 per-PR-total
    counter counts all three, so the shipped max_rounds of 2 would fire the
    cap-file arm and the refusal would name the cap, not the finding. This
    test pins the by-key refusal, which needs budget room; the cap arm has
    its own tests."""
    _specimen_gates(monkeypatch)
    monkeypatch.setattr(
        _coverage_gate, "resolved_max_rounds", lambda repo: 5
    )
    _seed_specimen(tmp_path, extra_lines=[_ac5b_finding()])
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        1179, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "cli/src/fno/pr/_coverage_gate.py:999:correctness" in refusal


def test_ac5_hp_fixed_confirmed_plus_untouched_nits_covered(monkeypatch, tmp_path):
    """AC5-HP: fixed CONFIRMED + two nits with no disposition -> COVERED, named."""
    _specimen_gates(monkeypatch)
    r1 = {
        "ts": "2026-08-25T21:00:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": "a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3",
            "verdict": "fail",
            "session_id": "s-hp",
            "branch": "feature/x-8439",
            "reviewed_base_sha": "17a3b85b1a70a22014f1fc4e04b7aa35a632757f",
            "reviewed_head_sha": "a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3",
            "reviewed_file_count": 4,
            "reviewed_line_count": 90,
            "findings_blocking": 1,
            "findings_nonblocking": 2,
            "findings": [
                {"category": "correctness", "verdict": "CONFIRMED", "blocking": True,
                 "has_required_fields": True, "finding_key": "a.py:1:correctness"},
                {"category": "nit", "verdict": None, "blocking": False,
                 "has_required_fields": True, "finding_key": "b.py:2:nit"},
                {"category": "typo", "verdict": None, "blocking": False,
                 "has_required_fields": True, "finding_key": "c.py:3:typo"},
            ],
        },
    }
    r2 = {
        "ts": "2026-08-25T21:30:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": FIXTURE_HEAD,
            "verdict": "pass",
            "session_id": "s-hp",
            "branch": "feature/x-8439",
            "reviewed_base_sha": "a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3",
            "reviewed_head_sha": FIXTURE_HEAD,
            "reviewed_file_count": 4,
            "reviewed_line_count": 95,
            "findings_blocking": 0,
            "findings_nonblocking": 0,
            "findings": [],
            "dispositions": [
                {"finding_key": "a.py:1:correctness", "disposition": "fixed", "reason": "commit f00"},
            ],
        },
    }
    row = {
        "ts": "2026-08-25T21:31:00Z",
        "type": "review_coverage",
        "source": "hook",
        "data": {
            "pr": 1179, "coverage": "covered", "review_state": "reviewed",
            "reviewed_count": 1, "head_sha": FIXTURE_HEAD,
            "verdicts": [{
                "producer": "local_attestation", "name": "code-review",
                "verdict": "reviewed", "reviewed_sha": FIXTURE_HEAD,
                "freshness": "fresh", "attestation_origin": "other_session",
            }],
        },
    }
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in (r1, r2, row)) + "\n", encoding="utf-8"
    )
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        1179, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED
    assert note == "2 non-blocking finding(s) treated by class"


def test_ac5_err_self_attested_decline_refuses_with_both_remedies(monkeypatch, tmp_path):
    """AC5-ERR: a declined security finding on self-attestation alone REFUSES."""
    _specimen_gates(monkeypatch)
    r1 = {
        "ts": "2026-08-25T21:00:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": FIXTURE_HEAD,
            "verdict": "fail",
            "session_id": "s-err",
            "branch": "feature/x-8439",
            "reviewed_base_sha": "17a3b85b1a70a22014f1fc4e04b7aa35a632757f",
            "reviewed_head_sha": FIXTURE_HEAD,
            "reviewed_file_count": 2,
            "reviewed_line_count": 30,
            "findings_blocking": 1,
            "findings_nonblocking": 0,
            "findings": [
                {"category": "security", "verdict": None, "blocking": True,
                 "has_required_fields": True, "finding_key": "sec.rs:1:security"},
            ],
            "dispositions": [
                {"finding_key": "sec.rs:1:security", "disposition": "declined",
                 "reason": "not applicable here"},
            ],
        },
    }
    row = {
        "ts": "2026-08-25T21:05:00Z",
        "type": "review_coverage",
        "source": "hook",
        "data": {
            "pr": 1179, "coverage": "covered", "review_state": "reviewed",
            "reviewed_count": 1, "self_attested_count": 1,
            "author_session_id": "sess-author", "head_sha": FIXTURE_HEAD,
            "verdicts": [{
                "producer": "local_attestation", "name": "code-review",
                "verdict": "reviewed", "reviewed_sha": FIXTURE_HEAD,
                "freshness": "fresh", "attestation_origin": "self_attested",
            }],
        },
    }
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in (r1, row)) + "\n", encoding="utf-8"
    )
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        1179, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "corroboration" in refusal
    assert "second session's head-pinned attestation" in refusal
    assert "non-author GitHub approval" in refusal


def test_ac5_edge_producer_blocking_count_is_never_the_answer(monkeypatch, tmp_path):
    """AC5-EDGE: findings_blocking: 0 over a CONFIRMED style-tagged finding REFUSES."""
    _specimen_gates(monkeypatch)
    r1 = {
        "ts": "2026-08-25T21:00:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": FIXTURE_HEAD,
            "verdict": "pass",
            "session_id": "s-edge",
            "branch": "feature/x-8439",
            "reviewed_base_sha": "17a3b85b1a70a22014f1fc4e04b7aa35a632757f",
            "reviewed_head_sha": FIXTURE_HEAD,
            "reviewed_file_count": 2,
            "reviewed_line_count": 30,
            "findings_blocking": 0,
            "findings_nonblocking": 1,
            "findings": [
                {"category": "style", "verdict": "CONFIRMED", "blocking": False,
                 "has_required_fields": True, "finding_key": "lie.py:1:style"},
            ],
        },
    }
    row = {
        "ts": "2026-08-25T21:05:00Z",
        "type": "review_coverage",
        "source": "hook",
        "data": {
            "pr": 1179, "coverage": "covered", "review_state": "reviewed",
            "reviewed_count": 1, "head_sha": FIXTURE_HEAD,
            "verdicts": [{
                "producer": "local_attestation", "name": "code-review",
                "verdict": "reviewed", "reviewed_sha": FIXTURE_HEAD,
                "freshness": "fresh", "attestation_origin": "other_session",
            }],
        },
    }
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in (r1, row)) + "\n", encoding="utf-8"
    )
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        1179, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "lie.py:1:style" in refusal


def test_ac5_decline_with_corroboration_is_terminal(monkeypatch, tmp_path):
    """The same decline as AC5-ERR, corroborated by a second session: COVERED."""
    _specimen_gates(monkeypatch)
    r1 = {
        "ts": "2026-08-25T21:00:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": FIXTURE_HEAD,
            "verdict": "fail",
            "session_id": "s-err",
            "attester_session_id": "sess-author",
            "branch": "feature/x-8439",
            "reviewed_base_sha": "17a3b85b1a70a22014f1fc4e04b7aa35a632757f",
            "reviewed_head_sha": FIXTURE_HEAD,
            "reviewed_file_count": 2,
            "reviewed_line_count": 30,
            "findings_blocking": 1,
            "findings_nonblocking": 0,
            "findings": [
                {"category": "security", "verdict": None, "blocking": True,
                 "has_required_fields": True, "finding_key": "sec.rs:1:security"},
            ],
            "dispositions": [
                {"finding_key": "sec.rs:1:security", "disposition": "declined",
                 "reason": "not applicable here"},
            ],
        },
    }
    r2 = {
        "ts": "2026-08-25T21:04:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": FIXTURE_HEAD,
            "verdict": "pass",
            "session_id": "s-peer",
            "attester_session_id": "sess-peer",
            "branch": "feature/x-8439",
            "reviewed_base_sha": "17a3b85b1a70a22014f1fc4e04b7aa35a632757f",
            "reviewed_head_sha": FIXTURE_HEAD,
            "reviewed_file_count": 2,
            "reviewed_line_count": 30,
            "findings_blocking": 0,
            "findings_nonblocking": 0,
            "findings": [],
        },
    }
    row = {
        "ts": "2026-08-25T21:05:00Z",
        "type": "review_coverage",
        "source": "hook",
        "data": {
            "pr": 1179, "coverage": "covered", "review_state": "reviewed",
            "reviewed_count": 2, "self_attested_count": 1,
            "author_session_id": "sess-author", "head_sha": FIXTURE_HEAD,
            "verdicts": [
                {"producer": "local_attestation", "name": "code-review",
                 "verdict": "reviewed", "reviewed_sha": FIXTURE_HEAD,
                 "freshness": "fresh", "attestation_origin": "self_attested"},
                {"producer": "local_attestation", "name": "code-review",
                 "verdict": "reviewed", "reviewed_sha": FIXTURE_HEAD,
                 "freshness": "fresh", "attestation_origin": "other_session"},
            ],
        },
    }
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in (r1, r2, row)) + "\n", encoding="utf-8"
    )
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        1179, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED
    assert refusal == ""


# ---- the round budget and the fourth verdict (AC7) ----
#
# The PR-1170 shape: three fail rounds, the blocking finding declined rather
# than fixed, no corroboration. Under the cap (max_rounds 2) that chain is
# IMPOSSIBLE - exit 5, the word "impossible" literal on stderr, both remedies
# named, and no instruction that asks for another review.


def _ac7_round(ts: str, verdict: str, head: str, dispositions=None):
    data = {
        "reviewer": "code-review",
        "head_sha": head,
        "verdict": verdict,
        "session_id": "s-ac7",
        "attester_session_id": "sess-author",
        "branch": "feature/x-8439",
        "reviewed_base_sha": "a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3",
        "reviewed_head_sha": head,
        "reviewed_file_count": 3,
        "reviewed_line_count": 40,
        "findings_blocking": 1,
        "findings": [
            {
                "category": "security",
                # CONFIRMED is what makes this finding HARD: under the
                # operator's round-cap ruling only a CONFIRMED correctness or
                # security finding still reaches IMPOSSIBLE at the cap. The
                # softer shapes are filed and the PR merges (see the
                # file-at-cap tests below).
                "verdict": "CONFIRMED",
                "blocking": True,
                "has_required_fields": True,
                "finding_key": "hooks/git-protection.py:302:security",
            }
        ],
    }
    if dispositions is not None:
        data["dispositions"] = dispositions
    return {"ts": ts, "type": "review_attestation", "source": "hook", "data": data}


_AC7_DECLINE = [
    {
        "finding_key": "hooks/git-protection.py:302:security",
        "disposition": "declined",
        "reason": "not applicable here",
    }
]


def _ac7_seed(tmp_path, rounds):
    """A fail-round chain plus the covered self-attested row it tiles to.

    Each round reviews a DIFFERENT head (the loop's non-fix commits move it),
    which is also what keeps the chain dedup honest: identical data rounds
    would collapse to one event."""
    stamps = ["2026-08-25T21:00:00Z", "2026-08-25T21:30:00Z", "2026-08-25T22:00:00Z"]
    heads = [
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
        FIXTURE_HEAD,
    ]
    lines = []
    for i in range(rounds):
        dispositions = _AC7_DECLINE if i == 1 else None
        lines.append(_ac7_round(stamps[i], "fail", heads[i], dispositions))
    (tmp_path / ".fno").mkdir(exist_ok=True)
    with open(tmp_path / ".fno" / "events.jsonl", "w", encoding="utf-8") as fh:
        for line in lines:
            fh.write(json.dumps(line) + "\n")
        fh.write(
            json.dumps(
                {
                    "ts": "2026-08-25T22:01:00Z",
                    "type": "review_coverage",
                    "source": "hook",
                    "data": {
                        "pr": 42,
                        "coverage": "covered",
                        "review_state": "reviewed",
                        "reviewed_count": 1,
                        "self_attested_count": 1,
                        "head_sha": FIXTURE_HEAD,
                        "verdicts": [
                            {
                                "producer": "local_attestation",
                                "name": "code-review",
                                "verdict": "reviewed",
                                "attestation_origin": "self_attested",
                                "reviewed_sha": FIXTURE_HEAD,
                                "freshness": "fresh",
                            }
                        ],
                    },
                }
            )
            + "\n"
        )


def test_ac7_marker_exhausted_decline_exits_impossible(monkeypatch, tmp_path, capsys):
    """Three fail rounds, decline uncorroborated: exit 5, 'impossible' literal,
    both remedies named, and no instruction to run another review."""
    _specimen_gates(monkeypatch)
    _ac7_seed(tmp_path, rounds=3)
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    cap = capsys.readouterr()
    line = (cap.err.strip().splitlines() or [""])[0]
    assert rc == _coverage_gate.IMPOSSIBLE == 5
    assert "impossible" in line
    assert "non-author GitHub approval" in line
    assert "coverage-override label" in line
    assert "run the review verb" not in line
    assert "hooks/git-protection.py:302:security" in line


@pytest.mark.parametrize("max_rounds", [1, 2, 3, 5])
def test_max_rounds_n_means_exactly_n_rounds(monkeypatch, tmp_path, max_rounds):
    """The boundary tracks the CONFIGURED number, at four of them.

    Set it to 3 and the third round is the last one; set it to 5 and the fifth
    is. Pinning only max_rounds = 2 would leave the cap free to be hardcoded
    to 2 somewhere and still pass, which is the exact shape a single-value
    test cannot see.
    """
    monkeypatch.setattr(_coverage_gate, "resolved_max_rounds", lambda repo: max_rounds)
    seen = {}
    for n in (max_rounds - 1, max_rounds, max_rounds + 1):
        if n < 1:
            continue
        _specimen_gates(monkeypatch)
        _seed_soft_cap(tmp_path, rounds=n)
        monkeypatch.setattr(
            _coverage_gate, "file_findings_at_cap", lambda *a, **k: ["x-cap01"]
        )
        state, _refusal, _head, note = _coverage_gate.coverage_verdict(
            42, str(tmp_path), recompute=False
        )
        seen[n] = (state, note)

    if max_rounds - 1 >= 1:
        assert seen[max_rounds - 1][0] == _coverage_gate.REFUSED, (
            f"one short of {max_rounds} must still be under the cap: "
            f"{seen[max_rounds - 1]}"
        )
    assert seen[max_rounds][0] == _coverage_gate.COVERED, (
        f"round {max_rounds} of {max_rounds} is the last the budget funds: "
        f"{seen[max_rounds]}"
    )
    assert f"({max_rounds}/{max_rounds}" in seen[max_rounds][1], seen[max_rounds][1]
    assert seen[max_rounds + 1][0] == _coverage_gate.COVERED, seen[max_rounds + 1]


def test_ac7_hp_under_the_cap_refuses_and_says_how_many_remain(
    monkeypatch, tmp_path, capsys
):
    """The same chain UNDER the cap: exit 3 with the budget named, not 5.

    One round of a two-round maximum. This used to seed two rounds and still
    expect a refusal, which only held because `max_rounds` was compared with
    `>` and so meant three. Two means two, so the last fundable round here is
    the next one."""
    _specimen_gates(monkeypatch)
    _ac7_seed(tmp_path, rounds=1)
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    cap = capsys.readouterr()
    line = (cap.err.strip().splitlines() or [""])[0]
    assert rc == _coverage_gate.REFUSED == 3
    assert "1/2 review rounds used" in line
    assert "the next round is the last the budget funds" in line


def test_ac7_impossible_refuses_the_merge_with_its_own_name(
    enabled, live_head, monkeypatch, tmp_path, capsys  # noqa: F811
):
    """`fno do pr merge` on an IMPOSSIBLE row: refused, and the receipt says
    'coverage impossible', not 'unreviewed' - the two prescribe opposite
    next actions."""
    _specimen_gates(monkeypatch)
    _ac7_seed(tmp_path, rounds=3)
    fake = FakeRun(toplevel=str(tmp_path))
    monkeypatch_run = pytest.MonkeyPatch()
    monkeypatch_run.setattr(_merge, "run", fake)
    try:
        assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 2
        reason = _last_json(capsys, stream="err")["reason"]
    finally:
        monkeypatch_run.undo()
    assert reason.startswith("merge refused, coverage impossible: ")
    assert "impossible" in reason


def test_ac7_status_names_its_own_blocker(monkeypatch, tmp_path):
    """`fno do pr status` renders the IMPOSSIBLE chain as
    review_coverage_impossible, distinct from review_coverage_uncovered.

    The conjunct is re-derived from the PR's own chain through cap_verdict,
    never read off a stored flag: the row the status read returns carries
    no `impossible` key at all on the recompute path, and reading that
    absence as an acquittal is the defect this test guards against. It
    still never reads the raw budget: under the operator's round-cap ruling
    an exhausted budget alone MERGES (the remainder is filed), so a blocker
    named off `rounds_exhausted` would hold every capped PR the law says
    should land."""
    from fno.pr import _status

    _seed_cap_chain(tmp_path, _cap_chain(6))
    blockers = _status._ready_blockers(
        True,
        "green",
        0,
        {"coverage": "uncovered", "reviewed_count": 0, "head_sha": f"{5:040x}"},
        review_lane=True,
        head=f"{5:040x}",
        head_branch="feature/x-cap",
        code_review_required=False,
        repo=str(tmp_path),
    )
    assert "review_coverage_impossible" in blockers
    assert "review_coverage_uncovered" not in blockers

    # The demotion, pinned: a spent budget whose findings are SOFT is not a
    # blocker of its own - those findings are filed and the PR merges.
    _seed_cap_chain(tmp_path, _cap_chain(6, category="nit"))
    soft = _status._ready_blockers(
        True,
        "green",
        0,
        {"coverage": "uncovered", "reviewed_count": 0, "head_sha": f"{5:040x}"},
        review_lane=True,
        head=f"{5:040x}",
        head_branch="feature/x-cap",
        code_review_required=False,
        repo=str(tmp_path),
    )
    assert "review_coverage_impossible" not in soft


def test_ac7_exhausted_rounds_with_no_blocking_findings_stay_covered(
    monkeypatch, tmp_path
):
    """The budget alone never fires IMPOSSIBLE: rounds exhausted with every
    finding non-blocking by class is a COVERED answer (the loop was noisy,
    not stuck). Pins the conjunct the MARKER test needs to be load-bearing."""
    _specimen_gates(monkeypatch)
    stamps = ["2026-08-25T21:00:00Z", "2026-08-25T21:30:00Z", "2026-08-25T22:00:00Z"]
    heads = [
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
        FIXTURE_HEAD,
    ]
    nit_round = {
        "reviewer": "code-review",
        "verdict": "fail",
        "session_id": "s-ac7",
        "branch": "feature/x-8439",
        "reviewed_base_sha": "a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3",
        "reviewed_file_count": 3,
        "reviewed_line_count": 40,
        "findings_blocking": 0,
        "findings": [
            {
                "category": "nit",
                "verdict": None,
                "blocking": False,
                "has_required_fields": True,
                "finding_key": "a.py:1:nit",
            }
        ],
    }
    (tmp_path / ".fno").mkdir(exist_ok=True)
    with open(tmp_path / ".fno" / "events.jsonl", "w", encoding="utf-8") as fh:
        for i in range(3):
            data = dict(nit_round, head_sha=heads[i], reviewed_head_sha=heads[i])
            fh.write(
                json.dumps(
                    {"ts": stamps[i], "type": "review_attestation", "source": "hook", "data": data}
                )
                + "\n"
            )
        fh.write(
            json.dumps(
                {
                    "ts": "2026-08-25T22:01:00Z",
                    "type": "review_coverage",
                    "source": "hook",
                    "data": {
                        "pr": 42,
                        "coverage": "covered",
                        "review_state": "reviewed",
                        "reviewed_count": 1,
                        "head_sha": FIXTURE_HEAD,
                        "verdicts": [
                            {
                                "producer": "local_attestation",
                                "name": "code-review",
                                "verdict": "reviewed",
                                "reviewed_sha": FIXTURE_HEAD,
                                "freshness": "fresh",
                            }
                        ],
                    },
                }
            )
            + "\n"
        )
    state, refusal, _covered_head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED
    assert refusal == ""


# ---- the round cap under operator law: file the rest, keep the hard ----
#
# The operator's ruling: at the cap the PR MERGES with its remaining findings
# FILED as nodes, never dropped. The exception is a CONFIRMED correctness or
# security finding, which keeps IMPOSSIBLE and the human lever. One review
# stays the floor, so an unreviewed PR is still uncovered.


def _soft_round(ts: str, head: str, dispositions=None):
    """A fail round whose finding is blocking but NOT hard (unconfirmed)."""
    data = {
        "reviewer": "code-review",
        "head_sha": head,
        "verdict": "fail",
        "session_id": "s-cap",
        "attester_session_id": "sess-author",
        "branch": "feature/x-8439",
        "reviewed_base_sha": "a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3",
        "reviewed_head_sha": head,
        "reviewed_file_count": 3,
        "reviewed_line_count": 40,
        "findings_blocking": 1,
        "findings": [
            {
                "category": "correctness",
                "verdict": None,
                "blocking": True,
                "has_required_fields": True,
                "finding_key": "cli/src/fno/pr/_merge.py:77:correctness",
            }
        ],
    }
    if dispositions is not None:
        data["dispositions"] = dispositions
    return {"ts": ts, "type": "review_attestation", "source": "hook", "data": data}


def _seed_soft_cap(tmp_path, rounds=3):
    # Generated, not a fixed list: the boundary test walks max_rounds up to 5,
    # and a three-element table silently IndexErrors past three rather than
    # saying the fixture ran out. The LAST round always lands on FIXTURE_HEAD,
    # because the gate reads the newest round at the PR head.
    stamps = [f"2026-08-25T{21 + (i // 2):02d}:{(i % 2) * 30:02d}:00Z" for i in range(rounds)]
    heads = [f"{i + 1}" * 40 for i in range(rounds - 1)] + [FIXTURE_HEAD]
    (tmp_path / ".fno").mkdir(exist_ok=True)
    with open(tmp_path / ".fno" / "events.jsonl", "w", encoding="utf-8") as fh:
        for i in range(rounds):
            fh.write(json.dumps(_soft_round(stamps[i], heads[i])) + "\n")
        fh.write(
            json.dumps(
                {
                    "ts": "2026-08-25T22:01:00Z",
                    "type": "review_coverage",
                    "source": "hook",
                    "data": {
                        "pr": 42,
                        "coverage": "covered",
                        "review_state": "reviewed",
                        "reviewed_count": 1,
                        "self_attested_count": 1,
                        "head_sha": FIXTURE_HEAD,
                        "verdicts": [
                            {
                                "producer": "local_attestation",
                                "name": "code-review",
                                "verdict": "reviewed",
                                "attestation_origin": "self_attested",
                                "reviewed_sha": FIXTURE_HEAD,
                                "freshness": "fresh",
                            }
                        ],
                    },
                }
            )
            + "\n"
        )


def test_cap_files_the_soft_remainder_and_covers(monkeypatch, tmp_path):
    """Operator law: at the cap a non-hard finding is FILED and the PR merges.
    The note names the finding and the node it landed in, so nothing is
    dropped silently."""
    _specimen_gates(monkeypatch)
    _seed_soft_cap(tmp_path, rounds=3)
    calls = []

    def fake_file(keys, pr_number, repo):
        calls.append((tuple(keys), pr_number))
        return ["x-9f01" for _ in keys]

    monkeypatch.setattr(_coverage_gate, "file_findings_at_cap", fake_file)
    state, refusal, _head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED, refusal
    assert refusal == ""
    assert calls == [(("cli/src/fno/pr/_merge.py:77:correctness",), 42)]
    assert "filed at the round cap (3/2)" in note
    assert "cli/src/fno/pr/_merge.py:77:correctness -> x-9f01" in note


def test_cap_keeps_impossible_for_a_confirmed_security_finding(monkeypatch, tmp_path):
    """The exception the law names: a CONFIRMED security finding still refuses
    with IMPOSSIBLE and the human lever, and is never filed away."""
    _specimen_gates(monkeypatch)
    _ac7_seed(tmp_path, rounds=3)
    monkeypatch.setattr(
        _coverage_gate,
        "file_findings_at_cap",
        lambda *a, **k: pytest.fail("a hard finding must never be filed at the cap"),
    )
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.IMPOSSIBLE
    assert "hooks/git-protection.py:302:security" in refusal
    assert "non-author GitHub approval" in refusal


def test_cap_refuses_when_filing_fails(monkeypatch, tmp_path):
    """A finding the gate cannot file is one it must not wave through."""
    _specimen_gates(monkeypatch)
    _seed_soft_cap(tmp_path, rounds=3)

    def boom(keys, pr_number, repo):
        raise RuntimeError("backlog unavailable")

    monkeypatch.setattr(_coverage_gate, "file_findings_at_cap", boom)
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "nothing was waived" in refusal
    assert "backlog unavailable" in refusal


def test_under_the_cap_a_soft_finding_still_blocks(monkeypatch, tmp_path):
    """One review is the floor and the budget still bites: UNDER the cap the
    same soft finding REFUSES rather than being filed.

    One round of a two-round maximum. Seeded at two this passed only under the
    old `>` comparison, where a cap of 2 permitted a third round."""
    _specimen_gates(monkeypatch)
    _seed_soft_cap(tmp_path, rounds=1)
    monkeypatch.setattr(
        _coverage_gate,
        "file_findings_at_cap",
        lambda *a, **k: pytest.fail("nothing is filed under the budget"),
    )
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "cli/src/fno/pr/_merge.py:77:correctness" in refusal


def test_file_findings_at_cap_is_idempotent_on_the_finding_key(monkeypatch, tmp_path):
    """A re-run of the gate must not mint a second node for one finding."""
    from fno.pr._proc import Result

    seen = []

    def fake_run(cmd, **kwargs):
        seen.append(cmd)
        if cmd[:3] == ["fno", "backlog", "find"]:
            return Result(0, "x-77aa  review finding filed at round cap: a.py:1:correctness\n", "")
        raise AssertionError(f"unexpected shell call: {cmd}")

    monkeypatch.setattr("fno.pr._proc.run", fake_run)
    ids = _coverage_gate.file_findings_at_cap(
        ["a.py:1:correctness"], 42, str(tmp_path)
    )
    assert ids == ["x-77aa"]
    assert not any(c[:3] == ["fno", "backlog", "idea"] for c in seen)


# ---- the cap filing reaches the REAL argument parser ----
#
# The test above is the shape that let the defect ship: a mocked `run` accepts
# any argv, so it cannot see a flag the parser rejects. `fno backlog idea`
# requires --difficulty on a non-interactive filing, this call has no tty, and
# without the flag every filing raised - which made the cap's designed merge
# exit (file the remainder, then merge) unreachable on every capped PR.
#
# So the guard below EXECUTES the CLI out of process rather than mocking it.
# It is hermetic: HOME points at a tmp dir, so the graph the filing writes is
# the tmp graph and never ~/.fno/graph.json.


def _real_cli_runner(home, repo):
    """A ``_proc.run`` stand-in that executes the real fno CLI, hermetically.

    Only the TRANSPORT is substituted (a PATH lookup for `fno` becomes an
    interpreter running the same entry point). The argument list still reaches
    the same parser the shipped binary uses, which is the whole point: that
    parser is the thing a mock cannot represent.
    """
    import os
    import subprocess
    import sys

    import fno

    src_root = str(Path(fno.__file__).resolve().parent.parent)

    def run(cmd, **kwargs):
        assert cmd and cmd[0] == "fno", f"unexpected binary: {cmd[:1]}"
        env = dict(os.environ)
        env["HOME"] = str(home)
        env["PYTHONPATH"] = os.pathsep.join(
            [src_root, *([env["PYTHONPATH"]] if env.get("PYTHONPATH") else [])]
        )
        # A stale FNO_REPO_ROOT inherited from the running session would pin
        # config resolution back at the real checkout and defeat the isolation.
        env.pop("FNO_REPO_ROOT", None)
        proc = subprocess.run(
            [sys.executable, "-c", "from fno.cli import app; app()", *cmd[1:]],
            cwd=kwargs.get("cwd") or str(repo),
            env=env,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=300,
            check=False,
        )
        return Result(proc.returncode, proc.stdout, proc.stderr)

    return run


def _tmp_graph_titles(home):
    """Every node title in the throwaway graph the filing wrote."""
    graph = Path(home) / ".fno" / "graph.json"
    if not graph.is_file():
        return []
    entries = json.loads(graph.read_text()).get("entries") or []
    return [n.get("title", "") for n in entries if isinstance(n, dict)]


def test_file_findings_at_cap_files_through_the_real_cli(monkeypatch, tmp_path):
    """The filing succeeds against the real `fno backlog idea` parser.

    Asserts the node id AND the node's presence in the graph by title. A test
    that only asserted "no exception" would pass on a run that filed nothing.
    """
    home = tmp_path / "home"
    repo = tmp_path / "repo"
    (home / ".fno").mkdir(parents=True)
    repo.mkdir()
    import subprocess as _sp

    _sp.run(["git", "init", "-q"], cwd=str(repo), check=True)

    monkeypatch.setattr("fno.pr._proc.run", _real_cli_runner(home, repo))
    key = "cli/src/fno/pr/_coverage_gate.py:1057:correctness"
    ids = _coverage_gate.file_findings_at_cap([key], 42, str(repo))

    assert len(ids) == 1
    assert re.fullmatch(r"[a-z]+-[0-9a-f]{4,}", ids[0]), ids
    assert f"review finding filed at round cap: {key}" in _tmp_graph_titles(home)


def test_file_findings_at_cap_mints_once_through_the_real_cli(monkeypatch, tmp_path):
    """The idempotency branch holds against the real find/idea round trip.

    This branch is what unstuck a capped PR by hand: a worker pre-filed the
    finding with the exact cap title and the gate reused it. Asserting the
    SAME id back plus exactly one node with that title is the positive marker;
    asserting "no second call happened" would pass on a run that filed nothing
    at all.
    """
    home = tmp_path / "home"
    repo = tmp_path / "repo"
    (home / ".fno").mkdir(parents=True)
    repo.mkdir()
    import subprocess as _sp

    _sp.run(["git", "init", "-q"], cwd=str(repo), check=True)

    monkeypatch.setattr("fno.pr._proc.run", _real_cli_runner(home, repo))
    key = "cli/src/fno/pr/_reviews.py:430:correctness"
    first = _coverage_gate.file_findings_at_cap([key], 42, str(repo))
    second = _coverage_gate.file_findings_at_cap([key], 42, str(repo))

    assert first == second, (first, second)
    title = f"review finding filed at round cap: {key}"
    assert _tmp_graph_titles(home).count(title) == 1


def test_two_findings_at_the_cap_both_mint_through_the_real_cli(
    monkeypatch, tmp_path
):
    """The fold gate must never swallow the second finding.

    `--difficulty` is what OPENS the pre-mint fold gate, so the flag that made
    filing work also created this trap. Every cap filing shares a title prefix,
    which makes the first node a fold candidate for the second. Non-interactive,
    the offer prints and the command exits 0 having minted NOTHING - and the
    caller then scrapes an id out of the printed wave command and reports a
    finding it never filed.

    Asserts TWO DISTINCT ids and two nodes in the graph. Asserting the call
    succeeded passes on exactly the silent loss this guards.
    """
    home = tmp_path / "home"
    repo = tmp_path / "repo"
    (home / ".fno").mkdir(parents=True)
    repo.mkdir()
    import subprocess as _sp

    _sp.run(["git", "init", "-q"], cwd=str(repo), check=True)

    monkeypatch.setattr("fno.pr._proc.run", _real_cli_runner(home, repo))
    keys = [
        "cli/src/fno/pr/_coverage_gate.py:1057:correctness",
        "cli/src/fno/pr/_coverage_gate.py:1099:correctness",
    ]
    ids = _coverage_gate.file_findings_at_cap(keys, 42, str(repo))

    assert len(ids) == 2, ids
    assert len(set(ids)) == 2, f"the two findings folded into one node: {ids}"
    titles = _tmp_graph_titles(home)
    for key in keys:
        assert f"review finding filed at round cap: {key}" in titles, key


# ---- x-aecc: declining must satisfy coverage ----
#
# The pass condition is ANSWERED at this head, never clean at this head. The
# branch's ONLY attestation is a fail whose findings are all terminally
# dispositioned, and the row the Rust producer emits for that chain now reads
# covered/reviewed (its answered-fail local verdict). A pass-chain covering
# proves nothing here - the tests above already pin that arm - so every
# fixture's only attestation is a fail.


def _xaecc_fail_round(findings_keys, dispositioned_keys):
    """One fail round at FIXTURE_HEAD; `dispositioned_keys` get declines."""
    return {
        "ts": "2026-08-26T19:00:00Z",
        "type": "review_attestation",
        "source": "hook",
        "data": {
            "reviewer": "code-review",
            "head_sha": FIXTURE_HEAD,
            "verdict": "fail",
            "session_id": "s-xaecc",
            "attester_session_id": "sess-peer",
            "branch": "feature/x-8439",
            "reviewed_base_sha": "17a3b85b1a70a22014f1fc4e04b7aa35a632757f",
            "reviewed_head_sha": FIXTURE_HEAD,
            "reviewed_file_count": 3,
            "reviewed_line_count": 40,
            "findings_blocking": len(findings_keys),
            "findings": [
                {
                    "category": "correctness",
                    "verdict": None,
                    "blocking": True,
                    "has_required_fields": True,
                    "finding_key": k,
                }
                for k in findings_keys
            ],
            "dispositions": [
                {
                    "finding_key": k,
                    "disposition": "declined",
                    "reason": "not worth the churn",
                }
                for k in dispositioned_keys
            ],
        },
    }


def _xaecc_row(covered: bool):
    """The row the Rust producer emits: covered once the fail is answered,
    uncovered while a finding keeps it non-terminal."""
    verdicts = (
        [
            {
                "producer": "local_attestation",
                "name": "code-review",
                "verdict": "reviewed",
                "attestation_origin": "other_session",
                "reviewed_sha": FIXTURE_HEAD,
                "freshness": "fresh",
            }
        ]
        if covered
        else []
    )
    return {
        "ts": "2026-08-26T19:01:00Z",
        "type": "review_coverage",
        "source": "hook",
        "data": {
            "pr": 42,
            "coverage": "covered" if covered else "uncovered",
            "review_state": "reviewed" if covered else "unreviewed",
            "reviewed_count": 1 if covered else 0,
            "head_sha": FIXTURE_HEAD,
            "verdicts": verdicts,
        },
    }


def _xaecc_seed(tmp_path, attestations, row):
    if not isinstance(attestations, list):
        attestations = [attestations]
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text(
        "\n".join(json.dumps(e) for e in (*attestations, row)) + "\n",
        encoding="utf-8",
    )


def test_xaecc_marker1_fail_only_chain_fully_dispositioned_covers(monkeypatch, tmp_path):
    """MARKER 1: the only attestation is a fail, every finding declined with a
    reason on a corroborated row - literal COVERED, refusal empty."""
    _specimen_gates(monkeypatch)
    _xaecc_seed(
        tmp_path,
        _xaecc_fail_round(
            ["a.py:1:correctness", "b.py:2:correctness"],
            ["a.py:1:correctness", "b.py:2:correctness"],
        ),
        _xaecc_row(covered=True),
    )
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED, refusal
    assert refusal == ""
    # MARKER 3: the covered answer pins the SAME head the declining round
    # attested - no commit and no further review between declining and merge.
    assert covered_head == FIXTURE_HEAD


def test_xaecc_marker2_one_nonterminal_finding_refuses_and_names_it(monkeypatch, tmp_path):
    """MARKER 2: the same branch with ONE finding left non-terminal refuses,
    and the refusal NAMES that finding - never the generic '0 reviewed'
    sentence that taught the loop to re-review."""
    _specimen_gates(monkeypatch)
    _xaecc_seed(
        tmp_path,
        _xaecc_fail_round(
            ["a.py:1:correctness", "b.py:2:correctness"],
            ["a.py:1:correctness"],
        ),
        _xaecc_row(covered=False),
    )
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "b.py:2:correctness" in refusal, "the refusal names the open finding"
    assert "not terminal" in refusal
    assert "0 reviewed" not in refusal, "the generic text hid the finding"
    assert "1/2 review rounds used" in note


def test_xaecc_cap_files_the_remainder_and_covers_at_the_same_head(monkeypatch, tmp_path):
    """Filed at the cap is terminal: three fail rounds, one soft open finding,
    the row the producer emits reads covered - the gate files the finding and
    answers COVERED pinning the head the last fail attested."""
    _specimen_gates(monkeypatch)
    stamps = ["2026-08-26T18:00:00Z", "2026-08-26T18:30:00Z", "2026-08-26T19:00:00Z"]
    heads = [
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
        FIXTURE_HEAD,
    ]
    rounds = []
    for ts, head in zip(stamps, heads):
        r = _xaecc_fail_round(["a.py:1:correctness"], [])
        r["ts"] = ts
        r["data"]["head_sha"] = head
        r["data"]["reviewed_head_sha"] = head
        rounds.append(r)
    _xaecc_seed(tmp_path, rounds, _xaecc_row(covered=True))
    calls = []

    def fake_file(keys, pr_number, repo):
        calls.append(tuple(keys))
        return ["x-ae01" for _ in keys]

    monkeypatch.setattr(_coverage_gate, "file_findings_at_cap", fake_file)
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED, refusal
    assert calls == [("a.py:1:correctness",)]
    assert "filed at the round cap (3/2)" in note


def test_xaecc_r3_cap_filed_but_no_local_pass_is_waived_and_named(monkeypatch, tmp_path):
    """The cap arm files the findings and the row is uncovered on the
    no_local_pass conjunct: config REQUIRES a code-review attestation and only
    a peer ever attested.

    This case used to keep a sized refusal. It cannot: past the cap that
    conjunct is unsatisfiable by construction, because the only way to satisfy
    it is a code-review round the spent budget will not fund. That is the
    unpassable-guard shape the round cap exists to end, and the operator's
    ruling is two rounds maximum whatever the shape.

    So the budget discharges - and the FACT is not lost. The waived conjunct
    is named in the receipt beside the filed node, so a merge that happened
    without the required reviewer's attestation stays legible afterward."""
    _specimen_gates(monkeypatch)
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: True)
    stamps = ["2026-08-26T18:00:00Z", "2026-08-26T18:30:00Z", "2026-08-26T19:00:00Z"]
    heads = [
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
        FIXTURE_HEAD,
    ]
    rounds = []
    for ts, head in zip(stamps, heads):
        r = _xaecc_fail_round(["a.py:1:correctness"], [])
        r["ts"] = ts
        r["data"]["head_sha"] = head
        r["data"]["reviewed_head_sha"] = head
        # The reviewer label is a peer, not code-review: the row carries no
        # code-review local pass, which is the conjunct that must keep its
        # own remedy.
        r["data"]["reviewer"] = "peer"
        rounds.append(r)
    _xaecc_seed(tmp_path, rounds, _xaecc_row(covered=False))
    monkeypatch.setattr(
        _coverage_gate, "file_findings_at_cap", lambda keys, pr_number, repo: ["x-ae02"]
    )
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED, f"spent budget must discharge: {refusal}"
    assert not refusal, f"a discharged budget carries no refusal: {refusal}"
    assert covered_head, "a covered verdict must name the head it covers"
    # The filed node still rides the note - unchanged from the refusal this
    # replaces, and the reason the discharge is safe.
    assert "filed at the round cap (3/2)" in note, "the filed node rides the note"
    # And the waiver is explicit about WHAT it waived, so the missing
    # required-reviewer attestation is a recorded fact rather than a silence.
    assert "review budget discharged (3/2 rounds)" in note, note
    assert "waived at the cap: uncovered" in note, note
    # The specific fact their sized refusal carried: config demanded a
    # code-review attestation and none exists. Waived, but on the record.
    assert "code-review attestation required by config" in note, note
# --- rounds the attestation chain never saw: the GitHub review axis ---
# The Python half of the shared corpus. The Rust half lives in
# crates/fno-agents/tests/coverage_tiling.rs under the same case names, and
# the two must answer identically: the counter gates the merge on this side
# and the stop hook on that one.

_CONNECTOR = "chatgpt-codex-connector[bot]"
_PR_AUTHOR = "bllshttng"


def _review_object(login, state, commit, submitted_at):
    """One review object in the shape both gates read (gh pr view / REST,
    normalized): author.login, state, commit.oid, submittedAt."""
    return {
        "author": {"login": login},
        "state": state,
        "commit": {"oid": commit},
        "submittedAt": submitted_at,
    }


def test_round_budget_counts_rounds_that_only_github_review_objects_saw():
    """The connector lane: three review rounds, every one ended with
    findings, NO attestation exists anywhere. Each fix moved the head and
    the connector reviewed the new head, so the rounds exist only as three
    distinct reviewed commits. The chain alone answers 0 and the cap cannot
    fire; with the payload it must answer 3."""
    chain: list[dict] = []
    reviews = [
        _review_object(_CONNECTOR, "COMMENTED", "c1", "2026-08-26T11:00:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c2", "2026-08-26T13:00:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c3", "2026-08-26T15:00:00Z"),
    ]
    assert _coverage_gate.rounds_since_last_pass(chain, reviews=reviews) == 3


def test_round_budget_counts_every_verdict_across_the_pr_life():
    """The operator's ruling (x-2219): max_rounds is a per-PR TOTAL. A pass
    is a round like any verdict and refunds nothing - four attested rounds
    are four, not the reset answer 1."""
    chain = [
        {"verdict": "fail", "ts": "2026-08-26T10:00:00Z"},
        {"verdict": "fail", "ts": "2026-08-26T11:00:00Z"},
        {"verdict": "pass", "ts": "2026-08-26T12:00:00Z"},
        {"verdict": "fail", "ts": "2026-08-26T13:00:00Z"},
    ]
    assert _coverage_gate.rounds_since_last_pass(chain) == 4
    # Drop the pass from the chain: still three rounds.
    no_pass = [chain[0], chain[1], {"verdict": "fail", "ts": "2026-08-26T12:00:00Z"}]
    assert _coverage_gate.rounds_since_last_pass(no_pass) == 3


def test_round_budget_pass_no_longer_refunds_the_budget():
    """AC6: the chain pass, round, round is THREE rounds. The pass used to
    zero the counter; a self-signed pass refreshed the whole budget on
    demand (measured on PR #1225: one emit, 0/2 read back, three more
    rounds ran)."""
    chain = [
        {"verdict": "pass", "ts": "2026-08-26T10:00:00Z"},
        {"verdict": "fail", "ts": "2026-08-26T11:00:00Z"},
        {"verdict": "fail", "ts": "2026-08-26T12:00:00Z"},
    ]
    assert (
        _coverage_gate.rounds_since_last_pass(chain) == 3
    ), "pass, round, round is three rounds: the pass counts and refunds nothing"


def test_round_budget_pass_does_not_truncate_the_github_axis():
    """A pass at 12:00 no longer truncates the reviews axis: the connector
    round at 11:00 was a real round and stays counted. Two attested rounds
    plus three reviewed commits answer 3 (the max of the axes), never the
    refund answer 2."""
    chain = [
        {"verdict": "fail", "ts": "2026-08-26T10:00:00Z"},
        {"verdict": "pass", "ts": "2026-08-26T12:00:00Z"},
    ]
    reviews = [
        _review_object(_CONNECTOR, "COMMENTED", "c1", "2026-08-26T11:00:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c2", "2026-08-26T13:00:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c3", "2026-08-26T15:00:00Z"),
    ]
    assert _coverage_gate.rounds_since_last_pass(chain, reviews=reviews) == 3


def test_rounds_submitted_before_the_pass_still_count_on_the_reviews_axis():
    """AC7: the no-refund ruling on the reviews axis alone. The pass
    carries a readable ts, and every review object predates it - the old
    last_pass_ts filter dropped all three and answered 0, a fixture that
    failed SEPARATELY from the events-axis reset. The answer is 3."""
    chain = [{"verdict": "pass", "ts": "2026-08-26T12:00:00Z"}]
    reviews = [
        _review_object(_CONNECTOR, "COMMENTED", "c1", "2026-08-26T09:00:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c2", "2026-08-26T09:30:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c3", "2026-08-26T09:45:00Z"),
    ]
    assert _coverage_gate.rounds_since_last_pass(chain, reviews=reviews) == 3


def test_round_budget_pass_without_a_ts_still_counts_the_github_axis():
    """The pass_ts_unreadable guard is gone with the reset it served: a
    pass with no readable ts leaves the reviews axis whole. Fail plus a
    ts-less pass is two attested rounds, three reviewed commits - 3."""
    chain = [
        {"verdict": "fail", "ts": "2026-08-26T10:00:00Z"},
        {"verdict": "pass"},
    ]
    reviews = [
        _review_object(_CONNECTOR, "COMMENTED", "c1", "2026-08-26T09:00:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c2", "2026-08-26T09:30:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c3", "2026-08-26T09:45:00Z"),
    ]
    assert _coverage_gate.rounds_since_last_pass(chain, reviews=reviews) == 3


def test_round_budget_counts_review_objects_posted_under_the_pr_author_login():
    """The measured specimen: the codex cloud connector posts its review
    objects under the PR AUTHOR's own login - 116 of 117 objects on the
    branch that spun, one burst per reviewed commit. An author filter
    deletes the round trace on exactly that lane, so there is none: three
    bursts at three distinct commits under the author login are three
    rounds, and reply volume inside one burst is one round."""
    reviews = [
        _review_object(_PR_AUTHOR, "COMMENTED", "c1", "2026-08-26T11:00:00Z"),
        _review_object(_PR_AUTHOR, "COMMENTED", "c1", "2026-08-26T11:05:00Z"),
        _review_object(_PR_AUTHOR, "COMMENTED", "c2", "2026-08-26T12:00:00Z"),
        _review_object(_PR_AUTHOR, "COMMENTED", "c3", "2026-08-26T13:00:00Z"),
    ]
    assert _coverage_gate.rounds_since_last_pass([], reviews=reviews) == 3


def test_round_budget_takes_the_max_not_the_sum_of_both_axes():
    """A healthy lane leaves BOTH traces per round: a fail attestation and a
    connector review of the same head. Two rounds, not four."""
    chain = [{"verdict": "fail", "ts": "2026-08-26T10:00:00Z"}, {"verdict": "fail"}]
    reviews = [
        _review_object(_CONNECTOR, "COMMENTED", "c2", "2026-08-26T11:00:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c3", "2026-08-26T12:00:00Z"),
    ]
    assert _coverage_gate.rounds_since_last_pass(chain, reviews=reviews) == 2


def test_round_budget_no_reviews_evidence_keeps_the_events_only_answer():
    """No payload (the read failed, or the caller had none): behavior is
    exactly the events-only answer."""
    chain = [{"verdict": "fail"}, {"verdict": "fail"}]
    assert _coverage_gate.rounds_since_last_pass(chain) == 2
    assert _coverage_gate.rounds_since_last_pass(chain, reviews=None) == 2


def test_round_budget_naive_pass_timestamp_never_crashes_the_gate():
    """A pass row whose ts carries no offset, set against Z-suffixed review
    submittedAt values, used to raise TypeError out of rounds_since_last_pass
    and crash the whole coverage verdict. The x-2219 no-refund ruling removed
    the comparison entirely - no timestamp is read on either axis - so a
    naive ts is inert: the pass is one round, the two distinct reviewed
    commits are the other axis, and the gate answers 2 rather than
    raising."""
    chain = [{"verdict": "pass", "ts": "2026-08-26T12:00:00"}]
    reviews = [
        _review_object(_CONNECTOR, "COMMENTED", "c1", "2026-08-26T13:00:00Z"),
        _review_object(_CONNECTOR, "COMMENTED", "c2", "2026-08-26T15:00:00Z"),
    ]
    assert _coverage_gate.rounds_since_last_pass(chain, reviews=reviews) == 2


def test_attestation_chain_dedupes_the_global_mirror_on_invocation_id(
    monkeypatch, tmp_path
):
    """One attestation, two stores, ONE round.

    review_attestation rides GLOBAL_MIRROR_TYPES, and the mirror stamps
    `repo` onto its copy alone, so the project row and the global row carry
    different payloads. Dedup keyed on the whole payload therefore admitted
    both rows and the chain counted every mirrored attestation twice - which
    the per-PR-total budget turns into "one review reads 2/2" and fires the
    cap after a single round. The invocation_id is minted once by the
    producer and lands identically on both rows; it is the dedup key. The
    payload fallback keeps pre-invocation_id rows deduped when the two
    stores genuinely agree."""
    from fno.pr import _reviews

    head = "46695fffd00000000000000000000000000000000"
    base_data = {
        "reviewer": "code-review",
        "head_sha": head,
        "verdict": "fail",
        "session_id": "s-1",
        "attester_session_id": "s-1",
        "branch": "feature/x-8439",
        "reviewed_base_sha": "a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3",
        "reviewed_head_sha": head,
        "reviewed_file_count": 3,
        "reviewed_line_count": 40,
        "invocation_id": "ri-1",
    }

    def _row(data):
        return json.dumps(
            {"ts": "2026-08-26T10:00:00Z", "type": "review_attestation", "data": data}
        ) + "\n"

    project = tmp_path / "project-events.jsonl"
    project.write_text(_row(base_data), encoding="utf-8")
    global_log = tmp_path / "global-events.jsonl"
    global_log.write_text(
        _row({**base_data, "repo": "bllshttng/footnote"}), encoding="utf-8"
    )
    monkeypatch.setattr(
        _reviews,
        "_coverage_logs",
        lambda cwd, project_events: (project, global_log, "bllshttng/footnote"),
    )
    chain = _coverage_gate.attestation_chain(
        str(tmp_path), head_branch="feature/x-8439", head=head
    )
    assert len(chain) == 1, "the mirrored copy must dedup, not double-count"
    assert _coverage_gate.rounds_since_last_pass(chain) == 1

    # Fallback: rows with no invocation_id dedup only when the payloads
    # really are identical (the pre-field shape).
    legacy = {k: v for k, v in base_data.items() if k != "invocation_id"}
    project.write_text(_row(legacy), encoding="utf-8")
    global_log.write_text(_row(legacy), encoding="utf-8")
    chain = _coverage_gate.attestation_chain(
        str(tmp_path), head_branch="feature/x-8439", head=head
    )
    assert len(chain) == 1, "identical legacy payloads still dedup"

    # Negative control: two distinct invocations stay two chain entries, so
    # the dedup key is not so wide it collapses real attestations.
    project.write_text(_row(base_data), encoding="utf-8")
    second = {**base_data, "invocation_id": "ri-2"}
    global_log.write_text(
        _row({**base_data, "repo": "bllshttng/footnote"})
        + _row({**second, "repo": "bllshttng/footnote"}),
        encoding="utf-8",
    )
    chain = _coverage_gate.attestation_chain(
        str(tmp_path), head_branch="feature/x-8439", head=head
    )
    assert len(chain) == 2, "two distinct invocations are two chain entries"
    # Both entries pin the SAME head, so they are ONE round: the unit is a
    # reviewed head, not a verdict row. The dedup key and the round unit are
    # orthogonal, which is exactly what this pair of assertions pins.
    assert _coverage_gate.rounds_since_last_pass(chain) == 1


def test_attestation_chain_keeps_same_invocation_reattests_at_new_heads(
    monkeypatch, tmp_path
):
    """One invocation, one recovery emit per fixed head, EVERY row kept.

    The emit-attestation recovery path re-attests under the SAME invocation
    after each fix lands (the hold mints the id once per request). Keyed on
    the invocation alone, the chain kept the FIRST row - the fail - and
    swallowed every later pass and its dispositions, so a fixed blocking
    finding read as never resolved and the merge refused. The head rides the
    key: mirrors of one row still collapse (same invocation, same head), a
    re-attestation at a new head survives."""
    from fno.pr import _reviews

    head_a = "46695fffa00000000000000000000000000000000"
    head_b = "46695fffb00000000000000000000000000000000"
    finding = {
        "finding_key": "cli/src/fno/agents/retask.py:189:correctness",
        "category": "correctness",
        "verdict": "CONFIRMED",
    }
    fail_row = {
        "reviewer": "code-review",
        "head_sha": head_a,
        "verdict": "fail",
        "session_id": "s-1",
        "attester_session_id": "s-1",
        "branch": "feature/x-1394",
        "invocation_id": "ri-1",
        "findings": [finding],
    }
    pass_row = {
        **fail_row,
        "head_sha": head_b,
        "verdict": "pass",
        "findings": [],
        "dispositions": [
            {
                "finding_key": finding["finding_key"],
                "disposition": "fixed",
                "reason": "fixed in the head this row attests",
            }
        ],
    }

    def _row(ts, data):
        return json.dumps(
            {"ts": ts, "type": "review_attestation", "data": data}
        ) + "\n"

    project = tmp_path / "project-events.jsonl"
    project.write_text(
        _row("2026-09-04T16:26:11Z", fail_row)
        + _row("2026-09-04T16:30:19Z", pass_row),
        encoding="utf-8",
    )
    global_log = tmp_path / "global-events.jsonl"
    global_log.write_text(
        _row("2026-09-04T16:26:11Z", {**fail_row, "repo": "bllshttng/footnote"})
        + _row("2026-09-04T16:30:19Z", {**pass_row, "repo": "bllshttng/footnote"}),
        encoding="utf-8",
    )
    monkeypatch.setattr(
        _reviews,
        "_coverage_logs",
        lambda cwd, project_events: (project, global_log, "bllshttng/footnote"),
    )
    chain = _coverage_gate.attestation_chain(
        str(tmp_path), head_branch="feature/x-1394", head=head_b
    )
    assert [(row["head_sha"], row["verdict"]) for row in chain] == [
        (head_a, "fail"),
        (head_b, "pass"),
    ], "a same-invocation re-attestation at a new head must survive the mirror dedup"
    refusal, _note, nonterminal, _hard = _coverage_gate.disposition_refusal(
        chain, cov=None, cwd=str(tmp_path)
    )
    assert refusal == "", nonterminal


def test_pr_reviews_parses_paginated_rest_and_maps_fields(monkeypatch):
    """The helper rides the shared _rest_pages reader (page-per-call arrays)
    and maps the three fields the counter reads; a failed read answers with
    no payload so the budget keeps its events-only answer."""
    from fno.pr._proc import Result

    # Page one must be a FULL page (100 rows) or _rest_pages stops early.
    first = [
        {
            "user": {"login": "bllshttng"},
            "state": "COMMENTED",
            "submitted_at": "2026-08-26T11:00:00Z",
            "commit_id": "c1",
        },
        {
            "user": {"login": "chatgpt-codex-connector[bot]"},
            "state": "COMMENTED",
            "submitted_at": "2026-08-26T12:00:00Z",
            "commit_id": "c2",
        },
    ] + [
        {
            "user": {"login": "bllshttng"},
            "state": "COMMENTED",
            "submitted_at": "2026-08-26T12:30:00Z",
            "commit_id": f"filler-{i}",
        }
        for i in range(98)
    ]
    pages = [
        first,
        [
            {
                "user": {"login": "chatgpt-codex-connector[bot]"},
                "state": "APPROVED",
                "submitted_at": "2026-08-26T13:00:00Z",
                "commit_id": "c3",
            }
        ],
    ]

    def fake_run(cmd, **kwargs):
        joined = " ".join(str(c) for c in cmd)
        if "pulls/42" in joined:
            page = pages.pop(0)
            return Result(0, json.dumps(page), "")
        raise AssertionError(f"unexpected shell call: {cmd}")

    monkeypatch.setattr("fno.pr._proc.run", fake_run)
    monkeypatch.setattr(
        "fno.pr._rest._repo_slug_reason", lambda cwd, runner=None: ("o/r", "")
    )
    reviews, unread = _coverage_gate._pr_reviews(42, "/repo")
    assert unread == "", unread
    oids = [r["commit"]["oid"] for r in reviews]
    assert oids[:2] == ["c1", "c2"] and oids[-1] == "c3" and len(oids) == 101
    assert reviews[-1]["submittedAt"] == "2026-08-26T13:00:00Z"
    assert "author" not in reviews[0], "no reader consumes author; do not map it"

    # A failed read fails open to the events-only budget, never an exception -
    # and NAMES itself, so the budget can report a zero the reviews axis never
    # contributed to instead of presenting it as a measured count.
    def failing_run(cmd, **kwargs):
        return Result(1, "", "boom")

    monkeypatch.setattr("fno.pr._proc.run", failing_run)
    rows, unread = _coverage_gate._pr_reviews(42, "/repo")
    assert rows is None
    assert unread, "a failed reviews read must name its cause, not answer a bare None"


def test_round_count_equals_the_distinct_reviewed_commits_read_from_a_cwd(
    monkeypatch, tmp_path
):
    """The MEASUREMENT, end to end from a cwd: N distinct reviewed commits
    read off a real directory make the gate's round count exactly N.

    This is the composition the argument confusion broke (x-51f7), so it is
    asserted as one chain rather than as two green units: `_pr_reviews` takes
    a CWD, resolves the slug from that checkout's origin, and its payload
    feeds `rounds_since_last_pass`. Three reviews land on two distinct
    commits and a fourth on a third, so the answer is 3 and not the row count.
    """
    from fno.pr._proc import Result

    page = [
        {"state": "COMMENTED", "submitted_at": "2026-08-26T11:00:00Z", "commit_id": "aaa"},
        {"state": "COMMENTED", "submitted_at": "2026-08-26T11:05:00Z", "commit_id": "aaa"},
        {"state": "CHANGES_REQUESTED", "submitted_at": "2026-08-26T12:00:00Z", "commit_id": "bbb"},
        {"state": "APPROVED", "submitted_at": "2026-08-26T13:00:00Z", "commit_id": "ccc"},
    ]

    def fake_run(cmd, **kwargs):
        joined = " ".join(str(c) for c in cmd)
        if "get-url" in joined:
            return Result(0, "git@github.com:bllshttng/footnote.git\n", "")
        if "pulls/1264" in joined:
            return Result(0, json.dumps(page), "")
        raise AssertionError(f"unexpected shell call: {cmd}")

    monkeypatch.setattr("fno.pr._proc.run", fake_run)
    reviews, unread = _coverage_gate._pr_reviews(1264, str(tmp_path))
    assert unread == "", unread
    assert len(reviews) == 4, reviews
    assert _coverage_gate.rounds_since_last_pass([], reviews=reviews) == 3


def test_a_slug_passed_where_a_cwd_belongs_names_the_missing_directory(
    monkeypatch, tmp_path
):
    """The regression: `_pr_reviews` takes a CWD, and a repo slug handed to it
    must refuse by naming the directory, never by blaming the slug.

    Both are bare `str`, so the wrong argument is invisible at the call site.
    The old fixed sentence "repo slug unreadable" was correct in form and
    wrong in subject: the slug was perfectly readable and the directory was
    what did not exist. It cost a reader three slug spellings (x-51f7). The
    refusal now names the missing directory, and it fires before git is
    spawned, so a wrong argument cannot reach a subprocess at all.
    """
    monkeypatch.chdir(tmp_path)

    def never_runs(cmd, **kwargs):
        raise AssertionError(f"a slug must be refused before any spawn: {cmd}")

    monkeypatch.setattr("fno.pr._proc.run", never_runs)
    rows, unread = _coverage_gate._pr_reviews(1264, "bllshttng/footnote")
    assert rows is None
    assert unread == "no such directory: bllshttng/footnote", unread
    assert "repo slug unreadable" not in unread


def test_past_the_cap_the_spent_budget_discharges_the_obligation(monkeypatch, tmp_path):
    """The Python mirror of the spent-budget receipt: past the cap the
    uncovered refusal must not teach the review verb (that instruction is
    the loop this cap exists to bound) and must name the terminal act. The
    render itself is asserted first so an unrendered refusal cannot pass
    the absence checks."""
    _specimen_gates(monkeypatch)
    _seed_soft_cap(tmp_path, rounds=3)
    # Overwrite the coverage row the seed wrote: this PR is UNCOVERED (the
    # connector lane shape - no verdict ever counted), so after the soft
    # findings are filed the refusal falls through to the spent-budget arm.
    rows = []
    stamps = ["2026-08-25T21:00:00Z", "2026-08-25T21:30:00Z", "2026-08-25T22:00:00Z"]
    heads = [
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
        FIXTURE_HEAD,
    ]
    for i in range(3):
        rows.append(json.dumps(_soft_round(stamps[i], heads[i])))
    rows.append(
        json.dumps(
            {
                "ts": "2026-08-25T22:01:00Z",
                "type": "review_coverage",
                "source": "hook",
                "data": {
                    "pr": 42,
                    "coverage": "uncovered",
                    "review_state": "unreviewed",
                    "reviewed_count": 0,
                    "self_attested_count": 0,
                    "head_sha": FIXTURE_HEAD,
                    "verdicts": [],
                },
            }
        )
    )
    (tmp_path / ".fno" / "events.jsonl").write_text("\n".join(rows) + "\n")
    monkeypatch.setattr(_coverage_gate, "_pr_reviews", lambda *a, **k: (None, ""))
    monkeypatch.setattr(
        _coverage_gate, "file_findings_at_cap", lambda *a, **k: ["x-filed1"]
    )
    state, refusal, head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    # A SPENT budget discharges the review obligation; it does not fail it.
    # `max_rounds = 2` means "this PR gets two rounds, then review is done".
    # The old REFUSED here was a guard nothing could pass: every remedy that
    # could clear it names a review verb, and running one spends a round
    # already spent. That is what produced 12-round PRs against a cap of 2.
    assert state == _coverage_gate.COVERED, f"spent budget must discharge: {refusal}"
    assert not refusal, f"a discharged budget carries no refusal: {refusal}"
    assert head, "a covered verdict must name the head it covers"

    # The waiver is NAMED, never silent: a merge on a spent budget has to be
    # legible afterward, and the note is the only place that record lives.
    assert "review budget discharged (3/2 rounds)" in note, note
    assert "config.review.max_rounds" in note, note

    # file_findings_at_cap ran and created a real node. The remainder being
    # FILED is what makes the discharge safe, so the node id must reach the
    # receipt or the operator files the same finding twice.
    assert "x-filed1" in note, "the filed node must reach the receipt: " + note
    assert "filed at the round cap (3/2)" in note, note

    # Unchanged from the refusal this replaces, and still the point: past the
    # cap the gate must name NO review verb. That instruction is the loop the
    # cap exists to bound.
    for needle in ("/code-review", "/review", "/fno:review", "review verb"):
        assert needle not in note, f"past-cap receipt names {needle}: {note}"


def test_rounds_spent_with_zero_attestations_has_a_permitted_merge_path(
    monkeypatch, tmp_path
):
    """The connector-lane shape, measured on a live PR: the attestation chain
    is EMPTY, so disposition_refusal returns nothing, no finding is named, and
    the cap-file arm never runs. The question that decides whether the PR is
    stranded is not whether that arm works - it is whether the round count is
    honest, because the spent-budget waiver is keyed on `rounds >= max_rounds`
    alone and needs no chain at all.

    A GitHub-App reviewer leaves no attestation row anywhere, so its rounds
    exist only as review objects. With the reviews axis read, two distinct
    reviewed commits are two rounds, the budget is spent, and the waiver
    discharges the obligation. Asserting the filing succeeds proves nothing
    about this shape: the filing arm is never reached here.
    """
    _specimen_gates(monkeypatch)
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    # No review_attestation events at all: chain is empty by construction.
    (tmp_path / ".fno" / "events.jsonl").write_text(
        json.dumps(
            {
                "ts": "2026-08-28T05:00:00Z",
                "type": "review_coverage",
                "source": "hook",
                "data": {
                    "pr": 1252,
                    "coverage": "uncovered",
                    "review_state": "unreviewed",
                    "reviewed_count": 0,
                    "self_attested_count": 0,
                    "head_sha": FIXTURE_HEAD,
                    "verdicts": [],
                },
            }
        )
        + "\n",
        encoding="utf-8",
    )
    two_reviewed_commits = [
        {"state": "COMMENTED", "submittedAt": "2026-08-28T05:53:00Z",
         "commit": {"oid": "aaaa000000000000000000000000000000000000"}},
        {"state": "COMMENTED", "submittedAt": "2026-08-28T06:10:00Z",
         "commit": {"oid": "bbbb000000000000000000000000000000000000"}},
    ]
    monkeypatch.setattr(
        _coverage_gate, "_pr_reviews", lambda *a, **k: (two_reviewed_commits, "")
    )
    chain = _coverage_gate.attestation_chain(
        str(tmp_path), head_branch="feature/x-8439", head=FIXTURE_HEAD
    )
    assert chain == [], "the specimen shape is an EMPTY chain; fixture drifted"

    state, refusal, head, note = _coverage_gate.coverage_verdict(
        1252, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED, f"stranded with no merge path: {refusal}"
    assert head, "a covered verdict must name the head it covers"
    assert "review budget discharged (2/2 rounds)" in note, note
    # The waiver names what it waived, and the provenance names which
    # instrument produced the 2. Both figures, so a reader can see the count
    # came from the reviews axis and not from an attestation chain that is empty.
    assert "waived at the cap: uncovered" in note, note


def test_a_failed_reviews_read_is_not_rendered_as_a_measured_zero(
    monkeypatch, tmp_path
):
    """The reviews read FAILS and the gate says so.

    The budget keeps its answer either way - that fail-open is deliberate, a
    cap that fired on a broken read would waive a remainder it may not have
    spent - but a zero an instrument never contributed to must not read as one
    it measured. That discriminator is what the old bare None destroyed: the
    same cause produced opposite symptoms on two PRs, one escaping the cap
    entirely and the other locked out by it.

    Round counting itself is ONE axis (attestations), per the operator ruling
    that retired the two-axis provenance rendering, so this asserts the read
    failure is named and nothing more.
    """
    _specimen_gates(monkeypatch)
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text(
        json.dumps(_soft_round("2026-08-28T05:00:00Z", FIXTURE_HEAD)) + "\n",
        encoding="utf-8",
    )
    monkeypatch.setattr(
        _coverage_gate,
        "_pr_reviews",
        lambda *a, **k: (None, "gh: 403 secondary rate limit"),
    )
    _state, refusal, _head, note = _coverage_gate.coverage_verdict(
        1252, str(tmp_path), recompute=False
    )
    rendered = f"{refusal} {note}"
    assert "reviews read unavailable: gh: 403 secondary rate limit" in rendered, rendered


def test_coverage_verdict_refuses_a_non_directory_at_the_door(monkeypatch, tmp_path):
    """The guard belongs at the gate's entry, not three probes deep.

    Every probe below takes the same cwd, and the head fetch is the first to
    trip on a bad one. It drops its reason and answers None, so the verb used
    to report "pr head fetch failed": true of the probe, silent about the
    argument that broke it. That is the wrong-subject sentence this gate
    exists to stop printing (x-51f7). UNANSWERED, never REFUSED - a gate that
    cannot read its own inputs has not judged the PR.
    """
    monkeypatch.chdir(tmp_path)

    def never_runs(cmd, **kwargs):
        raise AssertionError(f"no probe may run on a bad cwd: {cmd}")

    monkeypatch.setattr("fno.pr._proc.run", never_runs)
    state, refusal, head, note = _coverage_gate.coverage_verdict(
        1264, "bllshttng/footnote", recompute=False
    )
    assert state == _coverage_gate.UNANSWERED
    assert note == "no such directory: bllshttng/footnote"
    assert refusal == "" and head == ""
    assert "pr head fetch failed" not in note


def test_coverage_verdict_and_the_slug_read_share_one_cwd_guard(monkeypatch, tmp_path):
    """The empty string is the seam two copies of a guard disagree on.

    A first pass wrote the gate's own `if cwd and not isdir(cwd)` beside
    `_repo_slug_reason`'s `cwd is not None`, so "" passed the gate and reached
    every probe as a subprocess cwd - which raises rather than meaning "here".
    Both now call `_rest._cwd_refusal`, so they cannot answer differently.
    """
    monkeypatch.chdir(tmp_path)

    def never_runs(cmd, **kwargs):
        raise AssertionError(f"no probe may run on an empty cwd: {cmd}")

    monkeypatch.setattr("fno.pr._proc.run", never_runs)
    state, _refusal, _head, note = _coverage_gate.coverage_verdict(
        1264, "", recompute=False
    )
    assert state == _coverage_gate.UNANSWERED
    assert note == "empty cwd: pass a directory or None, never an empty string"

    # And the same sentence, from the slug read the gate feeds.
    from fno.pr._rest import _repo_slug_reason

    assert _repo_slug_reason("")[1] == note


# ---- the operator-law exit: one standing subject, one head-pinned command ----
#
# The gate had two authority surfaces that disagreed: `fno.decide.current_law`
# recovered a live operator ruling for an exact subject and nothing consumed
# it, while the merge predicate recognized only a non-author approval or the
# `coverage-override` label. These tests hold the join: the deciding list is
# `current_law` (through the gate's one seam, `law_authority`), only `single`
# is authority, and every other answer keeps the ordinary verdict.


def _waive_env(monkeypatch, tmp_path):
    """An attended operator terminal, a hermetic journal, the sandbox index."""
    import fno.paths as paths_mod

    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
    paths_mod.resolve_repo_root.cache_clear()
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    from types import SimpleNamespace

    from fno.agents import self_stamp

    # No harness identity + a terminal is the one state the operator lane
    # permits; a resolved session identity is the one it refuses.
    monkeypatch.setattr(
        self_stamp,
        "resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None),
    )
    monkeypatch.setattr("fno.decide._attended_terminal", lambda: True)


def _law_law(monkeypatch, scoped="none", standing="none"):
    """Pin the gate's one law seam: what current_law answers per subject."""

    def fake(subject):
        if subject == _coverage_gate.STANDING_WAIVER_SUBJECT:
            if standing == "unknown":
                return "unknown", f"decision probe failed for {subject}: boom"
            return standing, ""
        if scoped == "unknown":
            return "unknown", f"decision probe: conflicting law rows for {subject}"
        return scoped, ""

    monkeypatch.setattr(_coverage_gate, "law_authority", fake)


WAIVE_HEAD = "f" * 40


def test_law_authority_reads_the_real_index_three_ways(tmp_path):
    """The seam is the engine's law-lane live read, not a second deciding
    list: none, single (affirmative value only), damaged -> unknown. Seeded
    straight into the sandboxed index the conftest pins, so the statuses come
    from the real reader."""
    from fno import paths

    def _seed(*rows):
        paths.decisions_jsonl().parent.mkdir(parents=True, exist_ok=True)
        paths.decisions_jsonl().write_text(
            "".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8"
        )

    def _row(decision):
        return {
            "type": "operator_decision",
            "ts": "2026-08-29T00:00:00Z",
            "data": {
                "decision_id": "d-law0001",
                "decision": decision,
                "subject": _coverage_gate.scoped_waiver_subject(
                    "acme/widgets", 42, WAIVE_HEAD
                ),
                "authority_source": "operator",
            },
        }

    subject = _coverage_gate.scoped_waiver_subject("acme/widgets", 42, WAIVE_HEAD)
    _seed()
    assert _coverage_gate.law_authority(subject) == ("none", "")
    _seed(_row(_coverage_gate.WAIVER_DECISION))
    assert _coverage_gate.law_authority(subject) == ("single", "")
    # Row existence carries no polarity: a denial recorded at the waiver
    # subject is a single law row whose text is not the affirmative value.
    _seed(_row("coverage waiver DENIED for this head"))
    assert _coverage_gate.law_authority(subject) == ("none", "")
    # A single row with NO decision field at all is malformed authority, not
    # a clean no: unknown, with the dead field nameable in the probe.
    row_no_decision = _row(_coverage_gate.WAIVER_DECISION)
    del row_no_decision["data"]["decision"]
    _seed(row_no_decision)
    status, probe = _coverage_gate.law_authority(subject)
    assert status == "unknown"
    assert "no decision" in probe, probe
    _seed(_row(_coverage_gate.WAIVER_DECISION), "not json at all")
    status, probe = _coverage_gate.law_authority(subject)
    assert status == "unknown"
    assert "damaged" in probe


def test_coverage_waive_records_one_head_scoped_operator_law(
    monkeypatch, tmp_path, capsys
):
    """The attended command records ONE live law row at the exact
    head-scoped subject and prints the positive receipt only after the index
    write lands."""
    _waive_env(monkeypatch, tmp_path)
    monkeypatch.setattr(_coverage_gate, "_repo_slug", lambda cwd: "acme/widgets")
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: WAIVE_HEAD)
    rc = _coverage_gate.run_coverage_waive(
        42, "operator read this PR by hand", cwd=str(tmp_path)
    )
    cap = capsys.readouterr()
    assert rc == 0
    assert cap.out.strip() == "coverage waiver recorded: acme/widgets#42@ffffffff"
    subject = _coverage_gate.scoped_waiver_subject("acme/widgets", 42, WAIVE_HEAD)
    assert _coverage_gate.law_authority(subject)[0] == "single"


def test_coverage_waive_publishes_the_status_positively(monkeypatch, tmp_path, capsys):
    """The command's immediate publish is a POSITIVE control, not an absence.

    The call is best-effort by design, so a bare except would swallow a
    renamed helper or a drifted signature and every other waive test would
    stay green while the documented post-on-record behavior silently stopped
    existing. A recorder pins the one call that must fire, and the failure
    branch must keep the receipt and name the cause on stderr."""
    _waive_env(monkeypatch, tmp_path)
    monkeypatch.setattr(_coverage_gate, "_repo_slug", lambda cwd: "acme/widgets")
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: WAIVE_HEAD)
    calls = []

    def record(pr_number, *, head=None, cwd=None):
        calls.append((pr_number, head, cwd))
        return True, "posted"

    monkeypatch.setattr("fno.pr._reviews.publish_coverage_status", record)
    rc = _coverage_gate.run_coverage_waive(42, "because", cwd=str(tmp_path))
    cap = capsys.readouterr()
    assert rc == 0
    assert calls == [(42, WAIVE_HEAD, str(tmp_path))], calls
    assert "coverage waiver recorded" in cap.out
    assert "status publish" not in cap.err

    calls.clear()
    monkeypatch.setattr(
        "fno.pr._reviews.publish_coverage_status",
        lambda *a, **k: (False, "gh: 403 secondary rate limit"),
    )
    rc = _coverage_gate.run_coverage_waive(42, "because", cwd=str(tmp_path))
    cap = capsys.readouterr()
    assert rc == 0, "a failed POST does not fail the recorded law"
    assert "coverage waiver recorded" in cap.out, "the receipt still prints"
    assert "status publish failed" in cap.err
    assert "gh: 403" in cap.err, "the stderr note names the cause"


def test_coverage_waive_refuses_an_agent_session_positively(
    monkeypatch, tmp_path, capsys
):
    """A session a harness identifies records nothing: exit nonzero, a
    refusal marker that says who was refused, and no row any gate could read
    as a waiver."""
    _waive_env(monkeypatch, tmp_path)
    from types import SimpleNamespace

    from fno.agents import self_stamp

    monkeypatch.setattr(
        self_stamp,
        "resolve_self_identity",
        lambda: SimpleNamespace(
            session_id="20260829T000000Z-cl71578-d947a1ff00aa", harness="claude"
        ),
    )
    monkeypatch.setattr("fno.decide._attended_terminal", lambda: True)
    monkeypatch.setattr(_coverage_gate, "_repo_slug", lambda cwd: "acme/widgets")
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: WAIVE_HEAD)
    rc = _coverage_gate.run_coverage_waive(42, "because", cwd=str(tmp_path))
    cap = capsys.readouterr()
    assert rc == 3
    assert "coverage-waive refused" in cap.err
    assert "cannot record under operator authority" in cap.err
    subject = _coverage_gate.scoped_waiver_subject("acme/widgets", 42, WAIVE_HEAD)
    assert _coverage_gate.law_authority(subject)[0] == "none"


def test_coverage_waive_refuses_an_unreadable_head(monkeypatch, tmp_path, capsys):
    """No head, no waiver: the failed instrument is named and nothing is
    recorded."""
    _waive_env(monkeypatch, tmp_path)
    monkeypatch.setattr(_coverage_gate, "_repo_slug", lambda cwd: "acme/widgets")
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: None)
    rc = _coverage_gate.run_coverage_waive(42, "because", cwd=str(tmp_path))
    cap = capsys.readouterr()
    assert rc == 4
    assert "pr head fetch failed" in cap.err
    assert cap.out.strip() == ""


def test_coverage_waive_requires_a_reason(monkeypatch, tmp_path, capsys):
    _waive_env(monkeypatch, tmp_path)
    monkeypatch.setattr(_coverage_gate, "_repo_slug", lambda cwd: "acme/widgets")
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: WAIVE_HEAD)
    rc = _coverage_gate.run_coverage_waive(42, "   ", cwd=str(tmp_path))
    cap = capsys.readouterr()
    assert rc == 2
    assert "--reason" in cap.err


def test_recorded_scoped_waiver_covers_only_its_head(
    monkeypatch, tmp_path, capsys
):
    """End to end through the real store: record the waiver at one head, the
    gate covers THAT head with the scoped receipt, and a push refuses again
    because the new head's subject matches nothing."""
    _waive_env(monkeypatch, tmp_path)
    _specimen_gates(monkeypatch)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: WAIVE_HEAD)
    _seed_row(tmp_path, coverage="uncovered", count=0, head=WAIVE_HEAD)
    monkeypatch.setattr(_coverage_gate, "_repo_slug", lambda cwd: "acme/widgets")
    rc = _coverage_gate.run_coverage_waive(
        42, "operator reviewed this head by hand", cwd=str(tmp_path)
    )
    assert rc == 0
    # The real current_law read, not the pinned seam: the recorded row is the
    # authority the gate consumed.
    state, refusal, covered_head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED
    assert covered_head == WAIVE_HEAD
    assert note == (
        _coverage_gate.OVERRIDE_NOTE_PREFIX
        + f"head-pinned operator waiver at {WAIVE_HEAD[:8]}"
    )
    # A push moves the head; the old subject no longer names it.
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: "e" * 40)
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED
    assert "HEAD is eeeeeeee" in refusal


def _seed_waiver_law_row(
    subject, decision_id, *, decision, authority_source, ts="2026-08-29T00:00:00Z"
):
    """One live law-lane row at an exact subject, in the sandboxed index."""
    from fno import paths

    paths.decisions_jsonl().parent.mkdir(parents=True, exist_ok=True)
    paths.decisions_jsonl().open("a", encoding="utf-8").write(
        json.dumps(
            {
                "type": "operator_decision",
                "ts": ts,
                "data": {
                    "decision_id": decision_id,
                    "decision": decision,
                    "subject": subject,
                    "authority_source": authority_source,
                },
            }
        )
        + "\n"
    )


def test_a_chat_attested_waiver_row_is_not_authority(monkeypatch, tmp_path):
    """The law door is open to any harness-descended process, so a
    chat_attested row cannot carry the person-at-a-terminal fact a waiver
    asserts - even when its decision text matches WAIVER_DECISION exactly.
    The gate reads a clean no and the merge verdict stays REFUSED."""
    subject = _coverage_gate.scoped_waiver_subject("acme/widgets", 42, WAIVE_HEAD)
    _seed_waiver_law_row(
        subject,
        "d-agent001",
        decision=_coverage_gate.WAIVER_DECISION,
        authority_source="chat_attested",
    )
    assert _coverage_gate.law_authority(subject) == ("none", "")

    # End to end: the forged-looking row opens nothing.
    _specimen_gates(monkeypatch)
    _seed_row(tmp_path, coverage="uncovered", count=0, head=WAIVE_HEAD)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: WAIVE_HEAD)
    monkeypatch.setattr(_coverage_gate, "_repo_slug", lambda cwd: "acme/widgets")
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.REFUSED


def test_an_agent_row_cannot_muddy_an_operator_waiver(monkeypatch, tmp_path):
    """The filter runs BEFORE the conflict count: an operator's real waiver
    beside an agent's chat_attested noise still reads single, so an agent
    cannot muddle a reachable operator exit into unknown authority."""
    subject = _coverage_gate.scoped_waiver_subject("acme/widgets", 42, WAIVE_HEAD)
    _seed_waiver_law_row(
        subject,
        "d-opera001",
        decision=_coverage_gate.WAIVER_DECISION,
        authority_source="operator",
    )
    _seed_waiver_law_row(
        subject,
        "d-agent002",
        decision=_coverage_gate.WAIVER_DECISION,
        authority_source="chat_attested",
    )
    assert _coverage_gate.law_authority(subject) == ("single", "")


def test_waiver_subjects_are_one_spelling_across_decide_and_the_gate():
    """The write guard keys on fno.decide's prefix; the gate keys on its own
    standing subject. Two spellings of one family is two drift traps unless a
    test holds them equal."""
    from fno.decide import WAIVER_SUBJECT_PREFIX

    assert WAIVER_SUBJECT_PREFIX == _coverage_gate.STANDING_WAIVER_SUBJECT


def test_standing_law_waives_an_uncovered_head_on_the_real_merge(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """The standing ruling, green ordinary verdict apart: run_merge reaches
    the merge call and the receipt says which law waived it."""
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    _law_law(monkeypatch, standing="single")
    fake = FakeRun(toplevel=str(tmp_path))
    monkeypatch_run = pytest.MonkeyPatch()
    monkeypatch_run.setattr(_merge, "run", fake)
    try:
        assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
        cap = capsys.readouterr()
    finally:
        monkeypatch_run.undo()
    assert "coverage waived: standing operator law" in cap.err


def test_standing_law_does_not_clear_a_hard_finding(
    monkeypatch, tmp_path, capsys
):
    """Standing law is the NARROW waiver: an unresolved CONFIRMED security
    finding at a spent budget keeps IMPOSSIBLE, its marker, and its finding
    key - and no merge call happens."""
    _specimen_gates(monkeypatch)
    _ac7_seed(tmp_path, rounds=3)
    _law_law(monkeypatch, standing="single")
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    cap = capsys.readouterr()
    line = (cap.err.strip().splitlines() or [""])[0]
    assert rc == _coverage_gate.IMPOSSIBLE
    assert "impossible" in line
    assert "hooks/git-protection.py:302:security" in line


def test_head_pinned_waiver_covers_the_impossible_shape(
    monkeypatch, tmp_path
):
    """The scoped waiver is the STRONG one: the same spent-budget hard
    finding shape, covered at that exact head with the scoped receipt."""
    _specimen_gates(monkeypatch)
    _ac7_seed(tmp_path, rounds=3)
    _law_law(monkeypatch, scoped="single")
    monkeypatch.setattr(_coverage_gate, "_repo_slug", lambda cwd: "acme/widgets")
    state, _refusal, covered_head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.COVERED
    assert covered_head == FIXTURE_HEAD
    assert note == (
        _coverage_gate.OVERRIDE_NOTE_PREFIX
        + f"head-pinned operator waiver at {FIXTURE_HEAD[:8]}"
    )


def test_unknown_authority_is_unanswered_naming_the_probe(
    enabled, live_head, monkeypatch, tmp_path  # noqa: F811
):
    """A dead or conflicted decision probe is UNKNOWN authority, never
    absence: when the gate needs the waiver, the answer is UNANSWERED with
    the probe named, not a refusal built on an unread store."""
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    _law_law(monkeypatch, scoped="unknown", standing="unknown")
    monkeypatch.setattr(_coverage_gate, "_repo_slug", lambda cwd: "acme/widgets")
    state, refusal, _head, note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.UNANSWERED
    assert refusal == ""
    assert "decision probe" in note
    assert note.count("decision probe") == 2


def test_a_covered_row_prints_no_waiver_receipt(
    enabled, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """Ordinary coverage that passes on its own never consults law and never
    prints a waiver receipt: a reviewed merge and a waived one stay legible
    apart."""
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: HEAD)
    monkeypatch.setattr(_merge, "_pr_base_head_refs", lambda pr, cwd: ("main", "feature/x"))
    _law_law(monkeypatch, standing="single")
    fake = FakeRun(toplevel=str(tmp_path))
    monkeypatch_run = pytest.MonkeyPatch()
    monkeypatch_run.setattr(_merge, "run", fake)
    try:
        assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0
        cap = capsys.readouterr()
    finally:
        monkeypatch_run.undo()
    assert "coverage waived" not in cap.err


def test_impossible_remedies_name_the_attended_command(monkeypatch, tmp_path, capsys):
    """Three truthful exits, not two: the refusal must name the command a
    single-account operator can actually run."""
    _specimen_gates(monkeypatch)
    _ac7_seed(tmp_path, rounds=3)
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    cap = capsys.readouterr()
    line = (cap.err.strip().splitlines() or [""])[0]
    assert rc == _coverage_gate.IMPOSSIBLE
    assert "coverage-waive" in line
    assert "non-author GitHub approval" in line
    assert "coverage-override label" in line


def test_the_waiver_text_names_substance_and_the_real_pr_number():
    """The rendered sentence must be runnable and resolvable by a stranger.

    No decision ID: every other one in `cli/src` sits in a comment or a data
    record, and an OSS operator querying their own decision store for ours
    gets nothing back, so citing it as the authority is unresolvable. The
    substance (attended operator, CI green, no unresolved P1) is the part that
    is actually actionable. The PR number is interpolated for the same reason
    the self-review hint drops a `<level>` placeholder twenty lines up: a
    worker copies a copy-me slot verbatim.
    """
    from fno.pr import _coverage_gate

    out = _coverage_gate._with_stale_waiver_guidance("base refusal", 1314)

    assert "base refusal" in out
    assert "CI green" in out
    assert "no unresolved P1" in out
    assert "P1 does not waive" in out
    assert "coverage-waive 1314 --reason" in out
    assert "<pr>" not in out
    # The ID SHAPE, not the bare prefix: `d-` alone also matches
    # "attended-operator", a substring hit on the wrong symbol.
    assert not re.search(r"\bd-[0-9a-f]{8}\b", out), out


def test_a_stale_verdict_is_matched_by_NAME_not_by_existence():
    """The discriminator that keeps the waiver on the refusal it answers.

    `_stale_verdicts` collects every producer, so "any stale verdict exists"
    says only that something once reviewed. That is a different claim from
    "the evidence THIS conjunct wanted exists but went stale", and conflating
    them attaches a merge lever to a refusal caused by a reviewer that never
    reviewed at any sha.
    """
    from fno.pr import _coverage_gate

    cov = {"stale_verdicts": [{"name": "some-bot", "producer": "github_app"}]}
    assert _coverage_gate._has_stale_verdict(cov) is True
    assert _coverage_gate._has_stale_verdict(cov, "code-review") is False

    cov2 = {"stale_verdicts": [{"name": "code-review", "producer": "local"}]}
    assert _coverage_gate._has_stale_verdict(cov2, "code-review") is True

    # Slash-spelled, the way _coverage_has_local_pass's sibling normalizer
    # expects some stored attestations to read.
    cov3 = {"stale_verdicts": [{"name": "/code-review", "producer": "local_attestation"}]}
    assert _coverage_gate._has_stale_verdict(cov3, "code-review") is True

    for empty in ({"stale_verdicts": []}, {}, None, "not-a-dict"):
        assert _coverage_gate._has_stale_verdict(empty) is False, empty
        assert _coverage_gate._has_stale_verdict(empty, "code-review") is False, empty


def test_a_missing_code_review_is_not_answered_by_another_bots_stale_row(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """The load-bearing regression: required code-review never ran at ANY sha.

    Same row as the attestation-refusal test above - covered, counted, one
    github_app verdict - so the failing conjunct is the missing local pass,
    not staleness. The spy answers True to the UNNAMED question and False to
    the named one, so the waiver sentence appears here only if the branch
    asked "does any stale verdict exist". It must ask for `code-review`.
    """
    from fno.pr import _coverage_gate

    asked = []

    def spy(cov, name=None):
        asked.append(name)
        return name is None

    monkeypatch.setattr(_coverage_gate, "_has_stale_verdict", spy)
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: True)
    _seed_row(
        tmp_path,
        coverage="covered",
        count=2,
        head=HEAD,
        verdicts=[{"name": "some-bot", "producer": "github_app", "verdict": "reviewed"}],
    )
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )

    assert state == _coverage_gate.REFUSED
    assert "required code-review has no head-pinned local pass attestation" in refusal
    assert "coverage-waive" not in refusal, refusal
    assert asked == ["code-review"], asked


def test_a_decline_at_head_is_not_offered_a_merge_lever(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """A reviewer that refused AT HEAD is not a head that moved.

    The waiver answers staleness. A decline is a stronger signal than a stale
    row and it is a different one, so appending "this may merge without a
    head-pinned attestation" underneath it reads as a way around the decline.
    The spy answers True to every staleness question, so the sentence appears
    here only if the branch forgot to exclude the refusal.
    """
    from fno.pr import _coverage_gate

    monkeypatch.setattr(_coverage_gate, "_has_stale_verdict", lambda cov, name=None: True)
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    # The conjunct is the seam under test. Seeding `review_state` alone does
    # not reach it: the row is reshaped on read, so a refusal with no verdicts
    # normalizes back to plain `uncovered` and the exclusion never runs.
    monkeypatch.setattr(
        _coverage_gate, "covered_conjuncts", lambda cov, head, req: (False, "reviewer_refused")
    )
    _seed_row(tmp_path, coverage="uncovered", count=0, head=HEAD)
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )

    assert state == _coverage_gate.REFUSED
    assert "coverage-waive" not in refusal, refusal


def test_a_decline_at_head_blocks_the_waiver_on_the_attestation_branch_too(
    enabled, live_head, monkeypatch, capsys, tmp_path  # noqa: F811
):
    """Same decline exclusion, attestation-refusal branch.

    `covered_conjuncts` answers reviewer_refused before the local-pass
    conjunct, so a row holding an active decline reaches the no-local-pass
    branch with `failed == "reviewer_refused"` - and the seeded stale
    code-review row is genuinely stale, no spy, so the waiver sentence
    appears here only if that branch skipped the exclusion the other branch
    has always had.
    """
    from fno.pr import _coverage_gate

    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: True)
    monkeypatch.setattr(
        _coverage_gate, "covered_conjuncts", lambda cov, head, req: (False, "reviewer_refused")
    )
    _seed_row(
        tmp_path,
        coverage="uncovered",
        count=0,
        head=HEAD,
        verdicts=[{"name": "code-review", "producer": "local_attestation", "verdict": "stale"}],
    )
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )

    assert state == _coverage_gate.REFUSED
    assert "coverage-waive" not in refusal, refusal


# ---- the shared cap computation (cap_verdict) ------------------------------
#
# Every fixture here is CONSTRUCTED. The two live PRs that measured this
# defect are being cleared while this code lands, so no test may source its
# chain from a live PR: a red test that expires the moment somebody fixes
# the specimen is a test that guards nothing. The divergence fixture is the
# load-bearing one - on both live specimens the two axes AGREE, so a
# fixture where they agree proves the helper runs, never which axis it
# reads (a green control aimed at the wrong symbol).


def _cap_chain(rounds, *, first_verdict="fail", dispositions_at=None, category="correctness"):
    """A constructed branch chain: `rounds` attestations, one per head.

    Round 0 raises a CONFIRMED finding of `category`; every later round is
    a findings-free pass unless `dispositions_at` names a round index whose
    pass carries a `fixed` disposition for that finding.
    """
    dispositions_at = dispositions_at if dispositions_at is not None else -1
    lines = []
    for i in range(rounds):
        data = {
            "reviewer": "code-review",
            "head_sha": f"{i:040x}",
            "verdict": first_verdict if i == 0 else "pass",
            "session_id": "s-cap",
            "branch": "feature/x-cap",
            "reviewed_base_sha": "a" * 40,
            "reviewed_head_sha": f"{i:040x}",
            "findings_blocking": 1 if i == 0 else 0,
            "findings": (
                [
                    {
                        "category": category,
                        "verdict": "CONFIRMED",
                        "blocking": True,
                        "has_required_fields": True,
                        "finding_key": f"cli/src/fake.py:779:{category}",
                    }
                ]
                if i == 0
                else []
            ),
        }
        if i == dispositions_at:
            data["dispositions"] = [
                {
                    "finding_key": f"cli/src/fake.py:779:{category}",
                    "disposition": "fixed",
                    "reason": "verified the fix delta",
                }
            ]
        lines.append(
            {"ts": f"2026-08-31T2{i:02d}:00:00Z", "type": "review_attestation",
             "source": "hook", "data": data}
        )
    return lines


def _seed_cap_chain(tmp_path, lines, *, extra_rows=()):
    (tmp_path / ".fno").mkdir(exist_ok=True)
    text = "\n".join(json.dumps(e) for e in lines) + "\n"
    for row in extra_rows:
        text += json.dumps(row) + "\n"
    (tmp_path / ".fno" / "events.jsonl").write_text(text, encoding="utf-8")


_CAP_HARD_KEY = "cli/src/fake.py:779:correctness"


def _cap_cov_row():
    return {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 1,
            "head_sha": f"{5:040x}", "verdicts": []}


def test_cap_verdict_agreement_chain_is_impossible_and_names_the_key(tmp_path):
    """One fail raising a CONFIRMED correctness finding, then five
    findings-free passes, at max_rounds 2: impossible True, the key named."""
    _seed_cap_chain(tmp_path, _cap_chain(6))
    cap = _coverage_gate.cap_verdict(
        str(tmp_path), f"{5:040x}", "feature/x-cap", _cap_cov_row()
    )
    assert cap.impossible is True
    assert cap.rounds_used == 6
    assert cap.max_rounds == 2
    assert cap.hard_keys == [_CAP_HARD_KEY]
    assert _CAP_HARD_KEY in cap.nonterminal_keys


def test_cap_verdict_a_fixed_disposition_clears_impossible(tmp_path):
    """The same chain plus one more pass carrying a `fixed` disposition for
    the finding: a later attestation reviewed the fix delta, so the finding
    is terminal and no budget can make it impossible."""
    _seed_cap_chain(tmp_path, _cap_chain(7, dispositions_at=6))
    cap = _coverage_gate.cap_verdict(
        str(tmp_path), f"{6:040x}", "feature/x-cap", _cap_cov_row()
    )
    assert cap.impossible is False
    assert cap.hard_keys == []


def test_cap_verdict_reads_the_events_axis_alone_for_impossible(tmp_path):
    """The divergence fixture: 1 events-axis round, a reviews payload naming
    5 distinct reviewed commits, max_rounds 2. The budget reports 5 (a bot
    round IS a round) while `impossible` stays False - the unspent capacity
    is local, and a local attestation carrying a `fixed` disposition still
    clears the state, so "cannot be cleared by re-reviewing" would be a
    false claim. This is the one case the pre-fix single-axis conjunct
    answered impossible=True on."""
    _seed_cap_chain(tmp_path, _cap_chain(1))
    reviews = [
        {"state": "APPROVED", "commit": {"oid": f"r{i:038x}"}} for i in range(5)
    ]
    cap = _coverage_gate.cap_verdict(
        str(tmp_path), f"{0:040x}", "feature/x-cap", _cap_cov_row(), reviews=reviews
    )
    assert cap.rounds_used == 5
    assert cap.impossible is False
    # reviews=None (the status caller's setting) never WIDENS impossible:
    # same chain, events axis alone, still not impossible at 1/2.
    events_only = _coverage_gate.cap_verdict(
        str(tmp_path), f"{0:040x}", "feature/x-cap", _cap_cov_row()
    )
    assert events_only.rounds_used == 1
    assert events_only.impossible is False


def test_cap_verdict_on_an_empty_chain_answers_zero_not_impossible(tmp_path):
    """No chain at all: rounds 0, impossible False, nothing omitted."""
    (tmp_path / ".fno").mkdir(exist_ok=True)
    (tmp_path / ".fno" / "events.jsonl").write_text("", encoding="utf-8")
    cap = _coverage_gate.cap_verdict(
        str(tmp_path), f"{5:040x}", "feature/x-cap", _cap_cov_row()
    )
    assert cap.rounds_used == 0
    assert cap.max_rounds == 2
    assert cap.impossible is False
    assert cap.hard_keys == []


def _cap_gates(monkeypatch):
    """The hermetic gate seams pointed at the constructed chain: live head,
    matching branch, no override valve."""
    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo, pr_number=0: True)
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, repo: f"{5:040x}")
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda repo, pr_number=0: False)
    monkeypatch.setattr(_merge, "_pr_base_head_refs", lambda pr, cwd: ("main", "feature/x-cap"))
    monkeypatch.setattr(_reviews, "_override_label_actor", lambda pr, repo, r: (False, None))


def test_status_and_merge_answer_one_word_on_one_constructed_chain(
    monkeypatch, tmp_path
):
    """The deliverable's regression guard: one constructed chain, read by
    BOTH surfaces, must yield the same verdict and name the same finding
    key. The two surfaces disagreeing is the defect; a test that pins them
    equal is the test."""
    from fno.pr import _status

    _cap_gates(monkeypatch)
    row = dict(_cap_cov_row(), pr=42)
    _seed_cap_chain(
        tmp_path,
        _cap_chain(6),
        extra_rows=[
            {
                "ts": "2026-08-31T23:00:00Z",
                "type": "review_coverage",
                "source": "hook",
                "data": row,
            }
        ],
    )
    # The merge side: the gate's own verdict on the same chain.
    state, refusal, _head, _note = _coverage_gate.coverage_verdict(
        42, str(tmp_path), recompute=False
    )
    assert state == _coverage_gate.IMPOSSIBLE
    assert _CAP_HARD_KEY in refusal
    # The status side: the ready conjunct, re-derived from the same chain.
    blockers = _status._ready_blockers(
        True,
        "green",
        0,
        dict(_cap_cov_row()),
        True,
        head=f"{5:040x}",
        head_branch="feature/x-cap",
        code_review_required=False,
        repo=str(tmp_path),
    )
    assert "review_coverage_impossible" in blockers
    assert "review_coverage_uncovered" not in blockers
    # A row still carrying a stale impossible flag must NOT block on it:
    # the re-derivation is the answer of record, never the stored flag.
    flagged_row = dict(_cap_cov_row())
    flagged_row["impossible"] = True
    _seed_cap_chain(tmp_path, _cap_chain(7, dispositions_at=6))
    clean = _status._ready_blockers(
        True,
        "green",
        0,
        flagged_row,
        True,
        head=f"{6:040x}",
        head_branch="feature/x-cap",
        code_review_required=False,
        repo=str(tmp_path),
    )
    assert "review_coverage_impossible" not in clean


def test_a_pr_without_a_head_branch_appends_no_impossible_blocker(tmp_path):
    """No head branch -> the chain cannot be scoped -> no impossible blocker,
    and the other coverage conjuncts are unchanged. Scoping by exact head
    alone would narrow the chain to the current round and acquit the very
    state the conjunct exists to name."""
    from fno.pr import _status

    _seed_cap_chain(tmp_path, _cap_chain(6))
    blockers = _status._ready_blockers(
        True,
        "green",
        0,
        _cap_cov_row(),
        True,
        head=f"{5:040x}",
        head_branch="",
        code_review_required=False,
        repo=str(tmp_path),
    )
    assert "review_coverage_impossible" not in blockers
    assert blockers == []

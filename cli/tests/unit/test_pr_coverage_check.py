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
    monkeypatch.setattr(
        _reviews, "_reviewed_sha_still_describes_head", lambda *a, **k: True
    )
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
    # Hermetic: a dead ancestry must not fall into the real content arm.
    monkeypatch.setattr(
        _reviews, "_reviewed_sha_still_describes_head", lambda *a, **k: False
    )
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
                "freshness": "fresh",
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

    monkeypatch.setattr(_reviews, "latest_review_coverage", boom)
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
    monkeypatch.setattr(_reviews, "_reviewed_sha_still_describes_head", lambda *a, **k: True)


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
    """AC5b: the same fixture plus one open correctness finding REFUSES, by key."""
    _specimen_gates(monkeypatch)
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


def test_ac7_hp_two_rounds_refuse_three_say_how_many_remain(
    monkeypatch, tmp_path, capsys
):
    """The same chain at two rounds: exit 3 with the budget named, not 5."""
    _specimen_gates(monkeypatch)
    _ac7_seed(tmp_path, rounds=2)
    rc = _coverage_gate.run_coverage_check(42, cwd=str(tmp_path))
    cap = capsys.readouterr()
    line = (cap.err.strip().splitlines() or [""])[0]
    assert rc == _coverage_gate.REFUSED == 3
    assert "2/2 review rounds used" in line
    assert "the next round reports impossible" in line


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
    """`fno do pr status` renders the IMPOSSIBLE row as
    review_coverage_impossible, distinct from review_coverage_uncovered.

    It reads `impossible`, never the raw `rounds_exhausted`: under the
    operator's round-cap ruling an exhausted budget alone MERGES (the
    remainder is filed), so naming the blocker off the budget flag would
    hold every capped PR the law says should land."""
    from fno.pr import _status

    blockers = _status._ready_blockers(
        True,
        "green",
        0,
        {"coverage": "uncovered", "rounds_exhausted": True, "impossible": True},
        review_lane=True,
        head="",
        code_review_required=False,
        repo=str(tmp_path),
    )
    assert "review_coverage_impossible" in blockers
    assert "review_coverage_uncovered" not in blockers

    # The demotion, pinned: a spent budget with no hard finding is not a
    # blocker of its own - those findings are filed and the PR merges.
    soft = _status._ready_blockers(
        True,
        "green",
        0,
        {"coverage": "uncovered", "rounds_exhausted": True, "impossible": False},
        review_lane=True,
        head="",
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
    stamps = ["2026-08-25T21:00:00Z", "2026-08-25T21:30:00Z", "2026-08-25T22:00:00Z"]
    heads = [
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
        FIXTURE_HEAD,
    ]
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
    """One review is the floor and the budget still bites: at two rounds the
    same soft finding REFUSES rather than being filed."""
    _specimen_gates(monkeypatch)
    _seed_soft_cap(tmp_path, rounds=2)
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

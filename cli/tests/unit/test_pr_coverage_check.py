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

import pytest

from fno.pr import _coverage_gate, _merge, _reviews
from fno.pr._proc import Result

from .test_pr_merge import FakeRun, _last_json, enabled  # noqa: F401

HEAD = "aaaa1111bbbb2222"


def _seed_row(tmp_path, *, coverage, count, head, verdicts=None, pr=42):
    """One review_coverage event in the project log the gate reads."""
    (tmp_path / ".fno").mkdir(exist_ok=True)
    data = {"pr": pr, "coverage": coverage, "head_sha": head}
    if coverage in ("covered", "uncovered"):
        data["reviewed_count"] = count
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

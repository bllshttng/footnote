"""Coverage receipts name self-attested-only merges and terminal skips."""

from __future__ import annotations

import json
from types import SimpleNamespace

from fno.pr import _coverage_gate, _merge, _reviews, _status
from fno.pr._proc import Result

from .test_pr_merge import FakeRun, enabled  # noqa: F401


HEAD = "a" * 40


def _self_attested_row(*, reviewed_count: int = 1, self_attested_count: int = 1) -> dict:
    return {
        "coverage": "covered",
        "review_state": "reviewed",
        "reviewed_count": reviewed_count,
        "self_attested_count": self_attested_count,
        "head_sha": HEAD,
        "verdicts": [
            {
                "verdict": "reviewed",
                "producer": "local_attestation",
                "attestation_origin": "self_attested",
                "reviewed_sha": HEAD,
                "freshness": "fresh",
            }
        ],
    }


def _prepare_gate(monkeypatch, tmp_path, row: dict, *, require_corroboration: bool) -> None:
    monkeypatch.setattr(_merge, "_pr_head_oid", lambda pr, cwd: HEAD)
    monkeypatch.setattr(_merge, "_review_coverage_for_pr", lambda pr, cwd, head=None: (row, ""))
    monkeypatch.setattr(_merge, "_review_lane_configured", lambda cwd, pr_number=0: True)
    monkeypatch.setattr(_merge, "_code_review_attestation_required", lambda cwd, pr_number=0: False)
    monkeypatch.setattr(_merge, "_pr_base_head_refs", lambda pr, cwd: ("main", "feature/x"))
    monkeypatch.setattr(_coverage_gate, "_override_valve", lambda pr, cwd: (False, "", ""))
    monkeypatch.setattr(_coverage_gate, "_repo_root", lambda cwd: tmp_path)
    monkeypatch.setattr(_coverage_gate, "_github_approval_satisfies", lambda cwd: False)
    monkeypatch.setattr(_coverage_gate, "attestation_chain", lambda *args, **kwargs: [])
    monkeypatch.setattr(
        _coverage_gate,
        "disposition_refusal",
        lambda chain, cov, cwd: ("", "", [], []),
    )
    monkeypatch.setattr(
        "fno.config.load_settings_for_repo",
        lambda root: SimpleNamespace(
            review=SimpleNamespace(
                require_corroboration=require_corroboration,
                posture=None,
                github_apps=None,
                peers=False,
                self_review_required=True,
                reviewers=[],
            )
        ),
    )


def test_policy_off_marks_self_attested_only_coverage(monkeypatch, tmp_path):
    row = _self_attested_row()
    _prepare_gate(monkeypatch, tmp_path, row, require_corroboration=False)

    state, refusal, covered_head, note, _ = _coverage_gate._ordinary_verdict(
        42, str(tmp_path), recompute=True
    )

    assert state == _coverage_gate.COVERED
    assert refusal == ""
    assert covered_head == HEAD
    assert note.startswith(_coverage_gate.SELF_ATTESTED_NOTE_PREFIX)


def test_policy_off_does_not_mark_corroborated_coverage(monkeypatch, tmp_path):
    row = _self_attested_row(reviewed_count=2, self_attested_count=1)
    row["verdicts"].append(
        {
            "verdict": "reviewed",
            "producer": "github_app",
            "attestation_origin": "github_review",
            "reviewed_sha": HEAD,
            "freshness": "fresh",
        }
    )
    _prepare_gate(monkeypatch, tmp_path, row, require_corroboration=False)

    state, _, _, note, _ = _coverage_gate._ordinary_verdict(42, str(tmp_path), recompute=True)

    assert state == _coverage_gate.COVERED
    assert _coverage_gate.SELF_ATTESTED_NOTE_PREFIX not in note


def test_policy_on_keeps_self_attested_coverage_refused(monkeypatch, tmp_path):
    _prepare_gate(monkeypatch, tmp_path, _self_attested_row(), require_corroboration=True)

    state, refusal, _, note, _ = _coverage_gate._ordinary_verdict(42, str(tmp_path), recompute=True)

    assert state == _coverage_gate.REFUSED
    assert "corroboration" in refusal
    assert _coverage_gate.SELF_ATTESTED_NOTE_PREFIX not in note


def test_covered_merge_receipt_names_self_attestation(
    enabled,
    monkeypatch,
    capsys,
    tmp_path,  # noqa: F811
):
    _prepare_gate(monkeypatch, tmp_path, _self_attested_row(), require_corroboration=False)
    fake = FakeRun(
        gh_merge=Result(0, "Merged pull request", ""),
        toplevel=str(tmp_path),
    )
    monkeypatch.setattr(_merge, "run", fake)

    assert _merge.run_merge(["42"], cwd=str(tmp_path)) == 0

    assert (
        "coverage is the author's own local attestation, uncorroborated" in capsys.readouterr().err
    )


def test_status_notes_terminal_and_unknown_coverage(monkeypatch, capsys):
    monkeypatch.setattr(
        _status,
        "_fetch",
        lambda pr, cwd: (
            {
                "state": "MERGED",
                "headRefOid": HEAD,
                "statusCheckRollup": [
                    {"name": "ci", "status": "COMPLETED", "conclusion": "SUCCESS"}
                ],
            },
            "",
        ),
    )

    _status.run_status("42")
    out = json.loads(capsys.readouterr().out)

    assert out["review_coverage"]["note"] == (
        "not asked: PR is terminal (merged or closed); this says nothing about "
        "coverage at merge time"
    )
    assert _reviews._UNKNOWN_COVERAGE["note"] == "coverage probe failed"

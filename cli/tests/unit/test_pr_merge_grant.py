"""Unit tests for the durable-grant transport and its merge entry point.

The resolver's decision arms live in ``crates/fno-agents/src/merge_grant.rs``
(tested there); this suite pins the Python transport and the consumers that
read its verdict: the status projection and the merge gate's durable-grant
authority arm.
"""
from __future__ import annotations

import json

import pytest

from fno.config import AutoMergeBlock
from fno.pr import _merge
from fno.pr._merge_grant import (
    ABSENT,
    GRANTED,
    HELD,
    REFUSED,
    UNKNOWN,
    GrantVerdict,
    resolve_durable_grant,
)
from fno.pr._proc import Result

NODE = "ab-grantunit1"
PR = 42


@pytest.fixture(autouse=True)
def _stub_pr_worktree_resolution(monkeypatch):
    monkeypatch.setattr(
        "fno.pr._review_hold.resolve_pr_worktree", lambda _pr, repo: repo
    )


def _verdict(monkeypatch, state, reason="r", claim_state="stale"):
    """Stub the transport: the Rust owner's answer, as the consumers see it."""
    verdict = GrantVerdict(state, reason, node_id=NODE, claim_state=claim_state)
    monkeypatch.setattr(
        "fno.pr._merge_grant.resolve_durable_grant", lambda pr, repo: verdict
    )
    return verdict


# ---------------------------------------------------------------------------
# AC4-HP / AC4-ERR: the transport reads the Rust owner and fails closed
# ---------------------------------------------------------------------------


def test_resolver_reads_the_rust_verdict(monkeypatch):
    monkeypatch.setattr(
        "fno.rust_binary.verb_call",
        lambda verb, payload, **kw: {
            "state": HELD,
            "reason": "node claim is live; only a positively not-live holder "
            "transfers execution",
            "node_id": NODE,
            "claim_state": "live",
            "grant": {"approved": True, "source": "config",
                      "recorded_by": "spawner", "recorded_at": "2026-08-24T12:00:00Z"},
        },
    )

    verdict = resolve_durable_grant(PR, "/tmp")

    assert verdict.state == HELD
    assert "node claim is live" in verdict.reason
    assert verdict.node_id == NODE
    assert verdict.claim_state == "live"
    assert verdict.merge_eligible is False


def test_resolver_reads_a_stateless_receipt_as_unknown(monkeypatch):
    monkeypatch.setattr(
        "fno.rust_binary.verb_call", lambda verb, payload, **kw: {"reason": "x"}
    )

    assert resolve_durable_grant(PR, "/tmp").state == UNKNOWN


def test_unreachable_resolver_reads_unknown(monkeypatch, tmp_path):
    from fno.rust_binary import VerbUnavailable

    def boom(verb, payload, **kw):
        raise VerbUnavailable("fno-agents exited 1")

    monkeypatch.setattr("fno.rust_binary.verb_call", boom)
    _stub_merge_world(monkeypatch, tmp_path)

    assert resolve_durable_grant(PR, str(tmp_path)).state == UNKNOWN

    code = _merge.run_merge([str(PR)], cwd=str(tmp_path), authority="durable_grant")

    assert code == 2


# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# The merge gate's durable-grant authority arm
# ---------------------------------------------------------------------------


def _stub_merge_world(monkeypatch, tmp_path):
    """The `enabled` fixture's hermetic stubs from test_pr_merge, narrowed to
    what the durable-arm cases need: gh present, no lane holds, coverage
    covered. Merge-behaviour beyond the authority arm is owned there."""
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda _repo: AutoMergeBlock(enabled=True))
    monkeypatch.setattr(_merge.shutil, "which", lambda _x: "/usr/bin/gh")
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(tmp_path))
    monkeypatch.setattr(
        _merge,
        "_review_coverage_for_pr",
        lambda pr, repo, head=None: (
            {"coverage": "covered", "review_state": "reviewed", "reviewed_count": 1},
            "",
        ),
    )
    monkeypatch.setattr(_merge, "_review_lane_configured", lambda repo, pr_number=0: True)
    monkeypatch.setattr(
        _merge, "_code_review_attestation_required", lambda repo, pr_number=0: False
    )
    monkeypatch.setattr(
        "fno.pr._reviews._override_label_actor", lambda pr, repo, r: (False, None)
    )
    monkeypatch.setattr(
        "fno.pr._reviews.publish_coverage_status",
        lambda pr, head=None, cwd=None, repo=None, gate_verdict=None: (True, ""),
    )
    from fno.rust_binary import VerbUnavailable

    def _fake(verb, payload, **kw):
        if payload.get("op") == "status-rerun":
            return {"recovered": False, "failed": []}
        raise VerbUnavailable("door op unavailable in tests")

    monkeypatch.setattr("fno.rust_binary.verb_call", _fake)


def test_merge_durable_grant_absent_skips_without_gh(tmp_path, monkeypatch, capsys):
    _verdict(monkeypatch, ABSENT)
    _stub_merge_world(monkeypatch, tmp_path)

    code = _merge.run_merge([str(PR)], cwd=str(tmp_path), authority="durable_grant")

    assert code == 2
    obj = json.loads(capsys.readouterr().out.strip().splitlines()[-1])
    assert obj["outcome"] == "skipped"
    assert "durable-grant" in obj["reason"]


def test_merge_durable_refusal_skips(tmp_path, monkeypatch, capsys):
    _verdict(
        monkeypatch,
        REFUSED,
        reason="newest durable grant records approved=false "
        "(source: no-merge-flag, recorded 2026-08-24T12:00:00Z)",
    )
    _stub_merge_world(monkeypatch, tmp_path)

    code = _merge.run_merge([str(PR)], cwd=str(tmp_path), authority="durable_grant")

    assert code == 2
    obj = json.loads(capsys.readouterr().out.strip().splitlines()[-1])
    assert obj["outcome"] == "skipped"
    assert "no-merge-flag" in obj["reason"]


def test_merge_live_claim_holds(tmp_path, monkeypatch, capsys):
    _verdict(monkeypatch, HELD, claim_state="live")
    _stub_merge_world(monkeypatch, tmp_path)

    code = _merge.run_merge([str(PR)], cwd=str(tmp_path), authority="durable_grant")

    assert code == 2
    obj = json.loads(capsys.readouterr().out.strip().splitlines()[-1])
    assert obj["outcome"] == "held"


def test_merge_durable_granted_reaches_the_canonical_guards(tmp_path, monkeypatch, capsys):
    """A granted verdict hands execution to the guard chain: the receipt the
    run emits is a DOWNSTREAM gate's (here the coverage probe, whose fake gh
    view is empty), never the durable arm's refusal. The full green-merge
    journey is the watcher integration test's job; this pins the handoff."""
    _verdict(monkeypatch, GRANTED)
    _stub_merge_world(monkeypatch, tmp_path)
    (tmp_path / ".fno").mkdir()
    fake_calls = []

    class FakeRun:
        def __call__(self, cmd, **kwargs):
            fake_calls.append(list(cmd))
            if cmd[:2] == ["gh", "pr"] and "merge" in cmd:
                return Result(0, "Merged pull request", "")
            return Result(0, "", "")

    monkeypatch.setattr(_merge, "run", FakeRun())

    code = _merge.run_merge([str(PR)], cwd=str(tmp_path), authority="durable_grant")

    cap = capsys.readouterr()
    lines = (cap.out + cap.err).strip().splitlines()
    receipts = [json.loads(line) for line in lines if line.startswith("{")]
    assert receipts, f"no merge receipt emitted; code={code}"
    assert all(
        "durable-grant" not in r.get("reason", "") for r in receipts
    ), receipts
    assert fake_calls, "a granted run must reach the guards, not stop at the arm"


def test_manifest_arm_ignores_the_durable_receipt(tmp_path, monkeypatch, capsys):
    """Authority isolation: the durable receipt decides ONLY the watcher lane.
    A session merge still reads its own manifest, where a per-run no-merge
    outranks everything - one receipt per caller, never a shared shortcut."""
    _verdict(monkeypatch, GRANTED)
    _stub_merge_world(monkeypatch, tmp_path)
    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        "session_id: s1\nauto_merge_approved: false\n", encoding="utf-8"
    )

    # The refusal is the owner's now, so this case has to reach it: the guards
    # between the posture read and the owner are other nodes' and are stubbed
    # to their clear answers here.
    from fno.pr import _base_lineage, _coverage_gate

    monkeypatch.setattr(
        _merge, "_pr_head_ref_and_oid", lambda pr, repo: ("feature/x", "abc123", "OPEN")
    )
    monkeypatch.setattr(
        _coverage_gate,
        "coverage_verdict",
        lambda pr, repo, recompute=False: (_coverage_gate.COVERED, "", "abc123", ""),
    )
    monkeypatch.setattr(_base_lineage, "lineage_verdict", lambda pr, cwd: ("ok", ""))

    seen: dict = {}

    def _authorized(pr_number, repo, *, effect, approved, source, **kwargs):
        seen["approved"] = approved
        seen["source"] = source
        return {"outcome": "refused", "detail": "per-run no-merge"}

    monkeypatch.setattr(_merge, "_authorized_merge", _authorized)

    code = _merge.run_merge([str(PR)], cwd=str(tmp_path))

    obj = json.loads(capsys.readouterr().out.strip().splitlines()[-1])
    assert code == 2
    assert obj["outcome"] == "skipped"
    # The session lane hands its OWN manifest down, never the durable receipt:
    # one posture per caller, never a shared shortcut.
    assert seen["approved"] is False

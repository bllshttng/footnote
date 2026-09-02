"""Unit tests for the typed durable-grant resolver and its merge entry point.

The spawner records its merge verdict on the worker's do row (the durable
receipt); this suite pins the ONE reader all three consumers share:

- AC10-HP: newest positive receipt + positively not-live claim + live
  dispatch config reads ``granted``.
- AC9-EDGE: a newer explicit refusal outranks an older grant (and vice
  versa) - ordering by recorded_at, never row position.
- AC10-CON: live/suspect/corrupt claims and a flipped config switch never
  grant.
- AC12-ERR: malformed receipts, ambiguity, and unreadable config read
  ``unknown`` - every arm fails closed, never to a guess.
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


def _receipt(approved=True, source="config", at="2026-08-24T12:00:00Z", by="spawner"):
    return {"approved": approved, "source": source, "recorded_by": by, "recorded_at": at}


def _do_row(grant, session="w1"):
    row = {"phase": "do", "harness": "claude", "session_id": session,
           "started_at": "2026-08-24T11:00:00Z"}
    if grant is not None:
        row["merge_grant"] = grant
    return row


def _write_graph(tmp_path, monkeypatch, entries):
    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    # Bare-number matching: the slug read is stubbed away so these tests stay
    # hermetic; the repo-scoped narrowing has its own coverage in the graph suite.
    monkeypatch.setattr("fno.pr._coverage_gate._repo_slug", lambda repo: None)
    return g


def _grant_node(tmp_path, monkeypatch, sessions):
    return _write_graph(tmp_path, monkeypatch, [
        {"id": NODE, "title": "t", "pr_number": PR, "sessions": sessions},
    ])


def _claim(monkeypatch, state="stale", holder="worker-1", error=None):
    def probe(key, **kwargs):
        out = {"key": key, "state": state, "holder": holder}
        if error:
            out["error"] = error
        return out

    monkeypatch.setattr("fno.claims.core.claim_status", probe)


def _config(monkeypatch, enabled=True, grant="dispatch", boom=False):
    real = AutoMergeBlock
    import fno.config as config_mod

    if boom:
        def loader(path):
            raise RuntimeError("config wedged")
    else:
        def loader(path):
            return config_mod.load_settings().model_copy(
                update={"auto_merge": real(enabled=enabled, grant=grant)}
            )
    monkeypatch.setattr("fno.config.load_settings_for_repo", loader)


# ---------------------------------------------------------------------------
# AC10-HP: the granted path
# ---------------------------------------------------------------------------


def test_granted_when_receipt_unheld_and_config_dispatches(tmp_path, monkeypatch):
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch, state="stale")
    _config(monkeypatch)

    verdict = resolve_durable_grant(PR, str(tmp_path))

    assert verdict.state == GRANTED
    assert verdict.merge_eligible is True
    assert verdict.node_id == NODE
    assert verdict.claim_state == "stale"
    assert verdict.grant["approved"] is True


def test_free_claim_is_also_positively_not_live(tmp_path, monkeypatch):
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch, state="free")
    _config(monkeypatch)

    assert resolve_durable_grant(PR, str(tmp_path)).state == GRANTED


# ---------------------------------------------------------------------------
# AC9-EDGE: newest explicit receipt wins, by recorded_at not row order
# ---------------------------------------------------------------------------


def test_newer_refusal_outranks_older_grant(tmp_path, monkeypatch):
    _grant_node(tmp_path, monkeypatch, [
        _do_row(_receipt(approved=True, at="2026-08-24T10:00:00Z"), session="w0"),
        _do_row(_receipt(approved=False, source="no-merge-flag",
                         at="2026-08-24T12:00:00Z"), session="w1"),
    ])
    _claim(monkeypatch)
    _config(monkeypatch)

    verdict = resolve_durable_grant(PR, str(tmp_path))

    assert verdict.state == REFUSED
    assert verdict.merge_eligible is False
    assert "no-merge-flag" in verdict.reason


def test_newer_grant_outranks_older_refusal(tmp_path, monkeypatch):
    _grant_node(tmp_path, monkeypatch, [
        _do_row(_receipt(approved=False, source="no-merge-flag",
                         at="2026-08-24T10:00:00Z"), session="w0"),
        _do_row(_receipt(approved=True, at="2026-08-24T12:00:00Z"), session="w1"),
    ])
    _claim(monkeypatch)
    _config(monkeypatch)

    assert resolve_durable_grant(PR, str(tmp_path)).state == GRANTED


def test_row_order_never_decides(tmp_path, monkeypatch):
    """Newest by timestamp even when the older receipt sits last on the node."""
    _grant_node(tmp_path, monkeypatch, [
        _do_row(_receipt(approved=False, source="no-merge-flag",
                         at="2026-08-24T12:00:00Z"), session="w1"),
        _do_row(_receipt(approved=True, at="2026-08-24T10:00:00Z"), session="w0"),
    ])
    _claim(monkeypatch)
    _config(monkeypatch)

    assert resolve_durable_grant(PR, str(tmp_path)).state == REFUSED


def test_disagreeing_newest_receipts_at_one_instant_never_grant(tmp_path, monkeypatch):
    _grant_node(tmp_path, monkeypatch, [
        _do_row(_receipt(approved=True, at="2026-08-24T12:00:00Z"), session="w0"),
        _do_row(_receipt(approved=False, at="2026-08-24T12:00:00Z"), session="w1"),
    ])
    _claim(monkeypatch)
    _config(monkeypatch)

    assert resolve_durable_grant(PR, str(tmp_path)).state == UNKNOWN


# ---------------------------------------------------------------------------
# AC10-CON: liveness and standing config hold the merge
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("state", ["live", "suspect"])
def test_live_or_suspect_claim_holds(tmp_path, monkeypatch, state):
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch, state=state, holder="still-working")
    _config(monkeypatch)

    verdict = resolve_durable_grant(PR, str(tmp_path))

    assert verdict.state == HELD
    assert verdict.merge_eligible is False
    assert "still-working" in verdict.reason


def test_corrupt_claim_is_unknown_not_held(tmp_path, monkeypatch):
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch, state="corrupted", error="bad json")
    _config(monkeypatch)

    assert resolve_durable_grant(PR, str(tmp_path)).state == UNKNOWN


def test_config_switched_off_holds_even_with_receipt(tmp_path, monkeypatch):
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch)
    _config(monkeypatch, enabled=False)

    assert resolve_durable_grant(PR, str(tmp_path)).state == HELD


def test_non_dispatch_grant_holds(tmp_path, monkeypatch):
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch)
    _config(monkeypatch, grant="operator")

    assert resolve_durable_grant(PR, str(tmp_path)).state == HELD


def test_unreadable_config_is_unknown(tmp_path, monkeypatch):
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch)
    _config(monkeypatch, boom=True)

    verdict = resolve_durable_grant(PR, str(tmp_path))

    assert verdict.state == UNKNOWN
    assert verdict.merge_eligible is False


# ---------------------------------------------------------------------------
# Absence and ambiguity never grant
# ---------------------------------------------------------------------------


def test_no_graph_node_is_absent(tmp_path, monkeypatch):
    _write_graph(tmp_path, monkeypatch, [])

    assert resolve_durable_grant(PR, str(tmp_path)).state == ABSENT


def test_node_without_receipts_is_absent(tmp_path, monkeypatch):
    """A pre-field do row (no merge_grant key) is honest absence, not approval."""
    _grant_node(tmp_path, monkeypatch, [_do_row(None)])
    _claim(monkeypatch)
    _config(monkeypatch)

    assert resolve_durable_grant(PR, str(tmp_path)).state == ABSENT


def test_two_nodes_on_one_pr_is_unknown(tmp_path, monkeypatch):
    _write_graph(tmp_path, monkeypatch, [
        {"id": "ab-grantunit1", "title": "a", "pr_number": PR},
        {"id": "ab-grantunit2", "title": "b", "pr_number": PR},
    ])

    assert resolve_durable_grant(PR, str(tmp_path)).state == UNKNOWN


def test_unreadable_graph_is_unknown(tmp_path, monkeypatch):
    g = tmp_path / "graph.json"
    g.write_text("{not json", encoding="utf-8")
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setattr("fno.pr._coverage_gate._repo_slug", lambda repo: None)

    assert resolve_durable_grant(PR, str(tmp_path)).state == UNKNOWN


# ---------------------------------------------------------------------------
# AC12-ERR: malformed receipts are loud unknowns
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("bad", [
    {"approved": True, "source": "config", "recorded_by": "s"},          # missing key
    {"approved": True, "source": "config", "recorded_by": "s",
     "recorded_at": "2026-08-24T12:00:00Z", "extra": 1},                  # unknown key
    {"approved": "yes", "source": "config", "recorded_by": "s",
     "recorded_at": "2026-08-24T12:00:00Z"},                              # non-bool
    {"approved": True, "source": "config", "recorded_by": "s",
     "recorded_at": "yesterday"},                                          # non-UTC
])
def test_malformed_receipt_is_unknown(tmp_path, monkeypatch, bad):
    _grant_node(tmp_path, monkeypatch, [_do_row(bad)])
    _claim(monkeypatch)
    _config(monkeypatch)

    verdict = resolve_durable_grant(PR, str(tmp_path))

    assert verdict.state == UNKNOWN
    assert verdict.merge_eligible is False


# ---------------------------------------------------------------------------
# The merge gate's durable-grant authority arm
# ---------------------------------------------------------------------------


def _stub_merge_world(monkeypatch, tmp_path):
    """The `enabled` fixture's hermetic stubs from test_pr_merge, narrowed to
    what the durable-arm cases need: gh present, no lane holds, coverage
    covered. Merge-behaviour beyond the authority arm is owned there."""
    monkeypatch.setattr(_merge, "_load_auto_merge", lambda: AutoMergeBlock(enabled=True))
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


def test_merge_durable_grant_absent_skips_without_gh(tmp_path, monkeypatch, capsys):
    _write_graph(tmp_path, monkeypatch, [])
    _stub_merge_world(monkeypatch, tmp_path)

    code = _merge.run_merge([str(PR)], cwd=str(tmp_path), authority="durable_grant")

    assert code == 2
    obj = json.loads(capsys.readouterr().out.strip().splitlines()[-1])
    assert obj["outcome"] == "skipped"
    assert "durable-grant" in obj["reason"]


def test_merge_durable_refusal_skips(tmp_path, monkeypatch, capsys):
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt(approved=False,
                                                         source="no-merge-flag"))])
    _claim(monkeypatch)
    _config(monkeypatch)
    _stub_merge_world(monkeypatch, tmp_path)

    code = _merge.run_merge([str(PR)], cwd=str(tmp_path), authority="durable_grant")

    assert code == 2
    obj = json.loads(capsys.readouterr().out.strip().splitlines()[-1])
    assert obj["outcome"] == "skipped"
    assert "no-merge-flag" in obj["reason"]


def test_merge_live_claim_holds(tmp_path, monkeypatch, capsys):
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch, state="live", holder="worker-1")
    _config(monkeypatch)
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
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch, state="stale")
    _config(monkeypatch)
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
    _grant_node(tmp_path, monkeypatch, [_do_row(_receipt())])
    _claim(monkeypatch)
    _config(monkeypatch)
    _stub_merge_world(monkeypatch, tmp_path)
    (tmp_path / ".fno").mkdir()
    (tmp_path / ".fno" / "target-state.md").write_text(
        "session_id: s1\nauto_merge_approved: false\n", encoding="utf-8"
    )

    code = _merge.run_merge([str(PR)], cwd=str(tmp_path))

    obj = json.loads(capsys.readouterr().out.strip().splitlines()[-1])
    assert code == 2
    assert obj["outcome"] == "skipped"
    assert "no-merge" in obj["reason"]

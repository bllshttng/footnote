"""Tests for `fno decide` - the durable decision record and its recovery query.

The record has three stores: the append-only ``operator_decision`` event in the
project journal (durability), the machine-wide ``decisions.jsonl`` index (the
reader's only source), and the projection onto the subject node's graph entry
(the node view). A record that is only greppable is not recoverable.

The defect these guard against is a write that succeeded and a read that could
not find it. So every recall assertion names a POSITIVE marker - the returned
``decision_id`` - never the absence of an error.
"""
from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace

import pytest
from typer.testing import CliRunner

from fno.decide.cli import decide_app

runner = CliRunner()


@pytest.fixture(autouse=True)
def _operator_terminal_by_default(monkeypatch: pytest.MonkeyPatch):
    """Persistence tests exercise the permitted writer unless they say otherwise."""
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr("fno.decide._attended_terminal", lambda: True)


def test_operator_lane_refusal_uses_proven_identity(monkeypatch):
    from types import SimpleNamespace

    from fno.decide import RefusedAuthorityError
    from fno.decide import _resolve_decider

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(
            session_id="2782a6e1-aaaa-bbbb-cccc-dddddddddddd",
            harness="claude",
            disposition="proven",
        ),
    )
    with pytest.raises(RefusedAuthorityError, match="2782a6e1"):
        _resolve_decider(None, "operator")


def test_backlog_decide_records_with_positional_subject_and_decision(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.graph.cli import cli as backlog_app

    result = runner.invoke(
        backlog_app,
        [
            "decide",
            "x-7d94",
            "move the decision record under backlog",
            "--rationale",
            "subjects are nodes or PRs",
        ],
    )

    assert result.exit_code == 0, result.output
    decision_id = result.stdout.strip().splitlines()[-1]
    assert decision_id.startswith("d-")


def _patch_claim_receipt_identity(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, session_id: str | None
) -> Path:
    from types import SimpleNamespace

    claims_root = tmp_path / "claims"
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(
            session_id=session_id, harness="codex", disposition="proven"
        ),
    )
    # These tests isolate the post-write claim receipt. The operator-only
    # writer gate has dedicated engine and CLI coverage below.
    monkeypatch.setattr("fno.decide.require_operator_session", lambda: None)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda _key: claims_root)
    return claims_root


def test_backlog_decide_receipts_foreign_live_claim(
    root: Path,
    tmp_graph: Path,
    index: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    caller = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    other = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae5"
    _patch_claim_receipt_identity(monkeypatch, tmp_path, caller)
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, *, root: {
            "key": key,
            "state": "live",
            "holder": f"target-session:{other}",
        },
    )

    result = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "rule on another holder"],
    )

    assert result.exit_code == 0, result.output
    assert result.stdout.strip().splitlines()[-1].startswith("d-")
    assert f"target-session:{other}" in result.stderr
    assert "another session holds the node, not this caller" in result.stderr


def test_backlog_decide_receipts_same_caller_live_claim(
    root: Path,
    tmp_graph: Path,
    index: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    caller = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    _patch_claim_receipt_identity(monkeypatch, tmp_path, caller)
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, *, root: {
            "key": key,
            "state": "live",
            "holder": f"target-session:{caller}",
        },
    )

    result = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "rule on my node"],
    )

    assert result.exit_code == 0, result.output
    assert "this caller holds the node" in result.stderr
    assert "another session holds the node" not in result.stderr


def test_backlog_decide_receipts_driver_assigned_claim_uses_target_session_id(
    root: Path,
    tmp_graph: Path,
    index: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    harness_session = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    target_session = "20260823T152504Z-mw12345-abcdef"
    _patch_claim_receipt_identity(monkeypatch, tmp_path, harness_session)
    monkeypatch.setenv("TARGET_SESSION_ID", target_session)
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, *, root: {
            "key": key,
            "state": "live",
            "holder": f"target-session:{target_session}",
        },
    )

    result = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "rule on driver-owned node"],
    )

    assert result.exit_code == 0, result.output
    assert "this caller holds the node" in result.stderr
    assert "another session holds the node" not in result.stderr


def test_backlog_decide_receipts_free_claim(
    root: Path,
    tmp_graph: Path,
    index: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    _patch_claim_receipt_identity(
        monkeypatch, tmp_path, "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    )
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, *, root: {"key": key, "state": "free"},
    )

    result = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "rule on a free node"],
    )

    assert result.exit_code == 0, result.output
    assert "claim state: free" in result.stderr


def test_backlog_decide_receipts_ambiguous_claim_without_comparison(
    root: Path,
    tmp_graph: Path,
    index: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    _patch_claim_receipt_identity(
        monkeypatch, tmp_path, "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    )
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, *, root: {
            "key": key,
            "state": "live",
            "holder": "legacy-holder",
        },
    )

    result = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "rule with legacy holder"],
    )

    assert result.exit_code == 0, result.output
    assert "legacy-holder" in result.stderr
    assert "caller comparison unavailable" in result.stderr


def test_backlog_decide_receipts_unreadable_claim_stays_advisory(
    root: Path,
    tmp_graph: Path,
    index: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    _patch_claim_receipt_identity(
        monkeypatch, tmp_path, "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    )
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, *, root: {
            "key": key,
            "state": "corrupted",
            "error": "invalid yaml",
        },
    )

    result = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "rule despite claim read"],
    )

    assert result.exit_code == 0, result.output
    assert result.stdout.strip().splitlines()[-1].startswith("d-")
    assert "claim state: corrupted" in result.stderr
    assert "caller comparison unavailable" in result.stderr


def test_backlog_decide_receipts_missing_caller_identity_stays_advisory(
    root: Path,
    tmp_graph: Path,
    index: Path,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    _patch_claim_receipt_identity(monkeypatch, tmp_path, None)
    monkeypatch.setattr(
        "fno.claims.core.claim_status",
        lambda key, *, root: {
            "key": key,
            "state": "live",
            "holder": "target-session:019f48e1-5b09-72a0-9bc8-6b364bcf4ae5",
        },
    )

    result = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "rule without caller identity"],
    )

    assert result.exit_code == 0, result.output
    assert "target-session:019f48e1-5b09-72a0-9bc8-6b364bcf4ae5" in result.stderr
    assert "caller comparison unavailable" in result.stderr


def test_backlog_decide_skips_claim_read_for_non_node_subject(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    calls: list[str] = []

    def fail_if_called(key: str, *, root: Path):
        calls.append(key)
        raise AssertionError("claim reader must not run for non-node subjects")

    monkeypatch.setattr("fno.claims.core.claim_status", fail_if_called)
    result = runner.invoke(
        decide_app,
        ["--subject", "area:coordination", "--decision", "rule on an area"],
    )

    assert result.exit_code == 0, result.output
    assert "subject names no graph node" in result.stderr
    assert calls == []


def test_backlog_decisions_reads_a_positional_subject_and_filters_lane(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.graph.cli import cli as backlog_app

    written = runner.invoke(
        backlog_app,
        ["decide", "x-7d94", "coordinate the migration", "--authority", "crown"],
    )
    assert written.exit_code == 0, written.output

    listed = runner.invoke(
        backlog_app, ["decisions", "x-7d94", "--lane", "coord", "--json"]
    )
    assert listed.exit_code == 0, listed.output
    payload = json.loads(listed.stdout)
    assert payload["decisions"][0]["lane"] == "coord"


def test_old_decide_spelling_warns_and_preserves_stdout(
    root: Path, tmp_graph: Path, index: Path
):
    result = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "compatibility ruling"],
    )

    assert result.exit_code == 0, result.output
    assert "fno decide is now `fno backlog decide`" in result.output
    assert result.stdout.strip().splitlines()[-1].startswith("d-")


def test_old_decide_json_shim_keeps_stdout_machine_readable(
    root: Path, tmp_graph: Path, index: Path
):
    written = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "json compatibility ruling"],
    )
    assert written.exit_code == 0, written.output

    result = runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"])

    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["decisions"][0]["decision"] == (
        "json compatibility ruling"
    )
    assert "fno decide is now" in result.stderr


def test_backlog_decide_keeps_subject_and_decision_flag_aliases(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.graph.cli import cli as backlog_app

    result = runner.invoke(
        backlog_app,
        [
            "decide",
            "--subject",
            "x-7d94",
            "--decision",
            "flag alias ruling",
        ],
    )

    assert result.exit_code == 0, result.output
    assert result.stdout.strip().splitlines()[-1].startswith("d-")
    assert "deprecated" in result.output


def test_both_decision_surfaces_preserve_engine_authority_refusal(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    from fno import decide as decide_engine
    from fno.graph.cli import cli as backlog_app

    def refuse(**_kwargs):
        raise decide_engine.RefusedAuthorityError("agent-test")

    monkeypatch.setattr(decide_engine, "record_decision", refuse)

    new_surface = runner.invoke(backlog_app, ["decide", "x-7d94", "new ruling"])
    old_surface = runner.invoke(
        decide_app, ["--subject", "x-7d94", "--decision", "old ruling"]
    )

    assert new_surface.exit_code == 3, new_surface.output
    assert old_surface.exit_code == 3, old_surface.output
    assert "agent-test" in new_surface.output
    assert "agent-test" in old_surface.output


def _node(nid: str, **over) -> dict:
    base = {
        "id": nid,
        "title": f"node {nid}",
        "status": "ready",
        "type": "feature",
        "priority": "p2",
    }
    base.update(over)
    return base


def test_top_level_decide_lazy_entry_points_to_the_shim():
    from fno.cli import LAZY_SUBCOMMANDS

    assert LAZY_SUBCOMMANDS["decide"][0] == "fno.decide.cli:shim_app"


@pytest.fixture
def index(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """The machine-wide decision index, pinned into the sandbox.

    Every test takes this: without it a test writes to the developer's real
    ``~/.fno/decisions.jsonl``, and reads back whatever else is in there.
    """
    path = tmp_path / "state" / "decisions.jsonl"
    monkeypatch.setattr("fno.paths.decisions_jsonl", lambda: path)
    return path


@pytest.fixture
def tmp_graph(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text(
        json.dumps({"entries": [_node("x-7d94", slug="fold-the-inbox")]}, indent=2) + "\n"
    )
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # The guarded metadata reader (decide's read side) resolves through
    # paths.graph_json at call time; pin it to the same hermetic file.
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    # entries_with_archive resolves the archive through fno.paths, which is
    # graph_json().parent / "graph-archive.json"; pin both so the read-through
    # test stays hermetic.
    monkeypatch.setattr(
        "fno.paths.graph_archive_json", lambda: tmp_path / "graph-archive.json"
    )
    return g


@pytest.fixture
def root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A hermetic canonical root for the events journal (FNO_REPO_ROOT hook).

    The journal is pinned as well as the root: the hermetic sandbox sets
    ``FNO_EVENTS_PATH`` to keep an unpathed ``append_event`` out of the real
    checkout, and that pin outranks repo-root resolution, so a test that reads
    ``<root>/.fno/events.jsonl`` back by hand has to name the same file.
    """
    monkeypatch.setenv("FNO_REPO_ROOT", str(tmp_path))
    monkeypatch.setenv("FNO_EVENTS_PATH", str(tmp_path / ".fno" / "events.jsonl"))
    import fno.paths as paths_mod

    paths_mod.resolve_repo_root.cache_clear()
    (tmp_path / ".fno").mkdir(parents=True, exist_ok=True)
    return tmp_path


def _events(root: Path) -> list[dict]:
    return [
        json.loads(line)
        for line in (root / ".fno" / "events.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def _write_decision_index(index: Path, *rows: dict) -> None:
    index.parent.mkdir(parents=True, exist_ok=True)
    index.write_text(
        "".join(
            json.dumps(
                {
                    "type": "operator_decision",
                    "ts": row.pop("ts"),
                    "data": row,
                }
            )
            + "\n"
            for row in rows
        ),
        encoding="utf-8",
    )


def test_coord_expiry_is_derived_from_closed_node_but_law_stays_live(
    root: Path, tmp_graph: Path, index: Path
):
    entries = json.loads(tmp_graph.read_text())
    entries["entries"][0]["completed_at"] = "2026-08-25T00:00:00Z"
    tmp_graph.write_text(json.dumps(entries) + "\n")
    _write_decision_index(
        index,
        {
            "decision_id": "d-coord0001",
            "decision": "coordinate this node",
            "subject": "x-7d94",
            "authority_source": "agent",
            "expiry_ref": {"kind": "node", "node_id": "x-7d94"},
            "ts": "2026-08-20T00:00:00Z",
        },
        {
            "decision_id": "d-law00001",
            "decision": "keep the standing law",
            "subject": "x-7d94",
            "authority_source": "operator",
            "ts": "2026-08-21T00:00:00Z",
        },
    )

    live = runner.invoke(
        decide_app, ["list", "--subject", "x-7d94", "--state", "live", "--json"]
    )
    assert live.exit_code == 0, live.output
    payload = json.loads(live.stdout)
    assert [row["decision_id"] for row in payload["decisions"]] == ["d-law00001"]

    history = runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"])
    assert history.exit_code == 0, history.output
    rows = {row["decision_id"]: row for row in json.loads(history.stdout)["decisions"]}
    assert rows["d-coord0001"]["lifecycle"] == "expired"
    assert rows["d-law00001"]["lifecycle"] == "live"


def test_ambiguous_coord_without_positive_closure_evidence_is_unscoped(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "decision_id": "d-unscoped1",
            "decision": "coordinate an ambiguous PR",
            "subject": "pr-99",
            "authority_source": "agent",
            "ts": "2026-08-20T00:00:00Z",
        }
    )

    live = runner.invoke(decide_app, ["list", "--state", "live", "--json"])
    assert live.exit_code == 0, live.output
    assert json.loads(live.stdout)["decisions"] == []

    history = runner.invoke(decide_app, ["list", "--json"])
    rows = {row["decision_id"]: row for row in json.loads(history.stdout)["decisions"]}
    assert rows["d-unscoped1"]["lifecycle"] == "unscoped"


def test_decide_retract_appends_an_audit_event_and_changes_only_the_projection(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "decision_id": "d-retract01",
            "decision": "coordinate this node",
            "subject": "x-7d94",
            "authority_source": "agent",
            "expiry_ref": {"kind": "node", "node_id": "x-7d94"},
            "ts": "2026-08-20T00:00:00Z",
        }
    )
    before = index.read_bytes()
    from fno.graph.cli import cli as backlog_app

    result = runner.invoke(
        backlog_app,
        [
            "decide-retract",
            "d-retract01",
            "--reason",
            "the coordination window ended",
            "--authority",
            "agent",
        ],
    )
    assert result.exit_code == 0, result.output
    assert index.read_bytes().startswith(before)
    events = _events(root)
    retractions = [event for event in events if event["type"] == "decision_retracted"]
    assert len(retractions) == 1
    assert retractions[0]["data"]["target_decision_id"] == "d-retract01"

    listed = runner.invoke(decide_app, ["list", "--state", "retracted", "--json"])
    assert listed.exit_code == 0, listed.output
    rows = json.loads(listed.stdout)["decisions"]
    assert rows[0]["decision_id"] == "d-retract01"
    assert rows[0]["lifecycle_reason"] == "the coordination window ended"


def test_agent_cannot_retract_law(root: Path, tmp_graph: Path, index: Path):
    _write_decision_index(
        index,
        {
            "decision_id": "d-law-retr1",
            "decision": "standing law",
            "subject": "law-topic",
            "authority_source": "operator",
            "ts": "2026-08-22T00:00:00Z",
        }
    )
    before = index.read_bytes()
    from fno.graph.cli import cli as backlog_app

    result = runner.invoke(
        backlog_app,
        [
            "decide-retract",
            "d-law-retr1",
            "--reason",
            "agent should not legislate",
            "--authority",
            "agent",
        ],
    )
    assert result.exit_code == 3, result.output
    assert index.read_bytes() == before


def test_agent_cannot_supersede_law(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "decision_id": "d-law-sup01",
            "decision": "standing law",
            "subject": "law-topic",
            "authority_source": "operator",
            "ts": "2026-08-22T00:00:00Z",
        }
    )
    result = runner.invoke(
        decide_app,
        [
            "--subject",
            "law-topic",
            "--decision",
            "coordination cannot replace law",
            "--authority",
            "agent",
            "--supersedes",
            "d-law-sup01",
        ],
    )
    assert result.exit_code == 3, result.output
    listed = runner.invoke(decide_app, ["list", "--subject", "law-topic", "--json"])
    rows = json.loads(listed.stdout)["decisions"]
    assert rows[0]["decision_id"] == "d-law-sup01"
    assert rows[0]["lifecycle"] == "live"


def test_reindex_preserves_distinct_retractions_for_one_target(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.decide import reindex
    from fno.graph.cli import cli as backlog_app

    recorded = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "temporary", "--authority", "crown"],
    )
    decision_id = recorded.stdout.strip().splitlines()[-1]
    for reason in ("first reason", "second reason"):
        result = runner.invoke(
            backlog_app,
            ["decide-retract", decision_id, "--reason", reason, "--authority", "agent"],
        )
        assert result.exit_code == 0, result.output
    index.unlink()
    assert reindex(sources=[root / ".fno" / "events.jsonl"])["added"] == 3
    listed = runner.invoke(decide_app, ["list", "--state", "retracted", "--json"])
    row = json.loads(listed.stdout)["decisions"][0]
    assert row["lifecycle_reason"] == "second reason"


def test_review_list_canonicalizes_node_ids_and_slugs(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "decision_id": "d-canon001",
            "decision": "first law",
            "subject": "x-7d94",
            "authority_source": "operator",
            "ts": "2026-08-21T00:00:00Z",
        },
        {
            "decision_id": "d-canon002",
            "decision": "second law",
            "subject": "fold-the-inbox",
            "authority_source": "operator",
            "ts": "2026-08-22T00:00:00Z",
        },
    )
    result = runner.invoke(decide_app, ["list", "--review-list", "--json"])
    assert result.exit_code == 0, result.output
    groups = json.loads(result.stdout)["groups"]
    assert len(groups) == 1
    assert {row["decision_id"] for row in groups[0]["decisions"]} == {
        "d-canon001",
        "d-canon002",
    }


def test_retraction_survives_reindex(root: Path, tmp_graph: Path, index: Path):
    from fno.decide import reindex
    from fno.graph.cli import cli as backlog_app

    recorded = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "temporary", "--authority", "crown"],
    )
    decision_id = recorded.stdout.strip().splitlines()[-1]
    retracted = runner.invoke(
        backlog_app,
        ["decide-retract", decision_id, "--reason", "no longer applies", "--authority", "agent"],
    )
    assert retracted.exit_code == 0, retracted.output
    index.unlink()
    assert reindex(sources=[root / ".fno" / "events.jsonl"])["added"] == 2
    listed = runner.invoke(decide_app, ["list", "--state", "retracted", "--json"])
    assert json.loads(listed.stdout)["decisions"][0]["decision_id"] == decision_id


def test_reindex_counts_decision_and_retraction_keys_once(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.decide import reindex
    from fno.graph.cli import cli as backlog_app

    recorded = runner.invoke(
        decide_app,
        ["--subject", "x-7d94", "--decision", "temporary", "--authority", "crown"],
    )
    decision_id = recorded.stdout.strip().splitlines()[-1]
    retracted = runner.invoke(
        backlog_app,
        ["decide-retract", decision_id, "--reason", "no longer applies", "--authority", "agent"],
    )
    assert retracted.exit_code == 0, retracted.output

    counts = reindex(sources=[root / ".fno" / "events.jsonl"])
    assert counts["added"] == 0
    assert counts["already"] == 2


def test_default_decision_read_retains_history_for_replay(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.events import decision_retracted
    from fno.decide import list_decisions

    entries = json.loads(tmp_graph.read_text())
    entries["entries"][0]["completed_at"] = "2026-08-25T00:00:00Z"
    tmp_graph.write_text(json.dumps(entries) + "\n")
    _write_decision_index(
        index,
        {
            "decision_id": "d-expired01",
            "decision": "expired coordination",
            "subject": "x-7d94",
            "authority_source": "agent",
            "expiry_ref": {"kind": "node", "node_id": "x-7d94"},
            "ts": "2026-08-20T00:00:00Z",
        },
        {
            "decision_id": "d-live0001",
            "decision": "live law",
            "subject": "live-topic",
            "authority_source": "operator",
            "ts": "2026-08-22T00:00:00Z",
        },
        {
            "decision_id": "d-retract1",
            "decision": "retracted law",
            "subject": "retracted-topic",
            "authority_source": "operator",
            "ts": "2026-08-23T00:00:00Z",
        },
        {
            "decision_id": "d-unscoped1",
            "decision": "unscoped coordination",
            "subject": "pr-99",
            "authority_source": "agent",
            "ts": "2026-08-24T00:00:00Z",
        },
    )
    event = decision_retracted(
        target_decision_id="d-retract1",
        subject="retracted-topic",
        reason="withdrawn",
        authority_source="agent",
    )
    with index.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(event) + "\n")

    _, rows, _ = list_decisions()
    assert {row["decision_id"] for row in rows} == {
        "d-expired01",
        "d-live0001",
        "d-retract1",
        "d-unscoped1",
    }


def test_missing_supersession_target_refuses_before_recording(
    root: Path, tmp_graph: Path, index: Path
):
    result = runner.invoke(
        decide_app,
        [
            "--subject",
            "x-7d94",
            "--decision",
            "coordination with missing target",
            "--authority",
            "agent",
            "--supersedes",
            "d-missing-law",
        ],
    )
    assert result.exit_code != 0, result.output
    assert not index.exists()
    assert not (root / ".fno" / "events.jsonl").exists()


def test_retraction_origin_is_floored_before_event_persistence(
    root: Path, tmp_graph: Path, index: Path, tmp_path: Path, monkeypatch
):
    _patch_claim_receipt_identity(monkeypatch, tmp_path, "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4")
    _write_decision_index(
        index,
        {
            "decision_id": "d-origin01",
            "decision": "coordination row",
            "subject": "x-7d94",
            "authority_source": "agent",
            "ts": "2026-08-22T00:00:00Z",
        },
    )
    result = runner.invoke(
        decide_app,
        [
            "retract",
            "d-origin01",
            "--reason",
            "withdrawn",
            "--authority",
            "agent",
            "--origin",
            "operator",
        ],
    )
    assert result.exit_code == 0, result.output
    event = [event for event in _events(root) if event["type"] == "decision_retracted"][-1]
    assert event["data"]["origin"] == "peer"


def test_retract_reports_unreadable_index_without_traceback(
    root: Path, tmp_graph: Path, index: Path
):
    index.parent.mkdir(parents=True, exist_ok=True)
    index.symlink_to(index.parent / "missing-decisions.jsonl")
    result = runner.invoke(
        decide_app,
        ["retract", "d-anything", "--reason", "withdrawn", "--authority", "agent"],
    )
    assert result.exit_code == 1
    assert "cannot read the decision index" in result.output
    assert "Traceback" not in result.output


def test_review_list_reports_multiple_live_rulings_without_picking_a_winner(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "decision_id": "d-review001",
            "decision": "keep the first option",
            "subject": "billing-policy",
            "authority_source": "operator",
            "ts": "2026-08-21T00:00:00Z",
        },
        {
            "decision_id": "d-review002",
            "decision": "keep the second option",
            "subject": "billing-policy",
            "authority_source": "operator",
            "ts": "2026-08-22T00:00:00Z",
        },
        {
            "decision_id": "d-subjectless",
            "decision": "legacy answer",
            "authority_source": "banana",
            "ts": "2026-08-20T00:00:00Z",
        },
    )

    result = runner.invoke(decide_app, ["list", "--review-list", "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert [group["subject"] for group in payload["groups"]] == [
        "billing-policy",
        "(unscoped)",
    ]
    assert [
        row["decision_id"] for row in payload["groups"][0]["decisions"]
    ] == ["d-review002", "d-review001"]
    unscoped = next(
        group for group in payload["groups"] if group["subject"] == "(unscoped)"
    )
    assert [row["decision_id"] for row in unscoped["decisions"]] == [
        "d-subjectless"
    ]
    assert unscoped["decisions"][0]["decision"] == "legacy answer"
    assert payload["data_quality"] == {
        "subjectless": 1,
        "invalid_authority": 1,
    }


def test_decisions_output_writes_the_full_requested_json_report(
    root: Path, tmp_graph: Path, index: Path, tmp_path: Path
):
    _write_decision_index(
        index,
        *[
            {
                "decision_id": f"d-output{number}",
                "decision": f"decision {number}",
                "subject": "output-subject",
                "authority_source": "operator",
                "ts": f"2026-08-2{number + 1}T00:00:00Z",
            }
            for number in range(3)
        ],
    )
    target = tmp_path / "reports" / "decisions.json"
    result = runner.invoke(
        decide_app,
        ["list", "--subject", "output-subject", "--limit", "1", "--output", str(target)],
    )
    assert result.exit_code == 0, result.output
    receipt = json.loads(result.stdout)
    assert receipt["output_path"] == str(target)
    assert receipt["bytes_written"] == target.stat().st_size
    report = json.loads(target.read_text())
    assert report["total"] == 3
    assert len(report["decisions"]) == 3


def test_decisions_output_refuses_an_unknown_format(tmp_path: Path):
    result = runner.invoke(decide_app, ["list", "--output", str(tmp_path / "report.txt")])
    assert result.exit_code == 2
    assert "--format" in result.output or "suffix" in result.output


def test_subjectless_outstanding_answer_gets_a_reserved_recovery_subject(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.outstanding.cli import outstanding_app

    asked = runner.invoke(outstanding_app, ["ask", "which lane owns this question?"])
    question_id = asked.stdout.strip().splitlines()[-1]
    cleared = runner.invoke(
        outstanding_app,
        ["clear", question_id, "--answer", "the coordination lane"],
    )
    assert cleared.exit_code == 0, cleared.output
    rows, _ = __import__("fno.decide", fromlist=["_read_index"])._read_index(index)
    decision = next(row for row in rows if row.get("decision_id"))
    assert decision["subject"] == f"question:{question_id}"


def test_record_appends_the_event_and_projects_onto_the_node(root: Path, tmp_graph: Path, index: Path):
    """fno decide writes the event AND the graph projection."""
    res = runner.invoke(
        decide_app,
        [
            "--subject", "x-7d94",
            "--decision", "fold every project's inbox",
            "--rationale", "a fold is a read; you do not migrate before you can see",
            "--option", "fold first",
            "--option", "migrate first",
        ],
    )
    assert res.exit_code == 0, res.output
    did = res.stdout.strip().splitlines()[-1]
    assert did.startswith("d-"), "stdout carries the new decision id"

    events = [e for e in _events(root) if e["type"] == "operator_decision"]
    assert len(events) == 1
    data = events[0]["data"]
    assert data["decision_id"] == did
    assert data["subject"] == "x-7d94"
    assert data["decision"] == "fold every project's inbox"
    # decided_by, not authority_source. Authority is legitimately ABSENT for a
    # caller with no session identity and no terminal: that state refuses to
    # claim one, and the reader's `unattributed` lane covers it.
    assert data["decided_by"]

    entry = json.loads(tmp_graph.read_text())["entries"][0]
    assert [d["decision_id"] for d in entry["decisions"]] == [did]
    assert entry["decisions"][0]["rationale"].startswith("a fold is a read")
    assert entry["decisions"][0]["options"] == ["fold first", "migrate first"]
    assert entry["decisions"][0]["superseded_by"] is None

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "options: fold first, migrate first" in listed.output


def test_new_coord_record_stamps_an_exact_node_expiry_reference(
    root: Path, tmp_graph: Path, index: Path
):
    result = runner.invoke(
        decide_app,
        [
            "--subject",
            "x-7d94",
            "--decision",
            "coordinate the node",
            "--authority",
            "crown",
        ],
    )
    assert result.exit_code == 0, result.output
    event = _events(root)[0]
    assert event["data"]["expiry_ref"] == {"kind": "node", "node_id": "x-7d94"}


def test_list_returns_decisions_newest_first(root: Path, tmp_graph: Path, index: Path):
    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "first"])
    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "second"])

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert listed.output.index("second") < listed.output.index("first")

    as_json = runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"])
    payload = json.loads(as_json.stdout)
    assert [d["decision"] for d in payload["decisions"]] == ["second", "first"]


def test_supersession_marks_the_older_decision(root: Path, tmp_graph: Path, index: Path):
    """Two decisions on one subject order themselves; the older one
    is marked, not hidden."""
    first = runner.invoke(
        decide_app, ["--subject", "x-7d94", "--decision", "migrate now"]
    ).stdout.strip().splitlines()[-1]
    second = runner.invoke(
        decide_app,
        [
            "--subject",
            "x-7d94",
            "--decision",
            "fold first",
            "--supersedes",
            first.upper(),
        ],
    )
    assert second.exit_code == 0, second.output

    entry = json.loads(tmp_graph.read_text())["entries"][0]
    by_id = {d["decision_id"]: d for d in entry["decisions"]}
    assert by_id[first]["superseded_by"] is not None
    assert by_id[first]["superseded_by"].startswith("d-")

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "superseded by" in listed.output, "the render marks the superseded row"


def test_list_survives_archiving_of_the_subject(root: Path, tmp_graph: Path, index: Path):
    """A decision recorded pre-archive is still listable post-archive
    through entries_with_archive."""
    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "fold first"])
    entries = json.loads(tmp_graph.read_text())["entries"]
    archive = tmp_graph.parent / "graph-archive.json"
    archive.write_text(json.dumps({"entries": entries}) + "\n")
    tmp_graph.write_text(json.dumps({"entries": []}) + "\n")

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "fold first" in listed.output


def test_list_of_a_subject_with_nothing_on_record_is_a_successful_read(
    root: Path, tmp_graph: Path, index: Path
):
    """Exit 0, not 1. A read that answered "none" ran; only a read that could
    not run is a failure, and the two must not share an exit code."""
    listed = runner.invoke(decide_app, ["list", "--subject", "x-nope"])
    assert listed.exit_code == 0, listed.output
    # A statement about the QUERY, never about the world. The old wording
    # ("no decisions recorded") read as a fact about the store, and a reader
    # acted on it.
    assert "no decision is indexed under the subject 'x-nope'" in listed.output


def test_lifecycle_filtered_empty_list_reports_all_state_recovery(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "ts": "2026-08-21T00:00:00Z",
            "decision_id": "d-history01",
            "decision": "standing answer",
            "subject": "x-history",
            "authority_source": "operator",
        },
    )

    listed = runner.invoke(
        decide_app, ["list", "--subject", "x-history", "--state", "expired"]
    )
    assert listed.exit_code == 0, listed.output
    assert "0 expired decisions" in listed.output
    assert "fno backlog decisions 'x-history' --state all" in listed.output


def test_state_filter_empty_preserves_requested_lane_in_recovery(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "ts": "2026-08-21T00:00:00Z",
            "decision_id": "d-stateLane1",
            "decision": "standing answer",
            "subject": "force-push",
            "authority_source": "operator",
        },
    )

    listed = runner.invoke(
        decide_app,
        [
            "list",
            "--subject",
            "force-push",
            "--lane",
            "law",
            "--state",
            "expired",
        ],
    )
    assert listed.exit_code == 0, listed.output
    assert "fno backlog decisions 'force-push' --lane law --state all" in listed.output


def test_empty_lane_filter_without_subject_builds_state_all_recovery(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "ts": "2026-08-21T00:00:00Z",
            "decision_id": "d-laneall1",
            "decision": "standing answer",
            "subject": "force-push",
            "authority_source": "operator",
        },
    )

    listed = runner.invoke(decide_app, ["list", "--lane", "coord"])
    assert listed.exit_code == 0, listed.output
    assert "0 coord decisions for '(all)'" in listed.output
    assert "fno backlog decisions --state all" in listed.output
    assert "fno backlog decisions --lane coord --state all" not in listed.output
    assert "fno backlog decisions '(all)'" not in listed.output


def test_empty_lane_filter_recovery_drops_lane_when_lane_caused_zero(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "ts": "2026-08-21T00:00:00Z",
            "decision_id": "d-lanesub1",
            "decision": "standing answer",
            "subject": "force-push",
            "authority_source": "operator",
        },
    )

    listed = runner.invoke(
        decide_app, ["list", "--subject", "force-push", "--lane", "coord"]
    )
    assert listed.exit_code == 0, listed.output
    assert "fno backlog decisions 'force-push' --state all" in listed.output
    assert "fno backlog decisions 'force-push' --lane coord --state all" not in listed.output


def test_state_plus_missing_lane_drops_lane_from_recovery(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "ts": "2026-08-21T00:00:00Z",
            "decision_id": "d-stateNoLane",
            "decision": "standing answer",
            "subject": "force-push",
            "authority_source": "operator",
        },
    )

    listed = runner.invoke(
        decide_app,
        [
            "list",
            "--subject",
            "force-push",
            "--lane",
            "coord",
            "--state",
            "expired",
        ],
    )
    assert listed.exit_code == 0, listed.output
    assert "fno backlog decisions 'force-push' --state all" in listed.output
    assert "fno backlog decisions 'force-push' --lane coord --state all" not in listed.output


def test_record_without_a_resolvable_subject_still_writes_the_event(
    root: Path, tmp_graph: Path, index: Path
):
    """A subject that names a file or area, not a node, loses the projection
    but keeps the durable event; the verb says so on stderr."""
    res = runner.invoke(
        decide_app, ["--subject", "docs/architecture.md", "--decision", "keep it"]
    )
    assert res.exit_code == 0, res.output
    events = [e for e in _events(root) if e["type"] == "operator_decision"]
    assert len(events) == 1
    assert events[0]["data"]["subject"] == "docs/architecture.md"
    assert "no node" in res.output.lower() or "projection" in res.output.lower()


def test_decisions_default_applies_on_read_for_legacy_rows(tmp_path: Path):
    """The decisions default lives in _apply_graph_defaults, the one migration
    seam: a pre-decision graph row reads [] without a rewrite."""
    from fno.graph.store import _apply_graph_defaults

    entries = _apply_graph_defaults([{"id": "x-old", "title": "old", "status": "ready"}])
    assert entries[0]["decisions"] == []


# --- recall parity: the reader takes every subject the writer takes ---------


@pytest.mark.parametrize(
    "subject",
    ["x-7d94", "fold-the-inbox", "pr-923", "docs/foo.md", "the mail bus"],
    ids=["node-id", "slug", "pr", "path", "area"],
)
def test_recall_answers_every_subject_shape_the_help_promises(
    root: Path, tmp_graph: Path, index: Path, subject: str
):
    """The defect, named. `--help` says the subject may be a node id/slug, a
    file, or an area; the writer took all of them and the reader took one, so a
    ruling about `pr-923` was written, receipted, and lost.

    Asserts the returned decision_id comes back - a positive marker. Restore the
    graph-only reader and the pr, path and area cases fail.
    """
    written = runner.invoke(
        decide_app, ["--subject", subject, "--decision", f"ruling about {subject}"]
    )
    assert written.exit_code == 0, written.output
    did = written.stdout.strip().splitlines()[-1]

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", subject, "--json"]).stdout
    )
    assert did in [d["decision_id"] for d in payload["decisions"]]


@pytest.mark.parametrize(
    "recorded_as,queried_as",
    [
        ("Fold-The-Inbox", "x-7d94"),
        ("fold-the-inbox", "x-7d94"),
        ("x-7d94", "fold-the-inbox"),
    ],
    ids=["mixed-case-slug", "slug", "id-queried-by-slug"],
)
def test_two_spellings_of_one_node_answer_each_other(
    root: Path, tmp_graph: Path, index: Path, recorded_as: str, queried_as: str
):
    """BOTH sides expand, not just the query.

    The operator records under whatever spelling was in front of them, and the
    receipt then prints the canonical id as the way back. A reader that expands
    only the query sends them to a command that returns nothing, which is this
    PR's own defect wearing a different word.

    Both sides run through the SAME resolver, so whichever spellings it accepts,
    the writer and the reader accept the same set. The bare-hex tier is left out
    here on purpose: it depends on the configured node prefix, so it would test
    the resolver's config rather than this symmetry.
    """
    written = runner.invoke(
        decide_app, ["--subject", recorded_as, "--decision", "one ruling"]
    )
    did = written.stdout.strip().splitlines()[-1]

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", queried_as, "--json"]).stdout
    )
    assert [d["decision_id"] for d in payload["decisions"]] == [did]


def test_recall_is_exact_never_a_prefix_match(root: Path, tmp_graph: Path, index: Path):
    """A decision about pr-92 must not answer a query for pr-921. Set
    membership on the recorded string, never a fuzzy match."""
    runner.invoke(decide_app, ["--subject", "pr-92", "--decision", "the short one"])
    on_921 = runner.invoke(decide_app, ["list", "--subject", "pr-921", "--json"])
    assert json.loads(on_921.stdout)["decisions"] == []

    on_92 = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-92", "--json"]).stdout
    )
    assert [d["decision"] for d in on_92["decisions"]] == ["the short one"]


def test_supersession_is_derived_from_index_rows_alone(
    root: Path, tmp_graph: Path, index: Path
):
    """The graph projection stamped superseded_by under the lock. For a subject
    that names no node there is no projection, so the reader must derive it."""
    first = runner.invoke(
        decide_app, ["--subject", "pr-922", "--decision", "merge it"]
    ).stdout.strip().splitlines()[-1]
    second = runner.invoke(
        decide_app,
        ["--subject", "pr-922", "--decision", "hold it", "--supersedes", first],
    )
    assert second.exit_code == 0, second.output
    newer = second.stdout.strip().splitlines()[-1]

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-922", "--json"]).stdout
    )
    by_id = {d["decision_id"]: d for d in payload["decisions"]}
    assert by_id[first]["superseded_by"] == newer
    assert by_id[newer]["superseded_by"] is None

    listed = runner.invoke(decide_app, ["list", "--subject", "pr-922"])
    assert f"[superseded by {newer}]" in listed.output


def test_a_subjectless_decision_is_reachable_only_without_a_subject(
    root: Path, tmp_graph: Path, index: Path
):
    """`fno outstanding clear --answer` on a question naming no node records a
    decision with subject=None. A subject-less list is the only way to it."""
    from fno.outstanding.cli import outstanding_app

    asked = runner.invoke(outstanding_app, ["ask", "which lane owns the retry?"])
    assert asked.exit_code == 0, asked.output
    qid = asked.stdout.strip().splitlines()[-1]

    cleared = runner.invoke(
        outstanding_app, ["clear", qid, "--answer", "the dispatcher owns it"]
    )
    assert cleared.exit_code == 0, cleared.output

    payload = json.loads(runner.invoke(decide_app, ["list", "--json"]).stdout)
    assert "the dispatcher owns it" in [d["decision"] for d in payload["decisions"]]
    assert payload["subject"] == "(all)"


def test_limit_caps_the_newest_and_zero_means_no_cap(
    root: Path, tmp_graph: Path, index: Path
):
    for n in range(4):
        runner.invoke(decide_app, ["--subject", "pr-900", "--decision", f"call {n}"])

    capped = json.loads(
        runner.invoke(
            decide_app, ["list", "--subject", "pr-900", "--limit", "2", "--json"]
        ).stdout
    )
    assert [d["decision"] for d in capped["decisions"]] == ["call 3", "call 2"]

    uncapped = json.loads(
        runner.invoke(
            decide_app, ["list", "--subject", "pr-900", "--limit", "0", "--json"]
        ).stdout
    )
    assert len(uncapped["decisions"]) == 4


@pytest.mark.parametrize(
    "authority,ts,question_id,expected",
    [
        ("operator", "2026-08-21T00:00:01Z", None, "law"),
        ("operator", "2026-08-20T23:59:59Z", "q-human", "unattributed"),
        ("agent", "2026-08-20T23:59:59Z", None, "coord"),
        ("beastmode", "2026-08-20T23:59:59Z", None, "grant"),
        ("operator", "2026-08-20T23:59:59Z", None, "unattributed"),
    ],
    ids=["post-cutover-law", "legacy-question", "coord", "grant", "legacy"],
)
def test_list_derives_and_filters_authority_lanes_in_the_engine(
    index: Path,
    authority: str,
    ts: str,
    question_id: str | None,
    expected: str,
):
    from fno.decide import list_decisions

    row = {
        "ts": ts,
        "decision_id": f"d-{expected}",
        "subject": "pr-923",
        "decision": f"{expected} ruling",
        "decided_by": "someone",
        "authority_source": authority,
    }
    if question_id:
        row["question_id"] = question_id
    _write_decision_index(index, row)

    _, decisions, _ = list_decisions("pr-923", lane=expected, state="all")
    assert [d["lane"] for d in decisions] == [expected]
    _, excluded, _ = list_decisions(
        "pr-923",
        lane="unattributed" if expected != "unattributed" else "law",
        state="all",
    )
    assert excluded == []


@pytest.mark.parametrize(
    "authority,ts,question_id,marker",
    [
        ("operator", "2026-08-21T00:00:01Z", None, "LIVE  LAW"),
        ("agent", "2026-08-20T23:59:59Z", None, "UNSCOPED  coord"),
        ("beastmode", "2026-08-20T23:59:59Z", None, "LIVE  grant"),
        ("operator", "2026-08-20T23:59:59Z", None, "UNSCOPED  unattributed"),
    ],
)
def test_human_render_leads_with_the_authority_lane(
    index: Path,
    authority: str,
    ts: str,
    question_id: str | None,
    marker: str,
):
    row = {
        "ts": ts,
        "decision_id": "d-render",
        "subject": "pr-923",
        "decision": "render me",
        "decided_by": "someone",
        "authority_source": authority,
    }
    if question_id:
        row["question_id"] = question_id
    _write_decision_index(index, row)

    rendered = runner.invoke(decide_app, ["list", "--subject", "pr-923"])
    assert rendered.exit_code == 0, rendered.output
    assert rendered.stdout.startswith(f"{marker}  d-render")


def test_json_rows_carry_the_derived_lane(index: Path):
    _write_decision_index(
        index,
        {
            "ts": "2026-08-20T23:59:59Z",
            "decision_id": "d-json",
            "subject": "pr-923",
            "decision": "coordinate",
            "decided_by": "worker",
            "authority_source": "agent",
        },
    )

    result = runner.invoke(
        decide_app, ["list", "--subject", "pr-923", "--lane", "coord", "--json"]
    )
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["decisions"][0]["lane"] == "coord"


def test_legacy_operator_rows_stay_byte_identical_and_empty_law_is_positive(
    index: Path,
):
    _write_decision_index(
        index,
        {
            "ts": "2026-08-20T23:59:59Z",
            "decision_id": "d-legacy",
            "subject": "pr-923",
            "decision": "old writer stamped this operator",
            "decided_by": "operator",
            "authority_source": "operator",
        },
    )
    before = index.read_bytes()

    legacy = runner.invoke(
        decide_app, ["list", "--subject", "pr-923", "--lane", "unattributed"]
    )
    assert legacy.exit_code == 0, legacy.output
    assert legacy.stdout.startswith("UNSCOPED  unattributed  d-legacy")

    law = runner.invoke(
        decide_app, ["list", "--subject", "pr-923", "--lane", "law"]
    )
    assert law.exit_code == 0, law.output
    assert "0 law decisions" in law.output
    # The law-only branch was merged into the general lane one, so this now
    # counts EVERY lane the subject holds rather than the pre-cutover rows
    # alone. The more specific branch was giving the less complete answer.
    assert "1 unattributed" in law.output
    assert "pre-cutover" in law.output
    assert index.read_bytes() == before


# --- reindex: the records already on disk become readable -------------------


def test_reindex_recovers_journal_records_and_is_idempotent(
    root: Path, tmp_graph: Path, index: Path
):
    """The backfill is the whole point: without it the fix helps no record that
    already exists."""
    from fno.decide import reindex

    journal = root / ".fno" / "events.jsonl"
    for subject in ("pr-923", "pr-921", "x-6352-worktree"):
        runner.invoke(decide_app, ["--subject", subject, "--decision", f"on {subject}"])
    index.unlink()  # the state before the index existed: journal only

    counts = reindex(sources=[journal])
    assert counts["added"] == 3, counts
    for subject in ("pr-923", "pr-921", "x-6352-worktree"):
        payload = json.loads(
            runner.invoke(decide_app, ["list", "--subject", subject, "--json"]).stdout
        )
        assert [d["decision"] for d in payload["decisions"]] == [f"on {subject}"]

    again = reindex(sources=[journal])
    assert again["added"] == 0 and again["already"] == 3, again


def test_reindex_reads_one_journal_once_through_a_symlink(
    root: Path, tmp_graph: Path, index: Path, tmp_path: Path
):
    """A linked checkout points .fno/events.jsonl at the canonical file. The
    (st_dev, st_ino) dedupe is what keeps a 54 MB journal from being read once
    per name it is reachable under."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    index.unlink()

    journal = root / ".fno" / "events.jsonl"
    link = tmp_path / "linked-events.jsonl"
    link.symlink_to(journal)

    counts = reindex(sources=[journal, link])
    assert counts["added"] == 1, counts
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"]).stdout
    )
    assert len(payload["decisions"]) == 1


def test_reindex_recovers_a_projection_row_that_stored_no_subject(
    root: Path, tmp_graph: Path, index: Path
):
    """The oldest projection on this machine predates the subject field. The
    row lives ON the node, so the node is the subject; without that fallback
    the recovered decision answers no query at all."""
    from fno.decide import reindex

    entries = json.loads(tmp_graph.read_text())["entries"]
    entries[0]["decisions"] = [
        {
            "decision_id": "d-legacy1",
            "decision": "fold every project's inbox first",
            "decided_by": "operator",
            "ts": "2026-08-15T00:31:06.178560Z",
        }
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    assert reindex(sources=[])["added"] == 1
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"]).stdout
    )
    assert [d["decision_id"] for d in payload["decisions"]] == ["d-legacy1"]


def test_reindex_folds_every_project_root_the_graph_names(
    root: Path, tmp_graph: Path, index: Path, tmp_path: Path
):
    """A free-text decision recorded from another repo has no graph projection
    to recover it. A backfill that folds only the invoking repo leaves exactly
    the records this verb exists to find."""
    from fno.decide import _default_journals, reindex

    sibling = tmp_path / "other-repo"
    (sibling / ".fno").mkdir(parents=True)
    entries = json.loads(tmp_graph.read_text())["entries"]
    entries.append(_node("x-9999", cwd=str(sibling)))
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    from fno.decide import record_decision

    did = record_decision(
        decision="the sibling repo ruled this", subject="pr-777", events_root=sibling
    )["decision_id"]
    index.unlink()

    assert any(sibling in p.parents for p in _default_journals()), _default_journals()
    assert reindex()["added"] >= 1
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-777", "--json"]).stdout
    )
    assert [d["decision_id"] for d in payload["decisions"]] == [did]


@pytest.mark.parametrize(
    "torn",
    ['{"type":"operator_decision","data":{"decision_id":"d-tru', '{"ts":"2026-'],
    ids=["tear-after-the-type", "tear-before-the-type"],
)
def test_a_damaged_index_row_is_skipped_but_never_skipped_silently(
    root: Path, tmp_graph: Path, index: Path, capsys, torn: str
):
    """A truncated append must not make an unreadable record and an empty one
    look the same. The good rows still come back.

    Both tear points, because a crash can end the line before the type string
    ever appears, and a substring prefilter would drop exactly that one without
    ever counting it.
    """
    from fno.decide import _read_index

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    with index.open("a", encoding="utf-8") as fh:
        fh.write(torn + "\n")

    capsys.readouterr()
    rows, _ = _read_index(index)
    err = capsys.readouterr().err
    assert [r["decision"] for r in rows] == ["merged"], "one bad row costs no others"
    assert "1 damaged row(s)" in err
    assert "fno backlog decide-reindex" in err


def test_reindex_drops_the_damaged_row_so_the_warning_can_clear(
    root: Path, tmp_graph: Path, index: Path, capsys
):
    """The index never rotates, so a torn line stays forever and reprints the
    same notice on every read. The recovery the notice names must succeed."""
    from fno.decide import _read_index, reindex

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    with index.open("a", encoding="utf-8") as fh:
        fh.write('{"type":"operator_decision","data":{"decision_id":"d-tru\n')

    counts = reindex(sources=[root / ".fno" / "events.jsonl"])
    assert counts["repaired"] == 1, counts

    capsys.readouterr()
    rows, _ = _read_index(index)
    assert [r["decision"] for r in rows] == ["merged"]
    assert "damaged row(s)" not in capsys.readouterr().err


def test_reindex_counts_a_journal_row_and_its_own_projection_once(
    root: Path, tmp_graph: Path, index: Path
):
    """A first backfill must not report rows as already indexed. The journal
    row and its projection are one decision seen twice in one run."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "fold first"])
    index.unlink()

    counts = reindex(sources=[root / ".fno" / "events.jsonl"])
    assert (counts["added"], counts["already"]) == (1, 0), counts

    # And on the SECOND run it is one already-indexed decision, not two
    # sightings of one.
    again = reindex(sources=[root / ".fno" / "events.jsonl"])
    assert (again["added"], again["already"]) == (0, 1), again


def test_a_failed_index_write_names_reindex_and_never_a_retry(
    root: Path, tmp_graph: Path, index: Path, monkeypatch: pytest.MonkeyPatch
):
    """By the time the index write fails, the durable event has landed. Telling
    the operator to re-run would record one ruling twice."""
    import fno.events as events_mod

    real = events_mod.append_event

    def boom(event, events_path=None, **kw):
        if events_path is not None and Path(events_path) == index:
            raise OSError("read-only file system")
        return real(event, events_path=events_path, **kw)

    monkeypatch.setattr(events_mod, "append_event", boom)
    res = runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    assert res.exit_code == 1
    assert "fno backlog decide-reindex" in res.output
    assert "Do NOT re-run decide" in res.output
    assert "recorded d-" in res.output, "the id it already holds"


def test_a_legacy_projection_row_with_no_ts_sorts_oldest(
    root: Path, tmp_graph: Path, index: Path
):
    """The event builder stamps NOW, which would float a legacy ruling to the
    top of a list whose whole promise is newest-first."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "recent"])
    entries = json.loads(tmp_graph.read_text())["entries"]
    entries[0]["decisions"].append(
        {"decision_id": "d-nots1", "decision": "ancient", "decided_by": "operator"}
    )
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    assert reindex(sources=[])["added"] == 1
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"]).stdout
    )
    assert [d["decision"] for d in payload["decisions"]] == ["recent", "ancient"]


def test_limit_says_so_when_it_truncates(root: Path, tmp_graph: Path, index: Path):
    """A silent cut on a recall verb is the same lie as a missing record."""
    for n in range(3):
        runner.invoke(decide_app, ["--subject", "pr-900", "--decision", f"call {n}"])

    payload = json.loads(
        runner.invoke(
            decide_app, ["list", "--subject", "pr-900", "--limit", "2", "--json"]
        ).stdout
    )
    assert (payload["total"], payload["truncated"]) == (3, True)

    human = runner.invoke(decide_app, ["list", "--subject", "pr-900", "--limit", "2"])
    assert "showing 2 of 3" in human.output


def test_a_torn_multibyte_append_stays_readable_and_recoverable(
    root: Path, tmp_graph: Path, index: Path
):
    """A crash can split a multi-byte character mid-append. A strict read
    raises on the WHOLE file, taking every good row with it - and breaking the
    reindex the damaged-row warning names as the cure."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    with index.open("ab") as fh:
        fh.write(b'{"type":"operator_decision","data":{"decision":"caf\xc3\n')

    listed = runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"])
    assert listed.exit_code == 0, listed.output
    assert [d["decision"] for d in json.loads(listed.stdout)["decisions"]] == ["merged"]

    assert reindex(sources=[root / ".fno" / "events.jsonl"])["repaired"] == 1
    assert index.with_suffix(".jsonl.corrupt").exists(), "the drop is reversible"


def test_one_unusable_projection_row_does_not_abort_the_backfill(
    root: Path, tmp_graph: Path, index: Path
):
    """The event builder slices strings and validates. An eager list build
    aborts on the first bad row and loses the journal half of the fold too."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "from the journal"])
    index.unlink()
    entries = json.loads(tmp_graph.read_text())["entries"]
    entries[0]["decisions"] = [
        {"decision_id": "d-bad001", "decision": "unusable", "rationale": 123},
        {"decision_id": "d-good01", "decision": "usable", "subject": "x-7d94"},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")

    counts = reindex(sources=[root / ".fno" / "events.jsonl"])
    assert counts["added"] == 2, counts
    for subject, decision in (("pr-923", "from the journal"), ("x-7d94", "usable")):
        payload = json.loads(
            runner.invoke(decide_app, ["list", "--subject", subject, "--json"]).stdout
        )
        assert decision in [d["decision"] for d in payload["decisions"]]


def test_an_unreachable_index_is_a_failed_read_not_an_empty_one(
    root: Path, tmp_graph: Path, index: Path
):
    """Path.exists() answers False for a dangling symlink, which would turn an
    unreachable store into "no decisions recorded" on exit 0."""
    index.parent.mkdir(parents=True, exist_ok=True)
    index.symlink_to(index.parent / "gone.jsonl")

    listed = runner.invoke(decide_app, ["list", "--subject", "pr-923"])
    assert listed.exit_code == 1, listed.output
    assert "cannot read the decision index" in listed.output


def test_the_second_producer_also_refuses_to_ask_for_a_retry(
    root: Path, tmp_graph: Path, index: Path, monkeypatch: pytest.MonkeyPatch
):
    """`fno outstanding clear --answer` is the other operator_decision writer.
    A guard on one of two producer paths is decorative."""
    import fno.events as events_mod
    from fno.outstanding.cli import outstanding_app

    qid = runner.invoke(
        outstanding_app, ["ask", "which lane owns the retry?"]
    ).stdout.strip().splitlines()[-1]

    real = events_mod.append_event

    def boom(event, events_path=None, **kw):
        if events_path is not None and Path(events_path) == index:
            raise OSError("read-only file system")
        return real(event, events_path=events_path, **kw)

    monkeypatch.setattr(events_mod, "append_event", boom)
    res = runner.invoke(outstanding_app, ["clear", qid, "--answer", "the dispatcher"])
    assert res.exit_code == 1
    assert "fno backlog decide-reindex" in res.output
    assert "records the same ruling a second time" in res.output


def test_equal_timestamps_do_not_invert_newest_first(
    root: Path, tmp_graph: Path, index: Path
):
    """Every legacy projection row shares the same no-ts fallback, and a stable
    sort keeps file order for ties - silently reversing the stated contract."""
    from fno.decide import reindex

    entries = json.loads(tmp_graph.read_text())["entries"]
    entries[0]["decisions"] = [
        {"decision_id": "d-aaa001", "decision": "first", "subject": "x-7d94"},
        {"decision_id": "d-bbb002", "decision": "second", "subject": "x-7d94"},
    ]
    tmp_graph.write_text(json.dumps({"entries": entries}) + "\n")
    assert reindex(sources=[])["added"] == 2

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"]).stdout
    )
    assert [d["decision_id"] for d in payload["decisions"]] == ["d-bbb002", "d-aaa001"]


def test_a_torn_journal_does_not_make_reindex_impossible(
    root: Path, tmp_graph: Path, index: Path
):
    """The fold reads every journal the graph names. One torn multi-byte append
    in any of them must not take out the recovery command itself."""
    from fno.decide import reindex

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    index.unlink()
    journal = root / ".fno" / "events.jsonl"
    with journal.open("ab") as fh:
        fh.write(b'{"type":"other","data":{"x":"caf\xc3\n')

    assert reindex(sources=[journal])["added"] == 1
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"]).stdout
    )
    assert [d["decision"] for d in payload["decisions"]] == ["merged"]


def test_a_failed_projection_never_reports_a_lost_capture(
    root: Path, tmp_graph: Path, index: Path, monkeypatch: pytest.MonkeyPatch
):
    """The projection is the third of three writes. Both durable stores already
    hold the decision, so failing here would invite the duplicate retry."""
    import fno.graph.store as gs

    def boom(*a, **kw):
        raise SystemExit(1)  # what locked_mutate_graph does on a corrupt graph

    monkeypatch.setattr(gs, "locked_mutate_graph", boom)
    res = runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "fold first"])
    assert res.exit_code == 0, res.output
    assert "graph projection failed" in res.output

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "x-7d94", "--json"]).stdout
    )
    assert [d["decision"] for d in payload["decisions"]] == ["fold first"]


def test_a_node_subject_folds_case_in_both_directions(
    root: Path, tmp_graph: Path, index: Path
):
    """The doc promises every spelling of a node, "any case". The resolver's id
    tier is case-sensitive, so only the query side folded before."""
    did = runner.invoke(
        decide_app, ["--subject", "X-7D94", "--decision", "shouted"]
    ).stdout.strip().splitlines()[-1]

    for query in ("x-7d94", "fold-the-inbox", "X-7D94"):
        payload = json.loads(
            runner.invoke(decide_app, ["list", "--subject", query, "--json"]).stdout
        )
        assert did in [d["decision_id"] for d in payload["decisions"]], query


def test_a_non_node_subject_is_case_insensitive_but_still_exact(
    root: Path, tmp_graph: Path, index: Path
):
    """`pr-<n>` is advertised as a first-class subject with no spelling rule,
    and a node subject already gets case-folding through the resolver."""
    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])

    found = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "PR-923", "--json"]).stdout
    )
    assert [d["decision"] for d in found["decisions"]] == ["merged"]

    miss = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-92", "--json"]).stdout
    )
    assert miss["decisions"] == [], "folding case is not a prefix match"


def test_reindex_exits_nonzero_when_the_index_cannot_be_written(
    root: Path, tmp_graph: Path, index: Path, monkeypatch: pytest.MonkeyPatch
):
    """The per-row counter cannot tell one unusable legacy row from a dead
    store, and exit 0 on the second says the recovery ran while every decision
    stayed unrecoverable."""
    import fno.events as events_mod

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    index.unlink()

    real = events_mod.append_event

    def boom(event, events_path=None, **kw):
        if events_path is not None and Path(events_path) == index:
            raise OSError("read-only file system")
        return real(event, events_path=events_path, **kw)

    monkeypatch.setattr(events_mod, "append_event", boom)
    res = runner.invoke(decide_app, ["reindex"])
    assert res.exit_code == 1, res.output
    assert "could not be written" in res.output


def test_an_unreadable_graph_says_recall_degraded(
    root: Path, tmp_graph: Path, index: Path
):
    """Without the graph a subject only matches its literal spelling, so a
    ruling recorded under a slug stops answering the id the receipt printed.
    Degrading in silence is indistinguishable from no such decision."""
    runner.invoke(decide_app, ["--subject", "fold-the-inbox", "--decision", "fold"])

    # A REAL half-written graph, not a monkeypatched raise. read_graph swallows
    # corruption and answers [], so a guard exercised through a patched
    # exception stays green on a path production never takes.
    tmp_graph.write_text('{"entries": [{"id": "x-7d9')

    listed = runner.invoke(decide_app, ["list", "--subject", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "the graph could not be read" in listed.output


def test_the_newest_superseder_wins_the_mark(root: Path, tmp_graph: Path, index: Path):
    """One ruling can be overturned twice, and a backfill interleaves journals
    with projections, so file order is not recency."""
    first = runner.invoke(
        decide_app, ["--subject", "pr-922", "--decision", "merge it"]
    ).stdout.strip().splitlines()[-1]
    runner.invoke(
        decide_app, ["--subject", "pr-922", "--decision", "hold it", "--supersedes", first]
    )
    newest = runner.invoke(
        decide_app,
        ["--subject", "pr-922", "--decision", "hold it again", "--supersedes", first],
    ).stdout.strip().splitlines()[-1]

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-922", "--json"]).stdout
    )
    by_id = {d["decision_id"]: d for d in payload["decisions"]}
    assert by_id[first]["superseded_by"] == newest


def test_the_json_surface_reports_damaged_rows(
    root: Path, tmp_graph: Path, index: Path
):
    """A machine-first surface must not under-report a total that looks
    complete: that is the lie "truncated" was added to prevent."""
    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    with index.open("a", encoding="utf-8") as fh:
        fh.write('{"type":"operator_decision","data":{"decision_id":"d-tru\n')

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"]).stdout
    )
    assert payload["damaged"] == 1
    assert payload["total"] == 1


def test_reindex_refuses_to_report_done_on_an_unreadable_graph(
    root: Path, tmp_graph: Path, index: Path
):
    """A query can answer usefully without the graph. A backfill cannot: it
    would fold zero projection rows and still print "+0 decisions" on exit 0."""
    tmp_graph.write_text('{"entries": [{"id": "x-7d9')

    res = runner.invoke(decide_app, ["reindex"])
    assert res.exit_code == 1, res.output
    assert "backlog decide-reindex: failed" in res.output


def test_a_row_the_schema_rejects_does_not_wedge_the_recovery_verb(
    root: Path, tmp_graph: Path, index: Path, monkeypatch: pytest.MonkeyPatch
):
    """`fno backlog decide-reindex` is what an IndexWriteError sends people to. One
    legacy row the schema will never accept must not make it exit 1 forever."""
    import fno.events as events_mod

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    index.unlink()

    real = events_mod.validate
    calls = {"n": 0}

    def picky(event):
        calls["n"] += 1
        if calls["n"] == 1:
            raise ValueError("source 'target' left the enum")
        return real(event)

    monkeypatch.setattr(events_mod, "validate", picky)
    res = runner.invoke(decide_app, ["reindex"])
    assert res.exit_code == 0, res.output
    assert "the schema will not accept" in res.output


def test_resolved_agent_identity_refuses_decision_write(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    session_id = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=session_id, harness="codex", disposition="single"),
    )

    refused = runner.invoke(
        decide_app, ["--subject", "pr-923", "--decision", "merged"]
    )
    assert refused.exit_code == 3, refused.output
    assert "fno law" in refused.output
    assert "fno backlog note" in refused.output


def test_no_identity_at_a_terminal_names_the_operator_but_claims_no_authority(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """Rewritten twice, not deleted. It first asserted that ANY caller with no
    session identity is the operator, which made attendedness an absence:
    scrubbing the environment reached it. A terminal is now required, and even
    at one the law lane is never DEFAULTED, because a tty is obtainable
    (`script -q /dev/null`) and law must never be inherited by silence."""
    from fno import decide as decide_mod
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: True)

    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"]).stdout
    )
    decision = payload["decisions"][0]
    assert decision["decided_by"] == "operator"
    assert decision["attested_by"] == "operator", "a person was at the terminal"
    assert "authority_source" not in decision, "law is never defaulted, only stated"
    assert decision["lane"] == "unattributed"


def test_no_identity_and_no_terminal_refuses_operator_authority(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """The forgery this closes: `env -u CLAUDE_CODE_SESSION_ID fno decide
    --authority operator` resolved no identity, took the attended branch, and
    landed a row in the law lane. Absence is not attendance."""
    from fno import decide as decide_mod
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: False)

    refused = runner.invoke(
        decide_app,
        ["--subject", "pr-923", "--decision", "law", "--authority", "operator"],
    )
    assert refused.exit_code != 0, refused.output
    assert "no terminal" in refused.output
    refused_without_flag = runner.invoke(
        decide_app,
        ["--subject", "pr-923", "--decision", "a note", "--decided-by", "J.N. Choi"],
    )
    assert refused_without_flag.exit_code == 3, refused_without_flag.output

    # Positive control: the attended operator reaches both stores, and its one
    # id proves neither refused invocation wrote first.
    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: True)
    allowed = runner.invoke(
        decide_app,
        ["--subject", "pr-923", "--decision", "operator ruling", "--authority", "operator"],
    )
    assert allowed.exit_code == 0, allowed.output
    decision_id = allowed.stdout.strip().splitlines()[-1]
    assert [e["data"]["decision_id"] for e in _events(root)] == [decision_id]
    assert [
        json.loads(line)["data"]["decision_id"]
        for line in index.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ] == [decision_id]


def test_an_agent_cannot_type_a_name_or_authority_into_a_decision(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """Names and authority flags cannot route around the operator-only gate."""
    session_id = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=session_id, harness="codex", disposition="single"),
    )

    named = runner.invoke(
        decide_app,
        ["--subject", "pr-923", "--decision", "merged", "--decided-by", "J.N. Choi"],
    )
    granted = runner.invoke(
        decide_app,
        [
            "--subject", "pr-921",
            "--decision", "held",
            "--decided-by", "worker-a",
            "--authority", "beastmode",
        ],
    )
    assert named.exit_code == 3, named.output
    assert granted.exit_code == 3, granted.output
    assert "fno backlog note" in named.output
    assert "fno law" in granted.output


def test_record_decision_refuses_agent_operator_authority_before_either_write(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """The engine is the choke point for every current and future CLI spelling."""
    from fno import harness_identity
    from fno.decide import RefusedAuthorityError, record_decision

    session_id = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    handle = harness_identity.canonical_handle(session_id)
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=session_id, harness="codex", disposition="single"),
    )

    for authority in (None, "agent", "crown", "beastmode", "operator"):
        with pytest.raises(RefusedAuthorityError, match=handle):
            record_decision(
                subject="pr-923",
                decision="agent-authored ruling",
                authority_source=authority,
                events_root=root,
            )

    from fno import decide as decide_mod

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: True)
    # Positive control: the operator reaches both instruments, proving that
    # the refusal-state contents below are meaningful rather than an unread probe.
    written = record_decision(
        subject="pr-923",
        decision="operator-authored ruling",
        authority_source="operator",
        events_root=root,
    )
    journal_events = [e for e in _events(root) if e["type"] == "operator_decision"]
    index_events = [
        json.loads(line)
        for line in index.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert [e["data"]["decision_id"] for e in journal_events] == [
        written["decision_id"]
    ]
    assert [e["data"]["decision_id"] for e in index_events] == [
        written["decision_id"]
    ]


@pytest.mark.parametrize("authority", [None, "agent", "crown", "beastmode", "operator"])
def test_backlog_decide_refuses_every_non_operator_authority_before_any_write(
    authority: str | None,
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    session_id = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(
            session_id=session_id, harness="codex", disposition="single"
        ),
    )
    args = ["--subject", "pr-923", "--decision", "agent-authored ruling"]
    if authority is not None:
        args.extend(["--authority", authority])

    refused = runner.invoke(decide_app, args)

    assert refused.exit_code == 3, refused.output
    assert "fno law" in refused.output
    assert "fno backlog note" in refused.output

    # Positive control: a terminal-attested operator can still reach both
    # stores. Their one decision id proves the refused agent invocation wrote
    # neither store; an absent file or zero count alone would not prove that.
    from fno import decide as decide_mod

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: True)
    allowed = runner.invoke(
        decide_app,
        [
            "--subject",
            "pr-923",
            "--decision",
            "operator-authored ruling",
            "--authority",
            "operator",
        ],
    )
    assert allowed.exit_code == 0, allowed.output
    decision_id = allowed.stdout.strip().splitlines()[-1]
    assert [e["data"]["decision_id"] for e in _events(root)] == [decision_id]
    assert [
        json.loads(line)["data"]["decision_id"]
        for line in index.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ] == [decision_id]


def test_cli_refuses_agent_operator_authority_with_actionable_guidance(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    from fno import harness_identity

    session_id = "019f48e1-5b09-72a0-9bc8-6b364bcf4ae4"
    handle = harness_identity.canonical_handle(session_id)
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=session_id, harness="codex", disposition="single"),
    )

    refused = runner.invoke(
        decide_app,
        [
            "--subject",
            "pr-923",
            "--decision",
            "claim operator authority",
            "--authority",
            "operator",
        ],
    )
    assert refused.exit_code == 3, refused.output
    assert handle in refused.output
    assert "fno law" in refused.output
    assert "fno backlog note" in refused.output

    # The successful-control write proves both stores were inspected after the
    # refused command, not merely absent because the writer never ran.
    from fno import decide as decide_mod

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: True)
    granted = runner.invoke(
        decide_app,
        [
            "--subject",
            "pr-923",
            "--decision",
            "operator-authored ruling",
            "--authority",
            "operator",
        ],
    )
    assert granted.exit_code == 0, granted.output
    did = granted.stdout.strip().splitlines()[-1]
    assert [e["data"]["decision_id"] for e in _events(root)] == [did]
    assert [
        json.loads(line)["data"]["decision_id"]
        for line in index.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ] == [did]


def test_no_identity_explicit_operator_authority_records(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """Kept, with the terminal now pinned. The operator lane stays open to a
    person who states their authority; only the silent inheritance closed."""
    from fno import decide as decide_mod
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: True)

    recorded = runner.invoke(
        decide_app,
        [
            "--subject",
            "pr-923",
            "--decision",
            "operator ruling",
            "--authority",
            "operator",
        ],
    )
    assert recorded.exit_code == 0, recorded.output
    did = recorded.stdout.strip().splitlines()[-1]
    assert _events(root)[0]["data"]["decision_id"] == did
    assert _events(root)[0]["data"]["authority_source"] == "operator"


def test_a_torn_archive_also_stops_the_backfill(
    root: Path, tmp_graph: Path, index: Path
):
    """entries_with_archive reads the archive softly. A guard on the working
    graph alone would drop every archived node's decisions from a backfill that
    still printed "+0" and exited 0."""
    (tmp_graph.parent / "graph-archive.json").write_text('{"entries": [{"id": "x-ar')

    res = runner.invoke(decide_app, ["reindex"])
    assert res.exit_code == 1, res.output
    assert "backlog decide-reindex: failed" in res.output


def test_a_corrupt_graph_does_not_produce_a_receipt_that_lies(
    root: Path, tmp_graph: Path, index: Path
):
    """The write path's pre-check used the soft reader, so a real node read as
    "names no graph node" with no hint that the graph was unreadable."""
    tmp_graph.write_text('{"entries": [{"id": "x-7d9')

    res = runner.invoke(decide_app, ["--subject", "x-7d94", "--decision", "fold"])
    assert res.exit_code == 0, res.output
    assert "the graph could not be read" in res.output


def test_one_id_is_one_row_even_if_the_index_holds_it_twice(
    root: Path, tmp_graph: Path, index: Path
):
    """reindex is read-then-write with no lock across the fold, so a decide
    landing mid-backfill can be appended twice under one id."""
    runner.invoke(decide_app, ["--subject", "pr-923", "--decision", "merged"])
    duplicate = index.read_text(encoding="utf-8")
    with index.open("a", encoding="utf-8") as fh:
        fh.write(duplicate)

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"]).stdout
    )
    assert len(payload["decisions"]) == 1, payload


def test_superseding_an_id_nobody_recorded_fails_closed(
    root: Path, tmp_graph: Path, index: Path
):
    """A missing target cannot bypass law protection during an index split."""
    res = runner.invoke(
        decide_app,
        ["--subject", "pr-923", "--decision", "held", "--supersedes", "d-1234657"],
    )
    assert res.exit_code != 0, res.output
    assert "decide-reindex" in res.output


def test_an_install_with_no_index_is_told_to_backfill(
    root: Path, tmp_graph: Path, index: Path
):
    """The upgrade path. Every decision on an existing install lives in the
    graph projection this reader no longer consults, so a bare "none recorded"
    would be the absence-reads-as-success shape on this verb's own rollout."""
    assert not index.exists()

    from fno.graph.cli import cli as backlog_app

    listed = runner.invoke(backlog_app, ["decisions", "x-7d94"])
    assert listed.exit_code == 0, listed.output
    assert "fno backlog decide-reindex" in listed.output

    runner.invoke(backlog_app, ["decide", "pr-923", "merged"])
    after = runner.invoke(backlog_app, ["decisions", "x-nope"])
    assert "fno backlog decide-reindex" not in after.output, "only while the index is missing"


def test_operator_decision_retention_is_durable_by_an_explicit_key():
    """It behaved this way only because it named no retention and the default
    is durable. The record the recall promise rests on is then one schema edit
    from being GC'd out of the project journal."""
    from fno.events import SCHEMA, retention_for

    assert retention_for("operator_decision") == "durable"
    entry = next(e for e in SCHEMA["event_types"] if e["name"] == "operator_decision")
    assert entry.get("retention") == "durable", "explicit, not inherited from the default"


def test_a_subject_the_exact_match_answered_is_not_reported_as_unreached(
    root: Path, tmp_graph: Path, index: Path
):
    """`--subject fold-the-inbox` resolves through the node tier and prints that
    row. Reporting it as a near miss tells the reader to go looking for a
    ruling they were just shown."""
    _write_decision_index(
        index,
        {
            "ts": "2026-08-18T10:00:00Z",
            "decision_id": "d-cccc0001",
            "decision": "the wave plan",
            "subject": "x-7d94",
            "decided_by": "operator",
            "authority_source": "operator",
        },
    )
    res = runner.invoke(decide_app, ["list", "--subject", "fold-the-inbox"])
    assert res.exit_code == 0, res.output
    assert "d-cccc0001" in res.output, "the node-tier match still answers"
    assert "nearly match" not in res.output, "and is not also called a near miss"


def test_a_lane_filtered_empty_answer_never_reads_as_an_empty_store(
    root: Path, tmp_graph: Path, index: Path
):
    """The LANE emptied the answer, not the store. Only `law` had this branch,
    so every other lane printed a claim about the world that was false."""
    _write_decision_index(
        index,
        {
            "ts": "2026-08-22T10:00:00Z",
            "decision_id": "d-cccc0002",
            "decision": "authorized",
            "subject": "force-push",
            "decided_by": "operator",
            "authority_source": "operator",
        },
    )
    res = runner.invoke(
        decide_app, ["list", "--subject", "force-push", "--lane", "coord"]
    )
    assert res.exit_code == 0, res.output
    assert "0 coord decisions" in res.output
    assert "law" in res.output, "it names the lane the decision IS in"
    assert "no decision is indexed" not in res.output


def test_an_id_lookup_does_not_hide_rulings_recorded_about_that_id(
    root: Path, tmp_graph: Path, index: Path
):
    """A ruling can be filed with a decision id as its SUBJECT. Answering only
    the id drops whatever overturned or qualified it."""
    _write_decision_index(
        index,
        {
            "ts": "2026-08-18T10:00:00Z",
            "decision_id": "d-3b26c1c6",
            "decision": "force-push is authorized",
            "subject": "force-push",
            "decided_by": "operator",
            "authority_source": "operator",
        },
        {
            "ts": "2026-08-19T10:00:00Z",
            "decision_id": "d-cccc0003",
            "decision": "that authorization is narrowed to feature branches",
            "subject": "d-3b26c1c6",
            "decided_by": "operator",
            "authority_source": "operator",
        },
    )
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "d-3b26c1c6", "--json"]).stdout
    )
    ids = {d["decision_id"] for d in payload["decisions"]}
    assert ids == {"d-3b26c1c6", "d-cccc0003"}, "the id AND what was said about it"
    # A single value here would deny that the subject key answered, which is
    # the confusion the field exists to prevent.
    assert payload["matched_by"] == ["decision_id", "subject"]


def test_a_lane_message_counts_every_lane_the_subject_holds(
    root: Path, tmp_graph: Path, index: Path
):
    """The law-only branch named the pre-cutover rows and stopped, so a subject
    with unattributed AND coord rulings heard about the first and never the
    second: the more specific branch gave the less complete answer."""
    _write_decision_index(
        index,
        {
            "ts": "2026-08-18T10:00:00Z",
            "decision_id": "d-dddd0001",
            "decision": "pre-cutover",
            "subject": "pr-923",
            "decided_by": "operator",
            "authority_source": "operator",
        },
        {
            "ts": "2026-08-22T10:00:00Z",
            "decision_id": "d-dddd0002",
            "decision": "a peer ruling",
            "subject": "pr-923",
            "decided_by": "king-g4",
            "authority_source": "crown",
        },
    )
    res = runner.invoke(decide_app, ["list", "--subject", "pr-923", "--lane", "law"])
    assert res.exit_code == 0, res.output
    assert "1 unattributed" in res.output
    assert "1 coord" in res.output, "the lane the law branch used to hide"


def test_near_miss_counts_are_deduped_the_way_the_listing_is(
    root: Path, tmp_graph: Path, index: Path
):
    """The index is append-only and a reindex landing mid-write appends one
    ruling twice. list_decisions dedupes by id, so a raw row count inflates the
    number this message exists to convey."""
    row = {
        "decision_id": "d-cccc0004",
        "decision": "freeze",
        "subject": "release scope",
        "decided_by": "operator",
        "authority_source": "operator",
    }
    _write_decision_index(
        index,
        {**row, "ts": "2026-08-18T10:00:00Z"},
        {**row, "ts": "2026-08-18T10:00:00Z"},
    )
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "release", "--json"]).stdout
    )
    assert payload["near_misses"] == [{"subject": "release scope", "count": 1}]


def test_every_projected_field_is_one_the_event_builder_accepts():
    """reindex rebuilds an event from a projection row by splatting
    PROJECTION_FIELDS into operator_decision. A field added to one and not the
    other raises TypeError there, and the backfill drops every row carrying it -
    silently, because that loop swallows a row it cannot rebuild."""
    import inspect

    from fno.decide import PROJECTION_FIELDS
    from fno.events import operator_decision

    accepted = set(inspect.signature(operator_decision).parameters)
    assert not set(PROJECTION_FIELDS) - accepted


def test_a_bad_authority_value_is_refused_before_anything_is_written(
    root: Path, tmp_graph: Path, index: Path
):
    """`--authority banana` recorded d-11eae39d on exit 0, and the reader then
    filed that ruling under `unattributed` because it recognised no such lane."""
    res = runner.invoke(
        decide_app,
        ["--subject", "pr-923", "--decision", "merged", "--authority", "banana"],
    )
    assert res.exit_code != 0, res.output
    for value in ("operator", "crown", "agent", "beastmode"):
        assert value in res.output, f"the message must name {value}"
    assert not index.exists(), "a refused write leaves no row behind"


def test_a_king_has_an_authority_value_to_pass_and_it_reads_as_coordination(
    root: Path, tmp_graph: Path, index: Path
):
    """Three rows on disk carry invented `crown-l2-<node>` spellings, written by
    kings who had no correct value. A king ruling in its own scope is
    coordination, so it needs the value, not a lane of its own."""
    res = runner.invoke(
        decide_app,
        ["--subject", "pr-923", "--decision", "held", "--authority", "crown"],
    )
    assert res.exit_code == 0, res.output
    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"]).stdout
    )
    assert payload["decisions"][0]["authority_source"] == "crown"
    assert payload["decisions"][0]["lane"] == "coord"


def test_a_legacy_invented_authority_row_survives_reindex(
    root: Path, tmp_graph: Path, index: Path
):
    """The enum binds the write path only. Enforcing it on the read path would
    make the backfill reject these rows and drop recall for real rulings."""
    _write_decision_index(
        index,
        {
            "ts": "2026-08-18T10:00:00Z",
            "decision_id": "d-1eaced01",
            "decision": "freeze the scope",
            "subject": "x-f7b9 scope",
            "decided_by": "king-g4",
            "authority_source": "crown-l2-x-f3d0",
        },
    )
    res = runner.invoke(decide_app, ["reindex"])
    assert res.exit_code == 0, res.output
    payload = json.loads(
        runner.invoke(
            decide_app, ["list", "--subject", "x-f7b9 scope", "--json"]
        ).stdout
    )
    assert payload["decisions"][0]["decision_id"] == "d-1eaced01"


def test_a_decision_id_is_a_lookup_key_whatever_subject_it_was_filed_under(
    root: Path, tmp_graph: Path, index: Path
):
    """The id is the first column of the row's own output, so denying it exists
    teaches a reader the key and then punishes the one who uses it."""
    _write_decision_index(
        index,
        {
            "ts": "2026-08-18T10:00:00Z",
            "decision_id": "d-3b26c1c6",
            "decision": "force-push is authorized on a feature branch",
            "subject": "force-push",
            "decided_by": "operator",
            "authority_source": "operator",
        },
    )
    res = runner.invoke(decide_app, ["list", "--subject", "d-3b26c1c6"])
    assert res.exit_code == 0, res.output
    assert "force-push is authorized" in res.output

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "d-3b26c1c6", "--json"]).stdout
    )
    assert payload["matched_by"] == ["decision_id"]
    assert payload["decisions"][0]["subject"] == "force-push"


def test_an_unknown_decision_id_is_named_as_one_not_denied_as_a_ruling(
    root: Path, tmp_graph: Path, index: Path
):
    _write_decision_index(
        index,
        {
            "ts": "2026-08-18T10:00:00Z",
            "decision_id": "d-3b26c1c6",
            "decision": "authorized",
            "subject": "force-push",
            "decided_by": "operator",
            "authority_source": "operator",
        },
    )
    res = runner.invoke(decide_app, ["list", "--subject", "d-deadbeef"])
    assert res.exit_code == 0, res.output
    assert "shaped like a decision id" in res.output
    assert "fno backlog decisions" in res.output


def test_a_near_miss_subject_names_what_it_nearly_matched(
    root: Path, tmp_graph: Path, index: Path
):
    """A freeze filed under the free-text subject `x-f7b9 scope` was invisible
    to `--subject x-f7b9`, and recovering it needed a raw grep of the index."""
    _write_decision_index(
        index,
        *[
            {
                "ts": f"2026-08-18T10:0{n}:00Z",
                "decision_id": f"d-aaaa000{n}",
                "decision": f"ruling {n}",
                "subject": "x-f7b9 scope",
                "decided_by": "operator",
                "authority_source": "operator",
            }
            for n in range(4)
        ],
    )
    res = runner.invoke(decide_app, ["list", "--subject", "x-f7b9"])
    assert res.exit_code == 0, res.output
    assert "x-f7b9 scope" in res.output
    assert "(4)" in res.output, "the count is what tells a reader it is worth a look"

    payload = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "x-f7b9", "--json"]).stdout
    )
    assert payload["near_misses"] == [{"subject": "x-f7b9 scope", "count": 4}]


def test_a_partial_answer_still_names_the_subjects_it_did_not_reach(
    root: Path, tmp_graph: Path, index: Path
):
    """The live specimen returns ONE row, not zero: `--subject x-f7b9` matched a
    wave plan and hid four rulings filed under `x-f7b9 scope`. A near-miss scan
    that only ran on an empty answer would stay silent on exactly that case."""
    _write_decision_index(
        index,
        {
            "ts": "2026-08-18T09:00:00Z",
            "decision_id": "d-3d15461b",
            "decision": "ten children go out as four waves",
            "subject": "x-f7b9",
            "decided_by": "king-g4",
            "authority_source": "beastmode",
        },
        *[
            {
                "ts": f"2026-08-18T10:0{n}:00Z",
                "decision_id": f"d-bbbb000{n}",
                "decision": f"freeze {n}",
                "subject": "x-f7b9 scope",
                "decided_by": "operator",
                "authority_source": "operator",
            }
            for n in range(4)
        ],
    )
    res = runner.invoke(decide_app, ["list", "--subject", "x-f7b9"])
    assert res.exit_code == 0, res.output
    assert "d-3d15461b" in res.output, "the exact hit is still answered"
    assert "'x-f7b9 scope' (4)" in res.output, "and the four it did not reach"


def test_no_miss_branch_ever_denies_that_rulings_exist(
    root: Path, tmp_graph: Path, index: Path
):
    """"no decisions recorded" is a claim about the world where only a claim
    about the query is true, and a reader cannot tell the two apart."""
    _write_decision_index(
        index,
        {
            "ts": "2026-08-18T10:00:00Z",
            "decision_id": "d-3b26c1c6",
            "decision": "authorized",
            "subject": "force-push",
            "decided_by": "operator",
            "authority_source": "operator",
        },
    )
    for probe in ("definitely-not-a-subject", "d-deadbeef", "force"):
        res = runner.invoke(decide_app, ["list", "--subject", probe])
        assert res.exit_code == 0, res.output
        assert "no decisions recorded" not in res.output, probe


def test_every_printed_row_carries_the_provenance_a_citation_needs(
    root: Path, tmp_graph: Path, index: Path
):
    """The lane column does not travel: a row quoted in mail carries only what
    the row itself says."""
    _write_decision_index(
        index,
        {
            "ts": "2026-08-22T10:00:00Z",
            "decision_id": "d-3b26c1c6",
            "decision": "authorized",
            "subject": "force-push",
            "decided_by": "king-g4",
            "authority_source": "crown",
        },
    )
    res = runner.invoke(decide_app, ["list", "--subject", "force-push"])
    assert res.exit_code == 0, res.output
    assert "king-g4" in res.output
    assert "crown" in res.output


def test_only_an_attended_caller_writes_attested_by(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """The field that makes a genuine ruling checkable on the row itself."""
    from fno import decide as decide_mod
    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: True)
    runner.invoke(
        decide_app,
        ["--subject", "pr-923", "--decision", "merged", "--decided-by", "J.N. Choi"],
    )
    attended = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"]).stdout
    )["decisions"][0]
    assert attended["decided_by"] == "J.N. Choi"
    assert attended["attested_by"] == "J.N. Choi"
    assert "relayed_by" not in attended
    assert (
        "[attested]"
        in runner.invoke(decide_app, ["list", "--subject", "pr-923"]).output
    )

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(
            session_id="019f48e1-5b09-72a0-9bc8-6b364bcf4ae4",
            harness="codex",
            disposition="single",
        ),
    )
    refused = runner.invoke(
        decide_app, ["--subject", "pr-921", "--decision", "held"]
    )
    assert refused.exit_code == 3, refused.output
    assert "fno backlog note" in refused.output


def test_operator_recording_own_name_records_no_relayed_by(
    root: Path,
    tmp_graph: Path,
    index: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """An operator naming themselves does not create relay provenance."""
    from fno import decide as decide_mod

    monkeypatch.setattr(
        "fno.agents.self_stamp.resolve_self_identity",
        lambda: SimpleNamespace(session_id=None, harness=None, disposition="empty"),
    )
    monkeypatch.setattr(decide_mod, "_attended_terminal", lambda: True)

    recorded = runner.invoke(
        decide_app,
        ["--subject", "pr-923", "--decision", "merged", "--decided-by", "J.N. Choi"],
    )
    assert recorded.exit_code == 0, recorded.output
    row = json.loads(
        runner.invoke(decide_app, ["list", "--subject", "pr-923", "--json"]).stdout
    )["decisions"][0]
    assert row["decided_by"] == "J.N. Choi"
    assert "relayed_by" not in row


def _write_repository_catalog(root: Path, body: str) -> Path:
    path = root / "docs" / "architecture" / "decisions.yaml"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    return path


def test_repository_catalog_absence_is_an_explicit_empty_source(root: Path):
    from fno.decide.catalog import load_catalog

    catalog = load_catalog(root)

    assert catalog.rows == ()
    assert catalog.canonical_subject("target self-handoff") == "target self-handoff"


def test_repository_catalog_dangling_symlink_is_damage_not_absence(root: Path):
    from fno.decide.catalog import DecisionCatalogError, load_catalog

    path = root / "docs" / "architecture" / "decisions.yaml"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.symlink_to(root / "missing-decisions.yaml")

    with pytest.raises(DecisionCatalogError) as exc:
        load_catalog(root)

    assert str(path) in str(exc.value)
    assert "unreachable" in str(exc.value)


def test_repository_catalog_normalizes_aliases_and_rows(root: Path):
    from fno.decide.catalog import load_catalog

    _write_repository_catalog(
        root,
        """version: 1
decisions:
  - decision_id: d-cab50789
    subject: target-self-handoff
    aliases: [handoff, target self-handoff]
    decision: Context pressure is not a handoff trigger.
    rationale: A harness compaction preserves ownership while a handoff recreates it.
""",
    )

    catalog = load_catalog(root)

    assert catalog.canonical_subject("HANDOFF") == "target-self-handoff"
    assert catalog.canonical_subject("target self-handoff") == "target-self-handoff"
    assert [row["decision_id"] for row in catalog.rows] == ["d-cab50789"]
    assert catalog.rows[0]["_source"] == "repository"
    assert catalog.rows[0]["subject"] == "target-self-handoff"


@pytest.mark.parametrize(
    "body,marker",
    [
        (
            """version: 1
decisions:
  - decision_id: d-aaa00001
    subject: first
    aliases: [shared]
    decision: First law.
    rationale: First reason.
  - decision_id: d-bbb00002
    subject: second
    aliases: [shared]
    decision: Second law.
    rationale: Second reason.
""",
            "alias 'shared'",
        ),
        (
            """version: 1
decisions:
  - decision_id: d-aaa00001
    subject: first
    aliases: []
    decision: First law.
    rationale: First reason.
    supersedes: d-bbb00002
  - decision_id: d-bbb00002
    subject: first
    aliases: []
    decision: Second law.
    rationale: Second reason.
    supersedes: d-aaa00001
""",
            "supersession cycle",
        ),
    ],
    ids=["duplicate-alias", "supersession-cycle"],
)
def test_repository_catalog_rejects_ambiguous_or_cyclic_law(
    root: Path, body: str, marker: str
):
    from fno.decide.catalog import DecisionCatalogError, load_catalog

    path = _write_repository_catalog(root, body)

    with pytest.raises(DecisionCatalogError) as exc:
        load_catalog(root)

    assert str(path) in str(exc.value)
    assert marker in str(exc.value)


def test_repository_catalog_is_live_law_on_a_fresh_clone(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.decide import list_decisions

    _write_repository_catalog(
        root,
        """version: 1
decisions:
  - decision_id: d-cab50789
    subject: target-self-handoff
    aliases: [handoff, target self-handoff]
    decision: Context pressure is not a handoff trigger.
    rationale: Compaction preserves the claim and worktree.
""",
    )

    label, rows, damaged = list_decisions("handoff", lane="law", state="live")

    assert label == "handoff"
    assert damaged == 0
    assert [row["decision_id"] for row in rows] == ["d-cab50789"]
    assert rows[0]["subject"] == "target-self-handoff"
    assert rows[0]["lane"] == "law"
    assert rows[0]["lifecycle"] == "live"


def test_repository_metadata_promotes_the_same_local_id_without_promoting_its_lane(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.decide import list_decisions

    _write_repository_catalog(
        root,
        """version: 1
decisions:
  - decision_id: d-cab50789
    subject: target-self-handoff
    aliases: [handoff, target self-handoff]
    decision: Shipped repository law.
    rationale: Code review promotes this exact row.
""",
    )
    _write_decision_index(
        index,
        {
            "decision_id": "d-cab50789",
            "subject": "target self-handoff",
            "decision": "machine-local wording",
            "rationale": "machine-local rationale",
            "authority_source": "agent",
            "decided_by": "king-session",
            "supersedes": "d-deadbeef",
            "ts": "2026-08-25T17:00:00Z",
        },
    )

    _, rows, _ = list_decisions("handoff", lane="law", state="live")

    assert [row["decision_id"] for row in rows] == ["d-cab50789"]
    assert rows[0]["decision"] == "Shipped repository law."
    assert rows[0]["decided_by"] == "king-session"
    assert rows[0]["_source"] == "repository"
    assert "supersedes" not in rows[0]


def test_catalog_supersession_and_local_override_share_one_projection(
    root: Path, tmp_graph: Path, index: Path
):
    from fno.decide import _decision_row_by_id, list_decisions

    _write_repository_catalog(
        root,
        """version: 1
decisions:
  - decision_id: d-880626b7
    subject: process-admission
    aliases: []
    decision: Never compare agent and process counts.
    rationale: They are different units.
  - decision_id: d-94b2df45
    subject: process-admission
    aliases: [process admission]
    decision: Give the process ceiling its own plumbing.
    rationale: Shared plumbing recreates the invalid comparison.
    supersedes: d-880626b7
""",
    )

    assert _decision_row_by_id("d-94b2df45")["_source"] == "repository"
    _, history, _ = list_decisions("process admission", state="all")
    by_id = {row["decision_id"]: row for row in history}
    assert by_id["d-880626b7"]["lifecycle"] == "superseded"
    assert by_id["d-880626b7"]["superseded_by"] == "d-94b2df45"
    assert by_id["d-94b2df45"]["supersedes"] == "d-880626b7"

    _write_decision_index(
        index,
        {
            "decision_id": "d-fedcba98",
            "subject": "process admission",
            "decision": "Project policy overrides the repository default.",
            "rationale": "This project needs a stricter ceiling.",
            "authority_source": "operator",
            "supersedes": "D-94B2DF45",
            "ts": "2026-08-26T00:00:00Z",
        },
    )
    _, live, _ = list_decisions("process-admission", lane="law", state="live")
    assert [row["decision_id"] for row in live] == ["d-fedcba98"]


def test_standing_query_reports_one_current_repository_law(
    root: Path, tmp_graph: Path, index: Path
):
    _write_repository_catalog(
        root,
        """version: 1
decisions:
  - decision_id: d-cab50789
    subject: target-self-handoff
    aliases: [handoff, target self-handoff]
    decision: Context pressure is not a handoff trigger.
    rationale: Compaction preserves ownership.
""",
    )

    machine = runner.invoke(
        decide_app,
        ["list", "--subject", "handoff", "--lane", "law", "--state", "live", "--json"],
    )
    assert machine.exit_code == 0, machine.output
    payload = json.loads(machine.stdout)
    assert payload["canonical_subject"] == "target-self-handoff"
    assert payload["current_law"] == {
        "status": "single",
        "decision_ids": ["d-cab50789"],
        "decision_id": "d-cab50789",
    }

    human = runner.invoke(
        decide_app,
        ["list", "--subject", "handoff", "--lane", "law", "--state", "live"],
    )
    assert "CURRENT LAW  target-self-handoff  d-cab50789" in human.stdout


def test_standing_query_and_review_list_surface_the_same_conflict(
    root: Path, tmp_graph: Path, index: Path
):
    _write_repository_catalog(
        root,
        """version: 1
decisions:
  - decision_id: d-aaa00001
    subject: deployment-policy
    aliases: [deployments]
    decision: Deploy on Tuesday.
    rationale: First current ruling.
  - decision_id: d-bbb00002
    subject: deployment-policy
    aliases: []
    decision: Deploy on Wednesday.
    rationale: Second unrelated current ruling.
""",
    )

    direct = runner.invoke(
        decide_app,
        ["list", "--subject", "deployments", "--lane", "law", "--state", "live", "--json"],
    )
    payload = json.loads(direct.stdout)
    assert payload["current_law"] == {
        "status": "conflict",
        "decision_ids": ["d-bbb00002", "d-aaa00001"],
    }

    human = runner.invoke(
        decide_app,
        ["list", "--subject", "deployments", "--lane", "law", "--state", "live"],
    )
    assert "LAW CONFLICT  deployment-policy  d-bbb00002,d-aaa00001" in human.stdout

    review = json.loads(
        runner.invoke(decide_app, ["list", "--review-list", "--json"]).stdout
    )
    group = next(item for item in review["groups"] if item["subject"] == "deployment-policy")
    assert [row["decision_id"] for row in group["decisions"]] == payload["current_law"][
        "decision_ids"
    ]


def test_standing_query_reports_none_but_catalog_damage_is_a_read_failure(
    root: Path, tmp_graph: Path, index: Path
):
    empty = runner.invoke(
        decide_app,
        ["list", "--subject", "unknown-policy", "--lane", "law", "--state", "live", "--json"],
    )
    assert empty.exit_code == 0, empty.output
    payload = json.loads(empty.stdout)
    assert payload["canonical_subject"] == "unknown-policy"
    assert payload["current_law"] == {"status": "none", "decision_ids": []}

    human = runner.invoke(
        decide_app,
        ["list", "--subject", "unknown-policy", "--lane", "law", "--state", "live"],
    )
    assert "NO CURRENT LAW  unknown-policy" in human.stdout

    path = _write_repository_catalog(root, "version: 2\ndecisions: []\n")
    damaged = runner.invoke(
        decide_app,
        ["list", "--subject", "unknown-policy", "--lane", "law", "--state", "live", "--json"],
    )
    assert damaged.exit_code == 1
    assert str(path) in damaged.stderr
    assert '"status":"none"' not in damaged.stdout


def test_standing_query_refuses_a_damaged_local_index(
    root: Path, tmp_graph: Path, index: Path
):
    index.parent.mkdir(parents=True, exist_ok=True)
    index.write_text('{"type":"operator_decision","data":', encoding="utf-8")

    result = runner.invoke(
        decide_app,
        ["list", "--subject", "safety-policy", "--lane", "law", "--state", "live", "--json"],
    )

    assert result.exit_code == 1
    assert "decision index has 1 damaged row" in result.stderr
    assert '"current_law"' not in result.stdout

"""Tests for fno plan migrate-keys (x-f34f US7).

Byte-preserving synonym-key collapse: graph_node_id->node, created_at->created,
depends_on->blocked_by, kind->type, and drop claims where it equals node.
"""
from __future__ import annotations

from pathlib import Path

from fno.plan.migrate_keys import migrate, migrate_text


def _plan(fm_lines: str, body: str = "\n# Title\n\nbody text\n") -> str:
    return f"---\n{fm_lines}\n---{body}"


def test_renames_all_legacy_keys_body_intact():
    text = _plan("graph_node_id: x-1\ncreated_at: 2026-07-08\ndepends_on: [x-2]\nkind: feature")
    out, notes = migrate_text(text)
    assert out is not None
    assert "\nnode: x-1\n" in out
    assert "\ncreated: 2026-07-08\n" in out
    assert "\nblocked_by: [x-2]\n" in out
    assert "\ntype: feature\n" in out
    assert "graph_node_id" not in out and "created_at" not in out
    assert out.endswith("\n# Title\n\nbody text\n")  # body byte-intact


def test_drops_claims_equal_to_node():
    text = _plan("node: x-1\nclaims: x-1\nstatus: done")
    out, notes = migrate_text(text)
    assert out is not None
    assert "claims" not in out
    assert "\nnode: x-1\n" in out
    assert "dropped claims (== node)" in notes


def test_keeps_claims_that_differs_from_node():
    text = _plan("node: x-1\nclaims: x-2\nstatus: done")
    out, notes = migrate_text(text)
    # claims != node -> preserved, flagged for review, no rename churn -> no write.
    assert out is None
    assert any("review" in n for n in notes)


def test_no_rename_when_canonical_target_present():
    """kind + type both present -> keep kind (never create a duplicate type key)."""
    text = _plan("type: feature\nkind: task\nnode: x-1")
    out, notes = migrate_text(text)
    assert out is None  # nothing safely renamable
    assert any("kind (type also present)" in n for n in notes)


def test_idempotent_second_run_no_change():
    text = _plan("graph_node_id: x-1\nkind: bug")
    once, _ = migrate_text(text)
    assert once is not None
    twice, _ = migrate_text(once)
    assert twice is None  # already canonical


def test_graph_node_id_plus_claims_single_pass():
    """A file with graph_node_id + a matching claims drops claims in ONE pass
    (node identity resolved from graph_node_id), so the migration is idempotent.
    """
    text = _plan("graph_node_id: x-1\nclaims: x-1\nstatus: done")
    once, notes = migrate_text(text)
    assert once is not None
    assert "\nnode: x-1\n" in once
    assert "claims" not in once  # dropped in the same pass, not deferred
    assert "dropped claims (== node)" in notes
    assert migrate_text(once)[0] is None  # idempotent


def test_deliverable_type_not_collapsed():
    text = _plan("deliverable_type: investigation\nnode: x-1")
    out, _ = migrate_text(text)
    assert out is None  # deliverable_type is a distinct axis, never touched


def test_no_frontmatter_is_noop():
    out, notes = migrate_text("# just a body\nno frontmatter\n")
    assert out is None and notes == []


def test_review_only_file_named_in_receipt(tmp_path: Path):
    """A file that only needs review (no safe rename) is listed by path in the
    receipt, not just counted - codex P2 (the 61-flagged follow-up needs names)."""
    p = tmp_path / "conflict.md"
    p.write_text(_plan("node: x-1\nclaims: x-2"))  # claims != node -> review, no write
    res = migrate(tmp_path, apply=True)
    assert res.review == 1 and res.migrated == 0
    assert len(res.review_files) == 1
    assert res.review_files[0][0].endswith("conflict.md")


def test_sweep_apply_writes_and_summarizes(tmp_path: Path):
    (tmp_path / "a.md").write_text(_plan("graph_node_id: x-1\nkind: feature"))
    (tmp_path / "b.md").write_text(_plan("node: x-2\nclaims: x-2"))
    (tmp_path / "c.md").write_text(_plan("node: x-3\ntype: feature"))  # already canonical
    res = migrate(tmp_path, apply=True)
    assert res.migrated == 2 and res.skipped == 1
    assert "\nnode: x-1\n" in (tmp_path / "a.md").read_text()
    assert "claims" not in (tmp_path / "b.md").read_text()
    # Re-run is idempotent.
    res2 = migrate(tmp_path, apply=True)
    assert res2.migrated == 0

"""Tests for fno plan reconcile-status (x-ff83 W2).

Covers the two-tier normalization map, in-place status rewrite (body
byte-intact), signal-gated archiving, and idempotency / never-downgrade.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.plan._status import KNOWN_STATUSES
from fno.plan.reconcile_status import (
    rewrite_status,
    sweep,
    target_status,
)


def _plan(status_line: str, *, body: str = "\n# Title\n\nbody text\n") -> str:
    fm = f"---\nnode: x-1\n{status_line}\n---" if status_line else "---\nnode: x-1\n---"
    return fm + body


# ---------------------------------------------------------------------------
# classification (target_status)
# ---------------------------------------------------------------------------


def test_superseded_is_a_known_terminal_not_on_the_axis():
    from fno.plan._status import STATUS_PROGRESSION

    assert "superseded" in KNOWN_STATUSES
    assert "done" in KNOWN_STATUSES
    assert "superseded" not in STATUS_PROGRESSION  # off-axis (AC / Locked #3)


@pytest.mark.parametrize("raw,expected", [
    ("PENDING", "design"),
    ("draft", "design"),
    ("designed", "design"),        # typo
    ("design-locked", "ready"),
    ("reviewing", "in_review"),    # pruned axis states (x-f34f) fold into in_review
    ("shipping", "in_review"),
    ("superseded-by-implementation", "superseded"),
])
def test_tier1_synonyms(raw, expected):
    assert target_status(raw, lambda: False) == expected


def test_canonical_status_left_alone():
    for s in KNOWN_STATUSES:
        assert target_status(s, lambda: True) is None  # never downgrade / re-touch


def test_tier2_blank_superseded_without_signal():
    """AC2-EDGE: undeterminable blank -> superseded, never a guessed done."""
    assert target_status("", lambda: False) == "superseded"
    assert target_status(None, lambda: False) == "superseded"


def test_tier2_done_with_signal():
    assert target_status("implemented", lambda: True) == "done"
    assert target_status("", lambda: True) == "done"


def test_quoting_and_case_normalized_before_lookup():
    assert target_status('"design"', lambda: True) is None  # already canonical
    assert target_status("Idea", lambda: False) == "design"


# ---------------------------------------------------------------------------
# in-place rewrite (rewrite_status)
# ---------------------------------------------------------------------------


def test_rewrite_replaces_status_line_body_untouched():
    """AC2-HP: single-line quoted scalar; body byte-intact."""
    original = _plan("status: PENDING")
    out = rewrite_status(original, "design")
    assert 'status: "design"' in out
    assert "PENDING" not in out
    assert out.endswith("\n# Title\n\nbody text\n")  # body preserved verbatim


def test_rewrite_inserts_status_when_absent():
    """AC2-EDGE: a (no status) plan gets the key added, not left blank."""
    original = _plan("")  # frontmatter present, no status key
    out = rewrite_status(original, "superseded")
    assert 'status: "superseded"' in out
    assert "node: x-1" in out  # existing keys preserved


def test_rewrite_returns_none_without_frontmatter():
    assert rewrite_status("# just a body\nno frontmatter\n", "design") is None


# ---------------------------------------------------------------------------
# sweep (end to end over a tmp plans dir)
# ---------------------------------------------------------------------------


def test_sweep_dry_run_reports_without_writing(tmp_path: Path):
    p = tmp_path / "a.md"
    p.write_text(_plan("status: PENDING"))
    res = sweep(tmp_path, apply=False, signal_for=lambda fm: False)
    assert res.normalized == 1 and res.superseded == 0
    assert "PENDING" in p.read_text()  # dry-run does not write


def test_sweep_apply_normalizes_and_summarizes(tmp_path: Path):
    (tmp_path / "a.md").write_text(_plan("status: PENDING"))
    (tmp_path / "b.md").write_text(_plan(""))  # blank -> superseded (no signal)
    (tmp_path / "c.md").write_text(_plan("status: in_review"))  # canonical -> skip
    res = sweep(tmp_path, apply=True, signal_for=lambda fm: False)
    assert res.summary() == "1 normalized, 1 superseded, 1 skipped"
    assert 'status: "design"' in (tmp_path / "a.md").read_text()
    assert 'status: "superseded"' in (tmp_path / "b.md").read_text()


def test_sweep_skips_malformed_body_intact(tmp_path: Path):
    """AC2-ERR: unparseable frontmatter skipped, file left byte-intact."""
    bad = "---\nnode: [unclosed\nstatus: idea\n---\nbody\n"
    p = tmp_path / "bad.md"
    p.write_text(bad)
    res = sweep(tmp_path, apply=True, signal_for=lambda fm: False)
    assert res.skipped == 1 and res.normalized == 0
    assert p.read_text() == bad  # byte-for-byte untouched


def test_sweep_idempotent_and_non_regressing(tmp_path: Path):
    """AC2-FR: a re-run changes nothing; a human re-activated plan is not re-superseded."""
    p = tmp_path / "a.md"
    p.write_text(_plan(""))  # blank
    sweep(tmp_path, apply=True, signal_for=lambda fm: False)  # -> superseded
    assert 'status: "superseded"' in p.read_text()
    # Human re-activates it.
    p.write_text(p.read_text().replace('status: "superseded"', 'status: "design"'))
    res = sweep(tmp_path, apply=True, signal_for=lambda fm: False)
    assert res.normalized == 0 and res.superseded == 0 and res.skipped == 1
    assert 'status: "design"' in p.read_text()  # not re-superseded


def test_sweep_tier2_uses_signal(tmp_path: Path):
    (tmp_path / "closed.md").write_text(_plan("status: implemented"))
    res = sweep(tmp_path, apply=True, signal_for=lambda fm: True, status_map={})
    assert res.normalized == 1 and res.superseded == 0
    assert 'status: "done"' in (tmp_path / "closed.md").read_text()


# ---------------------------------------------------------------------------
# Tier 3: canonical-but-stale -> node projection (x-f34f, US4)
# ---------------------------------------------------------------------------


def _linked_plan(status: str, node: str = "x-1") -> str:
    return f"---\nnode: {node}\nstatus: {status}\n---\n# T\n\nbody\n"


def test_tier3_fixes_stale_canonical(tmp_path: Path):
    """x-76ea class: plan `design`, node `done` -> plan rewritten to `done`."""
    p = tmp_path / "a.md"
    p.write_text(_linked_plan("design"))
    res = sweep(tmp_path, apply=True, status_map={"x-1": "done"})
    assert res.normalized == 1
    text = p.read_text()
    assert 'status: "done"' in text
    # A done promotion MUST carry a first-write done_at (else later sweeps see
    # done==done and never backfill it) - codex P1.
    assert "done_at:" in text


def test_tier3_disabled_when_graph_absent(tmp_path: Path):
    """AC2-ERR: an empty status_map (unreadable graph) leaves canonical untouched."""
    p = tmp_path / "a.md"
    p.write_text(_linked_plan("design"))
    res = sweep(tmp_path, apply=True, status_map={})
    assert res.skipped == 1 and res.normalized == 0
    assert "status: design" in p.read_text()  # untouched (not even re-quoted)


def test_tier3_forward_only(tmp_path: Path):
    """A node that regressed never rewrites a plan backward."""
    p = tmp_path / "a.md"
    p.write_text(_linked_plan("shipped"))
    res = sweep(tmp_path, apply=True, status_map={"x-1": "in_progress"})
    assert res.skipped == 1
    assert "status: shipped" in p.read_text()


def test_tier3_unlinked_plan_skipped(tmp_path: Path):
    """AC2-EDGE: a canonical plan with no node link is left alone by Tier 3."""
    p = tmp_path / "a.md"
    p.write_text("---\nstatus: design\n---\n# T\n\nbody\n")
    res = sweep(tmp_path, apply=True, status_map={"x-1": "done"})
    assert res.skipped == 1
    assert "status: design" in p.read_text()


def test_tier3_link_missing_from_graph_warns(tmp_path: Path):
    """A link resolving to no node in a readable graph is treated as unlinked."""
    p = tmp_path / "a.md"
    p.write_text(_linked_plan("design", node="x-ghost"))
    res = sweep(tmp_path, apply=True, status_map={"x-1": "done"})
    assert res.skipped == 1
    assert any("x-ghost" in w for w in res.warnings)


# ---------------------------------------------------------------------------
# CLI wiring (AC2-UI: verb registered + summary printed)
# ---------------------------------------------------------------------------


def test_default_signal_reads_claims_or_node(monkeypatch):
    """codex PR#149: plans link the node via `claims:` (preferred) or `node:`."""
    import fno.plan.reconcile_status as rs

    monkeypatch.setattr(rs, "_done_node_ids", lambda: frozenset({"x-closed"}))
    assert rs._default_signal({"claims": "x-closed"}) is True
    assert rs._default_signal({"node": "x-closed"}) is True
    assert rs._default_signal({"claims": "x-open"}) is False
    assert rs._default_signal({}) is False


def test_cli_verb_registered_and_prints_summary(tmp_path: Path):
    from typer.testing import CliRunner
    from fno.plan.cli import plan_app

    (tmp_path / "a.md").write_text(_plan("status: PENDING"))
    result = CliRunner().invoke(
        plan_app, ["reconcile-status", "--plans-dir", str(tmp_path)]
    )
    assert result.exit_code == 0, result.output
    assert "1 normalized" in result.output
    assert "[dry-run]" in result.output  # default is dry-run
    assert "PENDING" in (tmp_path / "a.md").read_text()  # not written


# --- link normalization: a list-valued node link must not crash the sweep ----
# A doc-generating path can emit `claims: [x-1d91]` (a one-element YAML list)
# where a scalar is expected. The raw value used to reach `status_map.get(link)`
# and `link in frozenset(...)`, both of which raise TypeError on an unhashable
# list and take down the whole reconcile-status sweep.


def test_plan_link_id_unwraps_single_element_list():
    from fno.plan.reconcile_status import _plan_link_id

    assert _plan_link_id({"claims": ["x-1d91"]}) == "x-1d91"
    assert _plan_link_id({"node": ["x-aa95"]}) == "x-aa95"


def test_plan_link_id_returns_none_for_unusable_link_shapes():
    from fno.plan.reconcile_status import _plan_link_id

    # Ambiguous (which node owns the status?) and malformed shapes both read as
    # unlinked, matching the module's never-rewrite-on-absent-evidence stance.
    assert _plan_link_id({"claims": ["x-1d91", "x-aa95"]}) is None
    assert _plan_link_id({"claims": []}) is None
    assert _plan_link_id({"claims": {"id": "x-1d91"}}) is None
    assert _plan_link_id({"claims": [{"id": "x-1d91"}]}) is None


def test_plan_link_id_hashable_so_lookups_never_raise():
    from fno.plan.reconcile_status import _plan_link_id

    link = _plan_link_id({"claims": ["x-1d91"]})
    assert {"x-1d91": "done"}.get(link) == "done"
    assert link in frozenset({"x-1d91"})


def test_AC5_ERR_sweep_leaves_both_vocabularies_untouched(tmp_path: Path):
    """x-3ad5: mid-migration, one doc on each spelling. Both are in
    KNOWN_STATUSES, so the sweep rewrites neither - a retired spelling is valid
    input, not drift, and the aliases are read-path translation only.
    """
    old = tmp_path / "old.md"
    new = tmp_path / "new.md"
    old.write_text(_plan("status: shipped"))
    new.write_text(_plan("status: in_review"))
    before = (old.read_text(), new.read_text())

    res = sweep(tmp_path, apply=True, signal_for=lambda fm: False)

    assert res.skipped == 2 and res.normalized == 0 and res.superseded == 0
    assert (old.read_text(), new.read_text()) == before  # byte-for-byte

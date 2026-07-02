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


def test_archived_is_a_known_terminal_not_on_the_axis():
    from fno.plan._status import STATUS_PROGRESSION

    assert "archived" in KNOWN_STATUSES
    assert "done" in KNOWN_STATUSES
    assert "archived" not in STATUS_PROGRESSION  # off-axis (AC / Locked #3)


@pytest.mark.parametrize("raw,expected", [
    ("PENDING", "design"),
    ("draft", "design"),
    ("designed", "design"),        # typo
    ("design-locked", "ready"),
    ("superseded", "archived"),
])
def test_tier1_synonyms(raw, expected):
    assert target_status(raw, lambda: False) == expected


def test_canonical_status_left_alone():
    for s in KNOWN_STATUSES:
        assert target_status(s, lambda: True) is None  # never downgrade / re-touch


def test_tier2_blank_archived_without_signal():
    """AC2-EDGE: undeterminable blank -> archived, never a guessed done."""
    assert target_status("", lambda: False) == "archived"
    assert target_status(None, lambda: False) == "archived"


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
    out = rewrite_status(original, "archived")
    assert 'status: "archived"' in out
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
    assert res.normalized == 1 and res.archived == 0
    assert "PENDING" in p.read_text()  # dry-run does not write


def test_sweep_apply_normalizes_and_summarizes(tmp_path: Path):
    (tmp_path / "a.md").write_text(_plan("status: PENDING"))
    (tmp_path / "b.md").write_text(_plan(""))  # blank -> archived (no signal)
    (tmp_path / "c.md").write_text(_plan("status: shipped"))  # canonical -> skip
    res = sweep(tmp_path, apply=True, signal_for=lambda fm: False)
    assert res.summary() == "1 normalized, 1 archived, 1 skipped"
    assert 'status: "design"' in (tmp_path / "a.md").read_text()
    assert 'status: "archived"' in (tmp_path / "b.md").read_text()


def test_sweep_skips_malformed_body_intact(tmp_path: Path):
    """AC2-ERR: unparseable frontmatter skipped, file left byte-intact."""
    bad = "---\nnode: [unclosed\nstatus: idea\n---\nbody\n"
    p = tmp_path / "bad.md"
    p.write_text(bad)
    res = sweep(tmp_path, apply=True, signal_for=lambda fm: False)
    assert res.skipped == 1 and res.normalized == 0
    assert p.read_text() == bad  # byte-for-byte untouched


def test_sweep_idempotent_and_non_regressing(tmp_path: Path):
    """AC2-FR: a re-run changes nothing; a human re-activated plan is not re-archived."""
    p = tmp_path / "a.md"
    p.write_text(_plan(""))  # blank
    sweep(tmp_path, apply=True, signal_for=lambda fm: False)  # -> archived
    assert 'status: "archived"' in p.read_text()
    # Human re-activates it.
    p.write_text(p.read_text().replace('status: "archived"', 'status: "design"'))
    res = sweep(tmp_path, apply=True, signal_for=lambda fm: False)
    assert res.normalized == 0 and res.archived == 0 and res.skipped == 1
    assert 'status: "design"' in p.read_text()  # not re-archived


def test_sweep_tier2_uses_signal(tmp_path: Path):
    (tmp_path / "closed.md").write_text(_plan("status: implemented"))
    res = sweep(tmp_path, apply=True, signal_for=lambda fm: True)
    assert res.normalized == 1 and res.archived == 0
    assert 'status: "done"' in (tmp_path / "closed.md").read_text()


# ---------------------------------------------------------------------------
# CLI wiring (AC2-UI: verb registered + summary printed)
# ---------------------------------------------------------------------------


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

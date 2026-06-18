"""Unit tests for fno.graph.depends - dependency resolver.

Ported from tests/test_graph.py (the adopt/depends_on test battery).
"""
from __future__ import annotations

import tempfile
from pathlib import Path

import pytest

from fno.graph.depends import (
    _parse_frontmatter,
    _resolve_depends_on,
    _sequence_token,
    _collect_frontmatter_depends,
)


# -- helpers --


def _write_plan(dirpath: Path, name: str, title: str, depends_on: list[str] | None = None) -> Path:
    lines = ["---"]
    if title:
        lines.append(f"title: {title}")
    if depends_on:
        lines.append("depends_on:")
        for d in depends_on:
            lines.append(f"  - {d}")
    lines.append("---")
    lines.append("")
    lines.append(f"# {title}")
    p = dirpath / name
    p.write_text("\n".join(lines) + "\n")
    return p


def _make_entry(eid: str, plan_path: str, cwd: str = None) -> dict:
    return {"id": eid, "plan_path": plan_path, "cwd": cwd}


# -- _parse_frontmatter tests --


def test_ac1_hp_parse_frontmatter_block_list(tmp_path):
    """AC1-HP: parse_frontmatter reads block-list depends_on."""
    p = tmp_path / "plan.md"
    p.write_text("---\ntitle: Foo\ndepends_on:\n  - ab-11111111\n  - ab-22222222\n---\n# Foo\n")
    fm = _parse_frontmatter(p)
    assert fm["title"] == "Foo"
    assert fm["depends_on"] == ["ab-11111111", "ab-22222222"]


def test_ac1_hp_parse_frontmatter_scalar(tmp_path):
    """AC1-HP: parse_frontmatter reads scalar depends_on."""
    p = tmp_path / "plan.md"
    p.write_text("---\ntitle: Bar\ndepends_on: ab-33333333\n---\n")
    fm = _parse_frontmatter(p)
    assert fm["depends_on"] == "ab-33333333"


def test_ac1_hp_parse_frontmatter_no_frontmatter(tmp_path):
    """AC1-HP: parse_frontmatter returns None when no --- block."""
    p = tmp_path / "plan.md"
    p.write_text("# Just a heading\nNo frontmatter here.\n")
    assert _parse_frontmatter(p) is None


def test_ac1_hp_parse_frontmatter_missing_file(tmp_path):
    """AC1-HP: parse_frontmatter returns None on missing file (with warning)."""
    p = tmp_path / "nonexistent.md"
    result = _parse_frontmatter(p)
    assert result is None


# -- _sequence_token tests --


def test_ac1_hp_sequence_token_two_digit():
    assert _sequence_token("01-foo.md") == "01"
    assert _sequence_token("12-bar.md") == "12"


def test_ac1_hp_sequence_token_letter_suffix():
    assert _sequence_token("02a-foo.md") == "02a"
    assert _sequence_token("04b-bar.md") == "04b"


def test_ac1_hp_sequence_token_no_match():
    assert _sequence_token("plan.md") is None
    assert _sequence_token("00-INDEX.md") == "00"  # 00 is a valid token


# -- _resolve_depends_on tests --


def test_ac1_hp_resolve_ab_id_direct():
    """AC1-HP: ab-ID references resolve directly."""
    entries = [_make_entry("ab-aabbccdd", "plans/foo.md")]
    resolved, unresolved = _resolve_depends_on(["ab-aabbccdd"], entries, Path("."))
    assert resolved == ["ab-aabbccdd"]
    assert unresolved == []


def test_ac1_hp_resolve_ab_id_missing():
    """AC1-HP: ab-ID not in graph goes to unresolved."""
    entries = []
    resolved, unresolved = _resolve_depends_on(["ab-deadbeef"], entries, Path("."))
    assert resolved == []
    assert unresolved == ["ab-deadbeef"]


def test_ac1_hp_resolve_by_path(tmp_path):
    """AC1-HP: path reference resolves against entries by plan_path."""
    plan_path = str(tmp_path / "plans" / "01-alpha.md")
    entries = [_make_entry("ab-11111111", plan_path)]
    resolved, unresolved = _resolve_depends_on([plan_path], entries, tmp_path)
    assert resolved == ["ab-11111111"]
    assert unresolved == []


def test_ac1_hp_resolve_by_slug(tmp_path):
    """AC1-HP: slug reference resolves against entries by plan_path basename."""
    entries = [_make_entry("ab-22222222", str(tmp_path / "plans" / "01-alpha.md"))]
    resolved, unresolved = _resolve_depends_on(["01-alpha.md"], entries, tmp_path)
    assert resolved == ["ab-22222222"]
    assert unresolved == []


def test_ac1_hp_resolve_by_sequence_token(tmp_path):
    """AC1-HP: bare sequence token resolves within same directory."""
    plan_dir = tmp_path / "batch"
    plan_dir.mkdir()
    entries = [_make_entry("ab-33333333", str(plan_dir / "01-foo.md"))]
    resolved, unresolved = _resolve_depends_on(["01"], entries, plan_dir)
    assert resolved == ["ab-33333333"]
    assert unresolved == []


def test_ac1_hp_resolve_numeric_token_normalize(tmp_path):
    """AC1-HP: bare '1' resolves same as '01' for numeric tokens."""
    plan_dir = tmp_path / "batch"
    plan_dir.mkdir()
    entries = [_make_entry("ab-44444444", str(plan_dir / "01-foo.md"))]
    resolved, unresolved = _resolve_depends_on(["1"], entries, plan_dir)
    assert resolved == ["ab-44444444"]
    assert unresolved == []


def test_ac2_err_unresolvable_goes_to_unresolved():
    """AC2-ERR: completely unknown reference is unresolved."""
    entries = []
    resolved, unresolved = _resolve_depends_on(["totally-unknown"], entries, Path("."))
    assert resolved == []
    assert unresolved == ["totally-unknown"]


def test_ac1_hp_collect_frontmatter_depends_file(tmp_path):
    """AC1-HP: _collect_frontmatter_depends reads block list from file."""
    p = tmp_path / "plan.md"
    p.write_text("---\ntitle: X\ndepends_on:\n  - ab-12341234\n---\n# X\n")
    raw, plan_dir = _collect_frontmatter_depends(str(p))
    assert raw == ["ab-12341234"]
    assert plan_dir == tmp_path


def test_ac1_hp_collect_frontmatter_depends_inline_list(tmp_path):
    """AC1-HP: _collect_frontmatter_depends parses inline [a, b] form."""
    p = tmp_path / "plan.md"
    p.write_text("---\ntitle: Y\ndepends_on: [ab-11111111, ab-22222222]\n---\n# Y\n")
    raw, plan_dir = _collect_frontmatter_depends(str(p))
    assert raw == ["ab-11111111", "ab-22222222"]


def test_ac1_hp_collect_frontmatter_depends_empty_list(tmp_path):
    """AC1-HP: _collect_frontmatter_depends handles [] gracefully."""
    p = tmp_path / "plan.md"
    p.write_text("---\ntitle: Z\ndepends_on: []\n---\n# Z\n")
    raw, plan_dir = _collect_frontmatter_depends(str(p))
    assert raw == []


def test_ac1_hp_collect_frontmatter_depends_folder_with_index(tmp_path):
    """AC1-HP: _collect_frontmatter_depends reads 00-INDEX.md for folders."""
    plan_dir = tmp_path / "feature"
    plan_dir.mkdir()
    idx = plan_dir / "00-INDEX.md"
    idx.write_text("---\ntitle: Feature\ndepends_on:\n  - ab-aaaabbbb\n---\n")
    raw, resolved_dir = _collect_frontmatter_depends(str(plan_dir))
    assert raw == ["ab-aaaabbbb"]
    assert resolved_dir == plan_dir

"""plan_rulings: the one scan for ``consolidation.rejected`` naming a node.

AC1 fixtures for x-6f98. The reader must answer a positive on real rows,
never read ``proceed_alone_against`` as a ruling, and never fold a broken
scan into a clean zero.
"""

from __future__ import annotations

from pathlib import Path

from fno.plan.rulings import plan_rulings, ruling_lines


def _write_plan(dir_: Path, name: str, frontmatter: str) -> Path:
    path = dir_ / name
    path.write_text(f"---\n{frontmatter}---\n\n# T\n", encoding="utf-8")
    return path


def _rejecting_plan(dir_: Path, name: str = "plan-x-aaaa.md") -> Path:
    return _write_plan(
        dir_,
        name,
        "claims: x-aaaa\ntitle: T\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  rejected:\n"
        "    - id: x-bbbb\n"
        "      reason: R\n",
    )


def test_ac1_hp_a_rejected_entry_answers_with_its_claims(tmp_path):
    _rejecting_plan(tmp_path)
    result = plan_rulings("x-bbbb", tmp_path)
    assert result["status"] == "ok"
    assert len(result["rulings"]) == 1
    row = result["rulings"][0]
    assert row["by"] == ["x-aaaa"]
    assert row["plan_path"] == str(tmp_path / "plan-x-aaaa.md")
    assert row["reason"] == "R"


def test_ac1_neg_proceed_alone_against_is_not_a_ruling(tmp_path):
    _write_plan(
        tmp_path,
        "contrast.md",
        "claims: x-aaaa\ntitle: T\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  proceed_alone_against:\n"
        "    - id: x-cccc\n"
        "      reason: different lock\n",
    )
    result = plan_rulings("x-cccc", tmp_path)
    assert result["status"] == "ok"
    assert result["rulings"] == []


def test_ac1_err_missing_dir_is_unavailable_never_ok(tmp_path):
    missing = tmp_path / "nope"
    result = plan_rulings("x-bbbb", missing)
    assert result["status"] == "unavailable"
    assert result["dir"] == str(missing)
    assert result["detail"]


def test_ac1_err_malformed_yaml_is_skipped_valid_row_survives(tmp_path):
    _rejecting_plan(tmp_path)
    torn = tmp_path / "torn.md"
    torn.write_text(
        "---\nclaims: x-bbbb\ntitle: [unclosed\n---\n\n# T\n", encoding="utf-8"
    )
    result = plan_rulings("x-bbbb", tmp_path)
    assert result["status"] == "ok"
    assert str(torn) in result["skipped"]
    assert [row["by"] for row in result["rulings"]] == [["x-aaaa"]]


def test_a_plan_with_no_frontmatter_is_counted_not_parsed(tmp_path):
    plain = tmp_path / "plain.md"
    plain.write_text("# no frontmatter, mentions x-bbbb in prose\n", encoding="utf-8")
    result = plan_rulings("x-bbbb", tmp_path)
    assert result["status"] == "ok"
    assert result["rulings"] == []
    assert result["scanned"] == 1
    assert result["skipped"] == []


def test_an_unclaimed_rejecting_plan_prints_unclaimed(tmp_path):
    _write_plan(
        tmp_path,
        "orphan.md",
        "title: T\n"
        "consolidation:\n"
        "  outcome: proceed_alone\n"
        "  rejected:\n"
        "    - id: x-bbbb\n"
        "      reason: ruled out\n",
    )
    result = plan_rulings("x-bbbb", tmp_path)
    assert result["rulings"][0]["by"] == []
    lines = ruling_lines(result, "undefer", "x-bbbb")
    assert lines == [
        f"undefer: x-bbbb is rejected by (unclaimed) in "
        f"{tmp_path / 'orphan.md'}: ruled out"
    ]


def test_ruling_lines_unavailable_names_the_dir(tmp_path):
    result = plan_rulings("x-bbbb", tmp_path / "nope")
    lines = ruling_lines(result, "unsupersede", "x-bbbb")
    assert len(lines) == 1
    assert lines[0].startswith("unsupersede: plan rulings for x-bbbb not read (")
    assert "directory does not exist" in lines[0]

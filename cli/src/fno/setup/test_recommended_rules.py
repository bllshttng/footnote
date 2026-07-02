"""Tests for the recommended-rules installer (node x-91ae).

Covers the plan's acceptance criteria:
  AC1-HP   clean install links each rule (index excluded)
  AC1-ERR  a real user-edited file at a target is preserved, never clobbered
  AC1-EDGE a second run is idempotent (one link per rule, no duplicates)
  boundary missing/empty source installs nothing

Run: cd cli && uv run pytest src/fno/setup/test_recommended_rules.py -v
"""
from __future__ import annotations

import os
from pathlib import Path

from fno.setup.recommended_rules import install_recommended_rules


def _pack(tmp: Path) -> Path:
    src = tmp / "rules"
    src.mkdir()
    (src / "pr-ready.md").write_text("# rule body\n")
    (src / "RULES.md").write_text("# index\n")  # must NOT be installed
    return src


def test_clean_install_links_rules_not_index(tmp_path: Path) -> None:
    src = _pack(tmp_path)
    tgt = tmp_path / ".claude" / "rules"  # does not exist yet

    results = install_recommended_rules(src, tgt)

    names = {r.name for r in results}
    assert names == {"pr-ready.md"}, "index RULES.md must be excluded"
    link = tgt / "pr-ready.md"
    assert link.is_symlink()
    assert os.path.realpath(link) == os.path.realpath(src / "pr-ready.md")
    assert all(r.action == "linked" for r in results)


def test_real_user_file_preserved(tmp_path: Path) -> None:
    src = _pack(tmp_path)
    tgt = tmp_path / ".claude" / "rules"
    tgt.mkdir(parents=True)
    edited = tgt / "pr-ready.md"
    edited.write_text("MY EDITS\n")  # real file, not a symlink

    results = install_recommended_rules(src, tgt)

    assert results[0].action == "skipped-real"
    assert not edited.is_symlink()
    assert edited.read_text() == "MY EDITS\n"  # untouched


def test_idempotent_no_duplicates(tmp_path: Path) -> None:
    src = _pack(tmp_path)
    tgt = tmp_path / ".claude" / "rules"

    install_recommended_rules(src, tgt)
    second = install_recommended_rules(src, tgt)

    assert second[0].action == "already"
    links = list(tgt.glob("*.md"))
    assert len(links) == 1  # exactly one link, no dupes


def test_missing_source_installs_nothing(tmp_path: Path) -> None:
    assert install_recommended_rules(tmp_path / "nope", tmp_path / "t") == []
    empty = tmp_path / "empty"
    empty.mkdir()
    assert install_recommended_rules(empty, tmp_path / "t") == []


def test_wizard_capstone_opt_in(tmp_path: Path) -> None:
    """AC1-UI: the setup capstone prompts, default No, installs only on yes."""
    from fno.setup_cli import offer_recommended_rules

    src = _pack(tmp_path)
    tgt = tmp_path / ".claude" / "rules"

    declined = offer_recommended_rules(
        confirm_fn=lambda _m: False, source_dir=src, target_dir=tgt
    )
    assert declined["installed"] is False
    assert not (tgt / "pr-ready.md").exists()

    accepted = offer_recommended_rules(
        confirm_fn=lambda _m: True, source_dir=src, target_dir=tgt
    )
    assert accepted["installed"] is True
    assert (tgt / "pr-ready.md").is_symlink()

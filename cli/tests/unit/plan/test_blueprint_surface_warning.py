"""A written plan with no file surface must warn, not pass quietly.

Collision detection compares plans by the files they touch. A plan with no
parseable file table is invisible to it, and an empty surface is
indistinguishable from a genuinely non-overlapping one - so the blueprint
that produced it says so on stderr.
"""
from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

_REPO_ROOT = Path(__file__).resolve().parents[4]
_SCRIPT_PATH = _REPO_ROOT / "skills" / "blueprint" / "scripts" / "mutate_doc.py"


def _load():
    spec = importlib.util.spec_from_file_location("mutate_doc_surface", _SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutate_doc_surface"] = module
    spec.loader.exec_module(module)
    return module


_mutate = _load()

_HEADER = "| File | Action | Owner |\n|---|---|---|\n"


def _plan(tmp_path, body: str) -> Path:
    p = tmp_path / "plan.md"
    p.write_text(body)
    return p


def test_warns_when_ownership_map_has_no_rows(tmp_path, capsys):
    p = _plan(tmp_path, f"# Plan\n\n## File Ownership Map\n\n{_HEADER}\n## Next\n\nbody\n")

    _mutate._warn_no_file_surface(p)

    assert "states no file surface" in capsys.readouterr().err


def test_warns_when_no_file_section_at_all(tmp_path, capsys):
    p = _plan(tmp_path, "# Plan\n\n## Context\n\nbody\n")

    _mutate._warn_no_file_surface(p)

    assert "states no file surface" in capsys.readouterr().err


def test_warns_when_every_file_cell_parses_to_nothing(tmp_path, capsys):
    """A row-counting check would call this populated; the parser strips a bare
    parenthetical to the empty string, so the plan really has no surface."""
    p = _plan(tmp_path, f"# Plan\n\n## File Ownership Map\n\n{_HEADER}| (TBD) | modify | x |\n")

    _mutate._warn_no_file_surface(p)

    assert "states no file surface" in capsys.readouterr().err


def test_quiet_when_ownership_map_is_populated(tmp_path, capsys):
    p = _plan(tmp_path, f"# Plan\n\n## File Ownership Map\n\n{_HEADER}| `a.py` | modify | x |\n")

    _mutate._warn_no_file_surface(p)

    assert capsys.readouterr().err == ""


def test_quiet_on_any_heading_the_parser_accepts(tmp_path, capsys):
    """`Files Touched` is a recognized heading; warning on it would contradict
    the parser this check is supposed to speak for."""
    p = _plan(tmp_path, "# Plan\n\n## Files Touched\n\n| File | Action |\n|---|---|\n| `a.py` | edit |\n")

    _mutate._warn_no_file_surface(p)

    assert capsys.readouterr().err == ""

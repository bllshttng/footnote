"""x-2ada: a written plan with no file surface must warn, not pass quietly.

Collision detection compares plans by the files they touch. A plan with no
populated file table is invisible to it, and an empty surface is
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


def test_warns_when_ownership_map_has_no_rows(tmp_path, capsys):
    doc = f"# Plan\n\n## File Ownership Map\n\n{_HEADER}\n## Next\n\nbody\n"

    _mutate._warn_no_file_surface(tmp_path / "plan.md", doc)

    assert "states no file surface" in capsys.readouterr().err


def test_warns_when_no_file_section_at_all(tmp_path, capsys):
    _mutate._warn_no_file_surface(tmp_path / "plan.md", "# Plan\n\n## Context\n\nbody\n")

    assert "states no file surface" in capsys.readouterr().err


def test_quiet_when_ownership_map_is_populated(tmp_path, capsys):
    doc = f"# Plan\n\n## File Ownership Map\n\n{_HEADER}| `a.py` | modify | /blueprint |\n"

    _mutate._warn_no_file_surface(tmp_path / "plan.md", doc)

    assert capsys.readouterr().err == ""


def test_quiet_when_files_to_modify_is_populated(tmp_path, capsys):
    doc = "# Plan\n\n## Files to Modify\n\n| File | Action |\n|---|---|\n| `a.py` | edit |\n"

    _mutate._warn_no_file_surface(tmp_path / "plan.md", doc)

    assert capsys.readouterr().err == ""

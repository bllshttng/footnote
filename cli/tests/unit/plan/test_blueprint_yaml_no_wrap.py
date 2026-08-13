"""Emitted YAML must never fold a prose scalar mid-sentence.

PyYAML defaults to ``best_width=80``, so any plain scalar longer than ~72
characters is folded across physical lines at the nearest space. Every plan
``notes:`` / ``title:`` and every free-text state field (``input``, ``reason``,
``last_failure_error``) is exactly that shape, so the generated document
violated the repo's own one-sentence-per-line rule on every emission. The fix
is ``width=float("inf")`` at each emitter that writes prose a human reads.

The assertion is on the emitted text, not on a round-trip: folding preserves
the value under ``yaml.load`` (the fold re-joins on a space), so a round-trip
test passes against the bug and proves nothing.
"""
from __future__ import annotations

import importlib.util
import sys
from collections import OrderedDict
from pathlib import Path

import yaml

from fno.state.io import read_frontmatter, write_frontmatter

_REPO_ROOT = Path(__file__).resolve().parents[4]
_SCRIPT_PATH = _REPO_ROOT / "skills" / "blueprint" / "scripts" / "mutate_doc.py"

# Long enough to fold at best_width=80, all single-space-separated words.
_LONG_PROSE = (
    "As an operator I can read a generated plan without the notes field being "
    "hard-wrapped mid-sentence at seventy-two characters by the YAML emitter."
)


def _load_mutate_module():
    spec = importlib.util.spec_from_file_location("mutate_doc_yaml_width", _SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutate_doc_yaml_width"] = module
    spec.loader.exec_module(module)
    return module


_mutate_doc = _load_mutate_module()


def _assert_value_on_one_line(text: str, key: str, value: str) -> None:
    """The full value must appear on the single physical line that carries its key."""
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith(f"{key}:"):
            assert value in stripped, (
                f"{key!r} was folded across lines; its line reads:\n  {stripped!r}\n"
                f"full emission:\n{text}"
            )
            return
    raise AssertionError(f"no {key!r} line in emitted YAML:\n{text}")


def test_execution_strategy_notes_not_folded() -> None:
    """A long User Story yields a `notes:` value on one physical line."""
    sections = OrderedDict()
    sections["User Stories"] = f"| ID | Story |\n|----|-------|\n| US1 | {_LONG_PROSE} |\n"

    strategy = _mutate_doc._build_execution_strategy(sections)

    _assert_value_on_one_line(strategy, "notes", f"Implement US1.")
    # `title` carries the story text itself (truncated to 80 chars by the builder).
    _assert_value_on_one_line(strategy, "title", _LONG_PROSE[:80])


def test_serialize_frontmatter_prose_not_folded() -> None:
    """A long frontmatter value survives serialization on one physical line."""
    emitted = _mutate_doc._serialize_frontmatter({"title": _LONG_PROSE, "status": "ready"})
    _assert_value_on_one_line(emitted, "title", _LONG_PROSE)
    assert yaml.safe_load(emitted)["title"] == _LONG_PROSE


def test_state_frontmatter_prose_not_folded(tmp_path: Path) -> None:
    """`fno state` manifests carry free text (`input`); it must not fold either."""
    path = tmp_path / "target-state.md"
    write_frontmatter(path, {"type": "target", "input": _LONG_PROSE}, "# State\n")

    text = path.read_text(encoding="utf-8")
    _assert_value_on_one_line(text, "input", _LONG_PROSE)
    data, _ = read_frontmatter(path)
    assert data["input"] == _LONG_PROSE

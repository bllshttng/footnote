"""Lossless list round-trip for the hand-rolled plan-frontmatter serializer.

`fno.plan._stamp` and `skills/blueprint/scripts/mutate_doc.py` are two
independent frontmatter serializers, and every plan doc passes through both.
This module pins them to one observable behavior: a list item's parsed value
survives any number of round-trips through either writer.

READ BEFORE ADDING A TEST HERE. Assert parsed VALUES, never file text. The
pre-fix parser split an inline list on `,` before stripping quotes, so
`sources: ["a, b", c]` parsed to `['"a', 'b"', 'c']` while re-serializing to
byte-identical text. A text-identity assertion therefore PASSES against the
broken parser - that mistake was made once already during the investigation.
"""
from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import yaml

from fno.plan._stamp import (
    RawBlock,
    parse_frontmatter,
    serialize_frontmatter,
)

_REPO_ROOT = Path(__file__).resolve().parents[4]
_MUTATE_DOC = _REPO_ROOT / "skills" / "blueprint" / "scripts" / "mutate_doc.py"


def _load_mutate_module():
    spec = importlib.util.spec_from_file_location("mutate_doc_corpus", _MUTATE_DOC)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules["mutate_doc_corpus"] = module
    spec.loader.exec_module(module)
    return module


def _roundtrip(fields: dict) -> dict:
    """Serialize through _stamp and read the result back."""
    text = serialize_frontmatter(fields)
    parsed, _block, _rest = parse_frontmatter(f"---\n{text}\n---\nbody\n")
    return parsed


# ---------------------------------------------------------------------------
# AC1-HP / AC3-HP: a comma-containing item survives repeated round-trips
# ---------------------------------------------------------------------------

_COMMA_ITEM = "docs/report 1,000 items.md"


def test_comma_item_survives_three_roundtrips() -> None:
    """AC1-HP: the reported case. Round two used to yield three items."""
    doc = f"---\nsources:\n  - {_COMMA_ITEM}\n  - docs/b.md\n---\nbody\n"
    fields, _block, _rest = parse_frontmatter(doc)
    assert fields["sources"] == [_COMMA_ITEM, "docs/b.md"]

    for _ in range(3):
        fields = _roundtrip(fields)
        assert fields["sources"] == [_COMMA_ITEM, "docs/b.md"]


def test_unquoted_comma_item_is_quoted_on_write() -> None:
    """AC3-HP: serializing an unquoted comma item must emit it re-readably."""
    text = serialize_frontmatter({"sources": [_COMMA_ITEM, "b.md"]})
    parsed, _block, _rest = parse_frontmatter(f"---\n{text}\n---\nbody\n")
    assert parsed["sources"] == [_COMMA_ITEM, "b.md"]


# ---------------------------------------------------------------------------
# AC2-ERR: a quoted comma item parses to its authored value
# ---------------------------------------------------------------------------


def test_quoted_comma_item_parses_to_authored_value() -> None:
    """AC2-ERR: the silent face.

    Current main round-trips this input byte-identically while handing callers
    `['"a', 'b"', 'c']`. The assertion is on the value for that reason.
    """
    fields, _block, _rest = parse_frontmatter('---\nsources: ["a, b", c]\n---\nbody\n')
    assert fields["sources"] == ["a, b", "c"]


def test_item_containing_a_quote_character_roundtrips() -> None:
    """An interior double quote must not swallow the rest of the list."""
    items = ['he said "hi"', "plain.md"]
    assert _roundtrip({"sources": items})["sources"] == items


def test_apostrophe_item_roundtrips() -> None:
    """A bare apostrophe is ordinary text, not a YAML quote to balance."""
    items = ["don't split me", "b.md"]
    assert _roundtrip({"sources": items})["sources"] == items


# ---------------------------------------------------------------------------
# AC5-EDGE: nothing else about the output changes
# ---------------------------------------------------------------------------


def test_empty_and_single_item_lists_keep_their_output() -> None:
    """AC5-EDGE: the boundary shapes keep their current inline spelling."""
    text = serialize_frontmatter({"urls": [], "session_ids": ["sess-1"]})
    assert "urls: []" in text
    assert "session_ids: [sess-1]" in text


def test_scalar_with_comma_is_untouched() -> None:
    """Only list items change; a raw scalar stays raw."""
    fields, _block, _rest = parse_frontmatter("---\ntitle: a, b\n---\nbody\n")
    assert fields["title"] == "a, b"
    assert "title: a, b" in serialize_frontmatter(fields)


def test_kill_criteria_rawblock_passes_through_byte_identical() -> None:
    """AC5-EDGE: opaque nested structures must be emitted unchanged."""
    block = (
        "---\n"
        "kill_criteria:\n"
        "  - name: iteration_ceiling\n"
        "    predicate: iteration > 15\n"
        "    reason: Too many iterations, planning likely wrong\n"
        "---\nbody\n"
    )
    fields, _b, _r = parse_frontmatter(block)
    assert isinstance(fields["kill_criteria"], RawBlock)
    assert serialize_frontmatter(fields) == block[4:-10].rstrip("\n")


def test_no_frontmatter_returns_empty() -> None:
    fields, block, rest = parse_frontmatter("# just a body\n")
    assert (fields, block, rest) == ({}, "", "# just a body\n")


# ---------------------------------------------------------------------------
# AC4-HP: block form survives the projection
# ---------------------------------------------------------------------------


def test_block_list_stays_block() -> None:
    """AC4-HP: a block list must not be flattened to inline on rewrite.

    Cosmetic on its own - the comma is already safe after the parse/serialize
    fix - but it ends the block -> inline -> block oscillation between this
    writer and the vault's formatter.
    """
    doc = "---\nsources:\n  - docs/a.md\n  - docs/b.md\n---\nbody\n"
    fields, _block, _rest = parse_frontmatter(doc)
    assert serialize_frontmatter(fields) == "sources:\n  - docs/a.md\n  - docs/b.md"


def test_added_keys_stay_inline() -> None:
    """Only keys READ as block keep block form; new ones stay inline."""
    fields, _block, _rest = parse_frontmatter("---\ntitle: t\n---\nbody\n")
    fields["session_ids"] = ["sess-1"]
    assert "session_ids: [sess-1]" in serialize_frontmatter(fields)


def test_empty_block_list_falls_back_to_inline() -> None:
    """A childless block list would read back as a scalar, so it goes inline."""
    fields, _block, _rest = parse_frontmatter(
        "---\nsources:\n  - only.md\n---\nbody\n"
    )
    fields["sources"].clear()
    assert "sources: []" in serialize_frontmatter(fields)


def test_projection_preserves_block_form(tmp_path: Path) -> None:
    """AC4-HP through the real call site: the graph -> doc projection."""
    from fno.plan._project import project_node_to_plan

    plan = tmp_path / "plan.md"
    plan.write_text(
        "---\ntitle: t\nstatus: ready\npriority: p2\n"
        f"sources:\n  - {_COMMA_ITEM}\n  - docs/b.md\n---\nbody\n",
        encoding="utf-8",
    )

    assert project_node_to_plan({"priority": "p1"}, plan) is True

    text = plan.read_text(encoding="utf-8")
    assert f"sources:\n  - {_COMMA_ITEM}\n" in text
    fields, _block, _rest = parse_frontmatter(text)
    assert fields["sources"] == [_COMMA_ITEM, "docs/b.md"]


# ---------------------------------------------------------------------------
# AC7-ERR: malformed input degrades rather than raises
# ---------------------------------------------------------------------------


def test_unbalanced_quote_degrades_without_raising() -> None:
    """AC7-ERR: `project_node_to_plan` must not fail because a doc is odd."""
    fields, _block, _rest = parse_frontmatter('---\nsources: ["a, b]\n---\nbody\n')
    assert isinstance(fields["sources"], list)
    serialize_frontmatter(fields)  # must not raise


# ---------------------------------------------------------------------------
# AC6-CON: both serializers agree on a shared corpus
# ---------------------------------------------------------------------------

# One item per interesting shape. Values only - text identity is not asserted.
CORPUS: dict[str, list[str]] = {
    "plain": ["docs/a.md", "docs/b.md"],
    "comma": [_COMMA_ITEM, "docs/b.md"],
    "quoted": ['he said "hi"'],
    "quoted_comma": ['"a, b"', "c"],
    "apostrophe": ["don't", "b"],
    "empty": [],
    "single": ["only.md"],
    "leading_space": [" padded ", "b"],
}


def test_corpus_roundtrips_through_stamp() -> None:
    """AC6-CON: the hand-rolled writer preserves every corpus value."""
    fields = dict(CORPUS)
    for _ in range(2):
        fields = _roundtrip(fields)
        for key, expected in CORPUS.items():
            assert fields[key] == expected, f"{key} lost its value"


def test_corpus_roundtrips_through_mutate_doc() -> None:
    """AC6-CON: the PyYAML writer preserves the same corpus values."""
    mutate_doc = _load_mutate_module()
    text = mutate_doc._serialize_frontmatter(dict(CORPUS))
    assert yaml.safe_load(text) == CORPUS


def test_corpus_survives_a_handoff_between_the_two_writers() -> None:
    """The real cycle: /blueprint writes with PyYAML, the projection rewrites.

    Both directions, because a plan doc alternates between them in practice.
    """
    mutate_doc = _load_mutate_module()

    # PyYAML -> _stamp
    via_yaml = yaml.safe_load(mutate_doc._serialize_frontmatter(dict(CORPUS)))
    assert _roundtrip(via_yaml) == CORPUS

    # _stamp -> PyYAML
    via_stamp = _roundtrip(dict(CORPUS))
    assert yaml.safe_load(mutate_doc._serialize_frontmatter(via_stamp)) == CORPUS

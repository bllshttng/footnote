"""The Rust ephemeral set equals the schema's, entry for entry.

Two write boundaries route on this class (Python `append_event`, Rust
`EventEmitter::write_line`). The Rust side states the set as a const in
`crates/fno-agents/src/events.rs`; this file parses that const out of the
source text - the same derivation pattern as
`test_rust_events_documented.py` - so a schema.yaml edit that flips a type's
class without touching the Rust const fails HERE, naming the drifted type,
instead of routing differently per language in production.
"""
from __future__ import annotations

import re
from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parents[3]
SCHEMA_PATH = REPO_ROOT / "cli/src/fno/events/schema.yaml"
EVENTS_RS = REPO_ROOT / "crates/fno-agents/src/events.rs"

# Floor guarding against a VACUOUS pass: a regex that matched the wrong block
# or nothing would compare an empty set against the schema's and pass on
# nothing. 12 is the schema's declared ephemeral count at the time of writing;
# legitimate growth moves the number up with it.
MIN_EPHEMERAL_TYPES = 12


def parse_ephemeral_types(text: str) -> list[str]:
    """Pull the EPHEMERAL_EVENT_TYPES entries out of events.rs source text."""
    start = text.find("pub const EPHEMERAL_EVENT_TYPES")
    assert start != -1, "EPHEMERAL_EVENT_TYPES const not found in events.rs"
    end = text.find("];", start)
    assert end != -1, "EPHEMERAL_EVENT_TYPES block is unterminated"
    names = re.findall(r'"([a-z0-9_]+)"', text[start:end])
    assert len(names) >= MIN_EPHEMERAL_TYPES, (
        f"parsed only {len(names)} names out of EPHEMERAL_EVENT_TYPES, expected "
        f"at least {MIN_EPHEMERAL_TYPES}. The const was probably reformatted and "
        "this parser needs updating; check the parse before concluding names "
        "were legitimately removed."
    )
    return names


@pytest.fixture(scope="module")
def schema_ephemeral() -> set[str]:
    schema = yaml.safe_load(SCHEMA_PATH.read_text(encoding="utf-8"))
    default = schema.get("retention", {}).get("default", "durable")
    return {
        e["name"]
        for e in schema.get("event_types", [])
        if e.get("retention", default) == "ephemeral"
    }


def test_floor_rejects_a_truncated_parse() -> None:
    with pytest.raises(AssertionError, match="check the parse"):
        parse_ephemeral_types(
            'pub const EPHEMERAL_EVENT_TYPES: &[&str] = &[\n    "a",\n];\n'
        )


def test_const_exists_and_is_not_empty() -> None:
    assert EVENTS_RS.exists(), f"{EVENTS_RS} not found; did the crate move?"
    names = parse_ephemeral_types(EVENTS_RS.read_text(encoding="utf-8"))
    assert len(names) >= MIN_EPHEMERAL_TYPES


def test_rust_ephemeral_set_equals_schema(schema_ephemeral: set[str]) -> None:
    names = parse_ephemeral_types(EVENTS_RS.read_text(encoding="utf-8"))
    rust = set(names)
    assert rust == schema_ephemeral, (
        f"EPHEMERAL_EVENT_TYPES drifted from schema.yaml: "
        f"only-in-rust={sorted(rust - schema_ephemeral)} "
        f"only-in-schema={sorted(schema_ephemeral - rust)}. Flip the class in "
        "both places or the two write boundaries route differently."
    )


def test_sibling_suffix_identical_in_both_languages() -> None:
    from fno.events import EPHEMERAL_SUFFIX

    text = EVENTS_RS.read_text(encoding="utf-8")
    m = re.search(r'const EPHEMERAL_SUFFIX: &str = "([^"]+)"', text)
    assert m, "EPHEMERAL_SUFFIX string const not found in events.rs"
    assert m.group(1) == EPHEMERAL_SUFFIX, (
        f"Rust suffix {m.group(1)!r} != Python suffix {EPHEMERAL_SUFFIX!r}; "
        "the two boundaries would write two different sibling files."
    )

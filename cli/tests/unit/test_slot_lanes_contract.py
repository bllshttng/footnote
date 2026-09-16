"""The slot-lane vocabulary has ONE owner: crates/fno-agents/src/slot_lanes.toml.

Three copies must agree: the canonical table the Rust fold include_str!s,
the byte copy build.rs projects for the Python lanes projection, and the
agents.profiles Meta prose that documents the inline lane table.
"""

import re
import tomllib
from importlib.resources import files
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
CANONICAL = REPO / "crates" / "fno-agents" / "src" / "slot_lanes.toml"


def _canonical() -> dict:
    return tomllib.loads(CANONICAL.read_text(encoding="utf-8"))


def test_projected_copy_is_byte_equal_to_canonical():
    projected = files("fno.agents").joinpath("slot_lanes.toml").read_bytes()
    assert projected == CANONICAL.read_bytes(), (
        "cli/src/fno/agents/slot_lanes.toml drifted from "
        "crates/fno-agents/src/slot_lanes.toml; run cargo build -p fno-agents "
        "and commit the regenerated copy"
    )


def test_passthrough_is_a_subset_of_fields():
    table = _canonical()
    assert set(table["passthrough"]) <= set(table["fields"])


def test_meta_prose_names_every_declared_field():
    from fno.config.registry import FIELD_META

    match = re.search(r"\{([a-z_]+(?:,[a-z_]+)+)\}", FIELD_META["agents.profiles"].doc)
    assert match, "agents.profiles Meta prose lost the inline-lane {..} example"
    named = set(match.group(1).split(","))
    declared = set(_canonical()["fields"])
    assert named == declared, (
        "agents.profiles Meta prose vs slot_lanes.toml fields differ: "
        f"prose-only={sorted(named - declared)} table-only={sorted(declared - named)}"
    )

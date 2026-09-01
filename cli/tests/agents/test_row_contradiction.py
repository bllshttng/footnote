"""The Python emitter asserts the shared row-contradiction truth table."""

import json
from pathlib import Path

from fno.agents.row_contradiction import project_row


_FIXTURE = Path(__file__).resolve().parents[3] / "schemas" / "agents-row-contradiction.json"


def test_shared_row_contradiction_fixture() -> None:
    fixture = json.loads(_FIXTURE.read_text())
    for case in fixture["cases"]:
        actual = project_row(case["row"], now=fixture["now"])
        for key, expected in case["expected"].items():
            assert actual[key] == expected, case["name"]


# ---------------------------------------------------------------------------
# v23 (x-2019): the substitution verdict - the node's specimen table, verbatim
# ---------------------------------------------------------------------------

import pytest

from fno.agents.row_contradiction import model_substitution


@pytest.mark.parametrize(
    ("requested", "observed", "expected"),
    [
        # The operator's specimen trio, verbatim from the node.
        ("glm-5.3[1m]", {"kind": "observed", "model": "glm-5.3"}, "match"),
        (
            "glm-5.3-flash[1m]",
            {"kind": "observed", "model": "glm-5.3-flash"},
            "match",
        ),
        (
            "glm-5.3[1m]",
            {"kind": "observed", "model": "glm-5.3-flash"},
            "substituted",
        ),
        # A family change in either direction reads the same.
        (
            "glm-5.3-flash",
            {"kind": "observed", "model": "glm-5.3"},
            "substituted",
        ),
        # Missing / unreadable sides are UNKNOWN, never a match.
        (None, {"kind": "observed", "model": "glm-5.3"}, "unknown"),
        ("glm-5.3[1m]", {"kind": "no-transcript"}, "unknown"),
        ("glm-5.3[1m]", None, "unknown"),
        ("", {"kind": "observed", "model": "glm-5.3"}, "unknown"),
        (
            "glm-5.3[1m]",
            {"kind": "observed", "model": None},
            "unknown",
        ),
        # A bare observed string (the direct-call shape) still compares.
        ("glm-5.3[1m]", "glm-5.3", "match"),
        ("glm-5.3[1m]", "glm-5.3-flash", "substituted"),
    ],
)
def test_model_substitution_specimen_table(requested, observed, expected) -> None:
    assert model_substitution(requested, observed) == expected


@pytest.mark.parametrize(
    ("token", "family"),
    [
        # Twin-parity specimens: `_model_family` is line-for-line the Rust
        # `model_family` in state.rs, and each row here is asserted against
        # the Rust function's rule by hand. A divergence between the two
        # languages would make a claude-lane row and a daemon-served row
        # disagree on whether the same pair is substituted.
        ("glm-5.3[1m]", "glm-5.3"),
        ("[1m]", "[1m]"),  # a leading bracket group is the token, not a suffix
        ("a[b]c]", "a"),  # strip from the last '[' when the token ends ']'
        ("GLM-5.3", "glm-5.3"),
        (" glm-5.3 ", "glm-5.3"),
    ],
)
def test_model_family_matches_the_rust_rule(token, family) -> None:
    from fno.agents.row_contradiction import _model_family

    assert _model_family(token) == family

"""Decision graduation declarations are local, explicit, and machine-readable."""

from __future__ import annotations

import pytest


@pytest.mark.parametrize(
    ("kind", "reference", "expected"),
    [
        (
            "enforced",
            "test:tests.unit.test_decide::test_operator_only",
            {
                "kind": "enforced",
                "artifact": "test:tests.unit.test_decide::test_operator_only",
            },
        ),
        ("guidance", None, {"kind": "guidance"}),
        (
            "should-be-enforced-but-i-did-not",
            "node:x-90f5",
            {
                "kind": "should-be-enforced-but-i-did-not",
                "follow_up": "node:x-90f5",
            },
        ),
    ],
)
def test_validate_graduation_returns_structured_declaration(
    kind: str, reference: str | None, expected: dict[str, str]
):
    from fno.decide.graduation import validate_graduation

    assert validate_graduation(kind, reference) == expected


def test_omitted_graduation_defaults_to_honest_guidance():
    from fno.decide.graduation import graduation_or_guidance

    assert graduation_or_guidance(None, None) == {"kind": "guidance"}


@pytest.mark.parametrize(
    ("kind", "reference"),
    [
        ("", None),
        ("unknown", None),
        ("enforced", None),
        ("enforced", "future:maybe"),
        ("guidance", "file:cli/src/fno/decide/__init__.py"),
        ("should-be-enforced-but-i-did-not", None),
        ("should-be-enforced-but-i-did-not", "x-90f5"),
    ],
)
def test_validate_graduation_refuses_missing_or_malformed_declarations(
    kind: str, reference: str | None
):
    from fno.decide.graduation import InvalidGraduationError, validate_graduation

    with pytest.raises(InvalidGraduationError):
        validate_graduation(kind, reference)

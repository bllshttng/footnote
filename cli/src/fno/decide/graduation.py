"""Structured graduation declarations for durable decisions."""

from __future__ import annotations

import re

GRADUATION_KINDS = (
    "enforced",
    "guidance",
    "should-be-enforced-but-i-did-not",
)
_ARTIFACT_RE = re.compile(r"^(file|test|doc|gate|default):\S(?:.*\S)?$")
_FOLLOW_UP_RE = re.compile(r"^node:[a-z][a-z0-9]*-[0-9a-f]+$", re.IGNORECASE)


class InvalidGraduationError(ValueError):
    """A decision graduation declaration is missing or malformed."""


def validate_graduation(kind: str | None, reference: str | None) -> dict[str, str]:
    """Validate one local declaration without guessing a future artifact."""
    normalized_kind = (kind or "").strip().lower()
    normalized_ref = (reference or "").strip()
    if normalized_kind not in GRADUATION_KINDS:
        raise InvalidGraduationError(
            "graduation must be enforced, guidance, or "
            "should-be-enforced-but-i-did-not"
        )
    if normalized_kind == "guidance":
        if normalized_ref:
            raise InvalidGraduationError("guidance takes no graduation reference")
        return {"kind": "guidance"}
    if normalized_kind == "enforced":
        if not _ARTIFACT_RE.fullmatch(normalized_ref):
            raise InvalidGraduationError(
                "enforced graduation requires file:, test:, doc:, gate:, or default:"
            )
        return {"kind": "enforced", "artifact": normalized_ref}
    if not _FOLLOW_UP_RE.fullmatch(normalized_ref):
        raise InvalidGraduationError(
            "should-be-enforced-but-i-did-not requires node:<id>"
        )
    return {
        "kind": "should-be-enforced-but-i-did-not",
        "follow_up": normalized_ref.lower(),
    }


def graduation_or_guidance(
    kind: str | None, reference: str | None
) -> dict[str, str]:
    """Default an omitted declaration to honest guidance."""
    if kind is None and reference is None:
        return {"kind": "guidance"}
    return validate_graduation(kind, reference)


def normalize_graduation(value: dict[str, str] | None) -> dict[str, str]:
    """Validate an engine-level declaration or supply the guidance default."""
    if value is None:
        return {"kind": "guidance"}
    if not isinstance(value, dict):
        raise InvalidGraduationError("graduation must be an object")
    kind = value.get("kind")
    reference = value.get("artifact") or value.get("follow_up")
    return validate_graduation(kind, reference)


__all__ = [
    "GRADUATION_KINDS",
    "InvalidGraduationError",
    "graduation_or_guidance",
    "normalize_graduation",
    "validate_graduation",
]

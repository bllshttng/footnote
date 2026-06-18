"""Section ownership allowlist for lean single-doc plan architecture.

Enforces Locked Decision #3: /blueprint may only write to the sections
it owns. Any attempt to write to a /think-owned or /do-owned section
raises OwnershipViolation.

The allowlist is a frozenset to prevent accidental mutation at runtime.
Future skill additions must extend their own allowlist constants, never
widen BLUEPRINT_WRITE_ALLOWLIST.
"""

from __future__ import annotations

BLUEPRINT_WRITE_ALLOWLIST: frozenset[str] = frozenset(
    {
        "Execution Strategy",
        "File Ownership Map",
        "Patterns to Reuse",
        # Frontmatter keys /blueprint writes. Routed through the same allowlist
        # so the ownership model covers frontmatter writes, not just sections.
        "kill_criteria",
        "execution_mode",
        "waves",
    }
)


class OwnershipViolation(ValueError):
    """Raised when /blueprint attempts to write a section it doesn't own."""


def assert_blueprint_can_write(section_name: str) -> None:
    """Raise OwnershipViolation if section_name is not in BLUEPRINT_WRITE_ALLOWLIST.

    The error message includes:
    - the attempted section name
    - the full allowlist (sorted) so callers can debug

    Per Locked Decision #3 in the lean-blueprint plan: /blueprint's writes
    are guarded by this allowlist; attempting to write any other section is
    a programmer error.
    """
    if section_name not in BLUEPRINT_WRITE_ALLOWLIST:
        sorted_allowlist = sorted(BLUEPRINT_WRITE_ALLOWLIST)
        raise OwnershipViolation(
            f"/blueprint attempted to write section {section_name!r}, which is not in "
            f"BLUEPRINT_WRITE_ALLOWLIST. Allowed sections: {sorted_allowlist}"
        )


def check_blueprint_can_write(section_name: str) -> bool:
    """Non-raising variant. Returns True if section_name is in the allowlist."""
    return section_name in BLUEPRINT_WRITE_ALLOWLIST

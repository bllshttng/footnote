"""Tests for the locked-decision parser (``fno.executor._locked``)."""
from __future__ import annotations

from fno.executor._locked import parse_locked_executor


def _doc(locked_body: str) -> str:
    return (
        "# Some design\n\n"
        "## Architecture\n\n"
        "The operator resolves an executor: impeccable for frontend surfaces.\n\n"
        "## Locked Decisions\n\n" + locked_body + "\n\n"
        "## Open Questions\n\n"
        "1. Anything?\n"
    )


# --- the canonical shapes /think is prompted to emit -------------------------


def test_canonical_plan_level_entry():
    body = "5. **Executor routing**: plan-level `executor: impeccable` (auto-detected)."
    assert parse_locked_executor(_doc(body)) == "impeccable"


def test_canonical_bullet_entry():
    body = "- **Executor routing**: plan-level `executor: tdd` (cli-flag)."
    assert parse_locked_executor(_doc(body)) == "tdd"


def test_do_alias_normalizes_to_tdd():
    body = "- **Executor routing**: plan-level `executor: do` (cli-flag)."
    assert parse_locked_executor(_doc(body)) == "tdd"


def test_canonical_mixed_entry_is_mixed():
    """Plan-level ``do`` plus per-task ``impeccable`` overrides is ``mixed``.

    Taking the last kv in the entry would return ``impeccable`` and route the
    whole plan through the frontend pipeline, which is both wrong and the more
    expensive of the two mistakes.
    """
    body = (
        "5. **Executor routing**: plan-level `executor: tdd` with per-task overrides\n"
        "   `executor: impeccable` on tasks touching `**/*.tsx`, `components/**` (auto-detected).\n"
        "   Rationale: design has a frontend page and a backend migration."
    )
    assert parse_locked_executor(_doc(body)) == "mixed"


# --- the bare ``Executor:`` entry shape --------------------------------------


def test_bare_executor_entry_parses():
    """A numbered Locked Decision that states the executor directly.

    ``parse_locked_model`` already accepts a line-anchored bare ``Model:``; the
    executor lock accepted only the two-word ``Executor routing:`` phrasing, so
    a doc written this way lost its lock silently and fell through to surface
    inference.
    """
    body = "5. **Executor: `tdd` (archer / TDD).** Backend-only Python + config, no UI surface."
    assert parse_locked_executor(_doc(body)) == "tdd"


def test_bare_executor_entry_bold_key_form():
    assert parse_locked_executor(_doc("- **Executor:** impeccable")) == "impeccable"


# --- the guards that must survive both changes -------------------------------


def test_mid_prose_executor_mention_is_not_a_lock():
    """Anchoring at line start keeps prose inside Locked Decisions from locking."""
    body = "1. We rejected the idea that the executor: impeccable resolver should be per-record."
    assert parse_locked_executor(_doc(body)) == ""


def test_executor_mention_outside_locked_decisions_is_not_a_lock():
    assert parse_locked_executor(_doc("1. Nothing about routing here.")) == ""


def test_unknown_value_emits_empty():
    assert parse_locked_executor(_doc("5. **Executor: archer**")) == ""


def test_last_entry_wins_across_separate_entries():
    """Two separate entries: most-recent intent wins (not ``mixed``)."""
    body = "1. **Executor: impeccable**\n\n2. **Executor: `tdd`**"
    assert parse_locked_executor(_doc(body)) == "tdd"


def test_no_locked_decisions_section():
    assert parse_locked_executor("# Doc\n\n## Architecture\n\nexecutor: do\n") == ""


def test_empty_text():
    assert parse_locked_executor("") == ""


def test_rejected_alternative_prose_does_not_become_mixed():
    """Naming a rejected option must not manufacture a 'mixed' routing decision.

    Last-wins still governs WHICH value survives, and it reads position, not
    negation - so an entry that trails with its rejected option still resolves to
    that option. That is a pre-existing limit of the ported last-wins rule, not
    something this gate changes; what it must never do is silently upgrade the
    entry to 'mixed' and route the whole plan through the frontend pipeline.
    """
    doc = (
        "## Locked Decisions\n\n"
        "5. **Executor routing**: `executor: do` (archer / TDD), "
        "not the rejected `executor: impeccable` option.\n"
    )
    assert parse_locked_executor(doc) != "mixed"


def test_decision_history_prose_keeps_the_stated_choice():
    doc = (
        "## Locked Decisions\n\n"
        "5. **Executor routing**: changed from `executor: impeccable` "
        "to `executor: tdd` after review.\n"
    )
    assert parse_locked_executor(doc) == "tdd"


def test_documented_override_shape_still_resolves_mixed():
    doc = (
        "## Locked Decisions\n\n"
        "5. **Executor routing**: plan-level `executor: tdd` with per-task "
        "overrides `executor: impeccable` on tasks touching `**/*.tsx`.\n"
    )
    assert parse_locked_executor(doc) == "mixed"


def test_explicit_mixed_needs_no_inference():
    doc = "## Locked Decisions\n\n5. **Executor**: `executor: mixed`\n"
    assert parse_locked_executor(doc) == "mixed"

"""A wrapped frontmatter scalar fails loud, and says what to do about it.

Plan frontmatter is read by a line-based parser, so a scalar wrapped across
two physical lines is unparseable. It already raised - what it did not do was
name the key that ran on, leaving the reader a line number and a continuation
fragment to trace back by hand.

Both the parser and the `plan graduate` verb are asserted, because the message
is only useful if it survives to stderr; a guard checked one layer below the
one an operator actually reads is a guard nobody sees.
"""
from __future__ import annotations

import argparse

import pytest

from fno.plan._stamp import cmd_graduate, cmd_stamp, parse_frontmatter

WRAPPED = """---
node: x-1234
kill_criteria: kill any lint whose false-positive rate exceeds its catch
  rate in the first month
status: in_review
---

# body
"""


def test_wrapped_scalar_names_the_key_and_the_rule() -> None:
    with pytest.raises(ValueError) as exc:
        parse_frontmatter(WRAPPED)
    msg = str(exc.value)
    assert "kill_criteria" in msg, f"error does not name the runaway key: {msg}"
    assert "must be single-line" in msg, f"error does not state the rule: {msg}"


def test_single_line_scalar_of_the_same_value_parses() -> None:
    """Positive control: the value is fine, only the line break is not.

    Without this, the test above would still pass if the parser had started
    rejecting `kill_criteria` for some unrelated reason.
    """
    fields, _, _ = parse_frontmatter(WRAPPED.replace(
        "catch\n  rate in the first month", "catch rate in the first month"
    ))
    assert fields["kill_criteria"].endswith("in the first month")
    assert fields["status"] == "in_review"


def test_comment_between_key_and_continuation_names_no_key() -> None:
    """A wrong key name is worse than none.

    A commented-out key followed by an indented line must not be blamed on the
    last real key above the comment; the message drops the attribution instead.
    """
    with pytest.raises(ValueError) as exc:
        parse_frontmatter(
            "---\nstatus: in_review\n# Optional: depends_on:\n  - x-1234\n---\n\n# body\n"
        )
    msg = str(exc.value)
    assert "must be single-line" in msg
    assert "status" not in msg, f"blamed the wrong key: {msg}"
    assert "continuation of" not in msg, f"claimed an attribution it lacks: {msg}"


def test_graduate_surfaces_the_message_on_stderr(tmp_path, capsys) -> None:
    """AC1-UI: the operator-facing verb exits 1 and prints the guidance."""
    plan = tmp_path / "plan.md"
    plan.write_text(WRAPPED, encoding="utf-8")

    rc = cmd_graduate(argparse.Namespace(plan_path=str(plan)))

    assert rc == 1
    err = capsys.readouterr().err
    assert "kill_criteria" in err
    assert "must be single-line" in err


def test_stamp_surfaces_the_message_on_stderr(tmp_path, capsys) -> None:
    """The ship-time path gets the same treatment as graduate.

    `read_plan_file` has sibling callers with the same handler shape, and stamp
    is the one the ship gate actually runs, so covering only graduate would leave
    the layering argument in this module's docstring half-tested.
    """
    plan = tmp_path / "plan.md"
    plan.write_text(WRAPPED, encoding="utf-8")

    rc = cmd_stamp(argparse.Namespace(
        plan_path=str(plan),
        expected_url_count=None,
        status=None,
        url=None,
        session_id=None,
        dry_run=False,
    ))

    assert rc == 1
    err = capsys.readouterr().err
    assert "kill_criteria" in err
    assert "must be single-line" in err

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

from fno.plan._stamp import cmd_graduate, parse_frontmatter

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


def test_graduate_surfaces_the_message_on_stderr(tmp_path, capsys) -> None:
    """AC1-UI: the operator-facing verb exits 1 and prints the guidance."""
    plan = tmp_path / "plan.md"
    plan.write_text(WRAPPED, encoding="utf-8")

    rc = cmd_graduate(argparse.Namespace(plan_path=str(plan)))

    assert rc == 1
    err = capsys.readouterr().err
    assert "kill_criteria" in err
    assert "must be single-line" in err

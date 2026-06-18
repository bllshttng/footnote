#!/usr/bin/env python3
"""Tests for the in-package fno.cost._session_cost module (the former
scripts/metrics/session-cost.py).

Run: python3 tests/test_session_cost.py   OR   pytest tests/test_session_cost.py
"""
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
# session-cost.py moved into the fno package as fno.cost._session_cost.
sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))
from fno.cost import _session_cost as session_cost  # noqa: E402


def test_render_tasks_md_pr_url_without_pr_number():
    """Regression test for ab-bff83e85.

    Most real ledger entries (1529/1605 at time of filing) lack the
    pr_number key, and at least one of those also carries pr_url. The
    pr_url branch indexed e['pr_number'] directly, so rendering crashed
    with KeyError and blocked ledger.md regeneration for ALL sessions.
    """
    entries = [{"title": "no-number entry", "pr_url": "https://github.com/o/r/pull/7"}]
    md = session_cost.render_tasks_md(entries)
    assert "[#?](https://github.com/o/r/pull/7)" in md, (
        "pr_url-without-pr_number entry should render a placeholder link"
    )


def test_render_tasks_md_pr_url_with_pr_number():
    """Happy path: both keys present renders a real numbered link."""
    entries = [{
        "title": "numbered entry",
        "pr_number": 42,
        "pr_url": "https://github.com/o/r/pull/42",
    }]
    md = session_cost.render_tasks_md(entries)
    assert "[#42](https://github.com/o/r/pull/42)" in md


def test_render_tasks_md_no_pr_fields():
    """Neither key present: existing .get() fallback renders #?."""
    entries = [{"title": "bare entry"}]
    md = session_cost.render_tasks_md(entries)
    assert "PR: #?" in md


def test_render_tasks_md_pr_number_explicit_none():
    """Gemini on PR #442: pr_number: null in JSON loads as None; the
    placeholder must render '?', not 'None'."""
    entries = [{
        "title": "null entry",
        "pr_number": None,
        "pr_url": "https://github.com/o/r/pull/9",
    }]
    md = session_cost.render_tasks_md(entries)
    assert "[#?](https://github.com/o/r/pull/9)" in md
    assert "#None" not in md


def _run_standalone() -> int:
    failed = 0
    for name, fn in list(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS  {name}")
            except AssertionError as exc:
                failed += 1
                print(f"FAIL  {name}\n      {exc}")
            except Exception as exc:
                failed += 1
                print(f"ERROR {name}\n      {type(exc).__name__}: {exc}")
    return failed


if __name__ == "__main__":
    sys.exit(_run_standalone())

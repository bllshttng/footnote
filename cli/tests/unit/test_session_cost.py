#!/usr/bin/env python3
"""Tests for the in-package fno.cost._session_cost module (the former
scripts/metrics/session-cost.py).

Run: fno doctor test cli/tests/unit/test_session_cost.py
"""
import sys

# Family goldens live under cli/tests (the cost family child owns them);
# fno resolves through the pinned worktree PYTHONPATH.
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


def test_count_user_vs_mail_separates_delivered_mail():
    """A user turn that IS delivered mail counts as mail, never operator."""
    metrics = session_cost.SessionMetrics(session_id="test-session")

    # Regular human operator message
    session_cost._count_user_vs_mail(metrics, ["Please implement feature X"])
    assert metrics.user_messages == 1
    assert metrics.mail_messages == 0

    # Peer mail message carrying <fno_mail> tag
    session_cost._count_user_vs_mail(
        metrics,
        ['<fno_mail from="cc-12345678" harness="claude">here is the status</fno_mail>'],
    )
    assert metrics.user_messages == 1
    assert metrics.mail_messages == 1

    # A delivered header line reads as mail too
    session_cost._count_user_vs_mail(
        metrics, ["`@candor · fmail-abc123def456 · peer update`"]
    )
    assert metrics.user_messages == 1
    assert metrics.mail_messages == 2


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

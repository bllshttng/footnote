#!/usr/bin/env python3
"""Tests for fno.cost._session_cost render_tasks_md (the former
scripts/metrics/session-cost.py).

Run: python3 tests/test_session_cost_render.py   OR   pytest tests/test_session_cost_render.py
"""
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
# session-cost.py moved into the fno package as fno.cost._session_cost.
sys.path.insert(0, str(REPO_ROOT / "cli" / "src"))
from fno.cost import _session_cost as session_cost  # noqa: E402


def test_render_entry_with_pr_url_but_no_pr_number():
    """Regression test for ab-bff83e85.

    1529 of ~1605 ledger.json entries predate the pr_number key; at least
    one also carries pr_url. The renderer indexed e['pr_number'] unguarded
    in the pr_url branch, so a single legacy entry broke ledger.md
    regeneration for ALL sessions.
    """
    entries = [
        {
            "title": "legacy entry",
            "pr_url": "https://github.com/bllshttng/footnote/pull/1",
        }
    ]
    out = session_cost.render_tasks_md(entries)
    assert "pull/1" in out, "pr_url should still be rendered as a link"
    assert "legacy entry" in out


def test_render_entry_with_neither_pr_key():
    """Entries with no PR info at all render the '#?' placeholder."""
    entries = [{"title": "no pr"}]
    out = session_cost.render_tasks_md(entries)
    assert "#?" in out
    assert "no pr" in out


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
                print(f"FAIL  {name}\n      {type(exc).__name__}: {exc}")
    return failed


if __name__ == "__main__":
    sys.exit(1 if _run_standalone() else 0)

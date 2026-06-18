"""Unit tests for graph.cli._stamp_and_graduate_plan (ab-bd9f476c).

The completion path (``done``/``reconcile``) must STAMP a plan shipped before
graduating it: ``graduate`` alone is a no-op on a plan that never went through
target's ship gate (status never became ``shipped``), so a never-stamped plan's
frontmatter would never record the ship. These tests drive the real in-package
``fno.plan._stamp`` module (run via ``python3 -m``) against temp plan files.

Filter: `python -m pytest tests/ -k stamp_and_graduate`
"""
from __future__ import annotations

from pathlib import Path

from fno.graph.cli import _stamp_and_graduate_plan


def _write_plan(p: Path, *, status: str | None = None) -> None:
    fm = ["---", "title: t"]
    if status is not None:
        fm.append(f"status: {status}")
    fm += ["---", "", "body"]
    p.write_text("\n".join(fm) + "\n", encoding="utf-8")


def test_never_shipped_plan_with_url_reaches_done(tmp_path):
    """A ready plan + a ship url is stamped shipped then graduated to done."""
    plan = tmp_path / "plan.md"
    _write_plan(plan, status="ready")

    ok = _stamp_and_graduate_plan(
        str(plan), url="https://github.com/o/r/pull/9", session_id="sess-1"
    )

    assert ok is True
    text = plan.read_text()
    assert "status: done" in text  # one url >= expected (default 1) -> graduated
    assert "shipped_at:" in text
    assert "https://github.com/o/r/pull/9" in text
    assert "session_ids: [sess-1]" in text


def test_no_url_is_graduate_only_noop_on_never_shipped(tmp_path):
    """Without a url we do NOT assert a ship: graduate alone no-ops, leaving a
    never-shipped plan untouched (prior conservative behavior)."""
    plan = tmp_path / "plan.md"
    _write_plan(plan, status="ready")

    _stamp_and_graduate_plan(str(plan), url=None)

    text = plan.read_text()
    assert "status: ready" in text  # unchanged
    assert "shipped_at:" not in text


def test_already_done_plan_is_not_downgraded(tmp_path):
    """Re-closing a node whose plan is already done leaves status done."""
    plan = tmp_path / "plan.md"
    _write_plan(plan, status="done")

    ok = _stamp_and_graduate_plan(
        str(plan), url="https://github.com/o/r/pull/9", session_id="s"
    )

    assert ok is True
    assert "status: done" in plan.read_text()


def test_default_session_id_when_unset(tmp_path):
    """A close with no owning session id still stamps, using a synthetic id."""
    plan = tmp_path / "plan.md"
    _write_plan(plan, status="ready")

    ok = _stamp_and_graduate_plan(str(plan), url="https://github.com/o/r/pull/3")

    assert ok is True
    assert "session_ids: [backlog-close]" in plan.read_text()


def test_stamp_run_failure_returns_false(tmp_path, monkeypatch):
    """When the stamp module run fails, the helper is a safe no-op (returns False).

    The stamper is now an always-importable in-package module, so the prior
    "older install lacking the script" path is gone; the surviving non-fatal
    contract is that a failed/raised subprocess run yields False without raising.
    """
    import subprocess

    def _boom(*a, **k):
        raise OSError("spawn failed")

    monkeypatch.setattr(subprocess, "run", _boom)
    plan = tmp_path / "plan.md"
    _write_plan(plan, status="ready")

    assert _stamp_and_graduate_plan(str(plan), url="https://x/pull/1") is False
    # plan untouched
    assert "status: ready" in plan.read_text()

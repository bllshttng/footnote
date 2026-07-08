"""Characterization (parity) test for the moved stamp module (ab-58645f63).

`scripts/lib/stamp-plan.py` was moved verbatim into `cli/src/fno/plan/_stamp.py`
(a git mv, no logic change). This is a move-not-rewrite: the risk is silent
behavioral drift. This test pins the move two ways:

1. CONTRACT: run `python3 -m fno.plan._stamp <verb>` on temp fixtures and assert
   the resulting frontmatter + exit codes match the documented contract
   (stamp -> status: shipped + urls/session_ids; graduate -> status: done once
   the URL count is met; set-expected -> writes expected_url_count; bad count
   exits 2).

2. PARITY vs the pre-move script: pull the OLD `scripts/lib/stamp-plan.py` out of
   git history (`git show HEAD:...`) and run it on the SAME fixtures, then assert
   the resulting frontmatter is byte-identical to the module's (modulo the
   inherently-time-varying `shipped_at` timestamp, which is normalized out). If
   the move drifted, this diff catches it.

The parity leg is skipped (not failed) when git history is unavailable (e.g. a
shallow checkout that lacks the pre-move blob), so the contract leg always runs.
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
CLI_SRC = REPO_ROOT / "cli" / "src"

# Module invocation: `python3 -m fno.plan._stamp`, with cli/src on PYTHONPATH so
# the child resolves the package even when this test runs from a bare checkout.
MODULE_ARGS = [sys.executable, "-m", "fno.plan._stamp"]

# The pre-move script path inside git history (the blob still exists at HEAD's
# parent commits; we read it from whichever revision still carried it).
OLD_SCRIPT_GIT_PATH = "scripts/lib/stamp-plan.py"


def _module_env():
    import os

    env = os.environ.copy()
    existing = env.get("PYTHONPATH", "")
    env["PYTHONPATH"] = str(CLI_SRC) + (os.pathsep + existing if existing else "")
    return env


def _run_module(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [*MODULE_ARGS, *args],
        capture_output=True,
        text=True,
        env=_module_env(),
    )


# A representative plan with a nested block (kill_criteria) that exercises the
# RawBlock opaque-preservation path - the fragile part of the parser.
PLAN_FIXTURE = """\
---
title: Char test
created: 2026-05-01
scope: single-project
project: fno
expected_url_count: 1
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 10
    reason: too many
---

# Body content
"""


def _normalize(text: str) -> str:
    """Drop the time-varying shipped_at timestamp so two runs are comparable."""
    return re.sub(r"^shipped_at:.*$", "shipped_at: <NORMALIZED>", text, flags=re.MULTILINE)


# ---------------------------------------------------------------------------
# 1. CONTRACT
# ---------------------------------------------------------------------------

def test_module_stamp_then_graduate_contract(tmp_path):
    plan_dir = tmp_path / "plan"
    plan_dir.mkdir()
    idx = plan_dir / "00-INDEX.md"
    idx.write_text(PLAN_FIXTURE)

    r = _run_module(
        "stamp", "--plan-path", str(plan_dir),
        "--session-id", "SID-X", "--url", "https://example.com/pull/7",
    )
    assert r.returncode == 0, r.stderr
    text = idx.read_text()
    assert "status: shipped" in text
    assert "shipped_at:" in text
    assert "https://example.com/pull/7" in text
    assert "SID-X" in text
    # RawBlock preserved verbatim.
    assert "iteration_ceiling" in text

    r = _run_module("graduate", "--plan-path", str(plan_dir))
    assert r.returncode == 0, r.stderr
    text = idx.read_text()
    assert "status: done" in text  # one url >= expected (1) -> graduated
    assert "iteration_ceiling" in text  # still preserved across graduate


def test_module_set_expected_contract(tmp_path):
    doc = tmp_path / "design.md"
    doc.write_text("---\ntitle: Epic\nstatus: draft\n---\n# body\n")

    r = _run_module("set-expected", "--plan-path", str(doc), "--count", "3")
    assert r.returncode == 0, r.stderr
    assert "expected_url_count: 3" in doc.read_text()


def test_module_set_expected_rejects_below_one(tmp_path):
    doc = tmp_path / "design.md"
    original = "---\ntitle: Epic\nstatus: draft\n---\n# body\n"
    doc.write_text(original)

    r = _run_module("set-expected", "--plan-path", str(doc), "--count", "0")
    assert r.returncode != 0
    assert doc.read_text() == original  # untouched on rejection


def test_module_stamp_rejects_expected_url_count_below_one(tmp_path):
    doc = tmp_path / "design.md"
    original = "---\nstatus: draft\n---\n# body\n"
    doc.write_text(original)

    r = _run_module(
        "stamp", "--plan-path", str(doc),
        "--session-id", "sid", "--url", "https://x/pr/1",
        "--expected-url-count", "0",
    )
    assert r.returncode == 2, r.stderr
    assert doc.read_text() == original  # no partial stamp


# ---------------------------------------------------------------------------
# 2. PARITY vs the pre-move script
# ---------------------------------------------------------------------------

def _old_script_source() -> str | None:
    """Return the pre-move script source from git history, or None if absent.

    The blob was deleted at the tip of this branch (the move commit), so it
    lives on a parent commit. Probe HEAD and a few ancestors for it.
    """
    for rev in ("HEAD", "HEAD~1", "HEAD~2", "HEAD~3", "origin/main"):
        try:
            out = subprocess.run(
                ["git", "show", f"{rev}:{OLD_SCRIPT_GIT_PATH}"],
                cwd=REPO_ROOT,
                capture_output=True,
                text=True,
            )
        except OSError:
            return None
        if out.returncode == 0 and out.stdout.strip():
            return out.stdout
    return None


@pytest.mark.parametrize("verb_args", [
    ("stamp", "--session-id", "SID-P", "--url", "https://example.com/pull/3"),
    ("stamp", "--session-id", "SID-P", "--url", "https://example.com/pull/3",
     "--expected-url-count", "2"),
])
def test_parity_old_script_vs_module(tmp_path, verb_args):
    """The module and the pre-move script produce byte-identical frontmatter
    (modulo the normalized shipped_at timestamp) on the same fixture."""
    src = _old_script_source()
    if src is None:
        pytest.skip("pre-move stamp-plan.py not available in git history")

    old_script = tmp_path / "old_stamp.py"
    old_script.write_text(src)

    # Two identical plan dirs, one stamped by each implementation.
    old_dir = tmp_path / "old_plan"
    new_dir = tmp_path / "new_plan"
    for d in (old_dir, new_dir):
        d.mkdir()
        (d / "00-INDEX.md").write_text(PLAN_FIXTURE)

    old = subprocess.run(
        [sys.executable, str(old_script), *verb_args, "--plan-path", str(old_dir)],
        capture_output=True, text=True,
    )
    new = _run_module(*verb_args, "--plan-path", str(new_dir))

    assert old.returncode == new.returncode, (
        f"exit code drift: old={old.returncode} new={new.returncode}\n"
        f"old stderr: {old.stderr}\nnew stderr: {new.stderr}"
    )

    old_text = _normalize((old_dir / "00-INDEX.md").read_text())
    new_text = _normalize((new_dir / "00-INDEX.md").read_text())
    assert old_text == new_text, (
        "frontmatter drift between pre-move script and in-package module"
    )

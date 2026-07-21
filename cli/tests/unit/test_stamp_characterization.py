"""Contract test for the stamp module (cli/src/fno/plan/_stamp.py).

Runs `python3 -m fno.plan._stamp <verb>` on temp fixtures and asserts the
resulting frontmatter + exit codes match the documented contract (stamp ->
status: in_review + urls/session_ids; graduate -> status: done once the URL
count is met; set-expected -> writes expected_url_count; bad count exits 2).
"""
from __future__ import annotations

import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
CLI_SRC = REPO_ROOT / "cli" / "src"

# Module invocation: `python3 -m fno.plan._stamp`, with cli/src on PYTHONPATH so
# the child resolves the package even when this test runs from a bare checkout.
MODULE_ARGS = [sys.executable, "-m", "fno.plan._stamp"]


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


# ---------------------------------------------------------------------------
# 1. CONTRACT
# ---------------------------------------------------------------------------

def test_module_stamp_then_graduate_contract(tmp_path):
    doc = tmp_path / "plan.md"
    doc.write_text(PLAN_FIXTURE)

    r = _run_module(
        "stamp", "--plan-path", str(doc),
        "--session-id", "SID-X", "--url", "https://example.com/pull/7",
    )
    assert r.returncode == 0, r.stderr
    text = doc.read_text()
    assert "status: in_review" in text
    assert "shipped_at:" in text
    assert "https://example.com/pull/7" in text
    assert "SID-X" in text
    # RawBlock preserved verbatim.
    assert "iteration_ceiling" in text

    r = _run_module("graduate", "--plan-path", str(doc))
    assert r.returncode == 0, r.stderr
    text = doc.read_text()
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


def test_module_stamp_leaves_a_retired_spelling_in_place(tmp_path):
    """x-3ad5: a doc already stamped `shipped` reads AS in_review, so a restamp
    accumulates urls/session_ids without touching the status. The alias is
    read-path translation; the rename must never become a migration write.
    """
    doc = tmp_path / "plan.md"
    doc.write_text(PLAN_FIXTURE.replace("scope: single-project",
                                        "scope: single-project\nstatus: shipped"))

    r = _run_module(
        "stamp", "--plan-path", str(doc),
        "--session-id", "SID-Y", "--url", "https://example.com/pull/8",
    )
    assert r.returncode == 0, r.stderr
    text = doc.read_text()
    assert "status: shipped" in text  # spelling untouched
    assert "status: in_review" not in text
    assert "SID-Y" in text  # ...but the stamp still did its job

    # And it still graduates, because graduate reads the alias too.
    r = _run_module("graduate", "--plan-path", str(doc))
    assert r.returncode == 0, r.stderr
    assert "status: done" in doc.read_text()

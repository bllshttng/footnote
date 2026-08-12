"""Tests for the --curriculum complement layer of scripts/diagnostics/verb-callers.py.

verb-callers.py is the canonical caller-finding instrument (its iter_corpus
include-list is what avoids the skills/target rg-glob pitfall). These tests
cover only the curriculum layer added on top; the sweep, controls, and
skills/target-safe walk are exercised by the tool's own --self-check.
"""
from __future__ import annotations

import importlib.util
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "scripts" / "diagnostics" / "verb-callers.py"


def _load():
    spec = importlib.util.spec_from_file_location("verb_callers", SCRIPT)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


vc = _load()


def test_load_curriculum_strips_comments_and_flags_unknown(tmp_path):
    leaves = {"backlog get", "mail send", "agents loop-check"}
    curr = tmp_path / "curriculum.txt"
    curr.write_text(
        "# full-line comment\n"
        "backlog get\n"
        "mail send # trailing comment\n"
        "backlog nope\n"          # not a real leaf
        "\n"
    )
    taught, unknown = vc.load_curriculum(curr, leaves)
    assert taught == {"backlog get", "mail send"}
    assert unknown == ["backlog nope"]


def test_load_curriculum_empty_file(tmp_path):
    curr = tmp_path / "curriculum.txt"
    curr.write_text("# only comments\n\n")
    taught, unknown = vc.load_curriculum(curr, {"backlog get"})
    assert taught == set() and unknown == []


def test_curriculum_end_to_end_reports_complement():
    """The real curriculum computes a 277-verb complement with cull candidates.

    Slow (runs the corpus sweep); verifies the feature integrates with the
    canonical sweep + controls and that the checked-in curriculum is valid.
    """
    proc = subprocess.run(
        [sys.executable, str(SCRIPT),
         "--curriculum", str(REPO_ROOT / "scripts" / "ci" / "curriculum.txt")],
        cwd=REPO_ROOT, capture_output=True, text=True,
    )
    assert proc.returncode == 0, proc.stderr
    assert "taught (curriculum): 90" in proc.stdout
    assert "complement (untaught): 277" in proc.stdout
    assert "cull candidates in complement" in proc.stdout

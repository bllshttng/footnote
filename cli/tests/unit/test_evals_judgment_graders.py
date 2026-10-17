"""The judgment bank tasks' graders pass an honest answer and fail fakes.

A grader a degenerate answer can pass measures nothing. Each judgment task
is graded here against one honest end state and the fakes that would game
it: no change, a guard at the wrong site, a fix at one site of two, a review
that lists every file, a plan that keeps the restated count.
"""
from __future__ import annotations

import json
import shutil
from pathlib import Path

import pytest

from fno.evals import bank, grading

_REPO = Path(__file__).resolve().parents[3]
_BANK = _REPO / "evals" / "bank"
_FIX = "evals/fixtures/judgment-root-cause"
_REVIEW = "evals/fixtures/judgment-blast-radius"
_DESIGN = "evals/fixtures/judgment-design"
_INV = "evals/fixtures/judgment-investigate"
_INV_CODE = {
    f"{_INV}/cache.py": "_store = {}\n\n\ndef write(key, value, etag):\n    _store[key] = (value, etag)\n\n\n"
    "def read(key, etag):\n    value, stored = _store.get(key, (None, None))\n    return value if stored == etag else None\n",
    f"{_INV}/status.py": "import cache\n\n\ndef pr_state(n, etag):\n    return cache.read(f\"pr:{n}\", etag)\n",
    f"{_INV}/heal.py": "from cache import read\n\n\ndef needs_heal(n, etag):\n    return read(f\"pr:{n}\", etag) == \"conflicting\"\n",
    f"{_INV}/reconcile.py": "import cache as c\n\n\ndef merged(prs, etag):\n"
    "    return [n for n in prs if c.read(\"pr:%d\" % n, etag) == \"merged\"]\n",
}


def _inv_plan(unknowns: str, chosen: str) -> str:
    src = (_REPO / _INV / "plan.md").read_text(encoding="utf-8")
    routes, _, rest = src.partition("## Unknowns\n")
    done = rest.partition("## Done\n")[2]
    return f"{routes.rstrip()}\n{chosen}\n\n## Unknowns\n\n{unknowns}\n\n## Done\n{done}"


def _ledger(close: str, cancel: str) -> str:
    src = (_REPO / _FIX / "ledger.py").read_text(encoding="utf-8")
    return src.replace('row.get("closing_status")', close).replace(
        'row.get("cancelling_status")', cancel
    )


def _guarded_report() -> str:
    src = (_REPO / _FIX / "report.py").read_text(encoding="utf-8")
    return src.replace('row["status"].upper()', '(row["status"] or "closed").upper()')


def _findings(*hits: tuple[str, int]) -> str:
    return json.dumps([{"file": f"{_REVIEW}/{name}", "line": line} for name, line in hits])


# (task id, workdir-relative path -> content). The first case per task is the
# honest answer; every later one is a fake that must fail.
_HONEST = {
    "capability-judgment-root-cause": {
        f"{_FIX}/ledger.py": _ledger('"closed"', '"cancelled"'),
    },
    "capability-judgment-blast-radius": {
        "review-findings.json": _findings(("nightly.sh", 3), ("README.md", 5)),
    },
    "capability-judgment-design": {
        f"{_DESIGN}/plan.md": "## Routes\n\n1. Bump to `[&str; 5]` - ruled out: the length restates a count.\n"
        "2. KEYS becomes `&[&str]` so a new key carries no count.\n",
    },
    "capability-judgment-investigate": {
        **_INV_CODE,
        f"{_INV}/plan.md": _inv_plan(
            "- [x] which modules call `read_any` - settled: status.py, heal.py (from-import), reconcile.py (alias c)",
            "chosen: 2 - heal and reconcile also call read_any, which rules out route 1",
        ),
    },
}
_FAKES = {
    "root-cause: no change": ("capability-judgment-root-cause", {}),
    "root-cause: guard in the reader": (
        "capability-judgment-root-cause",
        {f"{_FIX}/report.py": _guarded_report(), f"{_FIX}/ledger.py": _ledger('row.get("closing_status")', '"cancelled"')},
    ),
    "root-cause: one site of two": (
        "capability-judgment-root-cause",
        {f"{_FIX}/ledger.py": _ledger('"closed"', 'row.get("cancelling_status")')},
    ),
    "blast-radius: empty review": ("capability-judgment-blast-radius", {"review-findings.json": "[]"}),
    "blast-radius: every file at line 1": (
        "capability-judgment-blast-radius",
        {"review-findings.json": _findings(("export.py", 1), ("nightly.sh", 1), ("README.md", 1))},
    ),
    "blast-radius: doc reader only": (
        "capability-judgment-blast-radius",
        {"review-findings.json": _findings(("README.md", 5))},
    ),
    "design: bumps the length": (
        "capability-judgment-design",
        {f"{_DESIGN}/plan.md": "## Design\n\nnone: change SUPERVISOR_KEYS to `[&str; 5]` and add the keys.\n"},
    ),
    "investigate: codes without settling": ("capability-judgment-investigate", dict(_INV_CODE)),
    "investigate: deletes the unknown": (
        "capability-judgment-investigate",
        {**_INV_CODE, f"{_INV}/plan.md": _inv_plan("none: obvious", "chosen: 2 - cleaner")},
    ),
    "investigate: the guessed caller only": (
        "capability-judgment-investigate",
        {
            f"{_INV}/status.py": _INV_CODE[f"{_INV}/status.py"],
            f"{_INV}/plan.md": _inv_plan("- [x] which modules call `read_any` - settled: status.py", "chosen: 1 - one caller"),
        },
    ),
    "design: names both shapes": (
        "capability-judgment-design",
        {f"{_DESIGN}/plan.md": "Use `[&str; 5]` now; `&[&str]` later.\n"},
    ),
}


def _grade(tmp_path: Path, task_id: str, files: dict[str, str]) -> grading.GradeOutcome:
    for fixture in (_FIX, _REVIEW, _DESIGN, _INV):
        shutil.copytree(_REPO / fixture, tmp_path / fixture)
    for rel, content in files.items():
        (tmp_path / rel).write_text(content, encoding="utf-8")
    task = bank.load_task(_BANK / f"{task_id}.yaml")
    return grading.grade(task, tmp_path)


@pytest.mark.parametrize("task_id", sorted(_HONEST))
def test_honest_answer_passes(tmp_path: Path, task_id: str) -> None:
    outcome = _grade(tmp_path, task_id, _HONEST[task_id])
    assert outcome.passed, outcome.reason


@pytest.mark.parametrize("case", sorted(_FAKES))
def test_fake_answer_fails(tmp_path: Path, case: str) -> None:
    task_id, files = _FAKES[case]
    outcome = _grade(tmp_path, task_id, files)
    assert not outcome.passed, f"{case} passed the {task_id} grader"

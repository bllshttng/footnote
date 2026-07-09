"""Bank loader + load-time validation (US1, AC4-EDGE).

The load-time discipline is the point: a task without a mechanical grade is
rejected naming the id and file, and an all-trivial grade warns.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.evals import bank


def _write(p: Path, text: str) -> Path:
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text, encoding="utf-8")
    return p


VALID = """
id: t-valid
tier: regression
grade:
  - kind: exit
    command: "pytest -q"
    expect: 0
  - kind: file-exists
    path: out.txt
  - kind: grep
    path: out.txt
    pattern: PASS
timeout_minutes: 10
tags: [smoke]
"""


def test_load_valid_task(tmp_path: Path) -> None:
    task = bank.load_task(_write(tmp_path / "t.yaml", VALID))
    assert task.id == "t-valid"
    assert task.tier == "regression"
    assert task.prompt is None  # grade-only
    assert [c.kind for c in task.grade] == ["exit", "file-exists", "grep"]
    assert task.grade[0].command == "pytest -q"
    assert task.grade[2].pattern == "PASS"
    assert task.timeout_minutes == 10
    assert task.tags == ["smoke"]


# AC4-EDGE: a bank YAML with an empty grade list exits non-zero naming id + file.
def test_empty_grade_rejected_naming_id_and_file(tmp_path: Path) -> None:
    p = _write(tmp_path / "bad.yaml", "id: t-nograde\ntier: capability\ngrade: []\n")
    with pytest.raises(bank.BankError) as exc:
        bank.load_task(p)
    msg = str(exc.value)
    assert "t-nograde" in msg
    assert "bad.yaml" in msg


def test_missing_grade_key_rejected(tmp_path: Path) -> None:
    p = _write(tmp_path / "bad.yaml", "id: t-x\ntier: regression\n")
    with pytest.raises(bank.BankError, match="t-x"):
        bank.load_task(p)


def test_bad_tier_rejected(tmp_path: Path) -> None:
    p = _write(tmp_path / "b.yaml",
               "id: t\ntier: nonsense\ngrade:\n  - {kind: exit, command: 'x'}\n")
    with pytest.raises(bank.BankError, match="tier"):
        bank.load_task(p)


def test_unknown_check_kind_rejected(tmp_path: Path) -> None:
    p = _write(tmp_path / "b.yaml",
               "id: t\ntier: regression\ngrade:\n  - {kind: bogus}\n")
    with pytest.raises(bank.BankError, match="kind"):
        bank.load_task(p)


def test_grep_requires_pattern(tmp_path: Path) -> None:
    p = _write(tmp_path / "b.yaml",
               "id: t\ntier: regression\ngrade:\n  - {kind: grep, path: a.txt}\n")
    with pytest.raises(bank.BankError, match="pattern"):
        bank.load_task(p)


# Silent-failure-hunter countermeasure: an all-trivial exit grade warns.
def test_decorative_grade_warns(tmp_path: Path) -> None:
    p = _write(tmp_path / "d.yaml",
               "id: t-deco\ntier: regression\ngrade:\n  - {kind: exit, command: 'true'}\n")
    with pytest.warns(UserWarning, match="always passes"):
        bank.load_task(p)


def test_real_command_does_not_warn(tmp_path: Path, recwarn: pytest.WarningsRecorder) -> None:
    p = _write(tmp_path / "r.yaml",
               "id: t-real\ntier: regression\ngrade:\n  - {kind: exit, command: 'pytest -q'}\n")
    bank.load_task(p)
    assert not [w for w in recwarn if "always passes" in str(w.message)]


def test_discover_bank_sorted_and_dedup(tmp_path: Path) -> None:
    _write(tmp_path / "b.yaml", "id: b\ntier: regression\ngrade:\n  - {kind: exit, command: 'pytest'}\n")
    _write(tmp_path / "a.yaml", "id: a\ntier: capability\ngrade:\n  - {kind: exit, command: 'pytest'}\n")
    tasks = bank.discover_bank(tmp_path)
    assert [t.id for t in tasks] == ["a", "b"]


def test_discover_bank_duplicate_id_rejected(tmp_path: Path) -> None:
    _write(tmp_path / "one.yaml", "id: dup\ntier: regression\ngrade:\n  - {kind: exit, command: 'pytest'}\n")
    _write(tmp_path / "two.yaml", "id: dup\ntier: capability\ngrade:\n  - {kind: exit, command: 'pytest'}\n")
    with pytest.raises(bank.BankError, match="duplicate"):
        bank.discover_bank(tmp_path)


def test_discover_missing_dir_rejected(tmp_path: Path) -> None:
    with pytest.raises(bank.BankError, match="not found"):
        bank.discover_bank(tmp_path / "nope")


def test_seed_bank_loads() -> None:
    """The committed seed bank must itself be valid (self-demonstrating)."""
    repo_root = Path(__file__).resolve().parents[3]
    seed = repo_root / "evals" / "bank"
    if not seed.is_dir():
        pytest.skip("seed bank not present in this checkout")
    tasks = bank.discover_bank(seed)
    tiers = {t.tier for t in tasks}
    assert "capability" in tiers and "regression" in tiers
    assert any("ci-flake" in t.tags for t in tasks)

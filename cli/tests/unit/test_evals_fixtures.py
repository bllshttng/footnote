"""Tests for fno.evals.fixtures and fno.evals.tap (Task 1.2).

Matches the -k evals_fixture filter.

Covers:
- AC1-HP: load_task happy path
- AC2-HP: missing budget_usd defaults to 3.0
- AC3-ERR: missing title -> FixtureError
- AC4-ERR: malformed yaml -> FixtureError
- AC5-ERR: missing repo dir -> FixtureError
- AC6-ERR: missing plan.md -> FixtureError
- AC7-ERR: plan.md without status: ready -> FixtureError
- AC8-ERR: missing assert.sh -> FixtureError
- AC9-HP: discover_fixtures sorted; invalid fixture raises
- AC10-HP: TAP parser ok/not-ok mix; ignores noise; empty input
- AC11-VERIFY: real seed fixtures discovered (4 slugs)
- AC12-VERIFY: seeded-bug-fix repo fails tests; others have expected green/red state
"""
from __future__ import annotations

import shutil
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Helper: build a minimal valid fixture in tmp_path
# ---------------------------------------------------------------------------

def _write_fixture(
    base: Path,
    *,
    slug: str = "my-task",
    task_yaml: str | None = None,
    plan_md: str | None = None,
    assert_sh: str | None = None,
    test_file: str | None = None,
) -> Path:
    """Write a minimal valid fixture directory structure."""
    fx = base / slug
    fx.mkdir(parents=True)
    repo = fx / "repo"
    repo.mkdir()

    # Defaults
    if task_yaml is None:
        task_yaml = "title: My Task\ntags: [test]\nbudget_usd: 3.0\nmax_iterations: 8\ntimeout_secs: 1800\n"
    if plan_md is None:
        plan_md = "---\nstatus: ready\n---\n# My Task\n## Goal\nDo the thing.\n"
    if assert_sh is None:
        assert_sh = "#!/usr/bin/env bash\nset -u\ncheck() { local name=\"$1\"; shift; if \"$@\" >/dev/null 2>&1; then echo \"ok $name\"; else echo \"not ok $name\"; fi; }\ncheck tests_pass python -m pytest -q\n"
    if test_file is None:
        test_file = "def test_placeholder():\n    assert True\n"

    (fx / "task.yaml").write_text(task_yaml)
    (fx / "plan.md").write_text(plan_md)
    (fx / "assert.sh").write_text(assert_sh)
    (repo / "test_placeholder.py").write_text(test_file)
    return fx


# ---------------------------------------------------------------------------
# AC1-HP: load_task happy path
# ---------------------------------------------------------------------------

def test_evals_fixture_load_task_happy_path(tmp_path: Path) -> None:
    """AC1-HP: load_task returns a fully-populated TaskSpec for a valid fixture."""
    from fno.evals.fixtures import load_task

    fx = _write_fixture(tmp_path)
    spec = load_task(fx)

    assert spec.slug == "my-task"
    assert spec.title == "My Task"
    assert spec.tags == ["test"]
    assert spec.budget_usd == 3.0
    assert spec.max_iterations == 8
    assert spec.timeout_secs == 1800
    assert spec.path == fx


# ---------------------------------------------------------------------------
# AC2-HP: missing budget_usd defaults to 3.0
# ---------------------------------------------------------------------------

def test_evals_fixture_default_budget(tmp_path: Path) -> None:
    """AC2-HP: budget_usd omitted in task.yaml defaults to 3.0 (never uncapped)."""
    from fno.evals.fixtures import load_task

    # Deliberately omit budget_usd (mirrors feature-add fixture design)
    fx = _write_fixture(
        tmp_path,
        task_yaml="title: No Budget Task\ntags: [feature]\nmax_iterations: 8\ntimeout_secs: 1800\n",
    )
    spec = load_task(fx)
    assert spec.budget_usd == 3.0


# ---------------------------------------------------------------------------
# AC3-ERR: missing title
# ---------------------------------------------------------------------------

def test_evals_fixture_missing_title(tmp_path: Path) -> None:
    """AC3-ERR: task.yaml without title -> FixtureError naming the slug."""
    from fno.evals.fixtures import load_task, FixtureError

    fx = _write_fixture(
        tmp_path,
        task_yaml="tags: [test]\nbudget_usd: 3.0\nmax_iterations: 8\ntimeout_secs: 1800\n",
    )
    with pytest.raises(FixtureError, match="my-task"):
        load_task(fx)


# ---------------------------------------------------------------------------
# AC4-ERR: malformed yaml
# ---------------------------------------------------------------------------

def test_evals_fixture_malformed_yaml(tmp_path: Path) -> None:
    """AC4-ERR: malformed task.yaml -> FixtureError."""
    from fno.evals.fixtures import load_task, FixtureError

    fx = _write_fixture(
        tmp_path,
        task_yaml="title: Bad\n  malformed: [not: valid",
    )
    with pytest.raises(FixtureError, match="my-task"):
        load_task(fx)


# ---------------------------------------------------------------------------
# AC5-ERR: missing repo dir
# ---------------------------------------------------------------------------

def test_evals_fixture_missing_repo(tmp_path: Path) -> None:
    """AC5-ERR: repo/ directory absent -> FixtureError naming the slug."""
    from fno.evals.fixtures import load_task, FixtureError

    fx = _write_fixture(tmp_path)
    shutil.rmtree(fx / "repo")
    with pytest.raises(FixtureError, match="my-task"):
        load_task(fx)


# ---------------------------------------------------------------------------
# AC5b-ERR: repo exists but has no test files
# ---------------------------------------------------------------------------

def test_evals_fixture_repo_no_tests(tmp_path: Path) -> None:
    """AC5b-ERR: repo/ has no test files -> FixtureError."""
    from fno.evals.fixtures import load_task, FixtureError

    fx = _write_fixture(tmp_path)
    (fx / "repo" / "test_placeholder.py").unlink()
    (fx / "repo" / "module.py").write_text("# not a test file\n")
    with pytest.raises(FixtureError, match="my-task"):
        load_task(fx)


# ---------------------------------------------------------------------------
# AC6-ERR: missing plan.md
# ---------------------------------------------------------------------------

def test_evals_fixture_missing_plan(tmp_path: Path) -> None:
    """AC6-ERR: plan.md absent -> FixtureError."""
    from fno.evals.fixtures import load_task, FixtureError

    fx = _write_fixture(tmp_path)
    (fx / "plan.md").unlink()
    with pytest.raises(FixtureError, match="my-task"):
        load_task(fx)


# ---------------------------------------------------------------------------
# AC7-ERR: plan.md without status: ready
# ---------------------------------------------------------------------------

def test_evals_fixture_plan_not_ready(tmp_path: Path) -> None:
    """AC7-ERR: plan.md frontmatter status != ready -> FixtureError."""
    from fno.evals.fixtures import load_task, FixtureError

    fx = _write_fixture(
        tmp_path,
        plan_md="---\nstatus: draft\n---\n# My Task\n",
    )
    with pytest.raises(FixtureError, match="my-task"):
        load_task(fx)


def test_evals_fixture_plan_no_frontmatter(tmp_path: Path) -> None:
    """AC7b-ERR: plan.md with no frontmatter at all -> FixtureError."""
    from fno.evals.fixtures import load_task, FixtureError

    fx = _write_fixture(
        tmp_path,
        plan_md="# My Task\nNo frontmatter here.\n",
    )
    with pytest.raises(FixtureError, match="my-task"):
        load_task(fx)


# ---------------------------------------------------------------------------
# AC8-ERR: missing assert.sh
# ---------------------------------------------------------------------------

def test_evals_fixture_missing_assert_sh(tmp_path: Path) -> None:
    """AC8-ERR: assert.sh absent -> FixtureError."""
    from fno.evals.fixtures import load_task, FixtureError

    fx = _write_fixture(tmp_path)
    (fx / "assert.sh").unlink()
    with pytest.raises(FixtureError, match="my-task"):
        load_task(fx)


# ---------------------------------------------------------------------------
# AC9-HP: discover_fixtures: sorted; invalid raises
# ---------------------------------------------------------------------------

def test_evals_fixture_discover_sorted(tmp_path: Path) -> None:
    """AC9-HP: discover_fixtures returns fixtures sorted by slug."""
    from fno.evals.fixtures import discover_fixtures

    _write_fixture(tmp_path, slug="zzz-task")
    _write_fixture(tmp_path, slug="aaa-task")
    _write_fixture(tmp_path, slug="mmm-task")

    specs = discover_fixtures(tmp_path)
    assert [s.slug for s in specs] == ["aaa-task", "mmm-task", "zzz-task"]


def test_evals_fixture_discover_ignores_files(tmp_path: Path) -> None:
    """AC9b-HP: discover_fixtures ignores plain files in the golden dir."""
    from fno.evals.fixtures import discover_fixtures

    _write_fixture(tmp_path, slug="valid-task")
    (tmp_path / "README.md").write_text("# readme\n")

    specs = discover_fixtures(tmp_path)
    assert len(specs) == 1
    assert specs[0].slug == "valid-task"


def test_evals_fixture_discover_raises_on_invalid(tmp_path: Path) -> None:
    """AC9c-ERR: discover_fixtures raises FixtureError on a broken fixture (never skips)."""
    from fno.evals.fixtures import discover_fixtures, FixtureError

    _write_fixture(tmp_path, slug="good-task")
    # broken fixture: no plan.md
    bad = _write_fixture(tmp_path, slug="bad-task")
    (bad / "plan.md").unlink()

    with pytest.raises(FixtureError):
        discover_fixtures(tmp_path)


# ---------------------------------------------------------------------------
# AC10-HP: TAP parser
# ---------------------------------------------------------------------------

def test_evals_tap_parse_mixed(tmp_path: Path) -> None:
    """AC10-HP: TAP parser handles ok/not-ok mix and ignores noise lines."""
    from fno.evals.tap import parse_tap

    text = textwrap.dedent("""\
        # test suite output
        ok tests_pass
        not ok slugify_basic
        1..2
        some diagnostic line
        ok third_check
    """)
    results = parse_tap(text)
    assert len(results) == 3
    assert results[0].name == "tests_pass"
    assert results[0].ok is True
    assert results[1].name == "slugify_basic"
    assert results[1].ok is False
    assert results[2].name == "third_check"
    assert results[2].ok is True


def test_evals_tap_parse_empty() -> None:
    """AC10b-EDGE: empty input -> empty list (caller treats as failure)."""
    from fno.evals.tap import parse_tap

    results = parse_tap("")
    assert results == []


def test_evals_tap_parse_only_noise() -> None:
    """AC10c-EDGE: only comment/diagnostic lines -> empty list."""
    from fno.evals.tap import parse_tap

    results = parse_tap("# comment\n1..3\nTAP version 13\n")
    assert results == []


def test_evals_tap_assertion_fields() -> None:
    """AC10d-HP: Assertion objects have .name and .ok attributes."""
    from fno.evals.tap import parse_tap, Assertion

    results = parse_tap("ok check_one\nnot ok check_two\n")
    assert isinstance(results[0], Assertion)
    assert results[0].name == "check_one"
    assert results[0].ok is True
    assert results[1].name == "check_two"
    assert results[1].ok is False


# ---------------------------------------------------------------------------
# AC11-VERIFY: real seed fixtures discovered
# ---------------------------------------------------------------------------

def _repo_golden_dir() -> Path:
    """Locate the evals/golden/ directory in the repo root."""
    # Walk up from this test file to find the repo root
    here = Path(__file__).resolve()
    for parent in here.parents:
        candidate = parent / "evals" / "golden"
        if candidate.is_dir():
            return candidate
    raise FileNotFoundError("Could not locate evals/golden/ from test file")


def test_evals_fixture_real_seeds_discovered() -> None:
    """AC11-VERIFY: evals/golden/ contains exactly the 4 expected seed fixtures."""
    from fno.evals.fixtures import discover_fixtures

    golden = _repo_golden_dir()
    specs = discover_fixtures(golden)
    slugs = [s.slug for s in specs]

    assert slugs == sorted(slugs), "Should be sorted"
    assert set(slugs) == {
        "edge-case-heavy",
        "feature-add",
        "refactor-under-tests",
        "seeded-bug-fix",
    }


def test_evals_fixture_real_seeds_all_valid() -> None:
    """AC11b-VERIFY: each seed fixture loads without FixtureError."""
    from fno.evals.fixtures import load_task

    golden = _repo_golden_dir()
    for slug in ("edge-case-heavy", "feature-add", "refactor-under-tests", "seeded-bug-fix"):
        spec = load_task(golden / slug)
        assert spec.slug == slug
        assert spec.title, f"{slug}: title must be non-empty"
        assert spec.budget_usd > 0, f"{slug}: budget must be positive"


# ---------------------------------------------------------------------------
# AC12-VERIFY: seeded repo test-suite states
# ---------------------------------------------------------------------------

def _copy_repo(golden_dir: Path, slug: str, tmp_path: Path) -> Path:
    """Copy a fixture's repo/ into a fresh tmp directory (never mutate template)."""
    src = golden_dir / slug / "repo"
    dst = tmp_path / slug
    shutil.copytree(src, dst)
    return dst


def test_evals_fixture_feature_add_repo_green(tmp_path: Path) -> None:
    """AC12-HP: feature-add/repo test suite passes (it ships with passing tests)."""
    golden = _repo_golden_dir()
    repo = _copy_repo(golden, "feature-add", tmp_path)

    result = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "--tb=short"],
        cwd=repo,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, f"feature-add repo should be green\n{result.stdout}\n{result.stderr}"


def test_evals_fixture_seeded_bug_repo_red(tmp_path: Path) -> None:
    """AC12-HP: seeded-bug-fix/repo test suite FAILS (intentional seeded bug)."""
    golden = _repo_golden_dir()
    repo = _copy_repo(golden, "seeded-bug-fix", tmp_path)

    result = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "--tb=short"],
        cwd=repo,
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0, (
        "seeded-bug-fix repo should be RED (seeded bug not yet fixed)\n"
        f"{result.stdout}\n{result.stderr}"
    )


def test_evals_fixture_refactor_repo_green(tmp_path: Path) -> None:
    """AC12-HP: refactor-under-tests/repo test suite passes before refactoring."""
    golden = _repo_golden_dir()
    repo = _copy_repo(golden, "refactor-under-tests", tmp_path)

    result = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "--tb=short"],
        cwd=repo,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, (
        f"refactor-under-tests repo should be green\n{result.stdout}\n{result.stderr}"
    )


def test_evals_fixture_edge_case_repo_red(tmp_path: Path) -> None:
    """AC12-HP: edge-case-heavy/repo test suite FAILS (NotImplementedError stub)."""
    golden = _repo_golden_dir()
    repo = _copy_repo(golden, "edge-case-heavy", tmp_path)

    result = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "--tb=short"],
        cwd=repo,
        capture_output=True,
        text=True,
    )
    assert result.returncode != 0, (
        "edge-case-heavy repo should be RED (stub raises NotImplementedError)\n"
        f"{result.stdout}\n{result.stderr}"
    )

"""Golden-task fixture loading and validation.

Each fixture lives at ``evals/golden/<slug>/`` and contains:

- ``task.yaml``  - title, tags, budget_usd, max_iterations, timeout_secs
- ``repo/``      - tiny template project with its own test suite
- ``plan.md``    - ready-status quick plan (frontmatter ``status: ready``)
- ``assert.sh``  - deterministic post-run assertions, TAP-lite output

``load_task`` is the primary entry point; ``discover_fixtures`` discovers
all fixtures under a golden directory.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml


# ---------------------------------------------------------------------------
# Public exception
# ---------------------------------------------------------------------------


class FixtureError(ValueError):
    """Raised when a fixture is missing required structure or has invalid data.

    The message always names the fixture slug so callers can identify which
    fixture is broken.
    """


# ---------------------------------------------------------------------------
# Data model
# ---------------------------------------------------------------------------

# Loader defaults applied when task.yaml omits optional fields.
_DEFAULT_BUDGET_USD: float = 3.0  # task must NEVER run uncapped
_DEFAULT_MAX_ITERATIONS: int = 10
_DEFAULT_TIMEOUT_SECS: int = 1800
_DEFAULT_TAGS: list[str] = []


@dataclass
class TaskSpec:
    """Parsed and validated task specification for a golden eval fixture.

    Attributes:
        slug:           Directory name; unique identifier for the fixture.
        title:          Human-readable task title (required in task.yaml).
        tags:           Category labels (e.g. ["feature"], ["bugfix"]).
        budget_usd:     Maximum spend allowed for this task run.
        max_iterations: Stop the /target session after this many iterations.
        timeout_secs:   Wall-clock timeout for the full run.
        path:           Absolute path to the fixture directory.
    """

    slug: str
    title: str
    tags: list[str]
    budget_usd: float
    max_iterations: int
    timeout_secs: int
    path: Path


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

_FRONTMATTER_RE = re.compile(r"^\s*---\s*\n(.*?)\n---", re.DOTALL)


def _parse_plan_status(plan_text: str) -> str | None:
    """Return the ``status`` value from the YAML frontmatter, or None."""
    m = _FRONTMATTER_RE.match(plan_text)
    if not m:
        return None
    try:
        data: Any = yaml.safe_load(m.group(1))
    except yaml.YAMLError:
        return None
    if not isinstance(data, dict):
        return None
    return data.get("status")


def _has_test_files(repo_dir: Path) -> bool:
    """Return True if any ``test_*.py`` or ``*_test.py`` exists under repo_dir."""
    for pattern in ("**/test_*.py", "**/*_test.py"):
        if any(repo_dir.glob(pattern)):
            return True
    return False


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def load_task(fixture_dir: Path) -> TaskSpec:
    """Parse and validate a single fixture directory.

    Args:
        fixture_dir: Path to ``evals/golden/<slug>/``.

    Returns:
        A fully-populated :class:`TaskSpec`.

    Raises:
        FixtureError: If task.yaml is missing/malformed, required fields are
            absent, or the fixture structure is incomplete.
    """
    slug = fixture_dir.name

    # ------------------------------------------------------------------ #
    # task.yaml                                                            #
    # ------------------------------------------------------------------ #
    task_yaml_path = fixture_dir / "task.yaml"
    if not task_yaml_path.exists():
        raise FixtureError(f"{slug}: task.yaml not found in {fixture_dir}")

    try:
        raw: Any = yaml.safe_load(task_yaml_path.read_text(encoding="utf-8"))
    except yaml.YAMLError as exc:
        raise FixtureError(f"{slug}: task.yaml is malformed YAML: {exc}") from exc

    if not isinstance(raw, dict):
        raise FixtureError(f"{slug}: task.yaml must be a YAML mapping, got {type(raw).__name__}")

    title: Any = raw.get("title")
    if not title or not isinstance(title, str):
        raise FixtureError(f"{slug}: task.yaml must have a non-empty 'title' field")

    tags_raw: Any = raw.get("tags", list(_DEFAULT_TAGS))
    if not isinstance(tags_raw, list):
        raise FixtureError(f"{slug}: task.yaml 'tags' must be a list, got {type(tags_raw).__name__}")
    tags: list[str] = [str(t) for t in tags_raw]

    try:
        budget_usd: float = float(raw.get("budget_usd", _DEFAULT_BUDGET_USD))
    except (TypeError, ValueError):
        raise FixtureError(
            f"{slug}: task.yaml 'budget_usd' must be a number, got {raw.get('budget_usd')!r}"
        ) from None
    if budget_usd <= 0:
        raise FixtureError(f"{slug}: task.yaml 'budget_usd' must be > 0, got {budget_usd}")

    try:
        max_iterations: int = int(raw.get("max_iterations", _DEFAULT_MAX_ITERATIONS))
    except (TypeError, ValueError):
        raise FixtureError(
            f"{slug}: task.yaml 'max_iterations' must be an integer, got {raw.get('max_iterations')!r}"
        ) from None
    if max_iterations <= 0:
        raise FixtureError(f"{slug}: task.yaml 'max_iterations' must be > 0, got {max_iterations}")

    try:
        timeout_secs: int = int(raw.get("timeout_secs", _DEFAULT_TIMEOUT_SECS))
    except (TypeError, ValueError):
        raise FixtureError(
            f"{slug}: task.yaml 'timeout_secs' must be an integer, got {raw.get('timeout_secs')!r}"
        ) from None
    if timeout_secs <= 0:
        raise FixtureError(f"{slug}: task.yaml 'timeout_secs' must be > 0, got {timeout_secs}")

    # ------------------------------------------------------------------ #
    # repo/ structure                                                      #
    # ------------------------------------------------------------------ #
    repo_dir = fixture_dir / "repo"
    if not repo_dir.is_dir():
        raise FixtureError(f"{slug}: repo/ directory not found in {fixture_dir}")

    if not _has_test_files(repo_dir):
        raise FixtureError(
            f"{slug}: repo/ must contain at least one test file "
            f"(test_*.py or *_test.py), none found under {repo_dir}"
        )

    # ------------------------------------------------------------------ #
    # plan.md with status: ready                                           #
    # ------------------------------------------------------------------ #
    plan_path = fixture_dir / "plan.md"
    if not plan_path.exists():
        raise FixtureError(f"{slug}: plan.md not found in {fixture_dir}")

    plan_status = _parse_plan_status(plan_path.read_text(encoding="utf-8"))
    if plan_status != "ready":
        raise FixtureError(
            f"{slug}: plan.md frontmatter must have 'status: ready', "
            f"got {plan_status!r}"
        )

    # ------------------------------------------------------------------ #
    # assert.sh                                                            #
    # ------------------------------------------------------------------ #
    assert_sh = fixture_dir / "assert.sh"
    if not assert_sh.exists():
        raise FixtureError(f"{slug}: assert.sh not found in {fixture_dir}")

    return TaskSpec(
        slug=slug,
        title=title,
        tags=tags,
        budget_usd=budget_usd,
        max_iterations=max_iterations,
        timeout_secs=timeout_secs,
        path=fixture_dir,
    )


def discover_fixtures(golden_dir: Path) -> list[TaskSpec]:
    """Discover all fixture directories under ``golden_dir``, sorted by slug.

    Non-directory entries (e.g. README.md) are silently ignored.  An invalid
    fixture directory raises :class:`FixtureError` immediately - broken fixtures
    are never skipped silently.

    Args:
        golden_dir: Path to the directory containing ``<slug>/`` subdirectories.

    Returns:
        A list of :class:`TaskSpec` objects sorted by slug.

    Raises:
        FixtureError: On the first invalid fixture encountered.
    """
    specs: list[TaskSpec] = []
    for entry in sorted(golden_dir.iterdir()):
        if not entry.is_dir():
            continue
        specs.append(load_task(entry))
    return specs

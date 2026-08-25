"""CI runs the smoke suite as two shards. They must cover the whole registry.

The workflow splits `smoke` into shard jobs that run in parallel on separate
runners. That halves wall clock only if nothing is lost in the split, so this
test asserts the cover against the registry the runner actually builds.

The selectors are READ OUT OF `.github/workflows/cli-ci.yml`, never restated
here. Hardcoding them would make this a test of two Python constants: someone
could widen `--skip 'Pytest*'` to `--skip '*'` in the workflow, gut the merge
gate, and watch every assertion below stay green.
"""
from __future__ import annotations

import re
from pathlib import Path

import yaml

from fno.test_cmd import _RUST_BUILD_STEP, _name_matches, smoke_steps

_REPO_ROOT = Path(__file__).resolve().parents[3]
_WORKFLOW = _REPO_ROOT / ".github" / "workflows" / "cli-ci.yml"
_RUST_WORKFLOW = _REPO_ROOT / ".github" / "workflows" / "rust-ci.yml"
_SMOKE_SETUP = _REPO_ROOT / ".github" / "actions" / "smoke-setup" / "action.yml"

# `--only 'a,b'` / `--skip 'a'`, quoted or bare, as the workflow writes them.
_SELECTOR = re.compile(r"--(only|skip)[= ]+'([^']*)'|--(only|skip)[= ]+(\S+)")


def _shard_selectors() -> list[tuple[str, str, str]]:
    """(job name, 'only'|'skip', globs) for every job the `smoke` gate needs."""
    workflow = yaml.safe_load(_WORKFLOW.read_text())
    jobs = workflow["jobs"]
    needs = jobs["smoke"].get("needs") or []
    if isinstance(needs, str):
        needs = [needs]

    found: list[tuple[str, str, str]] = []
    for name in needs:
        run = "\n".join(step.get("run", "") for step in jobs[name].get("steps", []))
        for match in _SELECTOR.finditer(run):
            flag = match.group(1) or match.group(3)
            globs = match.group(2) if match.group(1) else match.group(4)
            found.append((name, flag, globs))
    return found


def _names() -> list[str]:
    return [name for name, _cwd, _cmd in smoke_steps(_REPO_ROOT)]


def _selected(names: list[str], flag: str, globs: str) -> list[str]:
    if flag == "only":
        return [n for n in names if _name_matches(n, globs)]
    return [n for n in names if not _name_matches(n, globs)]


def test_the_workflow_actually_shards_the_gate() -> None:
    """Guard the guard: without this, every test below passes vacuously.

    If the gate stops needing shard jobs, or the shards stop carrying
    selectors, the cover assertions have nothing to read and an empty
    selector list would satisfy every `for` loop in this file.
    """
    selectors = _shard_selectors()
    assert selectors, "the smoke gate needs no shard jobs carrying --only/--skip"
    assert len(selectors) >= 2, f"expected at least two shards, found {selectors}"


def test_the_shards_cover_every_step() -> None:
    """Every registry step runs in at least one shard.

    Stated as a positive marker per step, never as "nothing was uncovered":
    an empty registry and a fully covered one both have zero uncovered steps,
    and only one of them is a real cover.
    """
    names = _names()
    assert names, "the smoke registry is empty; this test would pass vacuously"

    covered: set[str] = set()
    for _job, flag, globs in _shard_selectors():
        covered |= set(_selected(names, flag, globs))

    for name in names:
        assert name in covered, f"step {name!r} runs in no CI shard"


def test_pytest_runs_in_exactly_one_shard() -> None:
    """The expensive half must not run twice; it is 49 percent of the suite."""
    names = _names()
    step = "Pytest (unit + integration)"
    assert step in names, "the pytest step was renamed; re-check the shard seam"
    carriers = [job for job, flag, globs in _shard_selectors()
                if step in _selected(names, flag, globs)]
    assert len(carriers) == 1, f"pytest runs in {carriers}, expected exactly one shard"


def test_the_rust_binary_is_built_in_the_shard_that_needs_it() -> None:
    """The seam is the faithful-ordering guard, so the guard must stay true.

    When pytest is selected the runner DELETES the fno-agents debug binary so
    the @requires_rust parity tests skip. The rust journey steps that need the
    binary therefore have to live in a shard that also carries its build step,
    and pytest has to live in a shard that does not.
    """
    names = _names()
    build = "Build fno-agents debug binary (for journey tests)"
    pytest_step = "Pytest (unit + integration)"
    assert build in names, "the build step was renamed; re-check the shard seam"

    for job, flag, globs in _shard_selectors():
        selected = _selected(names, flag, globs)
        if pytest_step in selected:
            assert build not in selected, (
                f"{job} runs pytest and the rust build together; pytest deletes "
                "the binary that build produces")


def test_only_prerequisites_run_in_more_than_one_shard() -> None:
    """A prerequisite may run twice; a test may not.

    `Sync + build` costs about three seconds and removes any question of
    whether pytest can rely on `uv build` having run. Anything else in two
    shards is duplicated work that belongs on one side.
    """
    names = _names()
    counts: dict[str, int] = {}
    for _job, flag, globs in _shard_selectors():
        for name in _selected(names, flag, globs):
            counts[name] = counts.get(name, 0) + 1
    duplicated = sorted(n for n, c in counts.items() if c > 1)
    assert duplicated == ["Sync + build"], (
        f"steps running in more than one shard: {duplicated}")


def test_every_shard_clears_the_rust_binary_before_it_starts() -> None:
    """The pre-build steps must see no fno-agents binary, in EVERY shard.

    The runner deletes it up front when pytest or the rust build step is
    selected. Keyed on pytest alone, the shard without pytest skipped the
    deletion while smoke-setup had already built the binary, so the 17 steps
    between pytest and the build step ran with it present where they had
    always run without it. Assert the trigger fires per shard, by name.
    """
    names = _names()
    triggers = {"Pytest (unit + integration)", _RUST_BUILD_STEP}
    for job, flag, globs in _shard_selectors():
        selected = set(_selected(names, flag, globs))
        assert triggers & selected, (
            f"{job} selects neither pytest nor the rust build, so it never "
            "clears the binary and its pre-build steps run with it present")


def test_smoke_setup_cleans_fno_agents_before_building_cached_artifacts() -> None:
    action = yaml.safe_load(_SMOKE_SETUP.read_text())
    run = "\n".join(step.get("run", "") for step in action["runs"]["steps"])

    clean = run.index("cargo clean -p fno-agents")
    build = run.index("cargo build --manifest-path crates/fno-agents/Cargo.toml")

    assert clean < build, "smoke setup can execute a stale cached fno-agents binary"


def test_rust_ci_cleans_fno_agents_before_unit_tests() -> None:
    workflow = yaml.safe_load(_RUST_WORKFLOW.read_text())
    steps = workflow["jobs"]["test"]["steps"]
    names = [step.get("name", "") for step in steps]
    clean = names.index("Clean cached fno-agents package artifacts")
    unit = names.index("cargo test --lib --bins (fno-agents)")

    assert clean < unit, "rust-ci can test a stale cached fno-agents harness"

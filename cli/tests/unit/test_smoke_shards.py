"""CI runs the full smoke lanes in bounded shard counts. They must cover the whole registry.

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

from fno.test_cmd import (
    _RUST_BUILD_STEP,
    _name_matches,
    _shard_selected_indices,
    smoke_steps,
)

_REPO_ROOT = Path(__file__).resolve().parents[3]
_WORKFLOW = _REPO_ROOT / ".github" / "workflows" / "cli-ci.yml"
_RUST_WORKFLOW = _REPO_ROOT / ".github" / "workflows" / "rust-ci.yml"
_CLI_WORKFLOW = _REPO_ROOT / ".github" / "workflows" / "cli-ci.yml"
_SMOKE_SETUP = _REPO_ROOT / ".github" / "actions" / "smoke-setup" / "action.yml"

# `--only 'a,b'` / `--skip 'a'`, quoted or bare, as the workflow writes them.
_SELECTOR = re.compile(r"--(only|skip)[= ]+'([^']*)'|--(only|skip)[= ]+(\S+)")
_SHARD = re.compile(r"--shard\s+\$\{\{\s*matrix\.shard\s*\}\}/(\d+)")


def _shard_selectors() -> list[tuple[str, int, int, str, str]]:
    """(job, index, total, mode, globs) for every full-gate matrix leg."""
    workflow = yaml.safe_load(_WORKFLOW.read_text())
    jobs = workflow["jobs"]
    needs = jobs["smoke"].get("needs") or []
    if isinstance(needs, str):
        needs = [needs]

    found: list[tuple[str, int, int, str, str]] = []
    for name in needs:
        job = jobs[name]
        run = "\n".join(step.get("run", "") for step in job.get("steps", []))
        shard = _SHARD.search(run)
        matrix = job.get("strategy", {}).get("matrix", {}).get("shard", [])
        legs = [(int(index), int(shard.group(1))) for index in matrix] if shard else [(0, 0)]
        for match in _SELECTOR.finditer(run):
            flag = match.group(1) or match.group(3)
            globs = match.group(2) if match.group(1) else match.group(4)
            found.extend((name, index, total, flag, globs) for index, total in legs)
    return found


def _names() -> list[str]:
    return [name for name, _cwd, _cmd in smoke_steps(_REPO_ROOT)]


def _selected(
    names: list[str], flag: str, globs: str, shard: int = 0, total: int = 0
) -> list[str]:
    if flag == "only":
        indices = [i for i, name in enumerate(names) if _name_matches(name, globs)]
    else:
        indices = [i for i, name in enumerate(names) if not _name_matches(name, globs)]
    if total:
        steps = [(name, ".", "") for name in names]
        indices = _shard_selected_indices(steps, indices, shard, total)
    return [names[i] for i in indices]


def _command_lines(run: str) -> list[str]:
    return [line.strip() for line in run.splitlines() if line.strip()]


def test_the_workflow_actually_shards_the_gate() -> None:
    """Guard the guard: without this, every test below passes vacuously.

    If the gate stops needing shard jobs, or the shards stop carrying
    selectors, the cover assertions have nothing to read and an empty
    selector list would satisfy every `for` loop in this file.
    """
    selectors = _shard_selectors()
    assert selectors, "the smoke gate needs no shard jobs carrying --only/--skip"
    counts = {job: sum(1 for candidate, *_rest in selectors if candidate == job)
              for job, *_rest in selectors}
    assert counts == {"smoke-pytest": 13, "smoke-rest": 4}, (
        f"expected thirteen pytest legs and four rest legs, found {counts}"
    )


def test_matrix_legs_enumerate_the_denominator_in_each_command() -> None:
    workflow = yaml.safe_load(_WORKFLOW.read_text())
    jobs = workflow["jobs"]
    needs = jobs["smoke"].get("needs") or []
    if isinstance(needs, str):
        needs = [needs]

    checked = 0
    for name in needs:
        job = jobs[name]
        run = "\n".join(step.get("run", "") for step in job.get("steps", []))
        match = _SHARD.search(run)
        if not match:
            continue
        total = int(match.group(1))
        values = job.get("strategy", {}).get("matrix", {}).get("shard", [])
        assert values, f"{name} writes /{total} but declares no matrix legs"
        assert values == list(range(1, total + 1)), (
            f"{name} is missing shard index from 1..{total}: {values}"
        )
        checked += 1

    assert checked == 2, "both full-gate lanes must declare shard matrices"


def test_every_pr_affected_job_overrides_the_implicit_success_gate() -> None:
    """changed-packet-size is PR-only, so on a push it skips and GitHub's
    implicit success() would skip every transitive dependent with it: main
    then runs no tests at all. Each job gated on pr-affected must override
    the implicit gate with !cancelled(); pr-affected itself already does.
    """
    workflow = yaml.safe_load(_WORKFLOW.read_text())
    jobs = workflow["jobs"]

    for name, job in jobs.items():
        job_if = job.get("if") or ""
        if "needs.pr-affected" not in job_if:
            continue
        assert "!cancelled()" in job_if, (
            f"{name} is gated on pr-affected without !cancelled(); a push "
            "run skips it and main runs no tests"
        )

    # Guard the guard: a shard whose if stops referencing pr-affected would
    # silently drop out of the loop above.
    for name in (
        "smoke-pytest",
        "smoke-rest",
        "hook-latency",
        "test-agents",
        "test-agents-integration",
        "test-mux",
    ):
        assert "needs.pr-affected" in (jobs[name].get("if") or ""), (
            f"{name} stopped being gated on pr-affected; update this guard"
        )

    assert "pr-affected" in jobs["smoke"].get("needs", [])
    assert jobs["pr-affected"].get("if") == "${{ !cancelled() }}"
    assert jobs["changed-smoke"].get("if") == "github.event_name == 'pull_request'"


def test_the_shards_cover_every_step() -> None:
    """Every registry step runs in at least one shard.

    Stated as a positive marker per step, never as "nothing was uncovered":
    an empty registry and a fully covered one both have zero uncovered steps,
    and only one of them is a real cover.
    """
    names = _names()
    assert names, "the smoke registry is empty; this test would pass vacuously"

    covered: set[str] = set()
    for _job, shard, total, flag, globs in _shard_selectors():
        covered |= set(_selected(names, flag, globs, shard, total))

    for name in names:
        assert name in covered, f"step {name!r} runs in no CI shard"


def test_pytest_runs_in_every_pytest_shard() -> None:
    """The expensive half runs once in each of the thirteen pytest legs."""
    names = _names()
    step = "Pytest (unit + integration)"
    assert step in names, "the pytest step was renamed; re-check the shard seam"
    carriers = [
        (job, shard)
        for job, shard, total, flag, globs in _shard_selectors()
        if step in _selected(names, flag, globs, shard, total)
    ]
    assert carriers == [("smoke-pytest", shard) for shard in range(1, 14)]


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

    for job, shard, total, flag, globs in _shard_selectors():
        selected = _selected(names, flag, globs, shard, total)
        if pytest_step in selected:
            assert build not in selected, (
                f"{job} runs pytest and the rust build together; pytest deletes "
                "the binary that build produces")


def test_the_dev_build_harness_runs_after_the_build_in_one_rest_leg() -> None:
    """The door tests skip in every pytest leg; this harness is where they run."""
    names = _names()
    harness = "tests/test-dev-build-suites.sh"
    assert harness in names, "the dev-build harness left the registry, so the door tests run nowhere"
    assert names.index(harness) > names.index(_RUST_BUILD_STEP)
    legs = [
        (job, set(_selected(names, flag, globs, shard, total)))
        for job, shard, total, flag, globs in _shard_selectors()
    ]
    carriers = [(job, picked) for job, picked in legs if harness in picked]
    assert len(carriers) == 1, carriers
    job, picked = carriers[0]
    assert job == "smoke-rest"
    assert _RUST_BUILD_STEP in picked


def test_only_prerequisites_run_in_more_than_one_shard() -> None:
    """A prerequisite may run twice; a test may not.

    `Sync + build` costs about three seconds and removes any question of
    whether pytest can rely on `uv build` having run. Anything else in two
    shards is duplicated work that belongs on one side.
    """
    names = _names()
    counts: dict[str, int] = {}
    for _job, shard, total, flag, globs in _shard_selectors():
        for name in _selected(names, flag, globs, shard, total):
            counts[name] = counts.get(name, 0) + 1
    duplicated = sorted(n for n, c in counts.items() if c > 1)
    assert set(duplicated) <= {
        "Sync + build", "Pytest (unit + integration)", _RUST_BUILD_STEP
    }, f"steps running in more than one shard: {duplicated}"


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
    for job, shard, total, flag, globs in _shard_selectors():
        selected = set(_selected(names, flag, globs, shard, total))
        assert triggers & selected, (
            f"{job} selects neither pytest nor the rust build, so it never "
            "clears the binary and its pre-build steps run with it present")


def test_smoke_setup_cleans_fno_agents_and_fno_before_building_cached_artifacts() -> None:
    """The cache above keys on the Cargo.lock hash alone, not on .rs source.

    A commit that changes only source - the common case - leaves the key
    unchanged, so a cache hit can restore a binary built from an OLDER
    commit. A smoke test that lazily builds crates/fno/target/debug/fno only
    when the path is missing (tests/mux-restart-spares-live-panes.sh) then
    runs that stale binary instead of rebuilding it. Both packages this
    action provisions a binary for must be cleaned and rebuilt every run.
    """
    action = yaml.safe_load(_SMOKE_SETUP.read_text())
    run = "\n".join(step.get("run", "") for step in action["runs"]["steps"])
    lines = _command_lines(run)

    for package in ("fno-agents", "fno"):
        clean = lines.index(
            f"cargo clean -p {package} --manifest-path crates/{package}/Cargo.toml"
        )
        # The build pins CARGO_BUILD_BUILD_DIR (the repo build-dir config
        # would otherwise put the final binary in the cargo-home build
        # base, where a path-based lookup never reads).
        build = next(
            i
            for i, line in enumerate(lines)
            if line.startswith(f'CARGO_BUILD_BUILD_DIR="$PWD/crates/{package}/target" ')
            and f"--manifest-path crates/{package}/Cargo.toml" in line
        )
        assert clean < build, f"smoke setup can execute a stale cached {package} binary"


def test_rust_ci_cleans_fno_agents_before_unit_tests() -> None:
    # The heavy cargo job moved to cli-ci.yml (x-861c): the shards must gate
    # it, so the clean-before-test order is asserted there. The job is now
    # split by crate, and each crate's order lives in its own shard job; the
    # fno-agents integration shard carries the same invariant against its
    # own --test '*' leg.
    workflow = yaml.safe_load(_CLI_WORKFLOW.read_text())
    jobs = workflow["jobs"]

    for job_name, package, test_step in (
        ("test-agents", "fno-agents", "cargo test --lib --bins (fno-agents)"),
        (
            "test-agents-integration",
            "fno-agents",
            "cargo test --test '*' --test-threads=1 (fno-agents real-process integration)",
        ),
        ("test-mux", "fno", "cargo test --lib --bins (fno mux)"),
    ):
        steps = jobs[job_name]["steps"]
        names = [step.get("name", "") for step in steps]
        clean = names.index("Clean cached Rust package artifacts")
        clean_lines = _command_lines(steps[clean].get("run", ""))

        assert (
            f"cargo clean -p {package} --manifest-path crates/{package}/Cargo.toml"
            in clean_lines
        )
        unit = names.index(test_step)
        assert clean < unit, f"rust-ci can test a stale cached {package} harness"


def test_rust_stress_cleans_both_packages_before_building() -> None:
    workflow = yaml.safe_load(_RUST_WORKFLOW.read_text())
    stress_job = workflow["jobs"]["stress"]
    steps = stress_job["steps"]
    stress_env_step = next(
        (
            step
            for step in steps
            if step.get("name") == "Stress the process-backed e2e binaries"
        ),
        None,
    )
    assert stress_env_step is not None
    assert stress_env_step["env"]["STRESS_SKIP_SLOW"] == "1"
    cli_workflow = yaml.safe_load(_WORKFLOW.read_text())
    changed_step = next(
        (
            step
            for step in cli_workflow["jobs"]["changed-smoke"]["steps"]
            if step.get("name") == "Changed packet (CHANGED SUBSET)"
        ),
        None,
    )
    assert changed_step is not None
    assert changed_step["env"]["STRESS_SKIP_SLOW"] == "1"
    run = "\n".join(step.get("run", "") for step in steps)
    lines = _command_lines(run)
    stress = lines.index("bash scripts/tests/stress-rust-e2e-concurrency.sh > stress.log 2>&1 || rc=$?")

    for package in ("fno", "fno-agents"):
        clean = lines.index(
            f"cargo clean -p {package} --manifest-path crates/{package}/Cargo.toml"
        )
        assert clean < stress, f"stress can execute a stale cached {package} harness"

    smoke_env_step = next(
        (
            step
            for step in cli_workflow["jobs"]["smoke-rest"]["steps"]
            if step.get("name", "").startswith("Smoke shard: everything except pytest")
        ),
        None,
    )
    assert smoke_env_step is not None
    assert smoke_env_step["env"]["STRESS_SKIP_SLOW"] == "1"

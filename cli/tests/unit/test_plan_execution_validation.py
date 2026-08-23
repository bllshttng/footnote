from __future__ import annotations

import json
from pathlib import Path
import subprocess

import pytest
from typer.testing import CliRunner

from fno.plan._doc import load_plan
from fno.plan.cli import plan_app
from fno.plan.execution_validation import validate_execution


runner = CliRunner()
REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR = REPO_ROOT / "scripts" / "validate-plan.sh"


def _write_plan(tmp_path: Path, body: str, *, frontmatter: str = "") -> Path:
    plan = tmp_path / "plan.md"
    plan.write_text(
        "---\n"
        + (frontmatter or "status: ready\nkind: quick-plan\ncreated: 2026-07-25\n")
        + "---\n\n# Plan\n\n"
        + body,
        encoding="utf-8",
    )
    return plan


def _full_plan(strategy: str) -> str:
    return f"""## Overview

Executable plan.

## Execution Strategy

```yaml
{strategy}```
"""


VALID_STRATEGY = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    name: Build
    tasks: [\"1.1\"]
tasks:
  - id: \"1.1\"
    title: Build the validator
    surface: [cli/src/fno/plan/cli.py]
    verify: fno doctor test cli/tests/unit/test_plan_execution_validation.py
    acceptance: [AC1-HP]
"""


def test_valid_strategy_has_no_representation_warnings(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, _full_plan(VALID_STRATEGY))

    result = validate_execution(load_plan(plan))

    assert result.violations == []


def test_quick_plan_uses_sections_not_task_headings(tmp_path: Path) -> None:
    plan = _write_plan(
        tmp_path,
        """## Context

Why this is needed.

## Changes

### 1. Change

Do the work.

## Files to Modify

| File | Action |
|---|---|
| `src/a.py` | modify |

## Verification

1. `fno doctor test cli/tests/unit/test_a.py`
""",
    )

    assert validate_execution(load_plan(plan)).violations == []


def test_quick_plan_reports_missing_semantic_section(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, "## Context\n\nOnly context.\n")

    fields = {v.field for v in validate_execution(load_plan(plan)).violations}

    assert fields == {"Changes", "Files to Modify", "Verification"}


def test_quick_plan_rejects_canonical_template_placeholders(tmp_path: Path) -> None:
    plan = _write_plan(
        tmp_path,
        """## Context

Why this is needed.

## Changes

### 1. [Change name]

[Describe the change.]

## Files to Modify

| File | Action |
|---|---|
| `path/to/file.ts` | modify |

## Verification

1. [Concrete runnable check]
""",
    )

    fields = {v.field for v in validate_execution(load_plan(plan)).violations}

    assert fields == {"Changes", "Files to Modify", "Verification"}


_QUICK_FILES = "| File | Action |\n|---|---|\n| `cli/src/fno/plan/execution_validation.py` | modify |"
_QUICK_VERIFY = "1. `fno doctor test cli/tests/unit/test_plan_execution_validation.py`"


def _quick_plan(*, files: str = _QUICK_FILES, verification: str = _QUICK_VERIFY) -> str:
    return f"""## Context

Why this is needed.

## Changes

### 1. Do the work

Make the change.

## Files to Modify

{files}

## Verification

{verification}
"""


def _strategy_with_verify(verify: str) -> str:
    return VALID_STRATEGY.replace(
        "verify: fno doctor test cli/tests/unit/test_plan_execution_validation.py",
        f"verify: {json.dumps(verify)}",
    )


def _assert_verify_accepted_in_both_plan_shapes(
    tmp_path: Path, command: str, *, full_command: str | None = None
) -> None:
    quick_dir = tmp_path / "quick"
    full_dir = tmp_path / "full"
    quick_dir.mkdir()
    full_dir.mkdir()
    quick = _write_plan(quick_dir, _quick_plan(verification=f"1. `{command}`"))
    full = _write_plan(
        full_dir,
        _full_plan(_strategy_with_verify(full_command or command)),
    )

    assert validate_execution(load_plan(quick)).violations == [], command
    assert validate_execution(load_plan(full)).violations == [], command


@pytest.mark.parametrize(
    "command",
    (
        "mix test",
        "phpunit tests/",
        "gleam test",
        "zig build",
        "nim c src/main.nim",
        "sbt test",
        "dune build",
        "awk '... ' file",
        "sed -n '1p' file",
        "supabase db lint",
    ),
)
def test_ecosystem_commands_are_runnable_in_quick_and_full_plans(
    tmp_path: Path, command: str
) -> None:
    _assert_verify_accepted_in_both_plan_shapes(
        tmp_path,
        command,
        full_command=f"env FNO_TEST=1 {command}",
    )


_LEGACY_COMMAND_INVOCATIONS = (
    "[ -f out.txt ]",
    "bash -n script.sh",
    "bazel test //...",
    "black --check .",
    "bun test",
    "bundle exec rake test",
    "cargo check",
    "cd /tmp",
    "cmake --build build",
    "composer install",
    "ctest --test-dir build",
    "curl -fsS https://example.com",
    "deno test",
    "diff -u expected actual",
    "docker build .",
    "dotnet test",
    "eslint .",
    "fno doctor test",
    "fno-agents loop check",
    "flutter test",
    "git status",
    "go test ./...",
    "gradle test",
    "grep -n pattern file",
    "gh run list",
    "helm lint chart/",
    "java -version",
    "javac Main.java",
    "jest --runInBand",
    "jq . file.json",
    "just test",
    "kubectl get pods",
    "make test",
    "mvn test",
    "mypy src/",
    "node --version",
    "npm test",
    "npx prettier --check .",
    "php --version",
    "pip install -r requirements.txt",
    "pip3 --version",
    "pipenv run pytest",
    "pnpm test",
    "podman run image",
    "poetry run pytest",
    "pre-commit run",
    "prettier --check .",
    "psql --command 'select 1'",
    "pytest -q",
    "python -m pytest",
    "python3 --version",
    "rake test",
    "rg -n pattern src/",
    "rspec spec",
    "ruby -c script.rb",
    "ruff check .",
    "rustc main.rs",
    "sh -n script.sh",
    "shellcheck script.sh",
    "swift test",
    "terraform validate",
    "test -f out.txt",
    "tofu validate",
    "tox -e py",
    "tsc --noEmit",
    "uv run pytest",
    "vitest run",
    "wget -q https://example.com/file",
    "xcodebuild test",
    "yarn test",
    "yq '.foo' file.yaml",
    "zsh -n script.zsh",
)


@pytest.mark.parametrize("command", _LEGACY_COMMAND_INVOCATIONS)
def test_legacy_runnable_commands_remain_accepted(tmp_path: Path, command: str) -> None:
    _assert_verify_accepted_in_both_plan_shapes(
        tmp_path,
        command,
        full_command=f"env FNO_TEST=1 {command}",
    )


@pytest.mark.parametrize(
    "verification",
    (
        "Manually inspect the result",
        "make sure",
        "test the login flow manually",
        "go to settings",
        "timeout 30",
        "timeout 30 handwave",
        "docs/verify.md describes the check",
        "README.md",
        "--dry-run",
        "TODO",
        "Verify the login flow",
        "Check that the API works",
        "Ensure tests pass",
        "verify the login flow",
        "check that the API works",
        "ensure tests pass",
        'pytest tests/ && echo "unclosed',
    ),
)
def test_prose_and_non_commands_are_rejected_in_both_plan_shapes(
    tmp_path: Path, verification: str
) -> None:
    quick_dir = tmp_path / "quick"
    full_dir = tmp_path / "full"
    quick_dir.mkdir()
    full_dir.mkdir()
    quick = _write_plan(quick_dir, _quick_plan(verification=f"1. {verification}"))
    full = _write_plan(
        full_dir,
        _full_plan(_strategy_with_verify(verification)),
    )

    assert {v.field for v in validate_execution(load_plan(quick)).violations} == {
        "Verification"
    }, verification
    assert {
        v.field for v in validate_execution(load_plan(full)).violations
    } == {"tasks.1.1.verify"}, verification


def test_runnable_command_policy_has_no_positive_inventory() -> None:
    from fno.plan import execution_validation

    assert not hasattr(execution_validation, "_RUNNABLE_COMMANDS")


@pytest.mark.parametrize(
    "command",
    (
        '"$VIRTUAL_ENV/bin/pytest" -q',
        '"${REPO_ROOT}/scripts/check.sh"',
    ),
)
def test_shell_expanded_executable_paths_remain_accepted(
    tmp_path: Path, command: str
) -> None:
    _assert_verify_accepted_in_both_plan_shapes(
        tmp_path,
        command,
        full_command=f"env FNO_TEST=1 {command}",
    )


def test_quick_plan_prose_verification_is_not_runnable(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, _quick_plan(verification="1. Manually inspect the result."))

    result = validate_execution(load_plan(plan))
    cli_result = runner.invoke(plan_app, ["validate", str(plan), "--execution"])

    assert {v.field for v in result.violations} == {"Verification"}
    assert cli_result.exit_code == 1
    assert "Verification:" in cli_result.output


def test_quick_plan_arbitrary_surface_word_is_not_a_file(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, _quick_plan(files="- backend"))

    result = validate_execution(load_plan(plan))
    cli_result = runner.invoke(plan_app, ["validate", str(plan), "--execution"])

    assert {v.field for v in result.violations} == {"Files to Modify"}
    assert cli_result.exit_code == 1
    assert "Files to Modify:" in cli_result.output


def test_quick_plan_verification_stays_representation_tolerant(tmp_path: Path) -> None:
    for verification in (
        "1. `fno doctor test cli/tests/unit/test_a.py`",
        "- fno doctor test cli/tests/unit/test_a.py",
        "Run the check first.\n\n1. env FNO_DEBUG=1 fno doctor test cli/tests/unit/test_a.py",
        "1. `./scripts/ci/preflight.sh`",
        "1. `timeout 30 pytest cli/tests/unit/test_a.py`",
        "1. `timeout -k 5 1m fno doctor test cli/tests/unit/test_a.py`",
        "1. `gtimeout 30 pytest cli/tests/unit/test_a.py`",
        "1. `poetry run pytest -q`",
        "1. `tsc --noEmit`",
        # Command words that are also English verbs stay runnable when what
        # follows is an argument rather than a determiner.
        "1. `cargo check`",
        "1. `make test`",
        "1. `fno worktree ensure`",
        "1. `npm run check`",
        "1. `test -f out.txt`",
        "1. `make all`",
        # Segment splitting is quoting-blind; a quoted pipe is not a separator.
        "1. `rg -n 'foo|bar' src/`",
    ):
        plan = _write_plan(tmp_path, _quick_plan(verification=verification))

        assert validate_execution(load_plan(plan)).violations == [], verification


def test_quick_plan_accepts_concrete_paths_without_touching_the_filesystem(
    tmp_path: Path,
) -> None:
    for token in (
        "cli/src/fno/plan/does_not_exist_yet.py",
        "/etc/hosts",
        "~/.fno/config.toml",
        "app/[slug]/page.tsx",
        "AGENTS.md",
        "Dockerfile",
        ".gitignore",
        "gradle.properties",
        "site.webmanifest",
        # Extensionless paths are real files; accepting them is what keeps the
        # predicate lexical instead of asking the filesystem.
        "bin/mycli",
        "cli/src",
    ):
        plan = _write_plan(tmp_path, _quick_plan(files=f"- `{token}`"))

        assert validate_execution(load_plan(plan)).violations == [], token


def test_quick_plan_rejects_non_path_tokens(tmp_path: Path) -> None:
    for token in (
        "https://example.com/a.py",
        "--dry-run",
        "cli/**/*.py",
        "backend service",
        "cli/src/",
        "/",
        "~/.fno/",
        "..",
        "N/A",
    ):
        plan = _write_plan(tmp_path, _quick_plan(files=f"- `{token}`"))

        fields = {v.field for v in validate_execution(load_plan(plan)).violations}

        assert fields == {"Files to Modify"}, token


def test_quick_plan_malformed_shell_verification_fails_closed(tmp_path: Path) -> None:
    for verification in (
        '1. fno doctor test "unclosed',
        # A parseable segment must not rescue a value bash itself would reject.
        '1. pytest cli/tests/ && echo "done',
    ):
        plan = _write_plan(tmp_path, _quick_plan(verification=verification))

        fields = {v.field for v in validate_execution(load_plan(plan)).violations}

        assert fields == {"Verification"}, verification


def test_prose_that_opens_with_a_command_word_is_not_runnable(tmp_path: Path) -> None:
    for verification in (
        "1. docs/verify.md describes the check",
        # `make`, `test`, `go`, `cd` are English words as well as commands.
        "1. make sure the button works",
        "1. test the login flow manually",
        "1. go to the settings page",
        # A wrapper with no command behind it runs nothing.
        "1. timeout 30",
        "1. timeout 30 handwave",
    ):
        plan = _write_plan(tmp_path, _quick_plan(verification=verification))

        fields = {v.field for v in validate_execution(load_plan(plan)).violations}

        assert fields == {"Verification"}, verification


def test_full_plan_task_verify_accepts_a_timeout_wrapper(tmp_path: Path) -> None:
    strategy = VALID_STRATEGY.replace(
        "verify: fno doctor test cli/tests/unit/test_plan_execution_validation.py",
        "verify: timeout 300 fno doctor test cli/tests/unit/test_plan_execution_validation.py",
    )
    plan = _write_plan(tmp_path, _full_plan(strategy))

    assert validate_execution(load_plan(plan)).violations == []


def test_quick_plan_file_predicate_leaves_full_plan_surfaces_alone(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, _full_plan(VALID_STRATEGY.replace("cli/src/fno/plan/cli.py", "backend")))

    assert validate_execution(load_plan(plan)).violations == []


def test_placeholder_task_fields_fail_with_exact_paths(tmp_path: Path) -> None:
    strategy = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    tasks: [\"1.1\"]
tasks:
  - id: \"1.1\"
    title: Implement feature
    surface: []
    verify: \"# fill in verify command\"
    acceptance: []
"""
    plan = _write_plan(tmp_path, _full_plan(strategy))

    fields = {v.field for v in validate_execution(load_plan(plan)).violations}

    assert "tasks.1.1.surface" in fields
    assert "tasks.1.1.verify" in fields
    assert "tasks.1.1.acceptance" in fields


def test_prose_task_verification_is_not_runnable(tmp_path: Path) -> None:
    strategy = VALID_STRATEGY.replace(
        "verify: fno doctor test cli/tests/unit/test_plan_execution_validation.py",
        "verify: manually inspect the result",
    )
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "task verify must be a concrete runnable command" in messages


def test_wrapper_routes_incomplete_execution_strategy_to_semantic_validation(
    tmp_path: Path,
) -> None:
    strategy = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    tasks: ["1.1"]
"""
    plan = _write_plan(
        tmp_path,
        _full_plan(strategy),
        frontmatter="status: ready\ncreated: 2026-07-25\n",
    )

    result = subprocess.run(
        ["bash", str(VALIDATOR), str(plan)],
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
    )

    assert result.returncode == 1, result.stdout
    assert "Execution Strategy must declare at least one task" in result.stdout


def test_duplicate_task_and_unknown_wave_reference_fail(tmp_path: Path) -> None:
    strategy = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    tasks: [\"1.1\", \"9.9\"]
tasks:
  - id: \"1.1\"
    title: First
    surface: [src/a.py]
    verify: fno doctor test tests/test_a.py
    acceptance: [AC1]
  - id: \"1.1\"
    title: Duplicate
    surface: [src/b.py]
    verify: fno doctor test tests/test_b.py
    acceptance: [AC2]
"""
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "duplicate task id" in messages
    assert "unknown task '9.9'" in messages


def test_task_collections_must_be_yaml_lists(tmp_path: Path) -> None:
    strategy = VALID_STRATEGY.replace(
        "surface: [cli/src/fno/plan/cli.py]",
        "surface: cli/src/fno/plan/cli.py",
    )
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "task '1.1' surface must be a list" in messages


def test_parallel_wave_rejects_shared_surface(tmp_path: Path) -> None:
    strategy = """execution_mode: parallel
waves:
  - wave: 1
    mode: parallel
    tasks: [\"1.1\", \"1.2\"]
tasks:
  - id: \"1.1\"
    title: First
    surface: [src/shared.py]
    verify: fno doctor test tests/test_a.py
    acceptance: [AC1]
  - id: \"1.2\"
    title: Second
    surface: [src/shared.py]
    verify: fno doctor test tests/test_b.py
    acceptance: [AC2]
"""
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "parallel tasks share surface 'src/shared.py'" in messages


def test_wave_dependency_cycle_fails(tmp_path: Path) -> None:
    strategy = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    depends_on: 2
    tasks: [\"1.1\"]
  - wave: 2
    mode: sequential
    depends_on: 1
    tasks: [\"2.1\"]
tasks:
  - id: \"1.1\"
    title: First
    surface: [src/a.py]
    verify: fno doctor test tests/test_a.py
    acceptance: [AC1]
  - id: \"2.1\"
    title: Second
    surface: [src/b.py]
    verify: fno doctor test tests/test_b.py
    acceptance: [AC2]
"""
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "dependency cycle" in messages


def test_wave_contract_matches_scheduler(tmp_path: Path) -> None:
    strategy = VALID_STRATEGY.replace("wave: 1", "wave: nope").replace(
        "mode: sequential\n    name: Build",
        "mode: mixed\n    name: Build",
    )
    plan = _write_plan(tmp_path, _full_plan(strategy))

    fields = {v.field for v in validate_execution(load_plan(plan)).violations}

    assert "waves.nope.wave" in fields
    assert "waves.nope.mode" in fields


def test_forward_wave_dependency_fails(tmp_path: Path) -> None:
    strategy = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    depends_on: 2
    tasks: ["1.1"]
  - wave: 2
    mode: sequential
    tasks: ["2.1"]
tasks:
  - id: "1.1"
    title: First
    surface: [src/a.py]
    verify: fno doctor test tests/test_a.py
    acceptance: [AC1]
  - id: "2.1"
    title: Second
    surface: [src/b.py]
    verify: fno doctor test tests/test_b.py
    acceptance: [AC2]
"""
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "must reference an earlier declared wave" in messages


def test_task_id_matches_durable_state_grammar(tmp_path: Path) -> None:
    plan = _write_plan(
        tmp_path,
        _full_plan(VALID_STRATEGY.replace('"1.1"', '"setup-api"')),
    )

    fields = {v.field for v in validate_execution(load_plan(plan)).violations}

    assert "tasks.setup-api.id" in fields


def test_surface_items_are_paths_and_aliases_collide(tmp_path: Path) -> None:
    malformed = VALID_STRATEGY.replace(
        "surface: [cli/src/fno/plan/cli.py]",
        "surface: [{path: cli/src/fno/plan/cli.py}]",
    )
    malformed_plan = _write_plan(tmp_path, _full_plan(malformed))

    malformed_messages = "\n".join(
        v.message for v in validate_execution(load_plan(malformed_plan)).violations
    )

    assert "surface item 1 must be a non-empty path string" in malformed_messages

    alias_strategy = """execution_mode: parallel
waves:
  - wave: 1
    mode: parallel
    tasks: ["1.1", "1.2"]
tasks:
  - id: "1.1"
    title: First
    surface: [src/shared.py]
    verify: fno doctor test tests/test_a.py
    acceptance: [AC1]
  - id: "1.2"
    title: Second
    surface: [./src/shared.py]
    verify: fno doctor test tests/test_b.py
    acceptance: [AC2]
"""
    alias_plan = _write_plan(tmp_path, _full_plan(alias_strategy))

    alias_messages = "\n".join(
        v.message for v in validate_execution(load_plan(alias_plan)).violations
    )

    assert "parallel tasks share surface 'src/shared.py'" in alias_messages


def test_duplicate_yaml_keys_fail_loud(tmp_path: Path) -> None:
    strategy = VALID_STRATEGY + "tasks: []\n"
    plan = _write_plan(tmp_path, _full_plan(strategy))

    messages = "\n".join(v.message for v in validate_execution(load_plan(plan)).violations)

    assert "duplicate YAML key 'tasks'" in messages


def test_execution_cli_text_and_json(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, _full_plan(VALID_STRATEGY))

    text_result = runner.invoke(plan_app, ["validate", str(plan), "--execution"])
    json_result = runner.invoke(plan_app, ["validate", str(plan), "--execution", "--json"])

    assert text_result.exit_code == 0
    assert text_result.stdout == f"valid execution: {plan}\n"
    payload = json.loads(json_result.stdout)
    assert payload == {
        "valid": True,
        "path": str(plan),
        "scope": "execution",
        "violations": [],
        "warnings": [],
    }


def test_execution_cli_fails_nonzero_with_field_report(tmp_path: Path) -> None:
    plan = _write_plan(tmp_path, "## Context\n\nOnly context.\n")

    result = runner.invoke(plan_app, ["validate", str(plan), "--execution"])

    assert result.exit_code == 1
    assert "invalid execution:" in result.output
    assert "Changes:" in result.output


def test_default_cli_remains_frontmatter_only(tmp_path: Path) -> None:
    plan = _write_plan(
        tmp_path,
        "## Context\n\nExecution-invalid but irrelevant to default validation.\n",
        frontmatter="node: x-test\nstatus: ready\ncreated: 2026-07-25\n",
    )

    result = runner.invoke(plan_app, ["validate", str(plan)])

    assert result.exit_code == 0
    assert result.stdout == f"valid: {plan}\n"


# ---------------------------------------------------------------------------
# compiled-v1 acceptance-contract enforcement (x-f905)
# ---------------------------------------------------------------------------

_AC_PLAN_BODY = """## Overview

Executable plan.

## Acceptance Criteria

**AC1-HP:** Given valid input, the module returns the result.

## Execution Strategy

```yaml
{strategy}```
"""


def _ac_plan(strategy: str) -> str:
    return _AC_PLAN_BODY.format(strategy=strategy)


def _compiled_v1_frontmatter() -> str:
    return (
        "node: x-test\nstatus: ready\ncreated: 2026-07-25\n"
        "acceptance_contract: compiled-v1\n"
    )


_STRATEGY_WITH_AC9_REF = """execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    name: Build
    tasks: ["1.1"]
tasks:
  - id: "1.1"
    title: Build the validator
    surface: [cli/src/fno/plan/cli.py]
    verify: fno doctor test cli/tests/unit/test_plan_execution_validation.py
    acceptance: [AC9]
"""


def test_compiled_v1_refuses_unresolved_acceptance_reference(tmp_path: Path) -> None:
    """AC5-ERR: a compiled-v1 task ref that resolves to no criterion fails."""
    plan = _write_plan(
        tmp_path,
        _ac_plan(_STRATEGY_WITH_AC9_REF),
        frontmatter=_compiled_v1_frontmatter(),
    )

    result = validate_execution(load_plan(plan))

    fields = [v.field for v in result.violations]
    messages = [v.message for v in result.violations]
    assert any("tasks.1.1.acceptance" in f for f in fields), fields
    assert any("AC9" in m for m in messages), messages


def test_compiled_v1_accepts_resolving_reference(tmp_path: Path) -> None:
    """A compiled-v1 task ref that resolves to a compiled criterion passes."""
    strategy = _STRATEGY_WITH_AC9_REF.replace("acceptance: [AC9]", "acceptance: [AC1-HP]")
    plan = _write_plan(
        tmp_path,
        _ac_plan(strategy),
        frontmatter=_compiled_v1_frontmatter(),
    )

    result = validate_execution(load_plan(plan))

    assert result.violations == []


def test_unstamped_plan_keeps_permissive_reference_read(tmp_path: Path) -> None:
    """AC3-COMPAT: an unstamped historical plan does not acquire ref-resolution failures."""
    # Same unresolved AC9 ref, but no acceptance_contract marker.
    frontmatter = "node: x-test\nstatus: ready\ncreated: 2026-07-25\n"
    plan = _write_plan(tmp_path, _ac_plan(_STRATEGY_WITH_AC9_REF), frontmatter=frontmatter)

    result = validate_execution(load_plan(plan))

    assert [v for v in result.violations if "AC9" in v.message] == []


def test_validate_execution_acceptance_contract_override(tmp_path: Path) -> None:
    """The acceptance_contract param validates a PROPOSED state without writing it.

    mutate_doc.finalize passes compiled-v1 to validate a design->ready promotion
    before stamping; an unstamped doc is validated as if it were compiled-v1.
    """
    plan = _write_plan(
        tmp_path,
        _ac_plan(_STRATEGY_WITH_AC9_REF),
        frontmatter="node: x-test\nstatus: design\ncreated: 2026-07-25\n",
    )

    result = validate_execution(load_plan(plan), acceptance_contract="compiled-v1")

    assert any("AC9" in v.message for v in result.violations)

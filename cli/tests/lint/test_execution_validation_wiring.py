import ast
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]


def _read(relative: str) -> str:
    return (REPO_ROOT / relative).read_text(encoding="utf-8")


def _fno_argv_root(node: ast.List | ast.Tuple) -> str | None:
    first = node.elts[0]
    marker = ast.unparse(first).lower()
    constants = [
        elt.value for elt in node.elts
        if isinstance(elt, ast.Constant) and isinstance(elt.value, str)
    ]
    module_form = "fno.cli" in constants
    if "fno" not in marker and not module_form:
        return None
    if module_form:
        constants = constants[constants.index("fno.cli") + 1:]
    elif constants and constants[0] in {"fno", "fno-py", "fno-agents"}:
        constants = constants[1:]
    return constants[0] if constants else None


def test_target_validates_quick_plans_before_execution() -> None:
    target = _read("skills/target/SKILL.md")
    init_state = _read("skills/target/references/init-state.md")

    assert "Skip for single-file" not in target
    assert "Quick-plan exception" not in init_state
    assert "validate-plan.sh" in target


def test_flat_execute_uses_bundled_plan_validator() -> None:
    flat = _read("skills/execute/references/flat.md")
    bundles = _read("skill-bundles.yaml")

    assert 'scripts/validate-plan.sh" "$PLAN_PATH"' in flat
    execute_bundle = bundles.split("- skill: execute", 1)[1]
    assert "source: scripts/validate-plan.sh" in execute_bundle


def test_all_plan_validators_use_the_canonical_source_first_root() -> None:
    for relative in (
        "scripts/validate-plan.sh",
        "skills/blueprint/scripts/validate-plan.sh",
        "skills/execute/scripts/validate-plan.sh",
    ):
        validator = _read(relative)
        assert "-m fno.cli do plan rung" in validator
        assert "-m fno.cli plan" not in validator


def test_python_self_shellouts_do_not_depend_on_expiring_roots() -> None:
    expected = {
        "cli/src/fno/pr/_ritual.py": (
            '["do", "plan", "reconcile-status", "--apply"]',
            '["do", "pr", "sync-canonical"',
        ),
        "cli/src/fno/retro/keep_going.py": (
            '"do", "think", "dispatch", node_id, "--json"',
        ),
        "cli/src/fno/post_merge_route.py": (
            '"do", "pr", "ritual", str(pr_number), "--autonomous"',
        ),
    }
    for relative, markers in expected.items():
        source = _read(relative)
        for marker in markers:
            assert marker in source


def test_every_python_fno_argv_avoids_expiring_roots() -> None:
    moved = {
        "delivery", "loops", "phase", "plan", "pr", "pr-watch", "research",
        "resume", "review", "state", "stub-manifest", "target", "think",
    }
    failures = []
    for tree in ("cli/src", "hooks", "scripts"):
        for path in (REPO_ROOT / tree).rglob("*.py"):
            try:
                parsed = ast.parse(path.read_text(encoding="utf-8"))
            except (OSError, SyntaxError):
                continue
            for node in ast.walk(parsed):
                if not isinstance(node, (ast.List, ast.Tuple)) or not node.elts:
                    continue
                root = _fno_argv_root(node)
                if root in moved:
                    failures.append(f"{path.relative_to(REPO_ROOT)}:{node.lineno}:{root}")
    assert not failures, "deprecated-root self-shellouts:\n" + "\n".join(failures)


def test_python_module_argv_shape_reaches_the_expiring_root_guard() -> None:
    parsed = ast.parse(
        'subprocess.run([sys.executable, "-m", "fno.cli", "pr", "hold-check"])'
    )
    argv = next(node for node in ast.walk(parsed) if isinstance(node, ast.List))
    assert _fno_argv_root(argv) == "pr"


def test_literal_binary_argv_shape_reaches_the_expiring_root_guard() -> None:
    parsed = ast.parse('subprocess.run(["fno-py", "pr", "hold-check"])')
    argv = next(node for node in ast.walk(parsed) if isinstance(node, ast.List))
    assert _fno_argv_root(argv) == "pr"


def test_blueprint_enriches_and_validates_before_intake() -> None:
    blueprint = _read("skills/blueprint/SKILL.md")

    enrich = blueprint.index("Enrich the Execution Strategy")
    assert "mutate_doc.py` with `--draft`" in blueprint
    validate = blueprint.index("validate-plan.sh", enrich)
    collision = blueprint.index("3a. **Collision check", validate)
    intake = blueprint.index("3b. **Auto-intake", collision)

    assert enrich < validate < collision < intake


def test_raw_prose_and_node_seeded_inputs_keep_discovery() -> None:
    blueprint = _read("skills/blueprint/SKILL.md")

    assert "raw prose or a node-seeded path" in blueprint

from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[3]


def _read(relative: str) -> str:
    return (REPO_ROOT / relative).read_text(encoding="utf-8")


def test_target_validates_quick_plans_before_execution() -> None:
    target = _read("skills/target/SKILL.md")
    init_state = _read("skills/target/references/init-state.md")

    assert "Skip for single-file" not in target
    assert "Quick-plan exception" not in init_state
    assert "validate-plan.sh" in target


def test_flat_do_uses_bundled_plan_validator() -> None:
    flat = _read("skills/do/references/flat.md")
    bundles = _read("skill-bundles.yaml")

    assert 'scripts/validate-plan.sh" "$PLAN_PATH"' in flat
    do_bundle = bundles.split("- skill: do", 1)[1]
    assert "source: scripts/validate-plan.sh" in do_bundle


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

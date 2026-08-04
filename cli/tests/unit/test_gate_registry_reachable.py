"""The company boundary gates must be reachable from ``fno test smoke``."""

from fno.test_cmd import _STRUCTURAL_STEPS


def test_company_boundary_gate_family_has_registered_callers() -> None:
    commands = "\n".join(command for _, _, command in _STRUCTURAL_STEPS)
    required = (
        "scripts/ci/check-company-boundaries.sh",
        "tests/ci/test_check_company_boundaries.sh",
        "scripts/ci/check-company-function-agnostic.sh",
        "scripts/ci/check-plugins-function-agnostic.sh",
    )

    missing = [path for path in required if path not in commands]

    assert missing == [], f"unreachable company boundary gates: {missing}"


def test_intentionally_red_boundary_verdict_runs_after_the_other_family_checks() -> None:
    commands = [command for _, _, command in _STRUCTURAL_STEPS]
    boundary = commands.index("bash scripts/ci/check-company-boundaries.sh")
    prerequisites = (
        "bash tests/ci/test_check_company_boundaries.sh",
        "bash scripts/ci/check-company-function-agnostic.sh",
        "bash scripts/ci/check-plugins-function-agnostic.sh",
    )

    assert all(commands.index(command) < boundary for command in prerequisites)

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

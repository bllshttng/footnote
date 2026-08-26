"""Decision graduation declarations are local, explicit, and machine-readable."""

from __future__ import annotations

import pytest
import subprocess


@pytest.mark.parametrize(
    ("kind", "reference", "expected"),
    [
        (
            "enforced",
            "test:tests.unit.test_decide::test_operator_only",
            {
                "kind": "enforced",
                "artifact": "test:tests.unit.test_decide::test_operator_only",
            },
        ),
        ("guidance", None, {"kind": "guidance"}),
        (
            "should-be-enforced-but-i-did-not",
            "node:x-90f5",
            {
                "kind": "should-be-enforced-but-i-did-not",
                "follow_up": "node:x-90f5",
            },
        ),
    ],
)
def test_validate_graduation_returns_structured_declaration(
    kind: str, reference: str | None, expected: dict[str, str]
):
    from fno.decide.graduation import validate_graduation

    assert validate_graduation(kind, reference) == expected


def test_omitted_graduation_defaults_to_honest_guidance():
    from fno.decide.graduation import graduation_or_guidance

    assert graduation_or_guidance(None, None) == {"kind": "guidance"}


@pytest.mark.parametrize(
    ("kind", "reference"),
    [
        ("", None),
        ("unknown", None),
        ("enforced", None),
        ("enforced", "future:maybe"),
        ("guidance", "file:cli/src/fno/decide/__init__.py"),
        ("should-be-enforced-but-i-did-not", None),
        ("should-be-enforced-but-i-did-not", "x-90f5"),
    ],
)
def test_validate_graduation_refuses_missing_or_malformed_declarations(
    kind: str, reference: str | None
):
    from fno.decide.graduation import InvalidGraduationError, validate_graduation

    with pytest.raises(InvalidGraduationError):
        validate_graduation(kind, reference)


def test_passing_test_probe_returns_positive_retirement_marker(tmp_path):
    from fno.decide.graduation import evaluate_graduation

    calls = []

    def run(argv, *, cwd, timeout):
        calls.append((argv, cwd, timeout))
        return subprocess.CompletedProcess(argv, 0, stdout="1 passed", stderr="")

    artifact = "test:cli/tests/unit/test_decide.py::test_operator_lane_refusal_uses_proven_identity"
    result = evaluate_graduation(
        {"decision_id": "d-12345678", "graduation": {"kind": "enforced", "artifact": artifact}},
        root=tmp_path,
        run=run,
    )

    assert result["status"] == "retired"
    assert result["graduation_checked"].startswith("test_passed:")
    assert result["graduation_retired"] == "d-12345678"
    assert calls[0][0][-1].endswith("::test_operator_lane_refusal_uses_proven_identity")


def test_gate_probe_requires_named_marker_even_on_exit_zero(tmp_path):
    from fno.decide.graduation import evaluate_graduation

    artifact = "gate:fno backlog idea --priority p0=>marker:P0_REFUSED"
    missing = evaluate_graduation(
        {"decision_id": "d-12345678", "graduation": {"kind": "enforced", "artifact": artifact}},
        root=tmp_path,
        run=lambda argv, **kwargs: subprocess.CompletedProcess(argv, 0, stdout="", stderr=""),
    )
    assert missing["status"] == "unknown"
    assert missing["graduation_checked"] == "marker_missing:P0_REFUSED"

    present = evaluate_graduation(
        {"decision_id": "d-12345678", "graduation": {"kind": "enforced", "artifact": artifact}},
        root=tmp_path,
        run=lambda argv, **kwargs: subprocess.CompletedProcess(
            argv, 2, stdout="P0_REFUSED", stderr=""
        ),
    )
    assert present["status"] == "retired"
    assert present["graduation_checked"] == "marker_present:P0_REFUSED"


def test_missing_file_and_timed_out_test_remain_unknown(tmp_path):
    from fno.decide.graduation import evaluate_graduation

    missing = evaluate_graduation(
        {
            "decision_id": "d-missing",
            "graduation": {
                "kind": "enforced",
                "artifact": "file:missing.py:10=>marker:P0_REFUSED",
            },
        },
        root=tmp_path,
    )
    assert missing["status"] == "unknown"
    assert missing["graduation_checked"].startswith("artifact_missing:")

    def timeout(argv, **kwargs):
        raise subprocess.TimeoutExpired(argv, 30)

    timed_out = evaluate_graduation(
        {
            "decision_id": "d-timeout",
            "graduation": {"kind": "enforced", "artifact": "test:tests/test_slow.py::test_slow"},
        },
        root=tmp_path,
        run=timeout,
    )
    assert timed_out["status"] == "unknown"
    assert timed_out["graduation_checked"].startswith("probe_timeout:")


def test_guidance_without_a_probe_stays_guidance(tmp_path):
    from fno.decide.graduation import evaluate_graduation

    result = evaluate_graduation(
        {"decision_id": "d-guidance", "graduation": {"kind": "guidance"}},
        root=tmp_path,
    )
    assert result == {"decision_id": "d-guidance", "status": "guidance"}


def test_file_probe_requires_a_marker_and_reads_it_at_the_pinned_line(tmp_path):
    from fno.decide.graduation import evaluate_graduation

    target = tmp_path / "guard.py"
    target.write_text("import os\nRAISE_ON_P0 = True\n", encoding="utf-8")

    def probe(artifact: str) -> dict[str, str]:
        return evaluate_graduation(
            {
                "decision_id": "d-12345678",
                "graduation": {"kind": "enforced", "artifact": artifact},
            },
            root=tmp_path,
        )

    # Presence alone never retires: the declaration must name the evidence.
    unmarked = probe("file:guard.py:2")
    assert unmarked["status"] == "unknown"
    assert unmarked["graduation_checked"].startswith("artifact_marker_invalid:")

    # The file survives, but the enforcement moved off the pinned line.
    shifted = probe("file:guard.py:1=>marker:RAISE_ON_P0")
    assert shifted["status"] == "unknown"
    assert shifted["graduation_checked"].startswith("marker_missing:")

    # The line is gone entirely while the file remains.
    dropped = probe("file:guard.py:9=>marker:RAISE_ON_P0")
    assert dropped["status"] == "unknown"
    assert dropped["graduation_checked"].startswith("artifact_line_missing:")

    present = probe("file:guard.py:2=>marker:RAISE_ON_P0")
    assert present["status"] == "retired"
    assert present["graduation_checked"] == (
        "marker_present:file:guard.py:2=>marker:RAISE_ON_P0"
    )


def test_default_probe_compares_the_required_value(tmp_path):
    from fno.decide.graduation import evaluate_graduation

    def probe(artifact: str, stdout: str, returncode: int = 0) -> dict[str, str]:
        return evaluate_graduation(
            {
                "decision_id": "d-12345678",
                "graduation": {"kind": "enforced", "artifact": artifact},
            },
            root=tmp_path,
            run=lambda argv, **kwargs: subprocess.CompletedProcess(
                argv, returncode, stdout=stdout, stderr=""
            ),
        )

    # No required value in the declaration means nothing to compare against.
    unpinned = probe("default:config.review.self_review_required", "True")
    assert unpinned["status"] == "unknown"
    assert unpinned["graduation_checked"].startswith("default_expectation_missing:")

    # The key exists and reads nonempty, but contradicts the declared default.
    contradicted = probe("default:config.review.self_review_required=True", "False")
    assert contradicted["status"] == "unknown"
    assert contradicted["graduation_checked"] == (
        "default_mismatch:config.review.self_review_required=False"
    )

    matched = probe("default:config.review.self_review_required=True", "True")
    assert matched["status"] == "retired"
    assert matched["graduation_checked"] == (
        "default_present:config.review.self_review_required=True"
    )

"""Unit tests for `acceptance_evidence` finalize validation (x-d098)."""
from __future__ import annotations

from collections import OrderedDict

from fno.plan._doc import PlanDoc
from fno.plan.execution_validation import validate_execution

_STRATEGY = """## Execution Strategy

```yaml
execution_mode: sequential
waves:
  - wave: 1
    mode: parallel
    name: w
    difficulty: high
    tasks: ['1.1']
tasks:
  - id: '1.1'
    title: t
    surface: [a.py]
    verify: pytest -q
    acceptance: [AC1-HP]
```
"""

_AC = "**AC1-HP:** Given a probe, when it runs, then it passes."


def _violations(fm_body: str) -> list[tuple[str, str]]:
    import yaml

    fm = yaml.safe_load(fm_body)
    sections = OrderedDict(
        [
            ("Acceptance Criteria", _AC),
            ("Execution Strategy", _STRATEGY),
        ]
    )
    doc = PlanDoc(yaml.safe_load("acceptance_contract: compiled-v1\n" + fm_body), sections)
    result = validate_execution(doc)
    return [(v.field, v.message) for v in result.violations]


def test_required_done_binding_is_legal() -> None:
    assert (
        _violations(
            "done_probes:\n  - 'echo m1'\n"
            "acceptance_evidence:\n  required: true\n  bindings:\n    AC1-HP: done_probes[0]\n"
        )
        == []
    )


def test_unarmed_declaration_is_legal() -> None:
    assert _violations("done_probes:\n  - 'echo m1'\nacceptance_evidence: {}\n") == []


def test_close_binding_is_legal_when_close_probes_exist() -> None:
    assert (
        _violations(
            "close_probes:\n  - 'echo m1'\n"
            "acceptance_evidence:\n  bindings:\n    AC1-HP: close_probes[0]\n"
        )
        == []
    )


def test_unknown_ac_refused_by_name() -> None:
    got = _violations(
        "done_probes:\n  - 'echo m1'\n"
        "acceptance_evidence:\n  bindings:\n    AC9-HP: done_probes[0]\n"
    )
    assert any("AC9-HP" in m for _f, m in got)


def test_malformed_reference_refuses() -> None:
    got = _violations(
        "done_probes:\n  - 'echo m1'\n"
        "acceptance_evidence:\n  bindings:\n    AC1-HP: probes[0]\n"
    )
    assert any("expected `done_probes[n]`" in m for _f, m in got)


def test_missing_probe_refused_by_name() -> None:
    got = _violations(
        "done_probes: 'echo m1'\n"
        "acceptance_evidence:\n  bindings:\n    AC1-HP: done_probes[1]\n"
    )
    assert any("the bound probe is missing" in m for _f, m in got)


def test_required_without_done_binding_refuses() -> None:
    got = _violations(
        "close_probes:\n  - 'echo m1'\n"
        "acceptance_evidence:\n  required: true\n  bindings:\n    AC1-HP: close_probes[0]\n"
    )
    assert any(
        "asserts required evidence" in m and "session terminal" in m for _f, m in got
    )


def test_criterion_without_binding_stays_unmeasured() -> None:
    got = _violations(
        "done_probes:\n  - 'echo m1'\n"
        "acceptance_evidence:\n  bindings:\n    AC1-HP: done_probes[0]\n"
    )
    assert got == []

"""Report-builder tests for the aggregate-vs-per-agent contradiction.

Background: a sigma artifact once printed
``findings_critical/high/medium/low`` all 0 and a "Suspicious-clean note"
claiming "All 6 workers returned zero findings", while the Per-agent outcomes
block in the SAME artifact listed 5, 8, 6, 6, 0, 8 findings. The aggregate is
scored/filtered while the per-agent counts read the raw worker findings, so a
confidence scorer that drops every finding produces a self-contradictory
artifact that reads as a clean gate.

These tests pin the corrected behavior: the report must surface that
contradiction as a reliability failure, not paper over it with an "all workers
returned zero" note.
"""

from __future__ import annotations

from fno.review.orchestrator import Finding, OrchestratorResult, WorkerOutcome
from fno.review.report_builder import choose_verdict, render_artifact_markdown


def _outcome(agent: str, n: int) -> WorkerOutcome:
    return WorkerOutcome(
        agent=agent,
        ok=True,
        findings=[
            Finding(agent=agent, severity="high", message=f"{agent} issue {i}")
            for i in range(n)
        ],
        duration_seconds=1.0,
    )


def _result(*, findings: list[Finding], outcomes: list[WorkerOutcome],
            suspicious: bool = False) -> OrchestratorResult:
    return OrchestratorResult(
        findings=findings,
        workers_completed=sum(1 for o in outcomes if o.ok),
        workers_failed=sum(1 for o in outcomes if not o.ok),
        suspicious=suspicious,
        duration_seconds=10.0,
        outcomes=outcomes,
    )


class TestContradictionNote:
    """Workers returned findings but the aggregate dropped them all."""

    def test_renders_contradiction_note_not_all_zero(self) -> None:
        # Per-agent outcomes carry 5 + 8 findings, but the scored aggregate is
        # empty (the confidence scorer dropped everything).
        result = _result(
            findings=[],
            outcomes=[_outcome("code_reviewer", 5), _outcome("silent_failure_hunter", 8)],
            suspicious=True,  # worker layer sets this via threshold_drop_suspicious
        )
        md = render_artifact_markdown("sess", result, choose_verdict(result))

        assert "Aggregate contradiction" in md
        # The misleading "all workers returned zero" line must NOT appear when
        # workers actually returned findings.
        assert "returned zero findings" not in md
        # The note names the real per-agent count so a human can see what was lost.
        assert "13" in md

    def test_contradiction_verdict_is_not_ready_to_merge(self) -> None:
        result = _result(
            findings=[],
            outcomes=[_outcome("code_reviewer", 5)],
            suspicious=True,
        )
        # A dropped-findings contradiction is a reliability failure, never clean.
        assert choose_verdict(result) != "ready-to-merge"

    def test_contradiction_detected_even_without_suspicious_flag(self) -> None:
        # Defense-in-depth: even if a caller forgot to set suspicious, the
        # report must still refuse to look clean when workers found things the
        # aggregate dropped.
        result = _result(
            findings=[],
            outcomes=[_outcome("ux_flow_tester", 6)],
            suspicious=False,
        )
        assert choose_verdict(result) != "ready-to-merge"
        md = render_artifact_markdown("sess", result, choose_verdict(result))
        assert "Aggregate contradiction" in md


class TestGenuineAllClean:
    """Every worker truly returned zero findings."""

    def test_renders_suspicious_clean_note_when_all_zero(self) -> None:
        result = _result(
            findings=[],
            outcomes=[
                WorkerOutcome(agent="code_reviewer", ok=True, findings=[], duration_seconds=1.0),
                WorkerOutcome(agent="silent_failure_hunter", ok=True, findings=[], duration_seconds=1.0),
            ],
            suspicious=True,
        )
        md = render_artifact_markdown("sess", result, choose_verdict(result))

        # Genuine all-clean keeps the skepticism note, and it must be accurate.
        assert "Suspicious-clean note" in md
        assert "returned zero findings" in md
        # The contradiction note must not fire when nothing was dropped.
        assert "Aggregate contradiction" not in md

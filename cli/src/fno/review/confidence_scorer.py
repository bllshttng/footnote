"""Confidence scoring pass for review findings.

Resolves a default scorer at call time based on whether the ``claude``
CLI is on PATH. When present, uses the batched :func:`claude_scorer_batch`
so N findings cost one subprocess call instead of N. When absent, falls
back to ``pass_through_scorer`` with a one-shot stderr warning so the
review pipeline still produces an artifact.
"""

from __future__ import annotations

import dataclasses
import shutil
import sys
from typing import Callable, Iterable, Union

from fno.review.orchestrator import Finding

ScorerCallable = Callable[[Finding], int]
"""(finding) -> confidence 0-100. Return >= threshold to keep."""

BatchScorerCallable = Callable[[list[Finding]], list[int]]
"""(findings) -> list of confidences in the same order as the input."""

AnyScorer = Union[ScorerCallable, BatchScorerCallable]

# Module-level flag: warning about missing `claude` binary emits exactly once.
_no_claude_warned: bool = False


def pass_through_scorer(finding: Finding) -> int:
    """Assign max confidence to every finding (no-op filter)."""
    return 100


def score_and_filter(
    findings: Iterable[Finding],
    *,
    scorer: ScorerCallable = pass_through_scorer,
    threshold: int = 80,
) -> list[Finding]:
    """Score each finding and drop any below ``threshold``.

    Returns new Finding instances with ``confidence`` populated. The
    original findings are not mutated; the orchestrator's outcome
    list stays intact for audit.
    """
    kept: list[Finding] = []
    for f in findings:
        score = scorer(f)
        if score < threshold:
            continue
        kept.append(
            Finding(
                agent=f.agent,
                severity=f.severity,
                message=f.message,
                file=f.file,
                line=f.line,
                confidence=score,
                raw=f.raw,
            )
        )
    return kept


def _resolve_default_scorer() -> AnyScorer:
    """Return the appropriate default scorer based on environment.

    When the ``claude`` CLI is discoverable via :func:`shutil.which`,
    returns :func:`claude_scorer_batch` (which has ``__batch__ = True``
    so :func:`score_findings` can take the one-shot path). Otherwise
    returns :func:`pass_through_scorer` and emits a one-shot stderr
    warning.
    """
    global _no_claude_warned

    if shutil.which("claude"):
        from fno.review.scorers.claude_scorer import claude_scorer_batch

        return claude_scorer_batch

    if not _no_claude_warned:
        print(
            "review: `claude` CLI not on PATH - using pass-through confidence "
            "scoring; set threshold manually",
            file=sys.stderr,
        )
        _no_claude_warned = True

    return pass_through_scorer


def score_findings(
    findings: Iterable[Finding],
    *,
    scorer: AnyScorer | None = None,
    threshold: int = 80,
) -> list[Finding]:
    """Score each finding and drop any below ``threshold``.

    Args:
        findings: Iterable of ``Finding`` instances. Not mutated.
        scorer: Override scorer callable. When ``None``, resolves via
            :func:`_resolve_default_scorer`. If the resolved (or
            supplied) scorer has ``__batch__ = True``, the scorer is
            called once with the full findings list; otherwise it is
            called per-finding.
        threshold: Minimum confidence to keep a finding (inclusive).
            Default 80.

    Returns:
        List of new ``Finding`` instances with ``confidence`` stamped.
        Findings below ``threshold`` are excluded.
    """
    resolved = scorer if scorer is not None else _resolve_default_scorer()
    findings_list = list(findings)

    if getattr(resolved, "__batch__", False):
        scores = resolved(findings_list)  # type: ignore[arg-type]
        # User-supplied batch scorers can silently return the wrong length;
        # zip() would then drop or misalign findings. Zero out and warn so
        # no finding gets mis-attributed to another's score.
        if not isinstance(scores, list) or len(scores) != len(findings_list):
            got = len(scores) if isinstance(scores, list) else type(scores).__name__
            print(
                f"review: batch scorer returned {got} for {len(findings_list)} "
                "findings; zeroing all scores",
                file=sys.stderr,
            )
            scores = [0] * len(findings_list)
    else:
        scores = [resolved(f) for f in findings_list]  # type: ignore[arg-type]

    kept: list[Finding] = []
    for f, score in zip(findings_list, scores):
        if score < threshold:
            continue
        kept.append(dataclasses.replace(f, confidence=score))
    return kept

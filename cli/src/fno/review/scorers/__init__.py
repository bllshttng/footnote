"""Confidence scorer implementations for the review pipeline.

Each scorer callable has the signature::

    (finding: Finding) -> int

and returns an integer 0-100. The batched variant has the signature::

    (findings: list[Finding]) -> list[int]

and is tagged with ``__batch__ = True`` so
``fno.review.confidence_scorer.score_findings`` can detect it and
take the one-shot subprocess path.
"""

from fno.review.scorers.claude_scorer import claude_scorer, claude_scorer_batch

__all__ = ["claude_scorer", "claude_scorer_batch"]

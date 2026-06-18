"""Shared data model for the retro-triage pipeline.

A ``RawItem`` is one harvested left-out signal (a carve-out, a declined review
finding, or a done-with-concerns deferred finding). A ``Candidate`` is a
classified, dedup-ready node/inbox proposal carrying the originating reasoning
verbatim plus a source cite.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Optional

# Harvest source kinds.
KIND_CARVEOUT = "carveout"
KIND_REVIEW = "review"
KIND_DEFERRED = "deferred_finding"

# Landing tiers (severity-tiered, Locked Decision #5).
TIER_NODE = "node"
TIER_INBOX = "inbox"


@dataclass
class RawItem:
    """One harvested left-out signal, pre-classification."""

    kind: str  # KIND_CARVEOUT | KIND_REVIEW | KIND_DEFERRED
    text: str  # verbatim originating reasoning
    source_pr: Optional[int] = None
    source_id: str = ""  # comment id / carveout id / artifact:index
    severity: Optional[str] = None  # critical|high|medium|low (reviews)
    priority: Optional[str] = None  # pN hint (carve-outs)
    url: Optional[str] = None  # link back to the source comment/thread
    reviewer: Optional[str] = None  # reviewer bot login (reviews)
    title_hint: Optional[str] = None  # e.g. a carve-out's --need question
    subkind: Optional[str] = None  # a carve-out's kind: deferred | oos-bug


@dataclass
class Candidate:
    """A classified, dedup-ready node/inbox proposal."""

    title: str
    body: str
    tier: str  # TIER_NODE | TIER_INBOX
    priority: str  # pN
    source_pr: Optional[int]
    source_id: str
    content_hash: str = ""
    uncited: bool = False  # rejected: no source_pr/source id (anti-hallucination)
    # Raw finding text for content-hash dedup (NOT the cite, so the same issue
    # flagged by two reviewers collapses to one node - AC5-EDGE). Named field,
    # not buried in extra, so the classify->dedup contract is type-visible.
    finding_text: str = ""
    extra: dict = field(default_factory=dict)

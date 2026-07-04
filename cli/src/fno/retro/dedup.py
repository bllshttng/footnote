"""Dedup candidates by (source_pr + content-hash), idempotently.

The dedup key is ``{source_pr}:{blake2b(normalized_finding_body)}``. Normalization
strips reviewer severity badges, collapses whitespace, and lowercases, so the SAME
issue flagged by two different reviewers (different badge/cite) collapses to one
node (AC5-EDGE). Re-running triage on a PR creates nothing because the key is found
on an existing node (AC5-HP). A node a human later DELETED is re-created, because
dedup keys off LIVE nodes only (AC5-FR, documented behavior).

Persistence: ``land`` writes a machine trailer into each node's ``details``:

    <!-- retro-triage source_pr=343 finding_hash=ab12cd34ef56 -->

so a later run can read existing keys back without new schema fields (Discretion #6).
"""
from __future__ import annotations

import re
from hashlib import blake2b
from typing import Iterable

from fno.retro.types import Candidate

_BADGE_RE = re.compile(r"!\[[^\]]*\]\([^)]*\)|!\[[^\]]*\]")
_WS_RE = re.compile(r"\s+")

# Machine trailer land writes into node details; read back here for dedup.
# `None` is a legal pr: postmortem-sourced nodes (W6 6.2) have no PR, and their
# trailer must round-trip so a failed consumed_at stamp still dedups on rerun.
_TRAILER_RE = re.compile(
    r"<!--\s*retro-triage\s+source_pr=(?P<pr>\d+|None)\s+finding_hash=(?P<hash>[0-9a-f]+)\s*-->"
)


def normalize_finding(text: str) -> str:
    """Stable normalization for hashing: strip badges, collapse ws, lowercase."""
    t = _BADGE_RE.sub("", text or "")
    t = _WS_RE.sub(" ", t).strip().lower()
    return t


def content_hash(text: str) -> str:
    return blake2b(normalize_finding(text).encode("utf-8"), digest_size=8).hexdigest()


def dedup_key(source_pr: object, text: str) -> str:
    return f"{source_pr}:{content_hash(text)}"


def trailer(source_pr: object, finding_hash: str) -> str:
    """The machine trailer land appends to a node's details."""
    return f"<!-- retro-triage source_pr={source_pr} finding_hash={finding_hash} -->"


def _candidate_text(c: Candidate) -> str:
    # Prefer the raw finding text set by classify; fall back to the body.
    return c.finding_text or c.extra.get("finding_text") or c.body


def assign_hashes(candidates: Iterable[Candidate]) -> list[Candidate]:
    """Fill ``content_hash`` on each candidate (idempotent)."""
    out: list[Candidate] = []
    for c in candidates:
        c.content_hash = content_hash(_candidate_text(c))
        out.append(c)
    return out


def existing_keys_from_nodes(nodes: Iterable[dict]) -> set[str]:
    """Scan live nodes' ``details`` for retro-triage trailers -> set of dedup keys."""
    keys: set[str] = set()
    for node in nodes:
        details = node.get("details")
        if not details:
            continue
        for m in _TRAILER_RE.finditer(str(details)):
            keys.add(f"{m.group('pr')}:{m.group('hash')}")
    return keys


def dedup_candidates(
    candidates: Iterable[Candidate],
    *,
    existing_keys: Iterable[str] = (),
) -> "tuple[list[Candidate], list[Candidate]]":
    """Return (kept, skipped).

    Skips candidates whose key matches a live node (``existing_keys``) and
    collapses within-run duplicates (first wins). ``assign_hashes`` is applied
    so callers need not pre-hash.
    """
    seen = set(existing_keys)
    kept: list[Candidate] = []
    skipped: list[Candidate] = []
    for c in assign_hashes(candidates):
        key = f"{c.source_pr}:{c.content_hash}"
        if key in seen:
            skipped.append(c)
            continue
        seen.add(key)
        kept.append(c)
    return kept, skipped

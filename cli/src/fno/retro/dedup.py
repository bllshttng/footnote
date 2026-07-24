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
from typing import Any, Iterable, Optional

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


def anchor_verdict(candidate: Candidate, scan_fn: Any) -> str:
    """Filing-time anchor verdict (x-a7ab 1.1): is this finding's code still
    broken, or already addressed (fixed-on-main)?

    Returns one of:
      "present"      - not addressed, OR the check is not applicable (no
                       source_pr, no scan_fn, or the finding carries no
                       review-comment URL to resolve an anchor) -> mint normally.
      "dead"         - the source PR now shows the finding addressed -> skip with
                       a fixed-on-main receipt (AC1-HP).
      "unresolvable" - the scan itself errored -> fail toward filing with an
                       anchor-unverified note (AC5-EDGE); a false skip would lose
                       a real finding, a false mint costs one triage row.

    Reuses the x-7624 dispatch-time scan (``scan_addressed_findings``) by building
    a transient trailer-bearing entry from the candidate; the scan derives
    repo/comment_id from the review-comment URL embedded in the candidate body.
    """
    if not scan_fn or not candidate.source_pr:
        return "present"
    pseudo = {
        "id": f"candidate:{candidate.source_id}",
        "details": (
            f"{candidate.body or ''}\n"
            f"{trailer(candidate.source_pr, candidate.content_hash)}"
        ),
    }
    try:
        scan_warnings: list = []
        addressed = scan_fn([pseudo], include_planned=True, warnings=scan_warnings)
    except Exception:  # noqa: BLE001 - never close on uncertainty
        return "unresolvable"
    if scan_warnings:
        # A fetch was unavailable (GitHub outage, thread state unreadable): the
        # empty result means "could not determine", not "not addressed". Fail
        # toward filing with the anchor-unverified marker (AC5-EDGE) rather than
        # minting clean on an unverifiable empty result.
        return "unresolvable"
    return "dead" if addressed else "present"


def _unavailable_prs_from_warnings(warnings: Optional[Iterable[Any]]) -> set:
    """Source-PR numbers flagged unavailable in scan warnings (GitHub outage)."""
    out: set = set()
    for w in warnings or []:
        for m in re.finditer(r"#(\d+)", str(w)):
            out.add(int(m.group(1)))
    return out


def anchor_verdicts(candidates: list, scan_fn: Any) -> dict:
    """Batched filing-time anchor verdict (F10): one ``scan_addressed_findings``
    call for all candidates (each PR fetched once), not one per candidate.

    Returns ``{candidate.source_id: verdict}`` (present | dead | unresolvable). A
    candidate is ``dead`` only if the scan marked ITS finding addressed;
    ``unresolvable`` if its source PR was unavailable (a GitHub outage, read from
    the scan's warnings); ``present`` otherwise. If the scan warned but no PR was
    parseable, fall back to conservative (unresolvable) so an outage never reads
    as present (preserves F8)."""
    verdicts: dict = {}
    scanable = [c for c in candidates if getattr(c, "source_pr", None)]
    if not scan_fn or not scanable:
        return {getattr(c, "source_id", None): "present" for c in candidates}
    pseudos = [
        {
            "id": f"candidate:{c.source_id}",
            "details": (
                f"{c.body or ''}\n"
                f"{trailer(c.source_pr, c.content_hash)}"
            ),
        }
        for c in scanable
    ]
    scan_warnings: list = []
    try:
        addressed = scan_fn(pseudos, include_planned=True, warnings=scan_warnings)
    except Exception:  # noqa: BLE001 - never close on uncertainty
        return {c.source_id: "unresolvable" for c in scanable}
    addressed_ids = {getattr(a, "node_id", None) for a in (addressed or [])}
    unavailable_prs = _unavailable_prs_from_warnings(scan_warnings)
    conservative = bool(scan_warnings) and not unavailable_prs
    for c in scanable:
        if f"candidate:{c.source_id}" in addressed_ids:
            verdicts[c.source_id] = "dead"
        elif c.source_pr in unavailable_prs or conservative:
            verdicts[c.source_id] = "unresolvable"
        else:
            verdicts[c.source_id] = "present"
    return verdicts

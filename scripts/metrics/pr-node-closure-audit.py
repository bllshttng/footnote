#!/usr/bin/env python3
"""pr-node-closure-audit.py - corrected mention classifier (x-59a6 task 1.1).

The 2026-08-15 sweep (x-59a6's own `details`) counted every backlog node
mentioned in a merged PR body AFTER the first as a "secondary" and reported
61 as "not done" - a MENTION count, not a defect count. A king correction the
next day showed most of those were dependency notes, follow-up filings, or
collision notes, never close claims. This script is the auditable rerun:
classify each secondary mention by the sentence or table row that names it -
`close_claim`, `dependency`, `follow_up`, `collision`, or `other` - and report
only `close_claim` as the real defect population.

Read-only: never mutates the graph, never calls `fno backlog`. Runtime
closure binding (`fno.pr.closure`) NEVER imports this module or its
heuristics - the exact `Backlog-Closure:` trailer is the only thing that
closes a node; this script only measures how often prose used to look like
one.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

# Pre-install PYTHONPATH so `python scripts/metrics/pr-node-closure-audit.py`
# works from a bare checkout, mirroring cost-tracker.sh's convention.
_REPO_ROOT = Path(__file__).resolve().parents[2]
_CLI_SRC = _REPO_ROOT / "cli" / "src"
if _CLI_SRC.is_dir() and str(_CLI_SRC) not in sys.path:
    sys.path.insert(0, str(_CLI_SRC))

# Dependency/follow-up/collision language ALWAYS wins over a close verb in the
# same sentence (x-59a6 king correction): "closes both nodes" (PR 836) is a
# claim, but "x-3a91 is blocked_by this" or "branch x-3a91 ... untouched" is
# not, even though a naive scanner sees a plausible-looking verb nearby.
_DEPENDENCY_RE = re.compile(
    r"\b(blocked[_ -]by|depends?[ _]on|dependenc(?:y|ies))\b", re.IGNORECASE
)
_FOLLOWUP_RE = re.compile(r"\b(follow[- ]?ups?|filed|filing)\b", re.IGNORECASE)
_COLLISION_RE = re.compile(r"\b(collisions?|untouched|unmerged)\b", re.IGNORECASE)
_CLOSE_VERB_RE = re.compile(r"\b(close[sd]?|fix(?:es|ed)?|resolve[sd]?)\b", re.IGNORECASE)
_TRAILER_RE = re.compile(r"^Backlog-Closure:[ \t]*(.*)$", re.IGNORECASE | re.MULTILINE)

CLASSIFICATIONS = ("close_claim", "dependency", "follow_up", "collision", "other")


def classify_mention(sentence: str) -> str:
    """Classify ONE sentence/table-row naming a secondary node id."""
    if _DEPENDENCY_RE.search(sentence):
        return "dependency"
    if _FOLLOWUP_RE.search(sentence):
        return "follow_up"
    if _COLLISION_RE.search(sentence):
        return "collision"
    if _CLOSE_VERB_RE.search(sentence):
        return "close_claim"
    return "other"


def split_sentences(text: str) -> list[str]:
    """Split a PR body into sentence/table-row units for per-mention lookup.

    A markdown table row (starts with ``|``) is its own unit; every other
    line is split on sentence-ending punctuation. Not a linguistic parser -
    good enough to isolate the clause naming an id from its neighbors, which
    is all classification needs.
    """
    units: list[str] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        if line.startswith("|"):
            units.append(line)
            continue
        units.extend(s.strip() for s in re.split(r"(?<=[.!?])\s+", line) if s.strip())
    return units


@dataclass
class Mention:
    pr_number: int
    node_id: str
    classification: str
    source_text: str

    def as_dict(self) -> dict:
        return {
            "pr_number": self.pr_number,
            "node_id": self.node_id,
            "classification": self.classification,
            "source_text": self.source_text[:300],
        }


def scan_pr(pr_number: int, body: str, node_ids: set[str]) -> list[Mention]:
    """Every SECONDARY node-id mention in one PR body, classified.

    "Secondary" = every well-formed, graph-real id after the FIRST named in
    body order - the same methodology the original sweep used, so a rerun
    over the same corpus is directly comparable. An id on the exact
    ``Backlog-Closure:`` trailer is always ``close_claim`` regardless of what
    else the body's prose says about it.
    """
    from fno.graph._constants import extract_node_ids

    seen_order: list[str] = []
    for tok in extract_node_ids(body):
        if tok in node_ids and tok not in seen_order:
            seen_order.append(tok)
    if len(seen_order) < 2:
        return []
    secondaries = seen_order[1:]

    trailer_matches = _TRAILER_RE.findall(body)
    trailer_ids = (
        {t for t in trailer_matches[-1].split() if t in node_ids}
        if trailer_matches
        else set()
    )

    units = split_sentences(body)
    mentions: list[Mention] = []
    for nid in secondaries:
        if nid in trailer_ids:
            mentions.append(Mention(
                pr_number, nid, "close_claim",
                f"Backlog-Closure: {' '.join(sorted(trailer_ids))}",
            ))
            continue
        hit = next((u for u in units if nid in u), "")
        mentions.append(Mention(pr_number, nid, classify_mention(hit), hit))
    return mentions


@dataclass
class AuditResult:
    status: str
    corpus_size: int = 0
    unreadable: list[int] = field(default_factory=list)
    prs_with_secondaries: int = 0
    counts: dict = field(default_factory=lambda: {c: 0 for c in CLASSIFICATIONS})
    mentions: list[Mention] = field(default_factory=list)
    x_b28b_check: Optional[dict] = None
    error: Optional[str] = None

    def as_dict(self, *, specimen_cap: int) -> dict:
        specimens: dict = {c: [] for c in CLASSIFICATIONS}
        for m in self.mentions:
            bucket = specimens[m.classification]
            if len(bucket) < specimen_cap:
                bucket.append(m.as_dict())
        payload = {
            "status": self.status,
            "corpus_size": self.corpus_size,
            "unreadable": self.unreadable,
            "prs_with_secondaries": self.prs_with_secondaries,
            "counts": self.counts,
            "specimens": specimens,
        }
        if self.x_b28b_check is not None:
            payload["x_b28b_check"] = self.x_b28b_check
        if self.error is not None:
            payload["error"] = self.error
        return payload


def run_audit(prs: list[dict], node_ids: set[str]) -> AuditResult:
    """Pure aggregation over an already-fetched PR corpus. No I/O.

    AC1-ERR: a PR whose body is unreadable (missing/None, not merely empty -
    an empty body is a real PR with nothing to say) is recorded in
    ``unreadable`` rather than silently dropped from ``corpus_size`` - the
    denominator must never shrink to hide a fetch gap.
    """
    result = AuditResult(status="complete", corpus_size=len(prs))
    for pr in prs:
        number = pr.get("number")
        body = pr.get("body")
        if not isinstance(number, int):
            continue
        if body is None:
            result.unreadable.append(number)
            continue
        mentions = scan_pr(number, body or "", node_ids)
        if mentions:
            result.prs_with_secondaries += 1
            result.mentions.extend(mentions)
            for m in mentions:
                result.counts[m.classification] += 1

    x_b28b_prs = {m.pr_number: m.classification for m in result.mentions
                  if m.node_id == "x-b28b" and m.pr_number in (620, 740)}
    if x_b28b_prs:
        result.x_b28b_check = x_b28b_prs

    if result.unreadable:
        result.status = "complete_with_unreadable"
    return result


def _fetch_corpus(limit: int) -> list[dict]:
    from fno.graph._reconcile import fetch_recent_merged_prs

    return fetch_recent_merged_prs(limit=limit)


def _live_node_ids() -> set[str]:
    from fno.graph.store import read_graph
    from fno.paths import graph_json

    entries = read_graph(graph_json())
    return {e["id"] for e in entries if isinstance(e, dict) and isinstance(e.get("id"), str)}


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--limit", type=int, default=2000,
        help="Max merged PRs to fetch (default 2000 - covers this repo's full history).",
    )
    parser.add_argument("--json", action="store_true", help="Emit JSON (default: human summary).")
    parser.add_argument(
        "--specimen-cap", type=int, default=10,
        help="Max example mentions kept per classification in the output.",
    )
    args = parser.parse_args(argv)

    from fno.graph._reconcile import ReconcileError

    try:
        prs = _fetch_corpus(args.limit)
    except ReconcileError as exc:
        payload = {"status": "error", "error": f"could not fetch merged PRs: {exc}"}
        print(json.dumps(payload, indent=2) if args.json else payload["error"], file=sys.stderr)
        return 1

    try:
        node_ids = _live_node_ids()
    except Exception as exc:  # noqa: BLE001 - a graph read failure must not crash silently
        payload = {"status": "error", "error": f"could not read the live graph: {exc}"}
        print(json.dumps(payload, indent=2) if args.json else payload["error"], file=sys.stderr)
        return 1

    result = run_audit(prs, node_ids)
    payload = result.as_dict(specimen_cap=args.specimen_cap)

    if args.json:
        print(json.dumps(payload, indent=2))
    else:
        print(f"pr-node-closure-audit: {payload['corpus_size']} PR(s) scanned, "
              f"{payload['prs_with_secondaries']} with secondary node mentions")
        if payload["unreadable"]:
            print(f"  unreadable bodies: {payload['unreadable']}")
        for c in CLASSIFICATIONS:
            print(f"  {c}: {payload['counts'][c]}")
        if "x_b28b_check" in payload:
            print(f"  x-b28b check: {payload['x_b28b_check']}")
        print(f"defect population (close_claim only): {payload['counts']['close_claim']}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

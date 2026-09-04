"""Read-only discovery over the backlog's two recall lanes.

FTS5 supplies vocabulary recall.  Relatedness supplies the calibrated score
and the domain signal used by filing.  This module joins both observations
without creating another store or changing graph state.
"""
from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterable

from fno.graph import fts, relatedness


@dataclass(frozen=True)
class Candidate:
    """One possible match and the recall lanes that found it."""

    node_id: str
    score: float
    lanes: frozenset[str]
    reason: str = ""

    @property
    def id(self) -> str:
        """Short alias used by renderers that already call ids ``id``."""
        return self.node_id

    def as_dict(self) -> dict[str, Any]:
        return {
            "id": self.node_id,
            "node_id": self.node_id,
            "score": self.score,
            "lanes": sorted(self.lanes),
            "evidence": self.reason,
        }


class CandidateResults(list[Candidate]):
    """List-compatible result carrying an explicit FTS degradation receipt."""

    def __init__(
        self,
        values: Iterable[Candidate] = (),
        *,
        degraded: bool = False,
        warning: str | None = None,
    ) -> None:
        super().__init__(values)
        self.degraded = degraded
        self.warning = warning


@dataclass(frozen=True)
class Assessment:
    """A human-reviewable verdict with evidence or an explicit reason."""

    verdict: str
    evidence: list[str]
    reason: str = ""

    def as_dict(self) -> dict[str, Any]:
        return {
            "verdict": self.verdict,
            "evidence": list(self.evidence),
            "reason": self.reason,
        }


def _graph_path() -> Path:
    from fno.graph._constants import GRAPH_JSON

    return GRAPH_JSON


def _entries_for(graph_path: Path) -> list[dict[str, Any]]:
    from fno.graph.store import read_graph

    return [entry for entry in read_graph(graph_path) if isinstance(entry, dict)]


def candidates(
    title: str,
    details: str,
    *,
    limit: int = 20,
    entries: list[dict[str, Any]] | None = None,
    graph_path: Path | None = None,
    exclude_id: str | None = None,
    token_cache: dict[str, frozenset[str]] | None = None,
    domain: str = "code",
) -> CandidateResults:
    """Union FTS5 and relatedness recall, ranked by relatedness score.

    ``entries`` is an optional narrowed pool for callers such as the filing
    gate.  The FTS cache still searches the graph bytes, then ids are filtered
    to that pool.  Relatedness is allowed below the filing floor so an FTS-only
    vocabulary hit remains visible with its measured score.  ``domain`` is the
    incoming node's own domain: relatedness grants a same-domain bonus, so a
    caller that knows it must pass it rather than let every query read as
    ``code``.
    """
    if limit < 1:
        return CandidateResults()
    path = graph_path or _graph_path()
    pool = entries if entries is not None else _entries_for(path)
    pool = [
        entry
        for entry in pool
        if isinstance(entry.get("id"), str) and entry.get("id") != exclude_id
    ]
    by_id = {entry["id"]: entry for entry in pool}
    incoming = {
        "id": "__incoming__",
        "title": title or "",
        "details": details or "",
        "domain": domain,
    }

    fts_ids: list[str] = []
    degraded = False
    warning: str | None = None
    try:
        fts_ids = [node_id for node_id in fts.search(
            " ".join(part for part in (title, details) if part), path, limit=None
        ) if node_id in by_id]
    except fts.SearchUnavailableError as exc:
        degraded = True
        warning = str(exc)

    related_kwargs: dict[str, Any] = {"k": None}
    if token_cache is not None:
        related_kwargs["token_cache"] = token_cache
    related_rows = relatedness.similar_nodes(incoming, pool, **related_kwargs)
    related_by_id = {
        node_id: (score, reason)
        for node_id, score, reason in related_rows
        if node_id in by_id
    }
    fts_set = set(fts_ids)
    related_set = set(related_by_id)
    all_ids = fts_set | related_set
    result: list[Candidate] = []
    for node_id in all_ids:
        if node_id in related_by_id:
            score, reason = related_by_id[node_id]
        else:
            score, reason = relatedness.score_pair(
                incoming, by_id[node_id], include_epic=False
            )
        lanes = frozenset(
            lane
            for lane, present in (("fts", node_id in fts_set), ("relatedness", node_id in related_set))
            if present
        )
        result.append(Candidate(node_id, score, lanes, reason))
    result.sort(key=lambda candidate: (-candidate.score, candidate.node_id))
    return CandidateResults(result[:limit], degraded=degraded, warning=warning)


def positive_control(
    query: str,
    *,
    graph_path: Path | None = None,
    entries: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Run a known-positive query so an empty result is not trusted blindly."""
    path = graph_path or _graph_path()
    pool = entries if entries is not None else _entries_for(path)
    try:
        matches = [node_id for node_id in fts.search(query, path, limit=None)]
        return {"query": query, "matches": matches, "lane": "fts", "degraded": False}
    except fts.SearchUnavailableError as exc:
        lowered = query.casefold()
        matches = [
            entry["id"]
            for entry in pool
            if isinstance(entry.get("id"), str)
            and lowered in " ".join(
                str(entry.get(field) or "") for field in ("title", "slug", "details")
            ).casefold()
        ]
        return {
            "query": query,
            "matches": matches,
            "lane": "substring",
            "degraded": True,
            "warning": str(exc),
        }


_PATH_RE = re.compile(r"(?<![A-Za-z0-9_])(?:[A-Za-z0-9_.-]+/)+[A-Za-z0-9_.-]+")


def _file_evidence(node: dict[str, Any]) -> list[str]:
    text = " ".join(
        value for field in ("details", "title")
        if isinstance((value := node.get(field)), str)
    )
    evidence: list[str] = []
    for raw in _PATH_RE.findall(text):
        path = Path(raw)
        if path.is_file():
            evidence.append(raw)
    return evidence


def _stale_reason_still_true(node: dict[str, Any]) -> bool:
    from fno.graph.maintain import _STALE_IDEAS_DEFERRED_REASON_RE

    reason = node.get("deferred_reason")
    deferred_at = node.get("deferred_at")
    if (
        not isinstance(reason, str)
        or not _STALE_IDEAS_DEFERRED_REASON_RE.match(reason)
        or not isinstance(deferred_at, str)
    ):
        return False
    try:
        parked = datetime.fromisoformat(deferred_at.replace("Z", "+00:00"))
    except ValueError:
        return False
    if parked.tzinfo is None:
        parked = parked.replace(tzinfo=timezone.utc)
    age_days = (datetime.now(timezone.utc) - parked).total_seconds() / 86400
    return age_days >= 30


def assess(
    node: dict[str, Any],
    cands: Iterable[Candidate],
    pr_state: Callable[[int], bool] | None = None,
) -> Assessment:
    """Assess one node without changing it or making an external mutation.

    ``pr_state`` is the caller's gh-verified answer for whether a PR number is
    merged.  Deferral clears every node-side completion field, so a shipped
    node that later expired is only provable through evidence that survives
    the defer: a verified merged PR, or files recorded in its text that still
    exist on disk.
    """
    candidates_list = list(cands)
    pr_number = node.get("pr_number")
    has_pr = isinstance(pr_number, int) and not isinstance(pr_number, bool)
    if isinstance(pr_number, int) and not isinstance(pr_number, bool):
        has_pr = True
        if (
            node.get("status") == "done"
            or node.get("completed_at")
            or node.get("merged_at")
            or node.get("pr_merged") is True
            or (pr_state is not None and pr_state(pr_number))
        ):
            return Assessment("satisfied", [f"PR#{pr_number}"], "merged PR recorded on node")

    file_evidence = _file_evidence(node)
    if file_evidence and (node.get("status") == "done" or has_pr):
        return Assessment("satisfied", file_evidence, "existing file recorded by node")

    if candidates_list:
        top = candidates_list[0]
        return Assessment(
            "duplicate",
            [top.node_id],
            f"candidate surfaced by {', '.join(sorted(top.lanes))}",
        )

    if _stale_reason_still_true(node):
        return Assessment(
            "still_real",
            [f"deferred reason: {node.get('deferred_reason')}"],
            "the exact expiry condition remains true",
        )

    return Assessment(
        "undecided",
        [],
        "no duplicate or completion evidence, and the original condition is not provable",
    )

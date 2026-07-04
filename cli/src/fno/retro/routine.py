"""The one shared retro-triage routine: harvest -> classify -> dedup -> land.

Both triggers (the universal retro-sentinel consumer and the /target ship-gate
fast-path) call this, so both paths produce one deduped node set even when they
fire for the same PR. IO is injectable (comments, existing nodes, landing fns)
so the routine is unit-testable without a live gh/graph.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable, Optional

from fno.retro import harvest as _harvest
from fno.retro.classify import classify
from fno.retro.dedup import dedup_candidates, existing_keys_from_nodes
from fno.retro.land import LandResult, land_candidates
from fno.retro.types import Candidate


@dataclass
class TriageReport:
    pr_number: int
    source_counts: dict = field(default_factory=dict)
    results: list[LandResult] = field(default_factory=list)
    skipped_dupes: int = 0
    uncited: list[Candidate] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)
    # Set when the review-comment fetch could not fully read the source (gh
    # missing/auth/rate-limit OR unparseable output). A semantic flag, NOT a
    # warning-string scan, so a reworded warning can't silently flip retention.
    gh_unavailable: bool = False
    # Carve-out ids harvested this run; consumed (removed from the ledger) by the
    # caller on a clean land so they are never re-filed under a later PR.
    harvested_carveout_ids: list[str] = field(default_factory=list)
    # Count of carve-outs surfaced READ-ONLY this run (x-90b8): an explicit
    # `--pr-number` harvest that resolves no owning session must never file or
    # consume cross-session carve-outs under that PR. They are listed for the
    # operator but neither landed nor consumed.
    readonly_carveout_count: int = 0

    @property
    def partial(self) -> bool:
        """A harvest source was incomplete (e.g. gh down) - caller RETAINS the sentinel."""
        return self.gh_unavailable

    @property
    def landed_any(self) -> bool:
        return any(r.outcome in ("active", "queued", "inbox") for r in self.results)

    @property
    def failed(self) -> bool:
        return any(r.outcome == "failed" for r in self.results)

    @property
    def nothing_to_do(self) -> bool:
        return not self.results and not self.uncited


def triage_pr(
    *,
    repo_root: Path,
    pr_number: int,
    mode: str,
    project: Optional[str] = None,
    cwd: Optional[str] = None,
    session_ids: Optional[list[str]] = None,
    completion_md: Optional[Path] = None,
    comments: Optional[list[dict]] = None,
    resolved_ids: Optional[set[str]] = None,
    skipped_ids: Optional[set[str]] = None,
    commit_dates: Optional[list[str]] = None,
    author_login: Optional[str] = None,
    existing_nodes: Optional[list[dict]] = None,
    gh_runner: Optional[Callable] = None,
    repo: Optional[str] = None,
    create_fn=None,
    inbox_fn=None,
    carveout_root: Optional[Path] = None,
    carveouts_readonly: bool = False,
    origin_node_id: Optional[str] = None,
) -> TriageReport:
    warnings: list[str] = []

    # The carve-out ledger lives under the CANONICAL root (see
    # carveout.core.resolve_carveout_root). ``carveout_root`` carries it from
    # the production caller; it defaults to ``repo_root`` so tests that pass a
    # single tmp root stay hermetic (ab-44408b6e).
    carveouts = _harvest.harvest_carveouts(
        carveout_root or repo_root,
        session_ids=session_ids,
        source_pr=pr_number,
        warnings=warnings,
    )

    # x-90b8: when an explicit `--pr-number` harvest resolves NO owning session,
    # the carve-out source is READ-ONLY. A carve-out is session-scoped (an
    # unrelated session's deferred work), so stamping every unconsumed carve-out
    # onto an arbitrary PR and consuming it mis-attributes cross-session work
    # under the wrong lineage (cv-0932fa60 -> PR #522). Drop them from the
    # land/consume path; surface the count so the operator still sees them.
    # Reviews + COMPLETION are inherently PR-scoped, so they are untouched.
    readonly_carveout_count = 0
    if carveouts_readonly:
        readonly_carveout_count = len(carveouts)
        carveouts = []

    gh_unavailable = False
    if comments is None:
        kwargs = {"repo": repo, "warnings": warnings}
        if gh_runner is not None:
            kwargs["gh_runner"] = gh_runner
        before = len(warnings)
        comments = _harvest.fetch_review_comments(pr_number, **kwargs)
        # Any warning the fetch added (failed OR unparseable) means the review
        # source is incomplete -> retain the sentinel. Matches "gh api" broadly
        # rather than one exact phrase.
        gh_unavailable = any("gh api" in w for w in warnings[before:])
    reviews = _harvest.harvest_reviews(
        comments=comments,
        resolved_ids=resolved_ids,
        skipped_ids=skipped_ids,
        commit_dates=commit_dates,
        author_login=author_login,
        source_pr=pr_number,
        warnings=warnings,
    )

    deferred = []
    if completion_md is not None:
        deferred = _harvest.harvest_deferred_findings(
            completion_md, source_pr=pr_number, warnings=warnings
        )

    source_counts = {
        "carveouts": len(carveouts),
        "reviews": len(reviews),
        "deferred_findings": len(deferred),
    }

    raw = carveouts + reviews + deferred
    cited, uncited = classify(raw)

    existing_keys = existing_keys_from_nodes(existing_nodes or [])
    kept, skipped = dedup_candidates(cited, existing_keys=existing_keys)

    # Auto caused_by (W4 causal links, AC4-UI): a follow-up filed from this
    # PR's findings points back at the node that shipped the PR. Prefer the
    # trigger sentinel's node_id; fall back to the graph node carrying this
    # pr_number - repo-scoped when ``repo`` is known, because the graph is
    # global and bare PR numbers collide across projects. Unresolvable ->
    # nodes land without the link (manual `backlog update --caused-by`
    # remains).
    caused_by = origin_node_id
    # pr_number 0 is the synthetic-path placeholder (`int(... or 0)` upstream);
    # it must never match a node. Fail closed without repo context: the graph
    # is global, so a bare-number match against it can cross projects. And an
    # ambiguous same-repo match (two nodes on one number) links nothing - the
    # same exactly-one-or-nothing rule as revert detection.
    if not caused_by and repo and isinstance(pr_number, int) and pr_number > 0:
        from fno.graph._reconcile import repo_slug_from_url

        def _same_repo(n: dict) -> bool:
            slug = repo_slug_from_url(n.get("pr_url"))
            return slug is not None and slug.lower() == repo.lower()

        hits = {
            n.get("id")
            for n in existing_nodes or []
            if n.get("pr_number") == pr_number and _same_repo(n)
        }
        caused_by = hits.pop() if len(hits) == 1 else None

    results = land_candidates(
        kept,
        mode=mode,
        repo_root=repo_root,
        project=project,
        # Canonical node cwd threaded from retro.cli (ab-b4da4664); land_candidates
        # falls back to repo_root only when this is None (direct callers/tests).
        cwd=cwd,
        create_fn=create_fn,
        inbox_fn=inbox_fn,
        caused_by=caused_by,
    )

    return TriageReport(
        pr_number=pr_number,
        source_counts=source_counts,
        harvested_carveout_ids=[c.source_id for c in carveouts],
        readonly_carveout_count=readonly_carveout_count,
        results=results,
        skipped_dupes=len(skipped),
        uncited=uncited,
        warnings=warnings,
        gh_unavailable=gh_unavailable,
    )

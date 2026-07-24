"""The one shared retro-triage routine: harvest -> classify -> dedup -> land.

Both triggers (the universal retro-sentinel consumer and the /target ship-gate
fast-path) call this, so both paths produce one deduped node set even when they
fire for the same PR. IO is injectable (comments, existing nodes, landing fns)
so the routine is unit-testable without a live gh/graph.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Optional

if TYPE_CHECKING:
    from fno.retro.keep_going import FollowupResult

from fno.retro import harvest as _harvest
from fno.retro.classify import (
    DISPOSITION_ARCHIVE,
    classify,
    classify_postmortem,
)
from fno.retro.dedup import dedup_candidates, existing_keys_from_nodes
from fno.retro.reconcile_findings import scan_addressed_findings
from fno.retro.land import (
    MODE_AUTONOMOUS,
    MODE_INTERACTIVE,
    LandResult,
    land_candidates,
)
from fno.retro.types import KIND_CARVEOUT, Candidate


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
    # Autonomous keep-going follow-up dispatches (x-3360): one FollowupResult per
    # landed carve-out node the engine classified (think/build dispatched, or
    # file-only / capped). Empty unless the engine ran (autonomous mode +
    # config.keep_going.enabled).
    followups: "list[FollowupResult]" = field(default_factory=list)

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
        kwargs: dict[str, Any] = {"repo": repo, "warnings": warnings}
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
        anchor_scan_fn=scan_addressed_findings,
    )

    # Autonomous keep-going engine (x-3360): after the carve-out follow-ups are
    # filed as nodes, classify each and dispatch the next unit of work under the
    # shared per-day firehose ceiling. Autonomous mode only (interactive queues
    # nodes for a human ack; auto-dispatch would bypass it) and gated OFF by
    # default. Strictly non-fatal: a failure here never sinks the harvest.
    followups: list = []
    if mode == MODE_AUTONOMOUS:
        try:
            from fno.retro.keep_going import dispatch_followups, keep_going_enabled

            if keep_going_enabled(project_root=repo_root):
                carveout_landed = [
                    r
                    for r in results
                    if r.node_id
                    and r.candidate.extra.get("kind") == KIND_CARVEOUT
                ]
                followups = dispatch_followups(
                    carveout_landed, project_root=repo_root, cwd=cwd
                )
        except Exception as exc:  # noqa: BLE001 - additive; never wedge the harvest
            warnings.append(f"keep-going engine skipped (non-fatal): {exc}")

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
        followups=followups,
    )


# ── postmortem pass (W6 6.2, x-f063) ─────────────────────────────────────────

@dataclass
class PostmortemReport:
    harvested: int = 0
    # source_id -> "node" | "inbox" | "archived" | "dupe" | "failed"
    dispositions: dict = field(default_factory=dict)
    results: list[LandResult] = field(default_factory=list)
    stamped: int = 0
    warnings: list[str] = field(default_factory=list)

    @property
    def failed(self) -> bool:
        return any(d == "failed" for d in self.dispositions.values())


def triage_postmortems(
    *,
    postmortems_dir: Path,
    repo_root: Path,
    mode: str = MODE_INTERACTIVE,
    project: Optional[str] = None,
    cwd: Optional[str] = None,
    existing_nodes: Optional[list[dict]] = None,
    create_fn=None,
    inbox_fn=None,
    stamp_fn: Optional[Callable[[Path], None]] = None,
) -> PostmortemReport:
    """Drain unconsumed postmortems: harvest -> rule-first disposition ->
    dedup -> land -> stamp ``consumed_at``.

    Ordering is the AC6-FR invariant: the stamp lands AFTER the disposition,
    so an interrupted run re-processes only the not-yet-stamped tail. A stamp
    failure after a successful land is degraded-not-broken: the node tier
    collapses the re-proposal via the widened dedup trailer (source_pr=None);
    the inbox tier via add_item's own (where, title) pre-check.
    """
    report = PostmortemReport()
    items = _harvest.harvest_postmortems(postmortems_dir, warnings=report.warnings)
    report.harvested = len(items)
    if not items:
        return report

    stamp = stamp_fn or _harvest.stamp_postmortem_consumed
    seen = existing_keys_from_nodes(existing_nodes or [])

    def _stamp(source_id: str) -> None:
        path = postmortems_dir / source_id.split(":", 1)[1]
        try:
            stamp(path)
            report.stamped += 1
        except Exception as exc:  # degraded: dedup collapses the re-proposal
            report.warnings.append(f"{source_id}: consumed_at stamp failed: {exc}")

    for item in items:
        disposition, candidate = classify_postmortem(item)
        if disposition == DISPOSITION_ARCHIVE:
            # One-off: no work filed, but the entry is still consumed.
            report.dispositions[item.source_id] = "archived"
            _stamp(item.source_id)
            continue

        if candidate is None:
            continue

        kept, _skipped = dedup_candidates([candidate], existing_keys=seen)
        if not kept:
            # Landed by a prior run whose stamp failed: consume, don't re-file.
            report.dispositions[item.source_id] = "dupe"
            _stamp(item.source_id)
            continue
        seen.add(f"{kept[0].source_pr}:{kept[0].content_hash}")

        results = land_candidates(
            kept,
            mode=mode,
            repo_root=repo_root,
            project=project,
            cwd=cwd,
            create_fn=create_fn,
            inbox_fn=inbox_fn,
            anchor_scan_fn=scan_addressed_findings,
        )
        report.results.extend(results)
        outcome = results[0].outcome if results else "failed"
        if outcome == "failed":
            # Not stamped: the unconsumed entry retries next run (AC6-FR).
            report.dispositions[item.source_id] = "failed"
            continue
        if outcome == "skipped":
            report.dispositions[item.source_id] = "skipped"
        else:
            report.dispositions[item.source_id] = "node" if outcome in ("active", "queued") else "inbox"
        _stamp(item.source_id)

    return report

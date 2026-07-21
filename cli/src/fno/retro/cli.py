"""`fno retro` - consume retro-triage triggers and file left-out work.

`run` is the PRIMARY/universal trigger consumer: it reads the retro-pending
sentinels that `fno backlog reconcile` drops (for PRs merged outside the ship
gate - covers /goal and manual merges, not just /target) and runs the one shared
harvest->classify->dedup->land routine for each. A sentinel is removed ONLY after
a successful land, so a crash mid-run re-triages rather than dropping work; a
partial harvest (gh down) retains the sentinel for retry.
"""
from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Callable, Optional

import typer

from fno.retro import harvest as _harvest
from fno.retro.land import resolve_mode
from fno.retro.routine import TriageReport, triage_pr

retro_app = typer.Typer(
    no_args_is_help=True,
    help="Consume retro-triage triggers; file left-out work as backlog nodes.",
)

_PR_URL_RE = re.compile(r"github\.com/(?P<owner>[^/]+)/(?P<repo>[^/]+)/pull/\d+")


def _repo_slug_from_url(pr_url: Optional[str]) -> Optional[str]:
    if not pr_url:
        return None
    m = _PR_URL_RE.search(pr_url)
    return f"{m.group('owner')}/{m.group('repo')}" if m else None


def _sentinel_pr_number(path: Path) -> Optional[int]:
    """Best-effort ``pr_number`` from a sentinel / fast-path payload.

    Returns None on any failure (unreadable, malformed, missing field) so the
    --pr scope filter degrades to "not this PR" rather than crashing the run.
    """
    try:
        n = int(json.loads(path.read_text(encoding="utf-8")).get("pr_number") or 0)
    except Exception:
        return None
    return n or None


def _sentinel_repo_matches(path: Path, want_slug: Optional[str]) -> bool:
    """Whether a sentinel belongs to ``want_slug`` (owner/name).

    True when the repo cannot be constrained (``want_slug`` is None) or the
    sentinel carries no ``pr_url`` repo hint, so the --pr filter never drops a
    sentinel solely for lacking repo data; otherwise requires a case-insensitive
    slug match. Guards against draining a same-numbered sentinel from another
    repo (codex P2), since GitHub PR numbers are unique only within a repo.
    """
    if not want_slug:
        return True
    try:
        pr_url = json.loads(path.read_text(encoding="utf-8")).get("pr_url")
    except Exception:
        return True  # unreadable repo hint -> don't drop on repo grounds
    sentinel_slug = _repo_slug_from_url(pr_url)
    if sentinel_slug is None:
        return True
    return sentinel_slug.lower() == want_slug.lower()


def _resolve_pr_session_ids(
    ledger_path: Path, pr: int, repo_slug: Optional[str] = None
) -> list[str]:
    """Session id(s) whose ledger entry owns this PR, scoped to ``repo_slug``.

    Mirrors the post-merge Step 4b ledger scan (skills/pr/references/merged.md). Because
    ``ledger.json`` is GLOBAL and GitHub PR numbers collide across repos, a known
    ``repo_slug`` is REQUIRED to attribute any entry: an entry matches when its
    ``pr_url`` ends in ``/<slug>/pull/<pr>``. A url-less entry names no repo, so
    it attributes nothing and never matches. When ``repo_slug`` is None - the repo
    could not be resolved (no ``--repo``, ``gh`` down, or run outside a checkout)
    - ownership CANNOT be confirmed, so the scan returns ``[]`` and the caller
    falls through to the read-only path rather than risk consuming a same-numbered
    foreign PR's carve-outs (codex P2).

    Returns ``[]`` on any failure (missing/unreadable/malformed ledger, no repo
    scope, no match), so the caller treats "no owning session" as the read-only
    case (x-90b8) rather than crashing the harvest. The join itself lives in
    :mod:`fno.ledger_join` (shared with ``fno carveout list --pr-number``); this
    wrapper keeps retro's flatten-to-empty contract.
    """
    from fno.ledger_join import reason_is_infra_failure, resolve_pr_sessions

    sessions, reason = resolve_pr_sessions(ledger_path, pr, repo_slug)
    # Flattening to [] hides WHY. A benign no-match is the designed read-only
    # case and stays quiet; a broken environment must not read as "no owning
    # session", or nobody ever investigates it.
    if reason_is_infra_failure(reason):
        typer.echo(f"WARN {reason}", err=True)
    return sessions


def _completion_md_for(plan_path: Optional[str], repo_root: Path) -> Optional[Path]:
    """Resolve a plan's COMPLETION.md (folder plan -> inside; single-file -> sidecar).

    A relative ``plan_path`` (sentinels commonly store repo-relative paths) is
    resolved against ``repo_root``, NOT the process CWD - otherwise running
    ``fno retro run`` from a subdirectory silently misses COMPLETION.md.
    """
    if not plan_path:
        return None
    p = Path(plan_path)
    if not p.is_absolute():
        p = repo_root / p
    if p.is_dir():
        cand = p / "COMPLETION.md"
        return cand if cand.exists() else None
    sidecar = Path(str(p) + ".artifacts") / "COMPLETION.md"
    return sidecar if sidecar.exists() else None


def _emit_report(report: TriageReport, *, mode: str) -> None:
    """AC2-UI source counts (stderr) + AC4-UI per-node lines (stdout)."""
    sc = report.source_counts
    typer.echo(
        f"(triaging PR #{report.pr_number}: carve-outs={sc.get('carveouts', 0)} "
        f"reviews={sc.get('reviews', 0)} deferred={sc.get('deferred_findings', 0)})",
        err=True,
    )
    for w in report.warnings:
        typer.echo(f"WARN {w}", err=True)

    if report.readonly_carveout_count:
        typer.echo(
            f"(PR #{report.pr_number}: {report.readonly_carveout_count} carve-out(s) "
            "shown read-only - no owning session resolved for this PR, so "
            "they were NOT filed or consumed under this PR)",
            err=True,
        )

    queued_any = False
    for r in report.results:
        if r.outcome == "active":
            typer.echo(f"created {r.node_id} (active): {r.candidate.title}")
        elif r.outcome == "queued":
            queued_any = True
            typer.echo(f"queued {r.node_id} for review: {r.candidate.title}")
        elif r.outcome == "inbox":
            typer.echo(f"inbox line (nit): {r.candidate.title}")
        elif r.outcome == "failed":
            typer.echo(f"FAILED to land: {r.candidate.title} ({r.error})", err=True)

    for f in getattr(report, "followups", None) or []:
        if f.outcome == "dispatched":
            typer.echo(f"keep-going: dispatched {f.arm} for {f.node_id}")
        elif f.outcome == "failed":
            typer.echo(
                f"keep-going: {f.arm} dispatch failed for {f.node_id} "
                f"(left as a filed node)",
                err=True,
            )
        # 'filed'/'capped' already visible via the created-node line / the single
        # cap-reached line dispatch_followups prints; no per-node echo needed.

    if report.uncited:
        typer.echo(
            f"skipped {len(report.uncited)} uncited candidate(s) "
            f"(triage_uncited_candidate; no source cite)",
            err=True,
        )
    if report.nothing_to_do:
        typer.echo(f"(PR #{report.pr_number}: no left-out work to triage)")
    if queued_any:
        typer.echo("run `fno backlog pick` to review queued items.")


def _current_repo_slug(gh_runner: Optional[Callable] = None) -> Optional[str]:
    """Best-effort ``owner/name`` of the current repo via ``gh repo view``.

    Used by the synthetic ``--pr`` harvest path to resolve the repo for review
    harvesting when the caller did not pass ``--repo``. ``None`` on any failure
    (gh missing/unauthed, not a repo); the carve-out harvest still runs - only
    review harvesting needs the slug.
    """
    runner = gh_runner or _harvest._default_gh_runner
    try:
        rc, out, _err = runner(
            ["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"]
        )
    except Exception:
        # Best-effort, per the docstring: the default runner already maps a
        # missing gh to rc=127, but a custom runner could raise - a slug lookup
        # must never crash `fno retro run` (gemini MEDIUM on PR #405).
        return None
    slug = out.strip() if rc == 0 else ""
    return slug or None


def _process_payload(
    payload: dict,
    *,
    repo_root: Path,
    existing_nodes: list[dict],
    comments: Optional[list[dict]] = None,
    resolved_ids: Optional[set[str]] = None,
    skipped_ids: Optional[set[str]] = None,
    gh_runner: Optional[Callable] = None,
    create_fn=None,
    inbox_fn=None,
    carveout_root: Optional[Path] = None,
    current_repo_slug: Optional[str] = None,
    node_root: Optional[Path] = None,
    ledger_path: Optional[Path] = None,
) -> "tuple[TriageReport, bool]":
    """Triage one payload, from a sentinel file or a synthetic ``--pr`` request.

    Returns ``(report, clean)`` where ``clean`` is True iff the harvest was
    complete and the land fully succeeded (no partial harvest, no failed land,
    no unreadable resolved-thread state). The caller removes the trigger (the
    sentinel file) only when clean. Carve-outs processed on a clean run are
    CONSUMED here - for both the sentinel path and the synthetic ``--pr`` path -
    so they are never re-harvested / re-filed under a later PR.
    """
    pr_number = int(payload.get("pr_number") or 0)
    mode = resolve_mode(payload)
    repo_slug = _repo_slug_from_url(payload.get("pr_url"))
    completion_md = _completion_md_for(payload.get("plan_path"), repo_root)
    # The carve-out ledger is canonical-rooted (carveout.core.resolve_carveout_root);
    # default to repo_root so direct callers/tests passing one root stay hermetic.
    cr = carveout_root if carveout_root is not None else repo_root
    # The node's durable home (cwd) + project attribution must root at CANONICAL,
    # not the worktree: filed nodes outlive the worktree they were captured in
    # (archive-worktree.sh tears it down), and detect_project_from_settings only
    # matches canonical roots, never worktree paths (ab-b4da4664). repo_root stays
    # the worktree for finding worktree-local artifacts (COMPLETION.md above, the
    # .triage-pending fast-path in run()). Defaults to repo_root for direct
    # callers/tests passing a single root, exactly like carveout_root.
    eff_node_root = node_root if node_root is not None else repo_root
    # Scope carve-out harvest to the originating session(s) when the payload
    # carries them (the .triage-pending fast-path and --session do), so old
    # carve-outs from unrelated sessions are never re-filed under this PR.
    # Absent -> None (the consume-on-clean-run step below is the backstop that
    # bounds re-filing).
    session_ids = None
    if payload.get("session_ids"):
        session_ids = [str(s) for s in payload["session_ids"]]
    elif payload.get("session_id"):
        session_ids = [str(payload["session_id"])]

    # x-90b8: run()'s synthetic --pr-number path sets this when it could resolve
    # NO owning session for the PR (a manual / hotfix merge with no session<->PR
    # ledger link). Carve-outs are session-scoped, so an unscoped harvest under
    # an arbitrary PR mis-attributes another session's deferred work; triage_pr
    # then surfaces them read-only instead of filing/consuming them.
    carveouts_readonly = bool(payload.get("carveouts_readonly"))

    # x-23c0: a reconcile-dropped sentinel (fno backlog reconcile, for PRs merged
    # outside the ship gate) often carries NO session scoping, unlike the
    # .triage-pending fast-path / --session. Without resolving the PR's owning
    # session(s) here, harvest_carveouts runs with session_ids=None and DRAINS the
    # whole shared ledger, stamping another in-flight session's carve-outs onto
    # this PR (cv-5e4b9f4d, recorded in #123's session, harvested by #121). Resolve
    # the owning session(s) from the GLOBAL ledger, repo-scoped, exactly like
    # run()'s synthetic --pr-number path; no owner resolves -> read-only (x-90b8).
    # Plural by design (list return): a batch PR owns MULTIPLE member sessions and
    # must harvest all of them. The synthetic path pre-sets one of these fields in
    # the payload, so this fallback never re-runs for it.
    #
    # Fail-SAFE: this fires on the exact drain precondition (session_ids is None
    # and not already read-only), so an unscoped harvest is structurally
    # unreachable from here. A missing ledger_path (an internal caller that did
    # not thread it) resolves to no owner -> read-only, NOT to the old drain -
    # the guard must never fail open on the very footgun it exists to close.
    # `pr_number` is already `int(... or 0)` (top of this fn), so a missing/invalid
    # PR resolves to 0. Guard `> 0` before the ledger scan: a 0 would match a
    # placeholder/unassigned ledger row (`pr: 0`) and wrongly scope to it; with no
    # resolvable PR the safe answer is read-only, not a guessed owner (gemini).
    if session_ids is None and not carveouts_readonly:
        resolved = (
            _resolve_pr_session_ids(ledger_path, pr_number, repo_slug)
            if ledger_path is not None and pr_number > 0
            else []
        )
        if resolved:
            session_ids = resolved
        else:
            carveouts_readonly = True

    # Derive resolved/skipped finding sets from REAL PR data before harvesting
    # reviewer comments. Without this every comment becomes a candidate, so an
    # already-implemented finding gets re-filed (ab-bb7fa74f). Only runs on the
    # live path (comments is None); tests inject `comments` (and optionally
    # resolved_ids/skipped_ids) and skip derivation for determinism.
    derive_warnings: list[str] = []
    resolved_unavailable = False
    if comments is None:
        runner = gh_runner or _harvest._default_gh_runner
        threads, threads_unavailable = _harvest.fetch_review_thread_state(
            pr_number, repo=repo_slug, gh_runner=runner, warnings=derive_warnings
        )
        if threads_unavailable:
            # Cannot read the LATEST resolved state -> do NOT harvest reviews
            # at all this run. Without the resolved set we cannot tell an
            # implemented finding from a declined one, so harvesting would
            # re-file implemented findings even when the REST comment fetch
            # still works (GraphQL-only failure - Codex P1 on PR #348). Force
            # an empty comment list so triage_pr files zero review candidates;
            # carve-outs + COMPLETION still process. Retain the sentinel to
            # retry once GraphQL recovers.
            resolved_unavailable = True
            comments = []
            commit_dates = None
            author_login = None
        else:
            if resolved_ids is None:
                # Suppress findings the LATEST PR state shows as addressed:
                # resolved OR outdated threads. Outdated catches fixes pushed
                # without a manual "Resolve" click, the gap that re-queued 7
                # implemented findings (ab-158ab951).
                resolved_ids = _harvest.addressed_ids_from_threads(threads)
            if skipped_ids is None:
                # The Skipped-table cross-check is cosmetic (powers only the
                # discrepancy warning), so its own gh failure does NOT retain
                # the sentinel - the resolved filter above is the correctness
                # signal.
                rows, _skip_unavailable = _harvest.fetch_skipped_rows(
                    pr_number, repo=repo_slug, gh_runner=runner,
                    warnings=derive_warnings,
                )
                skipped_ids = _harvest.skipped_ids_from_rows(rows, threads)
            # Enrichment signals: commit dates (for addressed-finding suppression)
            # and PR author (to filter "Fixed in <sha>" reply comments). Both are
            # best-effort - a gh failure on either does NOT retain the sentinel
            # (same treatment as the Skipped-table cross-check: cosmetic, non-fatal).
            commit_dates, _cd_unavail = _harvest.fetch_pr_commit_dates(
                pr_number, repo=repo_slug, gh_runner=runner,
                warnings=derive_warnings,
            )
            author_login = _harvest.fetch_pr_author(
                pr_number, repo=repo_slug, gh_runner=runner,
                warnings=derive_warnings,
            )
    else:
        # comments were injected by the caller (test path or synthetic path);
        # commit_dates/author_login remain None unless explicitly passed.
        commit_dates = None
        author_login = None

    # Attribute filed nodes to the harvested PR's repo. retro's create path does
    # NOT auto-derive project the way `fno backlog idea` does, so without this
    # every queued node lands with project=None (ab-158ab951). The retro-pending
    # dir is GLOBAL, so a plain run can process sentinels from OTHER repos; only
    # attribute via the cwd-derived project when the sentinel actually belongs to
    # this repo (its pr_url repo matches current_repo_slug, or it carries no repo
    # hint). A foreign-repo sentinel is left project=None rather than mislabeled
    # with the cwd's project (codex P2). repo_slug/current_repo_slug compare
    # case-insensitively since GitHub owner/name are case-insensitive.
    project = None
    _is_local = (
        repo_slug is None
        or current_repo_slug is None
        or repo_slug.lower() == current_repo_slug.lower()
    )
    if _is_local:
        try:
            from fno.graph._intake import detect_project_from_settings

            project = detect_project_from_settings(str(eff_node_root))
        except Exception:
            project = None

    report = triage_pr(
        repo_root=repo_root,
        pr_number=pr_number,
        mode=mode,
        project=project,
        cwd=str(eff_node_root),
        session_ids=session_ids,
        completion_md=completion_md,
        comments=comments,
        resolved_ids=resolved_ids,
        skipped_ids=skipped_ids,
        commit_dates=commit_dates,
        author_login=author_login,
        existing_nodes=existing_nodes,
        gh_runner=gh_runner,
        repo=repo_slug,
        create_fn=create_fn,
        inbox_fn=inbox_fn,
        carveout_root=cr,
        carveouts_readonly=carveouts_readonly,
        # W4 causal links: the sentinel names the node whose PR spawned these
        # findings; filed follow-ups point back at it via caused_by.
        origin_node_id=payload.get("node_id"),
    )
    # Surface derivation warnings alongside the harvest's own (ordered first).
    if derive_warnings:
        report.warnings = derive_warnings + report.warnings
    _emit_report(report, mode=mode)

    # Clean = complete harvest + fully-landed; retain the trigger otherwise
    # (partial harvest, unreadable resolved state, or any land failure).
    clean = not report.partial and not report.failed and not resolved_unavailable
    if clean and report.harvested_carveout_ids:
        # Consume the carve-outs this clean run processed so they are never
        # re-harvested / re-filed under a later PR (bounds the ledger too).
        # ab-d4e8f852: the consume is best-effort - it must never block trigger
        # removal - but it must NOT be silent. A swallowed failure (a returned
        # 0, a lock timeout, an OSError) leaves the ids in the ledger and they
        # churn back in on the next run undetected, which is exactly what the
        # old bare `except: pass` hid.
        try:
            from fno.carveout.core import consume_carveouts

            want = len(set(report.harvested_carveout_ids))
            removed_n = consume_carveouts(cr, report.harvested_carveout_ids)
            if removed_n < want:
                typer.echo(
                    f"WARN consume_carveouts removed only {removed_n}/{want} "
                    f"processed carve-out id(s) from "
                    f"{cr}/.fno/carveouts.jsonl; the remainder may be "
                    f"re-harvested next run",
                    err=True,
                )
        except Exception as exc:  # never block trigger removal
            typer.echo(
                f"WARN consume_carveouts failed ({exc!r}); processed carve-outs "
                f"may be re-harvested next run",
                err=True,
            )
    return report, clean


def process_sentinel_file(
    sentinel_path: Path,
    *,
    repo_root: Path,
    existing_nodes: list[dict],
    comments: Optional[list[dict]] = None,
    resolved_ids: Optional[set[str]] = None,
    skipped_ids: Optional[set[str]] = None,
    gh_runner: Optional[Callable] = None,
    create_fn=None,
    inbox_fn=None,
    carveout_root: Optional[Path] = None,
    current_repo_slug: Optional[str] = None,
    node_root: Optional[Path] = None,
    ledger_path: Optional[Path] = None,
) -> "tuple[TriageReport, bool]":
    """Triage one sentinel file. Returns (report, removed).

    The sentinel is removed ONLY when the land fully succeeded and the harvest
    was not partial; otherwise it is retained for retry (AC4-ERR/AC6 idempotency).
    """
    payload = json.loads(sentinel_path.read_text(encoding="utf-8"))
    report, clean = _process_payload(
        payload,
        repo_root=repo_root,
        existing_nodes=existing_nodes,
        comments=comments,
        resolved_ids=resolved_ids,
        skipped_ids=skipped_ids,
        gh_runner=gh_runner,
        create_fn=create_fn,
        inbox_fn=inbox_fn,
        carveout_root=carveout_root,
        current_repo_slug=current_repo_slug,
        node_root=node_root,
        ledger_path=ledger_path,
    )
    removed = False
    if clean:
        try:
            sentinel_path.unlink()
            removed = True
        except OSError:
            pass
    return report, removed


def _run_postmortem_pass(repo_root: Path, node_root: Path) -> tuple[bool, int]:
    """Drain unconsumed postmortems (W6 6.2). Returns (failed, harvested):
    failed is True when any entry failed to land (caller retains the retry exit
    code); harvested is the count of postmortems drained (0 on the empty/error
    path). Fully self-contained: an exception here never sinks the sentinel
    loop."""
    from fno.graph.store import read_graph
    from fno.paths import graph_json, postmortems_dir
    from fno.retro.routine import triage_postmortems

    try:
        try:
            existing_nodes = read_graph(graph_json())
        except Exception:
            existing_nodes = []
        try:
            from fno.graph._intake import detect_project_from_settings

            project = detect_project_from_settings(str(node_root))
        except Exception:
            project = None
        pm = triage_postmortems(
            postmortems_dir=postmortems_dir(),
            repo_root=repo_root,
            project=project,
            cwd=str(node_root),
            existing_nodes=existing_nodes,
        )
    except Exception as exc:  # one bad source never sinks the run
        typer.echo(f"WARN postmortem harvest: {exc}", err=True)
        return True, 0
    if pm.harvested:
        counts = {
            k: sum(1 for d in pm.dispositions.values() if d == k)
            for k in ("node", "inbox", "archived", "dupe", "failed")
        }
        typer.echo(
            f"postmortems: {pm.harvested} unconsumed -> "
            f"{counts['node']} node(s), {counts['inbox']} inbox, "
            f"{counts['archived']} archived, {counts['dupe']} dupe(s), "
            f"{counts['failed']} failed; {pm.stamped} stamped consumed_at"
        )
    for w in pm.warnings:
        typer.echo(f"WARN postmortem: {w}", err=True)
    return pm.failed, pm.harvested


@retro_app.command("drain-postmortems")
def drain_postmortems() -> None:
    """Drain unconsumed postmortems ONLY - no sentinel triage, no carve-out
    harvest. The narrow verb (x-42f6 US3) co-fired in the SessionStart reconcile
    throttle so a stuck session's postmortem is harvested within one throttle
    window, not "whenever some other PR happens to merge." Idempotent: date+
    session-keyed filenames and consumed_at stamps mean a re-drain is a no-op."""
    from fno.paths import resolve_canonical_repo_root, resolve_repo_root

    repo_root = resolve_repo_root()
    # Filed nodes are scoped to the CANONICAL root (a node outlives the worktree
    # it was captured in), matching `retro run`'s split.
    node_root = resolve_canonical_repo_root()
    failed, harvested = _run_postmortem_pass(repo_root, node_root)
    if not harvested and not failed:
        # Genuinely empty drain. On the error path _run_postmortem_pass already
        # WARNed and returns (True, 0); don't also print a clean "0 unconsumed".
        typer.echo("postmortems: 0 unconsumed")
    raise typer.Exit(1 if failed else 0)


@retro_app.command("run")
def run(
    node: Optional[str] = typer.Option(None, "--node", help="Triage only this node's sentinel."),
    pr: Optional[int] = typer.Option(
        None,
        "--pr-number",
        help=(
            "Explicitly harvest left-out work for this merged PR even when no "
            "sentinel exists - the manual-merge path: a PR merged with no "
            "node<->PR link drops neither trigger, so a plain `retro run` "
            "harvests nothing. Carve-outs for the PR's session(s) are harvested "
            "and consumed. Pair with --session-id to scope; --repo for reviews."
        ),
    ),
    pr_legacy: Optional[int] = typer.Option(
        None, "--pr", hidden=True, help="[DEPRECATED] alias for --pr-number."
    ),
    session: Optional[list[str]] = typer.Option(
        None,
        "--session-id",
        help=(
            "Session id(s) to scope the --pr-number carve-out harvest to "
            "(repeatable). Omit to harvest every unconsumed carve-out."
        ),
    ),
    session_legacy: Optional[list[str]] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    repo: Optional[str] = typer.Option(
        None,
        "--repo",
        help=(
            "owner/name for the --pr review harvest. Defaults to the current "
            "repo via `gh repo view`."
        ),
    ),
    keep_going: bool = typer.Option(
        False,
        "--keep-going",
        help=(
            "Autonomous keep-going (x-3360): treat this harvest as a no-human "
            "run so the engine classifies surviving carve-outs and dispatches "
            "follow-up /think or /target work under the firehose ceiling. A no-op "
            "unless config.keep_going.enabled is set. Passed by the autonomous "
            "/fno:pr merged ritual; a real autonomous-mode sentinel triggers the "
            "engine without it."
        ),
    ),
) -> None:
    """Consume retro-pending sentinels and file left-out work."""
    from fno._flag_aliases import merge_deprecated_alias
    from fno.carveout.core import resolve_carveout_root

    pr = merge_deprecated_alias(
        pr, pr_legacy, canonical_flag="--pr-number", legacy_flag="--pr"
    )
    session = merge_deprecated_alias(
        session, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )
    from fno.graph.store import read_graph
    from fno.paths import (
        graph_json,
        ledger_json,
        resolve_canonical_repo_root,
        resolve_repo_root,
        retro_pending_dir,
    )

    repo_root = resolve_repo_root()
    # Filed nodes are scoped to the CANONICAL root, never the worktree: a node
    # outlives the worktree it was captured in, and only canonical roots match
    # settings.yaml for project attribution (ab-b4da4664). repo_root stays the
    # worktree below for the .triage-pending fast-path + COMPLETION.md lookups,
    # which are written by the in-worktree session.
    node_root = resolve_canonical_repo_root()
    # Carve-outs live under the canonical root (they outlive any one worktree);
    # the sentinels themselves stay repo_root-relative / global, unchanged.
    carveout_root = resolve_carveout_root()
    # Two slugs with distinct jobs (codex P2 on PR #419):
    #  - local_slug = the ACTUAL local repo (gh-derived), used to GATE project
    #    attribution: a node is attributed to this checkout's canonical project
    #    only when the harvested PR genuinely belongs to it.
    #  - current_slug = local_slug unless --repo overrode it, used for sentinel
    #    PR-scope filtering + the synthetic harvest's target URL.
    # Conflating them let `--pr N --repo other/repo` (a FOREIGN repo, from a
    # different checkout) pass the _is_local gate and mis-attribute the foreign
    # node to the caller's project, now that detection actually resolves.
    local_slug = _current_repo_slug()
    current_slug = repo or local_slug
    sentinel_dir = retro_pending_dir()
    if node:
        candidates = [p for p in [sentinel_dir / f"{node}.json"] if p.exists()]
    elif sentinel_dir.exists():
        candidates = sorted(sentinel_dir.glob("*.json"))
    else:
        candidates = []

    # Fast-path: the project-local .triage-pending dropped by pr-merge.sh. Same
    # JSON shape, same shared routine. Processed alongside the universal sentinels
    # so both triggers firing for one PR collapse to a single node set via dedup.
    fast_path = repo_root / ".fno" / ".triage-pending"
    if fast_path.exists() and not node:
        candidates.append(fast_path)

    # `--pr` is strictly scoped: harvest ONLY this PR. Without this, passing --pr
    # ALSO drains every unrelated retro-pending sentinel in the same run, pulling
    # another repo's carve-outs into this PR's triage (ab-158ab951). PR numbers
    # are unique only within a repo, so match BOTH the number AND (when the repo
    # is known) the sentinel's repo, or a same-numbered sentinel from another
    # repo would still be drained (codex P2). The synthetic --pr harvest below
    # covers the (common) no-sentinel case.
    if pr is not None and not node:
        candidates = [
            p for p in candidates
            if _sentinel_pr_number(p) == pr and _sentinel_repo_matches(p, current_slug)
        ]

    # W6 6.2 (x-f063): the postmortem source rides the PLAIN retro run - no
    # new trigger (the post-merge ritual and on-demand `fno retro run` both
    # land here). A --node/--pr-number run is a targeted harvest and skips it.
    # Same independence contract as the PR-scoped sources: its failure never
    # sinks the sentinel loop, and vice versa.
    pm_failed = False
    if node is None and pr is None:
        pm_failed, _ = _run_postmortem_pass(repo_root, node_root)
        # Autonomy-debt summary (x-f894): rank gate_escape events by reason so
        # the roadmap sees which reliability fix pays first. Rides the plain run
        # (like the postmortem pass); prints even with no sentinels, before the
        # early exit, so a clean repo still reports "0 by reason". Best-effort:
        # a read failure here never sinks the retro run.
        try:
            from fno.retro.gate_escape import (
                render_gate_escapes,
                summarize_gate_escapes,
            )

            _ge = summarize_gate_escapes(node_root / ".fno" / "events.jsonl")
            for _line in render_gate_escapes(_ge):
                typer.echo(_line, err=True)
        except Exception as _exc:
            typer.echo(f"WARN gate_escape summary failed: {_exc}", err=True)

    if not candidates and pr is None:
        typer.echo("(no retro-pending sentinels to triage)")
        raise typer.Exit(1 if pm_failed else 0)

    any_retained = pm_failed
    for sentinel_path in candidates:
        # Reload live nodes per sentinel so a node filed by an earlier sentinel
        # in THIS run is seen by dedup for the next one (AC6-FR: dual triggers).
        try:
            existing_nodes = read_graph(graph_json())
        except Exception:
            existing_nodes = []
        try:
            _report, removed = process_sentinel_file(
                sentinel_path,
                repo_root=repo_root,
                existing_nodes=existing_nodes,
                carveout_root=carveout_root,
                current_repo_slug=local_slug,
                node_root=node_root,
                ledger_path=ledger_json(),
            )
        except Exception as exc:  # never let one bad sentinel sink the rest
            typer.echo(f"WARN sentinel {sentinel_path.name}: {exc}", err=True)
            any_retained = True
            continue
        if not removed:
            any_retained = True

    # Synthetic --pr harvest: no sentinel exists (manual merge with no
    # node<->PR link), so build a payload from the flags and run the SAME
    # routine - carve-outs land + consume, and reviews/COMPLETION harvest too
    # when the PR maps to a repo. dedup (existing_nodes reloaded here) means a
    # carve-out already filed via a sentinel above is not double-filed.
    if pr is not None:
        slug = current_slug  # already resolved above (repo or gh repo view)
        payload: dict = {"pr_number": pr}
        # Scope the carve-out harvest to this PR's owning session(s). An explicit
        # --session-id wins; otherwise resolve from the ledger by PR number/url.
        # If NEITHER yields a session, the carve-out source is read-only: an
        # unscoped carve-out (another session's deferred work) must never be
        # stamped onto / consumed under an arbitrary --pr-number (x-90b8). The
        # post-merge Step 4b backfill slot uses the same guard.
        if session:
            payload["session_ids"] = list(session)
        else:
            resolved_sessions = _resolve_pr_session_ids(ledger_json(), pr, slug)
            if resolved_sessions:
                payload["session_ids"] = resolved_sessions
            else:
                payload["carveouts_readonly"] = True
        if slug:
            payload["pr_url"] = f"https://github.com/{slug}/pull/{pr}"
        # x-3360: an autonomous keep-going harvest (the /fno:pr merged ritual
        # passes --keep-going) has no sentinel, so mark the synthetic payload
        # autonomous so nodes land active AND the keep-going engine fires. Gated
        # by config so a stray flag on a keep_going-off install stays a plain run.
        if keep_going:
            from fno.retro.keep_going import keep_going_enabled

            if keep_going_enabled(project_root=repo_root):
                payload["mode"] = "autonomous"
        try:
            existing_nodes = read_graph(graph_json())
        except Exception:
            existing_nodes = []
        # The synthetic path is carve-out-FIRST. With no resolvable repo (gh
        # down, no --repo) reviews are unharvestable anyway, so pass an empty
        # comment list to SKIP review derivation rather than let it mark the run
        # unavailable - otherwise carve-outs would harvest but never consume and
        # re-file as duplicate nodes next run (gemini HIGH on PR #405). The
        # sentinel path keeps its conservative retain-on-no-url behavior.
        synthetic_comments: Optional[list] = None if slug else []
        try:
            _report, clean = _process_payload(
                payload,
                repo_root=repo_root,
                existing_nodes=existing_nodes,
                carveout_root=carveout_root,
                comments=synthetic_comments,
                current_repo_slug=local_slug,
                node_root=node_root,
                ledger_path=ledger_json(),
            )
            if not clean:
                any_retained = True
        except Exception as exc:
            typer.echo(f"WARN --pr {pr}: {exc}", err=True)
            any_retained = True

    # Non-zero when something was retained for retry (AC4-ERR), so a loop knows.
    raise typer.Exit(1 if any_retained else 0)

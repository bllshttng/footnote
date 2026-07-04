"""``fno observer`` - skill eval over a recorded corpus (x-57a5).

Two verbs over the pure fold in :mod:`fno.observer.fold`:

- ``sweep`` - retrospective, read-only. Folds ledger/graph/events/postmortems
  into a corpus, scores each attributable item, emits one ``skill_eval_finding``
  per (item, dimension) and one terminal ``skill_eval_run_complete``, and writes
  a human digest. ~$0 spend (a read-time fold, Locked Decision 1).
- ``replay`` - active, isolated, real-spend, /blueprint-only (Review Amendment
  A2). Runs the skill headless in a throwaway worktree against a corpus item's
  recorded input, scores the fresh output on structural dimensions only (A1),
  and hard-fails if a post-run detective scan finds the replay session id in
  real state (the ONE non-advisory failure mode).

I/O (plan reads, gh fetches, spawns) lives here; the scoring is pure in
``fold.py``. Registered in ``fno.cli`` LAZY_SUBCOMMANDS as ``observer``.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional

import typer

from fno.observer import fold

observer_app = typer.Typer(
    name="observer",
    no_args_is_help=True,
    help="Skill eval over a recorded corpus (sweep/replay).",
)


@observer_app.callback()
def _observer_callback() -> None:
    """No-op: keeps Typer from collapsing a multi-command sub-app into one."""


# --------------------------------------------------------------------------- #
# Shared corpus construction (read-only I/O around the pure fold)
# --------------------------------------------------------------------------- #


def _read_postmortems(postmortems_dir: Path) -> list[dict]:
    """Local frontmatter reader: ``{session_id, graph_node_id,
    blocked_reason_kind}`` per postmortem. ``harvest_postmortems`` returns
    gists (RawItems) for the inbox; the fold wants the raw attribution keys, so
    read them directly. Tolerant: a missing dir or a malformed file is skipped,
    never fatal (mirrors ``harvest_postmortems``)."""
    from fno.retro.harvest import _split_frontmatter

    if not postmortems_dir.is_dir():
        return []
    out: list[dict] = []
    for path in sorted(postmortems_dir.glob("*.md")):
        try:
            fm, _body = _split_frontmatter(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        fm = fm or {}
        blocked = fm.get("blocked_reason")
        kind = str(blocked.get("kind")) if isinstance(blocked, dict) and blocked.get("kind") else None
        out.append(
            {
                "session_id": fm.get("session_id"),
                "graph_node_id": fm.get("graph_node_id"),
                "blocked_reason_kind": kind,
            }
        )
    return out


def _load_corpus(skill: str, since: int) -> tuple[dict, dict]:
    """Build the read-only corpus and return ``(corpus, by_id)`` where ``by_id``
    maps graph node id -> node dict (for PR lookup in review scoring)."""
    from fno import paths as _paths
    from fno.scoreboard.fold import load_ledger_rows, read_graph_nodes

    ledger_path = _paths.ledger_json()
    graph_path = _paths.graph_json()
    rows = load_ledger_rows(ledger_path)
    nodes = read_graph_nodes(graph_path)
    postmortems = _read_postmortems(_paths.postmortems_dir())
    corpus = fold.build_corpus(
        rows, nodes, postmortems, skill=skill, since_days=since, now=datetime.now()
    )
    by_id = {n.get("id"): n for n in nodes if n.get("id")}
    return corpus, by_id


# --------------------------------------------------------------------------- #
# Per-skill scoring I/O
# --------------------------------------------------------------------------- #

_PR_URL_RE = re.compile(r"github\.com/([^/]+/[^/]+)/pull/")


def _repo_from_pr_url(pr_url: Optional[str]) -> Optional[str]:
    """`https://github.com/owner/name/pull/149` -> `owner/name`, else None."""
    if not pr_url:
        return None
    m = _PR_URL_RE.search(pr_url)
    return m.group(1) if m else None


def _read_plan_text(item: dict, node: Optional[dict], gh_runner) -> Optional[str]:
    """Plan text for a blueprint corpus item: on-disk plan file first (dir ->
    00-INDEX.md, else the file), then ``gh pr diff <n>`` fallback (AC1-EDGE),
    else None -> the item is excluded and counted in the coverage gap."""
    plan_path = item.get("plan_path")
    if plan_path:
        p = Path(plan_path)
        doc = p / "00-INDEX.md" if p.is_dir() else p
        try:
            if doc.exists():
                return doc.read_text(encoding="utf-8")
        except OSError:
            pass
    pr_number = (node or {}).get("pr_number")
    if pr_number:
        rc, out, _err = gh_runner(["pr", "diff", str(pr_number)])
        if rc == 0 and out.strip():
            return out
    return None


def _review_id_sets(pr_number: int, repo: Optional[str], gh_runner) -> Optional[dict]:
    """Fetch the finding/addressed/skipped id sets for a review corpus item.

    Returns None (-> exclude, coverage gap) when the correctness-bearing
    resolved-thread state is unavailable (gh down / rate-limited), per AC1-ERR.
    """
    from fno.retro import harvest

    threads, thread_unavail = harvest.fetch_review_thread_state(pr_number, repo=repo, gh_runner=gh_runner)
    if thread_unavail:
        return None
    comments = harvest.fetch_review_comments(pr_number, repo=repo, gh_runner=gh_runner)
    commit_dates, _ = harvest.fetch_pr_commit_dates(pr_number, repo=repo, gh_runner=gh_runner)
    skipped_rows, _ = harvest.fetch_skipped_rows(pr_number, repo=repo, gh_runner=gh_runner)

    # A finding = a top-level bot reviewer comment (the retro harvest's own
    # candidate definition). Author/human replies and threaded replies are not
    # findings.
    all_finding_ids = {
        c["id"] for c in comments if c.get("is_bot") and not c.get("in_reply_to_id") and c.get("id")
    }
    addressed = harvest.addressed_ids_from_threads(threads) | harvest.addressed_ids_from_comments(
        comments, commit_dates
    )
    skipped = harvest.skipped_ids_from_rows(skipped_rows, threads)
    return {
        "all_finding_ids": all_finding_ids,
        "addressed_ids": addressed,
        "skipped_ids": skipped,
    }


def _default_gh(args: list[str]) -> "tuple[int, str, str]":
    try:
        p = subprocess.run(["gh", *args], capture_output=True, text=True, timeout=60)
        return p.returncode, p.stdout, p.stderr
    except (OSError, subprocess.TimeoutExpired) as exc:
        return 1, "", str(exc)


def _score_item(item: dict, skill: str, by_id: dict, gh_runner) -> dict[str, Optional[str]]:
    """Score one corpus item, returning ``{dimension: verdict|None}``. A gh/disk
    failure for an attributed item yields all-None (a coverage gap), never a
    crash (AC1-ERR / AC1-EDGE)."""
    node = by_id.get(item.get("graph_node_id"))
    try:
        if skill == "blueprint":
            plan_text = _read_plan_text(item, node, gh_runner)
            return fold.score_blueprint_item(item, plan_text=plan_text)
        # review
        pr_number = (node or {}).get("pr_number")
        if not pr_number:
            return {"finding_precision": None}
        ids = _review_id_sets(int(pr_number), _repo_from_pr_url((node or {}).get("pr_url")), gh_runner)
        if ids is None:
            return {"finding_precision": None}
        return fold.score_review_item(**ids)
    except Exception as exc:  # any unforeseen I/O fault -> coverage gap, not crash
        print(f"observer: scoring item {item.get('session_id')} failed: {exc}", file=sys.stderr)
        dims = ("structural_validity", "collision_free", "shipped_outcome") if skill == "blueprint" else ("finding_precision",)
        return {d: None for d in dims}


# --------------------------------------------------------------------------- #
# Emit + digest
# --------------------------------------------------------------------------- #


def _events_paths() -> list[Path]:
    """Project events.jsonl (x-0ca7's trigger source) + the global log."""
    from fno import paths as _paths

    project = _paths.project_log("events.jsonl")
    glob = _paths.ledger_json().parent / "events.jsonl"
    return [project] if glob == project else [project, glob]


def _emit_finding(
    *,
    run_id: str,
    item: dict,
    dimension: str,
    verdict: str,
    evidence: str,
    cost_usd: float,
    skill_ref: Optional[str],
    events_paths: list[Path],
) -> None:
    from fno.events import _build, append_event

    data = {
        "run_id": run_id,
        "skill_id": item["skill_id"],
        "skill_version": item.get("skill_version") or "unknown",
        "corpus_item_id": item.get("session_id") or "unknown",
        "dimension": dimension,
        "verdict": verdict,
        "evidence": evidence[:500],
        "cost_usd": cost_usd,
    }
    if skill_ref is not None:
        data["skill_ref"] = skill_ref
    event = _build("skill_eval_finding", "observer", data)
    for p in events_paths:
        try:
            append_event(event, p)
        except Exception as exc:
            print(f"observer: finding emit to {p} failed: {exc}", file=sys.stderr)


def _emit_run_complete(summary: dict, events_paths: list[Path]) -> None:
    from fno.events import _build, append_event

    data = {k: v for k, v in summary.items() if k != "state"}
    event = _build("skill_eval_run_complete", "observer", data)
    for p in events_paths:
        try:
            append_event(event, p)
        except Exception as exc:
            print(f"observer: run_complete emit to {p} failed: {exc}", file=sys.stderr)


def _write_digest(summary: dict, skill: str, *, mode: str) -> Path:
    from fno import paths as _paths

    reports_dir = _paths.observer_reports_dir()
    reports_dir.mkdir(parents=True, exist_ok=True)
    date = datetime.now().strftime("%Y-%m-%d")
    path = reports_dir / f"{skill}-{date}.md"
    ranking = "\n".join(
        f"- {r['dimension']}: {r['fail_count']} fail" for r in summary.get("failure_ranking", [])
    ) or "- (no failing dimensions)"
    ref = summary.get("skill_ref")
    ref_line = f"\nskill_ref: `{ref}`" if ref else ""
    caveat = (
        "\n> Replay scores structural dimensions only (Review Amendment A1); "
        "shipped-outcome is confirmed by subsequent sweeps, not this run.\n"
        if mode == "replay"
        else ""
    )
    path.write_text(
        f"# Observer {mode}: {summary['skill_id']} ({date})\n\n"
        f"run_id: `{summary['run_id']}`{ref_line}\n"
        f"coverage: {summary['coverage_pct']}% "
        f"({summary['corpus_size']} attributable items)\n"
        f"verdicts: pass={summary['pass_count']} "
        f"degraded={summary['degraded_count']} fail={summary['fail_count']}\n"
        f"{caveat}\n"
        f"## Failure ranking\n{ranking}\n",
        encoding="utf-8",
    )
    return path


def _mint_run_id(skill_id: str) -> str:
    return f"obs-{skill_id}-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')}"


# --------------------------------------------------------------------------- #
# sweep
# --------------------------------------------------------------------------- #


@observer_app.command("sweep")
def sweep(
    skill: str = typer.Option(..., "--skill", help="blueprint | review"),
    since: int = typer.Option(28, "--since", help="Window in days (default 28)."),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit the run summary as JSON."),
) -> None:
    """Retrospective read-only sweep: score a recorded corpus and emit events.

    States on stdout: ``ok`` (run_complete emitted), ``insufficient`` (<10
    attributable items, no run_complete), ``partial`` (a coverage gap is
    present). Never exits 0 silently with no state word (the one anti-silent
    rule for this harness).
    """
    if skill not in ("blueprint", "review"):
        raise typer.BadParameter("--skill must be blueprint or review")
    if since < 1:
        raise typer.BadParameter("--since must be >= 1 (days).")

    corpus, by_id = _load_corpus(skill, since)
    items = corpus["items"]
    skill_id = fold._SKILL_IDS.get(skill, f"fno:{skill}")

    # Insufficient guard first (AC1-UI): <10 attributable -> no run_complete.
    if len(items) < fold.MIN_ATTRIBUTABLE:
        typer.echo(
            f"insufficient: {len(items)} attributable {skill_id} session(s) in the last "
            f"{since}d, need >={fold.MIN_ATTRIBUTABLE}. No skill_eval_run_complete emitted."
        )
        raise typer.Exit(0)

    run_id = _mint_run_id(skill_id)
    events_paths = _events_paths()
    gh_runner = _default_gh
    skill_version = items[0].get("skill_version") or "unknown"

    findings: list[tuple[str, str]] = []
    scored_count = 0
    for item in items:
        scores = _score_item(item, skill, by_id, gh_runner)
        item_scored = False
        for dimension, verdict in scores.items():
            if verdict is None:
                continue  # not scorable -> coverage gap, never a fabricated verdict
            item_scored = True
            findings.append((dimension, verdict))
            _emit_finding(
                run_id=run_id,
                item=item,
                dimension=dimension,
                verdict=verdict,
                evidence=_evidence(item, dimension, verdict),
                cost_usd=0.0,
                skill_ref=None,
                events_paths=events_paths,
            )
        if item_scored:
            scored_count += 1

    summary = fold.build_run_summary(
        run_id=run_id,
        skill_id=skill_id,
        skill_version=skill_version,
        findings=findings,
        corpus_size=len(items),
        scored_count=scored_count,
    )
    summary["cost_usd"] = 0.0  # sweep is a read-only fold
    _emit_run_complete(summary, events_paths)
    digest = _write_digest(summary, skill, mode="sweep")

    state = "ok" if scored_count == len(items) else "partial"
    if json_out:
        typer.echo(json.dumps({**summary, "state": state, "digest": str(digest)}, indent=2))
        return
    typer.echo(
        f"{state}: {skill_id} run {run_id}\n"
        f"  scored {scored_count}/{len(items)} attributable items "
        f"({summary['coverage_pct']}% coverage)\n"
        f"  verdicts: pass={summary['pass_count']} degraded={summary['degraded_count']} "
        f"fail={summary['fail_count']}\n"
        f"  digest: {digest}"
    )


def _evidence(item: dict, dimension: str, verdict: str) -> str:
    """One earned-specificity line: what drove this verdict for this item."""
    sid = item.get("session_id") or "?"
    nid = item.get("graph_node_id") or "?"
    if dimension == "shipped_outcome":
        return f"node {nid} outcome={item.get('outcome')} attribution={item.get('attribution_class')}"
    return f"session {sid} node {nid}: {dimension}={verdict}"


# --------------------------------------------------------------------------- #
# replay (blueprint-only, isolated, real spend)
# --------------------------------------------------------------------------- #

_PER_ITEM_CAP_USD = 3.0  # matches the old fixture default (never uncapped)


def _write_workdir_settings(workdir: Path) -> None:
    """Preventive isolation: redirect all mutable fno paths into *workdir*'s own
    throwaway ``.fno/`` and stamp the migration sentinel so the first ``fno``
    call in the spawned session does not rewrite this fragment. Resurrected from
    ``evals/runner.py`` (675b24e~1); the ``.path-migration-done`` touch is
    load-bearing (without it the preventive layer is silently undone)."""
    import yaml

    fno_dir = workdir / ".fno"
    fno_dir.mkdir(parents=True, exist_ok=True)
    settings = {
        "schema_version": 1,
        "config": {
            "no_ship": True,
            "state_dir": str(fno_dir) + "/",
            "paths": {
                "graph_json": str(fno_dir / "graph.json"),
                "ledger_json": str(fno_dir / "ledger.json"),
                "briefs_dir": str(fno_dir / "briefs/"),
            },
        },
    }
    (fno_dir / "settings.yaml").write_text(
        yaml.dump(settings, default_flow_style=False), encoding="utf-8"
    )
    (fno_dir / ".path-migration-done").touch()


def _default_spawn(name: str, prompt: str, *, cwd: Path, timeout: int) -> "tuple[int, str, str]":
    """Sanctioned headless spawn (never a bare ``claude -p``). fable-tier for
    /blueprint per the loops-roadmap routing table (Locked Decision 6)."""
    try:
        p = subprocess.run(
            [
                "fno", "agents", "spawn", name, prompt,
                "--provider", "claude",
                "--substrate", "headless",
                "--model", "fable",
                "--cwd", str(cwd),
                "--timeout", str(timeout),
            ],
            capture_output=True,
            text=True,
            timeout=timeout + 30,
        )
        return p.returncode, p.stdout, p.stderr
    except (OSError, subprocess.TimeoutExpired) as exc:
        return 1, "", str(exc)


def _already_scored(run_id: str, skill_ref: Optional[str], corpus_item: str, events_paths: list[Path]) -> bool:
    """AC2-FR idempotent resume: has this (run_id, skill_ref, corpus_item)
    triple already emitted a finding? Tolerant read (any parse failure -> not
    scored, so we re-run rather than skip real work)."""
    for p in events_paths:
        if not p.exists():
            continue
        try:
            for line in p.read_text(encoding="utf-8").splitlines():
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(e, dict):
                    continue  # a bare JSON scalar/array is not an event row
                d = e.get("data")
                if (
                    e.get("type") == "skill_eval_finding"
                    and isinstance(d, dict)
                    and d.get("run_id") == run_id
                    and d.get("corpus_item_id") == corpus_item
                    and (d.get("skill_ref") or None) == (skill_ref or None)
                ):
                    return True
        except OSError:
            continue
    return False


@observer_app.command("replay")
def replay(
    skill: str = typer.Option(..., "--skill", help="blueprint (review is not supported in v1)"),
    corpus_item: str = typer.Option(..., "--corpus-item", help="Historical session_id to replay."),
    skill_ref: Optional[str] = typer.Option(None, "--skill-ref", help="Git ref of the candidate skill under test (default HEAD)."),
    run_id: Optional[str] = typer.Option(None, "--run-id", help="Reuse a sweep/replay run_id for before/after lineage."),
    since: int = typer.Option(90, "--since", help="Corpus window to locate the item in."),
) -> None:
    """Replay one corpus item's recorded input through the skill in an isolated
    worktree, score the fresh plan on structural dimensions only (A1), and
    hard-fail if the replay session id leaks into real state (the one
    non-advisory failure).

    ``--skill review`` is rejected: a replayed review emits fresh findings no
    historical thread ever adjudicated, so ``finding_precision`` has no ground
    truth (Review Amendment A2)."""
    _replay(
        skill=skill, corpus_item=corpus_item, skill_ref=skill_ref,
        run_id=run_id, since=since, spawn=_default_spawn, run_worktree=True,
    )


def _replay(
    *,
    skill: str,
    corpus_item: str,
    skill_ref: Optional[str],
    run_id: Optional[str],
    since: int,
    spawn: Callable = _default_spawn,
    run_worktree: bool = True,
) -> None:
    """Replay core (spawn + worktree are injectable seams for tests)."""
    _spawn = spawn
    _run_worktree = run_worktree
    from fno import paths as _paths
    from fno.loops import loops_paused

    if skill == "review":
        typer.echo("replay not supported for review in v1 (Review Amendment A2): a replayed "
                   "review has no historical thread state to score finding_precision against.")
        raise typer.Exit(2)
    if skill != "blueprint":
        raise typer.BadParameter("--skill must be blueprint (v1 replay is blueprint-only)")

    # Pause gate at the corpus-item boundary (AC / Locked Decision 10).
    if loops_paused():
        typer.echo("paused: loops pause-all sentinel in effect; stopping cleanly before replay.")
        raise typer.Exit(0)

    corpus, by_id = _load_corpus(skill, since)
    item = next((it for it in corpus["items"] if it.get("session_id") == corpus_item), None)
    if item is None:
        typer.echo(f"no attributable {skill} corpus item with session_id={corpus_item} in the last {since}d.")
        raise typer.Exit(1)

    skill_id = item["skill_id"]
    run_id = run_id or _mint_run_id(skill_id)
    events_paths = _events_paths()

    if _already_scored(run_id, skill_ref, corpus_item, events_paths):
        typer.echo(f"already scored ({run_id}, {skill_ref or 'HEAD'}, {corpus_item}); skipping (AC2-FR).")
        raise typer.Exit(0)

    # The recorded input: the design-doc/plan text the historical blueprint ran
    # on (best-effort from disk). Absent -> a tool fault for this item, not a
    # skill-quality verdict.
    plan_text = _read_plan_text(item, by_id.get(item.get("graph_node_id")), _default_gh)
    if not plan_text:
        _emit_finding(
            run_id=run_id, item=item, dimension="structural_validity", verdict="fail",
            evidence=f"replay tool-fault: recorded input for {corpus_item} unresolvable",
            cost_usd=0.0, skill_ref=skill_ref, events_paths=events_paths,
        )
        typer.echo(f"tool-fault: recorded input for {corpus_item} unresolvable; emitted tool-fault finding.")
        raise typer.Exit(1)

    from fno.claims.core import ClaimHeldByOther, acquire_claim, release_claim
    from fno.observer import isolation

    repo_root = _paths.resolve_repo_root()
    worktree_name = f"observer-{skill}-{corpus_item[:12]}"
    scratch = _paths.worktrees_base() / repo_root.name / worktree_name
    claim_key = f"observer:{skill}:{worktree_name}"
    # Per-PROCESS holder (not corpus_item): two same-item replays share the
    # scratch path, and an identical holder would make acquire_claim an
    # idempotent re-acquire (no exclusion), letting both race on `git worktree
    # add/remove` of one path (AC2-EDGE). A distinct holder -> the second gets
    # ClaimHeldByOther and refuses cleanly instead of stomping.
    holder = f"{corpus_item}:{os.getpid()}"

    claim = None
    result_state = "ok"
    try:
        try:
            claim = acquire_claim(
                key=claim_key, holder=holder,
                reason=f"observer replay {run_id}", ttl_ms=30 * 60 * 1000,
            )
        except ClaimHeldByOther as exc:
            typer.echo(f"another replay holds {corpus_item}; exiting without touching its worktree ({exc}).")
            raise typer.Exit(4)

        if _run_worktree:
            subprocess.run(["git", "worktree", "prune"], cwd=repo_root, capture_output=True, text=True)
            subprocess.run(["git", "worktree", "remove", "--force", str(scratch)], cwd=repo_root, capture_output=True, text=True)
            ref = skill_ref or "HEAD"
            add = subprocess.run(
                ["git", "worktree", "add", "--force", str(scratch), ref],
                cwd=repo_root, capture_output=True, text=True,
            )
            if add.returncode != 0:
                raise RuntimeError(f"git worktree add failed: {add.stderr.strip()[:300]}")
            _write_workdir_settings(scratch)

        # Feed the recorded input to /blueprint headless (fable-tier, sized to a
        # real blueprint run, NOT verify_advise's 90s - Domain Pitfall).
        prompt = (
            "Run /blueprint on the following design doc and output the resulting "
            "implementation plan (with a `## Failure Modes` section and a "
            "`## Execution Strategy` yaml block declaring per-task `surface` file "
            "ownership). Design doc:\n\n" + plan_text[:20000]
        )
        rc, out, err = _spawn(
            f"observer-replay-{corpus_item[:12]}", prompt,
            cwd=(scratch if _run_worktree else repo_root), timeout=600,
        )
        cost_usd = _read_scratch_cost(scratch) if _run_worktree else 0.0
        cap = _replay_cost_cap()
        if cost_usd > cap:
            typer.echo(
                f"warning: replay of {corpus_item} spent ${cost_usd:.2f}, over the ${cap:.2f} cap "
                "(the 600s spawn timeout is the hard pre-spawn bound; dollar caps are post-hoc in v1).",
                err=True,
            )

        if rc != 0 or not (out or "").strip():
            _emit_finding(
                run_id=run_id, item=item, dimension="structural_validity", verdict="fail",
                evidence=f"replay spawn tool-fault rc={rc}: {(err or '')[:200]}",
                cost_usd=cost_usd, skill_ref=skill_ref, events_paths=events_paths,
            )
            typer.echo(f"tool-fault: replay spawn for {corpus_item} rc={rc}; emitted tool-fault finding.")
            result_state = "tool-fault"
        else:
            # A1: replay scores STRUCTURAL dimensions only (no shipped_outcome).
            replay_item = {**item, "include_shipped_outcome": False}
            scores = fold.score_blueprint_item(replay_item, plan_text=out)
            findings: list[tuple[str, str]] = []
            for dimension, verdict in scores.items():
                if verdict is None:
                    continue
                findings.append((dimension, verdict))
                _emit_finding(
                    run_id=run_id, item=item, dimension=dimension, verdict=verdict,
                    evidence=f"replay {skill_ref or 'HEAD'} {corpus_item}: {dimension}={verdict}",
                    cost_usd=cost_usd, skill_ref=skill_ref, events_paths=events_paths,
                )
            summary = fold.build_run_summary(
                run_id=run_id, skill_id=skill_id,
                skill_version=item.get("skill_version") or "unknown",
                findings=findings, corpus_size=1,  # truthful: a single-item replay batch
                scored_count=1 if findings else 0, skill_ref=skill_ref,
                require_min=False,  # replay is a targeted before/after, not a >=10 trend
            )
            summary["cost_usd"] = cost_usd
            summary["skill_ref"] = skill_ref
            _emit_run_complete(summary, events_paths)
            _write_digest(summary, skill, mode="replay")
            typer.echo(f"ok: replayed {corpus_item} under {skill_ref or 'HEAD'} (cost=${cost_usd:.2f})")

        # DETECTIVE scan: the ONE hard failure. A replay session id in real
        # ~/.fno/{ledger,graph}.json or the global events log = correctness bug.
        if _run_worktree:
            ids, _tp = isolation.collect_eval_session_ids(scratch)
            iso = isolation.check_isolation(ids, isolation.default_real_state_paths(repo_root))
            if iso.verdict == "violated":
                typer.echo(isolation.format_violation_report(iso), err=True)
                raise typer.Exit(3)
    finally:
        if _run_worktree:
            subprocess.run(["git", "worktree", "remove", "--force", str(scratch)], cwd=repo_root, capture_output=True, text=True)
        if claim is not None:
            try:
                release_claim(claim_key, holder=holder)
            except Exception:
                pass

    if result_state == "tool-fault":
        raise typer.Exit(1)


def _replay_cost_cap() -> float:
    """Per-item spend ceiling: the min of the configured aggregate per-run budget
    (``config.loops.observer_harness.budget_usd_per_run``) and the hard $3
    per-item cap (Locked Decision 6, never uncapped).

    ponytail: a headless spawn surfaces no cost until it finishes, so this is a
    post-hoc overage warning, not a pre-spawn gate; the 600s timeout is the real
    pre-spawn bound. Upgrade path if precise enforcement matters: a spawn-API
    budget arg that aborts the session at a dollar ceiling."""
    budget = None
    try:
        from fno.config import load_settings

        entry = load_settings().config.loops.get("observer_harness")
        budget = entry.budget_usd_per_run if entry is not None else None
    except Exception:
        budget = None
    return min(_PER_ITEM_CAP_USD, budget) if budget is not None else _PER_ITEM_CAP_USD


def _read_scratch_cost(scratch: Path) -> float:
    """Best-effort real spend from the throwaway worktree's isolated ledger.

    ponytail: the headless spawn API surfaces no cost on stdout; the isolated
    ledger is the honest source when the spawned session wrote one. Unknown ->
    0.0. Upgrade path if precise before/after $ matters: normalize on token
    counts (Domain Pitfall: cost drift across pricing changes)."""
    ledger = scratch / ".fno" / "ledger.json"
    try:
        data = json.loads(ledger.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return 0.0
    entries = data.get("entries") if isinstance(data, dict) else data
    if not isinstance(entries, list):
        return 0.0
    total = 0.0
    for e in entries:
        if isinstance(e, dict):
            try:
                total += float(e.get("cost_usd") or 0.0)
            except (TypeError, ValueError):
                continue
    return round(total, 4)

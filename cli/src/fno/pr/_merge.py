"""In-package port of ``scripts/lib/pr-merge.sh`` (ab-d4c98550, US1).

Skill-agnostic PR merge wrapper. Shells to ``gh``/``git`` and preserves the
caller-facing contract verbatim:

- Stdout: one JSON line ``{pr, outcome, reason, strategy[, cleanup]}`` on
  merged/skipped; failures print the same JSON shape to STDERR.
  ``outcome`` is merge truth only: whether the PR landed on main. A post-merge
  cleanup failure (local branch delete, worktree prune, sync) can never retract
  a landed merge, so it is reported as ``outcome=merged, cleanup=failed`` - the
  cleanup result in its own field, never fused into ``outcome``. A merge that
  lands never reports ``failed``.
- Exit codes: 0 merged, 1 failed (incl. bad args), 2 skipped
  (auto_merge disabled) or held, 127 gh not installed.
  ``held`` is the retry-later outcome and is never a failure claim: the merge was
  not attempted (lock contention, stale base, unreconciled stub-manifest) or its
  result could not be read back. In particular, when ``gh pr view`` itself fails
  the merge state is UNKNOWN, so the receipt says held rather than asserting a
  ``failed`` the caller would act on.
- The footnote-canonical merge guard (config.auto_merge ``enabled`` + the
  CI-green / external-review / stub-manifest guards) and the worktree
  server-side-recovery fallback are preserved. The who-may-merge gate
  (``--invoker`` + ``auto_merge.allowed_invokers``) was removed (x-04ab): the
  caller context is derivable and megawalk is deprecated, so the flag was
  redundant ceremony. A legacy ``--invoker=...`` arg is silently accepted and
  ignored so old callers never break.
- Post-merge followups (memory-pass + triage sentinels, the session_satisfied
  event, the per-PR artifact consolidation) fire for every merged outcome,
  best-effort: a followup failure never changes the already-emitted outcome.
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import sys
import time
from contextlib import contextmanager
from typing import Any, Iterator, List, Literal, Optional, Sequence

from fno.pr._proc import ToolMissing, run

_PR_RE = re.compile(r"^[1-9][0-9]*$")

# Merge serialization (parallel mode, epic x-42d5 G4, Locked Decision #9):
# builds run parallel, merges run ONE AT A TIME. The lock is held across the
# gh merge call and its post-merge followups (typically seconds), so the wait
# is short and bounded - a peer still holding it past the window is reported
# as "held" (exit 2) for the caller to retry, never an indefinite block.
#
# Scope: the lock + freshness hold cover the IMMEDIATE merge path. There is no
# queued lane anymore (x-9d11 dropped --auto): require_checks_pass is enforced
# by reading the checks under the lock and merging only on green, so every
# merge this verb performs is serialized here.
_MERGE_LOCK_WAIT_S = 120
_MERGE_LOCK_POLL_S = 5


def _emit(
    pr: int,
    outcome: str,
    reason: str,
    strategy: str,
    *,
    err: bool,
    cleanup: str = "",
) -> None:
    """Print the JSON line. ``err`` routes to stderr (failure cases).

    ``cleanup`` is emitted only when a post-merge step ran and must be surfaced
    separately from the merge outcome. A merge that lands while a post-merge
    cleanup step fails reports ``outcome=merged, cleanup=failed``: the merge
    truth lives in ``outcome`` (the field callers key on), the cleanup result in
    its own field, so a cosmetic cleanup failure can never read as a failed
    merge. Absent means no cleanup step is being reported, never an ambiguous
    silent success.

    Known over-report (documented, not narrowed): on an autonomous retry of an
    already-merged PR, ``gh pr merge`` exits non-zero ("already merged") and the
    landed-merge guard reports ``cleanup=failed`` even though no cleanup step ran
    - the merge was a no-op. The discriminator for that retry shape is not
    reliably in the error text, so the guard fails toward visibility rather than
    inventing one; a caller keying on ``cleanup`` should read it as "the merge
    call did not exit cleanly" there, not strictly "a cleanup step failed."
    """
    obj = {
        "pr": pr,
        "outcome": outcome,
        "reason": reason,
        "strategy": strategy,
    }
    if cleanup:
        obj["cleanup"] = cleanup
    line = json.dumps(obj, separators=(",", ":")) + "\n"
    (sys.stderr if err else sys.stdout).write(line)
    (sys.stderr if err else sys.stdout).flush()


def _gh(args: Sequence[str], cwd: str):
    return run(["gh", *args], cwd=cwd)


def _git(args: Sequence[str], cwd: str):
    return run(["git", *args], cwd=cwd)


def _load_auto_merge():
    from fno.config import load_settings

    return load_settings().auto_merge


def _repo_state_dir(cwd: str) -> str:
    res = _git(["rev-parse", "--show-toplevel"], cwd)
    root = res.stdout.strip() if res.ok and res.stdout.strip() else cwd
    return os.path.join(root, ".fno")


def _read_state_field(state_file: str, field: str) -> str:
    """Read ``field:`` from frontmatter, dequoting a matched pair (parser parity).

    Strips only a MATCHED surrounding quote pair (not a naive unbalanced
    strip that could mangle a value starting/ending with a quote; gemini on
    PR #524).
    """
    try:
        with open(state_file, "r", encoding="utf-8") as fh:
            for ln in fh:
                if ln.startswith(field + ":"):
                    val = ln[len(field) + 1:].strip()
                    if len(val) >= 2 and (
                        (val[0] == '"' and val[-1] == '"')
                        or (val[0] == "'" and val[-1] == "'")
                    ):
                        return val[1:-1]
                    return val
    except OSError:
        pass
    return ""


def _review_coverage_for_pr(
    pr_number: int, repo: str, head: Optional[str] = None
) -> "tuple[Optional[dict], str]":
    """The ``review_coverage`` event data for ``pr_number``, recomputed once
    when there is no usable row, or ``(None, note)``.

    loop-check emits one every gate eval (x-0eaf); a session with no manifest
    never does, which made the gate unsatisfiable for that shape - so a missing
    or head-mismatched row now fires the standalone producer once (x-3a3f).
    Python still consumes the event rather than recomputing coverage itself
    (Ownership: Rust computes, Python reads). Any failure degrades to the
    original row (or None) plus a note naming the recompute's outcome, which
    the caller treats as Unknown and refuses - never a pass.
    """
    try:
        from fno.pr._reviews import review_coverage_for_gate
    except Exception:  # noqa: BLE001 - events module unavailable -> Unknown
        return None, ""
    try:
        return review_coverage_for_gate(pr_number, repo, head)
    except Exception:  # noqa: BLE001 - corrupt log -> Unknown, not a crash
        return None, ""


def _coverage_sources(repo: str) -> list[str]:
    """The events logs a coverage read consults, for the refusal text."""
    try:
        from fno.pr._reviews import coverage_sources

        return coverage_sources(repo)
    except Exception:  # noqa: BLE001 - naming the location is best-effort
        return []


def _pr_head_oid(pr_number: int, repo: str) -> Optional[str]:
    """The PR's current ``headRefOid`` for a staleness check, or None."""
    if shutil.which("gh") is None:
        return None
    res = _gh(["pr", "view", str(pr_number), "--json", "headRefOid"], repo)
    if not res.ok:
        return None
    try:
        oid = str(json.loads(res.stdout).get("headRefOid", "")).strip()
    except (ValueError, TypeError):
        return None
    return oid or None


_REPO_FROM_URL = re.compile(r"github\.com/([^/]+/[^/]+?)(?:\.git)?/pull/")


def _row_repo(row: dict) -> Optional[str]:
    """The ``owner/name`` repo a ledger delivery row belongs to, parsed from its
    ``pr_url``. None when the row has no usable url (older rows); callers treat
    None as 'do not filter on repo' so a missing url never silently drops a plan.
    """
    url = row.get("pr_url")
    if isinstance(url, str):
        m = _REPO_FROM_URL.search(url)
        if m:
            return m.group(1)
    return None


def _plan_path_for_pr(pr_number: int, repo: Optional[str] = None) -> Optional[str]:
    """The plan_path bound to this PR's delivery row in the ledger, or None.

    The ledger is global (cross-repo), and PR numbers are per-repo, so a bare
    number match can return a foreign repo's plan. ``repo`` (``owner/name``)
    scopes the match to this repository via the row's ``pr_url``; a row without a
    parseable url is still considered, so the filter fails open rather than
    silently dropping a plan. None means no plan - the fidelity gate is a no-op
    for the PR. A ledger read failure is also None: a missing ledger must not
    block a merge that has no plan-fidelity signal to evaluate."""
    try:
        from fno import paths as _paths
        from fno.scoreboard.fold import load_ledger_rows

        for row in load_ledger_rows(_paths.ledger_json()):
            if row.get("pr_number") != pr_number:
                continue
            row_repo = _row_repo(row)
            if repo is not None and row_repo is not None and row_repo != repo:
                continue
            pp = row.get("plan_path")
            if isinstance(pp, str) and pp.strip():
                return pp
    except Exception:  # noqa: BLE001 - the gate is advisory on a missing ledger
        return None
    return None


def _coverage_refused_reason(
    cov: Optional[dict],
    head: Optional[str] = None,
    sources: Optional[list[str]] = None,
) -> str:
    """Why a coverage guard refused, for the blocked receipt line.

    Names the CAUSE and the next action, not just the count. A refusal that
    reports a bare number leaves the reader to explain it, and the nearest
    exclusion-flavoured vocabulary in this repo is ``attestation_origin``, which
    gates nothing - two workers escalated green PRs to the operator on that
    inference while actually looking at a head that had moved.

    ``head`` is the PR's observed head, passed only when the caller fetched it.
    Its absence is why the staleness branch cannot fire on an unobserved head:
    without it the reason falls back to the count, and the caller only refuses
    on a *confirmed* mismatch, so ``0 reviewed`` stays true wherever it prints.
    """
    if cov is None:
        # Name WHERE it looked, not just that it found nothing. The absence is
        # the one refusal a reader cannot diagnose from a count, and two workers
        # read the bare count as a policy problem and set about designing around
        # a gate that was already green somewhere else (x-f43c).
        where = f" (searched: {', '.join(sources)})" if sources else ""
        return f"no review_coverage event for this PR{where}"
    cov_word = str(cov.get("coverage"))
    # `uncovered` (x-5b99) is a real known zero - the exact case that used to
    # serialize as `covered` with `reviewed_count: 0`. It has to reach the
    # naming branches below: short-circuiting on it here would send every
    # zero-coverage refusal - the most common one there is - back to the bare
    # word this function exists to replace. Only `unknown` (and anything a
    # future producer invents) stops here.
    if cov_word not in ("covered", "uncovered"):
        return f"coverage {cov_word}"
    # Say the word once when it is not the legacy `covered`, then let the
    # actionable half of the message stand unchanged.
    prefix = f"coverage {cov_word}: " if cov_word != "covered" else ""
    ev_head = cov.get("head_sha")
    if head and ev_head and head != ev_head:
        return (
            f"coverage was computed at {ev_head[:8]} but HEAD is {head[:8]}; "
            "attestations are head-pinned by design - re-run the review verb at HEAD"
        )
    # Same rule as the Rust receipt, and this time the sameness is correct: the
    # rule no longer depends on which gate prints it. Never prescribe the local
    # verb while a reviewer is outstanding (it may be REQUIRED, and this gate
    # reads coverage alone, so nothing downstream catches a self-attest past
    # one), and never leave a bare wait either (an optional App that is never
    # installed sits absent forever). Name who, and point at the move that is
    # safe whichever they are.
    raw = cov.get("verdicts")
    verdicts = raw if isinstance(raw, list) else []
    absent = [
        str(v.get("name") or "")
        for v in verdicts
        if isinstance(v, dict) and v.get("verdict") == "absent" and v.get("name")
    ]
    # A reviewer that read an OLDER commit is not absent and not refused, and
    # "run the review verb at HEAD" is the wrong instruction for it: the move is
    # a re-read by that reviewer. Naming it is the whole reason a stale verdict
    # is recorded rather than dropped.
    stale = [
        str(v.get("name") or "")
        for v in verdicts
        if isinstance(v, dict) and v.get("verdict") == "stale" and v.get("name")
    ]
    if absent or stale:
        waits = []
        if absent:
            waits.append(f"waiting on {', '.join(absent)}")
        if stale:
            waits.append(
                f"{', '.join(stale)} reviewed an older commit and must re-read"
            )
        return (
            prefix + "0 reviewed (no head-pinned pass attestation; "
            f"{'; '.join(waits)} - if a reviewer there is uninstalled or no "
            "longer configured, check config.review)"
        )
    return (
        prefix
        + "0 reviewed (no head-pinned pass attestation - run the review verb at HEAD)"
    )


def _safe_int(val, default=0):
    """int() that returns default on ValueError/TypeError (finding 7)."""
    try:
        return int(val)
    except (ValueError, TypeError):
        return default


def _peer_is_identity_free(entry: Any) -> bool:
    """A peer counts as a local-attestation lane only if it has no posting identity."""
    if isinstance(entry, str):
        return True
    return isinstance(entry, dict) and not entry.get("identity")


def _is_documentation_path(path: str) -> bool:
    """A documentation path: `*.md` anywhere or anything under `docs/`.

    Mirrors is_documentation_path in loopcheck.rs - the two must agree on what
    'documentation' means or the merge gate and the stop gate classify the same
    PR differently. A single leading `./` is stripped once (lstrip would strip a
    char set and mangle `.github`/`.md`)."""
    p = (path or "").strip()
    if p.startswith("./"):
        p = p[2:]
    if not p:
        return False
    return p.endswith(".md") or p.startswith("docs/")


def _pr_payload_is_code(repo: str, pr_number: int) -> bool:
    """Whether the PR's diff carries a code payload.

    CODE iff any changed file is not documentation. Fails CLOSED - a missing gh,
    a failed view, or an unparseable file list all classify as code, so a
    degraded probe cannot bypass the coverage guard. An empty file list (no diff
    surfaced) is not code: nothing to review, no gate."""
    if not shutil.which("gh"):
        return True
    res = _gh(["pr", "view", str(pr_number), "--json", "files", "--jq", "[.files[].path]"], repo)
    if not res.ok:
        return True
    try:
        names = [str(p) for p in json.loads(res.stdout)]
    except (ValueError, TypeError):
        return True
    if not names:
        return False
    return any(not _is_documentation_path(p) for p in names)


def _harness_can_self_review() -> bool:
    """Mirror loop-check's floor boundary for the ambient harness."""
    from fno.harness_identity import present_harness_markers
    from fno.review_capability import harness_can_self_review

    families = {harness for _marker, harness, _value in present_harness_markers()}
    return len(families) == 1 and harness_can_self_review(next(iter(families)))


def _review_lane_configured(repo: str, pr_number: int = 0) -> bool:
    """Whether review is required for this PR: a configured lane, OR a code
    payload on a stock install (the self-review floor).

    Fail-closed (True) on config error: a misread config must not bypass the
    coverage guard. A stock install with no lane opts out of review UNLESS the
    payload is code and config.review.self_review_required is on (default), in
    which case the floor engages so the merge gate agrees with the stop gate.

    Peers form a local lane only via identity-free entries. A shared
    peer_identity, or a per-peer identity, means the review posts as a GitHub
    login already matched by the bot sets, not a distinct local lane. This
    mirrors resolved_local_peer_reviewers_for_author in loopcheck.rs so the
    merge gate and the loop-check gate agree on whether a lane exists for one
    PR - otherwise the loop ships DonePRGreen (no lane) while merge refuses as
    unreviewed (lane), wedging the pipeline on the same config.
    """
    try:
        from pathlib import Path

        from fno.config import load_settings_for_repo

        # Resolve the git toplevel first: load_settings_for_repo reads
        # <arg>/.fno/ with NO upward walk, so a merge invoked from a
        # subdirectory would see only the global layer, report "no lane", and
        # short-circuit the coverage guard - letting an unreviewed PR merge.
        # _repo_state_dir already does rev-parse --show-toplevel with a cwd
        # fallback, and the manifest and events reads in this file resolve the
        # root the same way.
        root = Path(_repo_state_dir(repo)).parent
        r = load_settings_for_repo(root).review
        if r.required_bots or r.optional_apps or r.reviewers:
            return True
        if r.peer_identity:
            return False
        if any(_peer_is_identity_free(p) for p in (r.peers or [])):
            return True
        # No configured lane: a code payload still requires review (the
        # self-review floor), unless the install opted out with
        # config.review.self_review_required = false. Mirrors the loop-check
        # floor so the merge gate and the stop gate cannot disagree.
        if (
            getattr(r, "self_review_required", True)
            and pr_number
            and _harness_can_self_review()
            and _pr_payload_is_code(repo, pr_number)
        ):
            return True
        return False
    except Exception:
        return True


def _code_review_attestation_required(repo: str, pr_number: int = 0) -> bool:
    """Whether this PR specifically requires a local ``code-review`` pass.

    Coverage answers whether anyone reviewed; it does not prove that a required
    local reviewer ran. Keep that second requirement explicit so an unrelated
    GitHub App review cannot satisfy the self-review gate on a direct merge.
    """
    try:
        from pathlib import Path

        from fno.config import load_settings_for_repo

        root = Path(_repo_state_dir(repo)).parent
        review = load_settings_for_repo(root).review
        configured = {
            str(name).strip().lstrip("/") for name in (review.reviewers or [])
        }
        if "code-review" in configured:
            return True
        lane_configured = bool(
            review.required_bots
            or review.optional_apps
            or review.reviewers
            or review.peer_identity
            or any(_peer_is_identity_free(p) for p in (review.peers or []))
        )
        return bool(
            not lane_configured
            and getattr(review, "self_review_required", True)
            and pr_number
            and _harness_can_self_review()
            and _pr_payload_is_code(repo, pr_number)
        )
    except Exception:
        return True


def _coverage_has_local_pass(cov: Optional[dict], reviewer: str) -> bool:
    """Whether coverage carries this head-pinned local reviewer pass."""
    if not isinstance(cov, dict):
        return False
    verdicts = cov.get("verdicts")
    if not isinstance(verdicts, list):
        return False
    target = reviewer.strip().lstrip("/")
    return any(
        isinstance(v, dict)
        and str(v.get("name") or "").strip().lstrip("/") == target
        and v.get("producer") == "local_attestation"
        and v.get("verdict") == "reviewed"
        for v in verdicts
    )


# ---------------------------------------------------------------------------
# Post-merge side-effects (best-effort; never change the merge outcome)
# ---------------------------------------------------------------------------


def _sync_graph_merge_status(merge_status: str, pr_number: int, cwd: str = "") -> None:
    """Set merge_status on the graph node carrying this pr_number (best-effort)."""
    try:
        from fno.paths import graph_json
        from fno.graph.store import locked_mutate_graph

        path = graph_json()
        if not path.exists():
            return

        def _mut(entries: List[dict]) -> List[dict]:
            for e in entries:
                if e.get("pr_number") == pr_number:
                    e["merge_status"] = merge_status
                    break
            return entries

        locked_mutate_graph(path, _mut)
    except (Exception, SystemExit):
        # Silent no-op on ANY failure (no graph, store error): the bash
        # `|| true`-guarded this, and it must never block the merge outcome.
        # SystemExit is included because locked_mutate_graph calls sys.exit(1)
        # on a corrupt graph.json - a bare `except Exception` would let that
        # abort the merge outcome before the post-merge followups run (codex
        # P2 on PR #524). KeyboardInterrupt is deliberately NOT swallowed.
        pass

    # Ship provenance used to be stamped here on a real merge, recording whoever
    # ran `fno pr merge`. That names the merger, not the implementer, and a merge
    # that lands out-of-band (gh, the web UI, or GitHub's native auto-merge)
    # bypasses this primitive entirely - so it served neither consumer. The ship
    # row now lands at `fno backlog update --pr-number` (the PR-link choke point
    # every shipped node passes through), which records the implementer.


def _find_pr_node_id(
    entries: List[dict], pr_number: int, pr_url: str = ""
) -> "Optional[str]":
    """The graph node linked to this PR, resolved by REPOSITORY (baked-in, no memory).

    The global graph is cross-project, so a bare ``pr_number`` can collide across
    repos (repo B's #5 vs repo A's #5). ``pr_url`` carries repo+number exactly, so:

      1. Match a node whose ``pr_url`` (primary or additional) equals ours - always
         unambiguous. This also finds a url-only / off-convention-branch node that
         bare ``fno backlog reconcile`` misses (its forward scan needs an int
         ``pr_number``; its reverse map needs the id in the branch name).
      2. Else a number match, but SCOPED to this repo: the matching node's own PR
         url must parse to the same ``owner/repo`` (a url-less node is accepted as
         best-effort). An ambiguous or cross-repo bare-number match is REFUSED, so
         merging repo B never closes repo A's same-numbered node (codex #403 class).

    Returns None when nothing resolves unambiguously - a safe skip, never a guess.
    """
    from fno.graph._reconcile import node_pr_refs, repo_slug_from_url

    url = (pr_url or "").strip()
    if url:
        for e in entries:
            if (e.get("pr_url") or "").strip() == url:
                return e.get("id")
            for extra in e.get("additional_prs") or []:
                if isinstance(extra, dict) and (extra.get("url") or "").strip() == url:
                    return e.get("id")

    # Number fallback needs a repo to scope against; without our slug (no/garbage
    # url) a bare-number match is cross-repo-unsafe, so refuse it entirely.
    our_slug = repo_slug_from_url(url)
    if our_slug is None:
        return None
    matches: list[str] = []
    for e in entries:
        nid = e.get("id")
        if not isinstance(nid, str):
            continue
        for num, u in node_pr_refs(e):
            if num != pr_number:
                continue
            node_slug = repo_slug_from_url((u or "").strip())
            if node_slug is None or node_slug == our_slug:
                matches.append(nid)
            break
    uniq = list(dict.fromkeys(matches))
    return uniq[0] if len(uniq) == 1 else None


def _reconcile_merged_pr_node(pr_number: int, cwd: str = "") -> None:
    """Close the just-merged PR's backlog node synchronously (baked into merge).

    ``_run_post_merge_followups`` only drops a ``.triage-pending`` sentinel for a
    later stop-hook / ritual to consume; a standalone ``fno pr merge`` from a
    worktree or bg session never fires that hook, so the node stays open - the
    exact gap that made ``fno pr merge`` no better than ``gh pr merge``. Close it
    here so the merge always closes its own loop, with no memory/workaround:

      1. Resolve THIS PR's node by number, else url (a url-only / off-convention
         link is invisible to bare reconcile).
      2. Stamp ``pr_number`` on it (idempotent) so the canonical link exists for
         this and every later pass - reconcile's forward scan keys on it.
      3. Run ``fno backlog reconcile --node <id>`` (mark done, stamp the plan,
         drop the retro sentinel) - the full, tested close path, reused not
         duplicated.

    Best-effort: any failure is a non-fatal stderr note; never blocks the merge.
    """
    try:
        from fno.paths import graph_json
        from fno.graph.store import locked_mutate_graph, read_graph

        path = graph_json()
        if not path.exists():
            return
        pr_url = ""
        view = _gh(
            ["pr", "view", str(pr_number), "--json", "url", "-q", ".url"],
            cwd or os.getcwd(),
        )
        if view.ok:
            pr_url = view.stdout.strip()
        nid = _find_pr_node_id(read_graph(path), pr_number, pr_url)
        if not nid:
            return  # no node linked to this PR - nothing to close

        def _mut(entries: List[dict]) -> List[dict]:
            for e in entries:
                if e.get("id") == nid:
                    # Backfill the PRIMARY link ONLY when it is ABSENT. A node
                    # that matched via an `additional_prs` entry already has a
                    # DIFFERENT primary pr_number; overwriting it (while keeping
                    # its old primary url) would corrupt the number<->url pair and
                    # break node_pr_refs (codex P2). The url-less node this fix
                    # targets has no primary number, so it is still backfilled.
                    if not isinstance(e.get("pr_number"), int):
                        e["pr_number"] = pr_number
                        if pr_url and not (e.get("pr_url") or "").strip():
                            e["pr_url"] = pr_url
                    break
            return entries

        locked_mutate_graph(path, _mut)

        from fno import _subprocess_util

        res = run(
            [*_subprocess_util.fno_py_cmd(), "backlog", "reconcile", "--node", nid],
            cwd=cwd or os.getcwd(),
        )
        if not res.ok:
            # A non-zero reconcile (gh query down, evidence refused) leaves the
            # node OPEN - the exact gap this closes. run() returns rather than
            # raises, so surface it explicitly instead of a silent success.
            print(
                f"fno pr merge: node reconcile for PR #{pr_number} (node {nid}) "
                f"failed: {(res.stderr or res.stdout or '').strip()[:200]}",
                file=sys.stderr,
            )
    except (Exception, SystemExit):
        # Never block the merge outcome on the node-close (mirrors
        # _sync_graph_merge_status: SystemExit covers locked_mutate_graph's
        # sys.exit on a corrupt graph).
        print(
            f"fno pr merge: post-merge node reconcile for PR #{pr_number} "
            "skipped (non-fatal)",
            file=sys.stderr,
        )


def _on_confirmed_merge(pr_number: int, cwd: str = "") -> None:
    """Every graph side-effect of a CONFIRMED (immediate) merge, in one place.

    Sync merge_status + stamp ship provenance (``_sync_graph_merge_status``), then
    close the node (``_reconcile_merged_pr_node``). The three merged code paths
    call this ONE function so the node-close can never be forgotten on one of
    them; the failure paths keep calling ``_sync_graph_merge_status`` alone.
    """
    _sync_graph_merge_status("merged", pr_number, cwd)
    _reconcile_merged_pr_node(pr_number, cwd)


def _post_merge_remote_delete(pr_number: int, repo: str, auto_merge) -> str:
    """Delete the REMOTE branch after a confirmed merge; warn-only (x-9d11).

    Branch cleanup is a separate operation from the merge and must never be
    able to fail it. `gh pr merge --delete-branch` also deletes the LOCAL
    branch, which errors with "is already used by worktree" whenever the
    session stands in that worktree - worktree-first is the standing principle,
    so the best-disciplined merge was the one most reliably reported failed.
    Splitting cleanup out: the merge command carries no delete flag at all, the
    remote ref goes through `gh api -X DELETE` against the PR's verified base
    repo, and the local branch/worktree lifecycle stays with
    `scripts/setup/archive-worktree.sh`.

    Returns the receipt's ``cleanup`` value: "" when nothing ran or the delete
    succeeded, "failed: <first line>" when it did not. Failure changes nothing
    about the merge outcome.
    """
    if not getattr(auto_merge, "delete_branch_on_merge", False):
        return ""
    # Tab separator: a nameWithOwner never contains one, and a NULL
    # headRepository (deleted fork repo) renders as an EMPTY field rather than
    # collapsing the columns together the way a space join would.
    ref = _gh(
        ["pr", "view", str(pr_number), "--json",
         "headRefName,headRepository,baseRepository",
         "-q", r'.headRefName + "\t" + (.headRepository.nameWithOwner // "") '
              r'+ "\t" + (.baseRepository.nameWithOwner // "")'],
        repo,
    )
    fields = ref.stdout.strip().split("\t") if ref.ok else []
    branch = fields[0] if fields else ""
    if not branch:
        return f"failed: remote branch name unreadable: {(ref.stderr or '').splitlines()[0][:120] if ref.stderr.strip() else 'no error output'}"
    head_repo = fields[1] if len(fields) > 1 else ""
    base_repo = fields[2] if len(fields) > 2 else ""
    # A fork PR's head branch lives on the fork, not on this repo's origin:
    # a same-repo delete would remove an unrelated SAME-NAMED branch on the
    # base repo (or error on a nonexistent one). Delete only on a POSITIVE
    # same-repo confirmation; a fork, an unreadable repo pair, or a deleted
    # head repo all skip (fail safe).
    if not (head_repo and base_repo and head_repo == base_repo):
        sys.stderr.write(
            f"pr-merge: skipping remote branch delete for PR #{pr_number}: "
            f"head repo {head_repo or '<unreadable>'} is not {base_repo or '<the base repo>'} (fork or deleted head repo)\n"
        )
        return ""
    # Delete through gh's API against the VERIFIED base repo, never `git push
    # origin`: the local clone's origin remote can point anywhere (a fork
    # clone, a renamed or mirror remote), and a delete that reaches the wrong
    # remote is the one cleanup mistake this warn-only path must not make
    # (round 11).
    res = _gh(
        ["api", "-X", "DELETE", f"repos/{base_repo}/git/refs/heads/{branch}"],
        repo,
    )
    if res.ok:
        return ""
    # An already-gone remote ref is the requested end state (repo-side
    # "delete head branches" beat us to it), not a cleanup failure.
    _out = (res.stderr or res.stdout or "")
    if "does not exist" in _out or "not found" in _out.lower() or "Reference does not exist" in _out:
        return ""
    return "failed: remote branch delete: {}".format(
        _out.splitlines()[0][:160] if _out.strip() else "no error output"
    )


def _emit_session_satisfied(pr_url: str, state_dir: str) -> None:
    """Emit a session_satisfied{source:pr_merge} event (best-effort)."""
    state_file = os.path.join(state_dir, "target-state.md")
    if not os.path.isfile(state_file):
        return
    sid = _read_state_field(state_file, "session_id")
    if not sid or sid == "null":
        return
    try:
        with open(state_file, "rb") as fh:
            gate_hash = hashlib.md5(fh.read()).hexdigest()
    except OSError:
        return
    if not gate_hash:
        return
    try:
        from pathlib import Path

        from fno.events import append_event, session_satisfied

        event = session_satisfied(
            trigger="pr_merge",
            reason="pr_merged",
            session_id=sid,
            gate_state_hash=gate_hash,
            evidence_url=pr_url or None,
            source="target",
        )
        append_event(event, events_path=Path(state_dir) / "events.jsonl")
    except Exception as exc:  # noqa: BLE001 - best-effort, surface a diagnostic
        sys.stderr.write(
            f"pr-merge: session_satisfied emit failed ({exc}); merge outcome unaffected\n"
        )


def _emit_human_touch_merge(pr_number: int, state_dir: str) -> None:
    """Emit ``human_touch{source:merge}`` for a MANUAL merge (W4 telemetry).

    Only a human at a terminal counts: the autonomous loop's ship gate runs
    this same followup path with no tty and must not inflate the touch count,
    so the gate is stdin-isatty. Best-effort: a failure prints a diagnostic
    and never changes the merge outcome.
    """
    if not sys.stdin.isatty():
        return
    # The CLI already rejects non-positive PR args (_PR_RE); this keeps the
    # helper safe for any future caller (0/negative must never match a node).
    if not isinstance(pr_number, int) or pr_number <= 0:
        return
    node_id = None
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json, resolve_canonical_repo_root

        # The graph is global across projects, so bare PR numbers collide;
        # only nodes homed in THIS repo (node.cwd == canonical root) may
        # claim the touch, and only an UNAMBIGUOUS match does (two same-repo
        # nodes on one number -> resolution=failed, never an arbitrary pick).
        root = str(resolve_canonical_repo_root())
        hits = set()
        for e in read_graph(graph_json()):
            if e.get("cwd") != root:
                continue
            if e.get("pr_number") == pr_number or any(
                isinstance(p, dict) and p.get("number") == pr_number
                for p in e.get("additional_prs") or []
            ):
                hits.add(e.get("id"))
        node_id = hits.pop() if len(hits) == 1 else None
    except Exception:
        node_id = None
    try:
        from pathlib import Path

        from fno.events import _build, append_event

        event = _build(
            "human_touch",
            "target",
            {
                "graph_node_id": node_id,
                "source": "merge",
                "resolution": "ok" if node_id else "failed",
            },
        )
        append_event(event, events_path=Path(state_dir) / "events.jsonl")
    except Exception as exc:  # noqa: BLE001 - best-effort, surface a diagnostic
        sys.stderr.write(
            f"pr-merge: human_touch emit failed ({exc}); merge outcome unaffected\n"
        )


def _run_post_merge_followups(pr_number: int, strategy: str, cwd: str) -> None:
    state_dir = _repo_state_dir(cwd)
    state_file = os.path.join(state_dir, "target-state.md")

    # Memory-pass sentinel.
    try:
        with open(os.path.join(state_dir, ".memory-pass-pending"), "w", encoding="utf-8") as fh:
            fh.write(f"{pr_number}\n")
    except OSError:
        pass

    # Retro-triage fast-path sentinel.
    try:
        mode = "interactive"
        if os.path.isfile(os.path.join(state_dir, "megawalk-state.md")) or os.environ.get(
            "TARGET_MISSION_ID"
        ):
            mode = "autonomous"
        plan_path = _read_state_field(state_file, "plan_path")
        session_id = _read_state_field(state_file, "session_id")
        pr_url = ""
        res = _gh(["pr", "view", str(pr_number), "--json", "url", "-q", ".url"], cwd)
        if res.ok:
            pr_url = res.stdout.strip()
        sentinel = {
            "pr_number": pr_number,
            "pr_url": pr_url,
            "mode": mode,
            "plan_path": plan_path,
            "session_id": session_id,
        }
        with open(os.path.join(state_dir, ".triage-pending"), "w", encoding="utf-8") as fh:
            fh.write(json.dumps(sentinel, separators=(",", ":")))
    except Exception:
        pass

    # Auto-complete signal.
    try:
        pr_url = ""
        res = _gh(["pr", "view", str(pr_number), "--json", "url", "-q", ".url"], cwd)
        if res.ok:
            pr_url = res.stdout.strip()
        _emit_session_satisfied(pr_url, state_dir)
    except Exception:
        pass

    # W4 touch telemetry: a manual (tty) merge is a human steering action.
    try:
        _emit_human_touch_merge(pr_number, state_dir)
    except Exception as exc:
        sys.stderr.write(
            f"pr-merge: human_touch emit failed ({exc}); merge outcome unaffected\n"
        )

    # Per-PR artifact consolidation (best-effort; degrades cleanly when the
    # script is absent, e.g. a bare pip install). The consolidator lives in the
    # PLUGIN tree (`<plugin>/scripts/lib/consolidate-artifacts.sh`), not the
    # target repo, so resolve the plugin root first - else `fno pr merge` run in
    # a footnote-managed target repo would silently skip it every time (codex P2
    # on PR #524). CLAUDE_PLUGIN_ROOT / FNO_REPO_ROOT are read directly (a
    # PRIVATE resolution, not the shared resolve_repo_root/resolve_plugin_script
    # names), so this stays out of scope for the shellout-drift guard (flock-
    # pattern carveout cv-ca99e324 posture); os.path.dirname(state_dir) is the
    # last-resort fallback (correct in the dogfooded footnote repo itself).
    try:
        plugin_root = (
            os.environ.get("CLAUDE_PLUGIN_ROOT")
            or os.environ.get("FNO_REPO_ROOT")
            or os.path.dirname(state_dir)
        )
        script = os.path.join(plugin_root, "scripts", "lib", "consolidate-artifacts.sh")
        if os.path.isfile(script):
            env = dict(os.environ, PR_NUMBER=str(pr_number))
            run(["bash", script], cwd=cwd, env=env)
    except Exception:
        sys.stderr.write(
            "pr-merge: artifact consolidation failed; merge outcome unaffected\n"
        )


# ---------------------------------------------------------------------------
# Merge serialization + stale-base hold (parallel mode G4, LD#9)
# ---------------------------------------------------------------------------


_MergeLockState = Literal["acquired", "held", "unavailable"]


@contextmanager
def _merge_lock() -> Iterator[_MergeLockState]:
    """Serialize merges repo-wide; yield ``acquired`` | ``held`` | ``unavailable``.

    One ``merge:<canonical-root>`` claim per project (repo-local routing, so
    every worktree lane contends on the SAME lock - like ``walker:<root>``),
    pid-liveness anchored so a crashed merger frees it instantly. Acquisition
    polls for up to ``_MERGE_LOCK_WAIT_S`` (a merge holds it for seconds), then
    yields ``held``. A claims-layer error yields ``unavailable`` and the merge
    proceeds unserialized: the lock is coordination, GitHub stays the merge
    authority, and our own tooling failing must never block a merge.
    """
    state: Literal["acquired", "held", "unavailable"] = "acquired"
    key = holder = release = None
    # Acquisition happens fully BEFORE the yield: an exception the consumer
    # body throws into the generator must reach the finally-release, never an
    # except-then-yield-again (which would RuntimeError inside contextmanager).
    try:
        from fno.claims.core import CLAIM_UNAVAILABLE, acquire_claim, release_claim
        from fno.paths import resolve_canonical_repo_root

        key = f"merge:{resolve_canonical_repo_root()}"
        holder = f"pr-merge:{os.getpid()}"
        deadline = time.monotonic() + _MERGE_LOCK_WAIT_S
        while True:
            try:
                acquire_claim(key, holder, reason="serialized PR merge (LD#9)")
                release = release_claim
                break
            except CLAIM_UNAVAILABLE:
                # At the exact moment two mergers are racing hardest, the
                # outer except Exception below yields "unavailable" (lock
                # disabled entirely), which is the wrong degrade for
                # contention specifically when the whole point is LD#9's
                # merge serialization under exactly this condition.
                if time.monotonic() >= deadline:
                    state = "held"
                    break
                time.sleep(_MERGE_LOCK_POLL_S)
    except Exception as exc:  # noqa: BLE001 - fail-open: lock is best-effort
        sys.stderr.write(f"pr-merge: merge lock unavailable ({exc}); proceeding\n")
        state = "unavailable"
    try:
        yield state
    finally:
        if release is not None and state == "acquired":
            assert key is not None and holder is not None  # set together before release
            try:
                release(key, holder)
            except Exception:  # noqa: BLE001 - pid-liveness frees it anyway
                pass


def _live_lane_count() -> int:
    """Live parallel-lane slots (0 on any probe miss, keeping sequential paths
    byte-identical: the stale-base hold below only arms while lanes run)."""
    try:
        from fno.claims.lanes import active_lane_count

        return active_lane_count()
    except Exception as exc:  # noqa: BLE001
        # A probe miss disarms the stale-base hold entirely - leave the audit
        # breadcrumb so an unguarded merge is distinguishable after the fact.
        sys.stderr.write(
            f"pr-merge: lane probe unavailable ({exc}); merging without freshness hold\n"
        )
        return 0


def _behind_by(pr_number: int, cwd: str) -> int:
    """Commits the PR head is behind its base branch. 0 on any probe miss:
    the hold must never block a merge because our own read failed, but each
    miss leaves a stderr breadcrumb - a gh outage is likeliest exactly when
    many lanes hammer gh, i.e. when the hold matters most."""

    def _miss(why: str) -> int:
        sys.stderr.write(
            f"pr-merge: stale-base probe unavailable ({why}); "
            "merging without freshness hold\n"
        )
        return 0

    try:
        view = _gh(
            ["pr", "view", str(pr_number), "--json", "baseRefName,headRefName"], cwd
        )
        if not view.ok:
            return _miss("gh pr view failed")
        try:
            refs = json.loads(view.stdout or "{}")
        except json.JSONDecodeError:
            return _miss("unparseable pr view output")
        base = refs.get("baseRefName") if isinstance(refs, dict) else None
        head = refs.get("headRefName") if isinstance(refs, dict) else None
        if not base or not head:
            return _miss("missing base/head ref")
        res = _gh(
            ["api", f"repos/{{owner}}/{{repo}}/compare/{base}...{head}", "-q", ".behind_by"],
            cwd,
        )
        if not res.ok:
            return _miss("gh compare failed")
        return int(res.stdout.strip())
    except Exception as exc:  # noqa: BLE001 - the hold must never BLOCK a merge
        return _miss(f"probe error: {exc}")


# ---------------------------------------------------------------------------
# Main flow
# ---------------------------------------------------------------------------


def run_merge(argv: Sequence[str], cwd: Optional[str] = None) -> int:
    repo = cwd or os.getcwd()
    pr_raw = ""
    for arg in argv:
        # A legacy ``--invoker=...`` is silently accepted and ignored (x-04ab
        # removed the flag + its gate). Never break a merge command on a stray
        # flag an un-updated caller still passes.
        if arg.startswith("--invoker="):
            continue
        elif arg[:1].isdigit():
            pr_raw = arg
        else:
            sys.stderr.write(f"Error: unknown arg '{arg}'\n")
            return 1

    if not pr_raw:
        sys.stderr.write("pr_number required\n")
        return 1

    # pr_number must be a positive integer.
    if not _PR_RE.match(pr_raw):
        _emit(0, "failed", f"invalid pr number: {pr_raw}", "none", err=True)
        return 1
    pr_number = int(pr_raw)

    # (-1) Incarnation fence (x-eea5 1.3): a losing incarnation - a forked or
    # supervisor-restarted session whose session:<uuid> single-writer claim
    # another incarnation now holds - must not merge by construction. Read-only,
    # fail-closed; no resolvable identity -> invisible (proceed). Same outcome
    # vocabulary as the other merge gates.
    try:
        from fno.claims.incarnation import incarnation_fence_blocks, resolve_fence_session_uuid

        _fence_uuid = resolve_fence_session_uuid(repo)
        _blocked, _fence_reason = incarnation_fence_blocks(_fence_uuid)
    except Exception as _exc:  # noqa: BLE001 - a fence-CODE crash fails OPEN (proceed + log); the helper already fail-closes on an unreadable claims dir (AC4-FR)
        _blocked, _fence_reason = False, ""
        sys.stderr.write(f"incarnation-fence: check crashed ({_exc}); proceeding\n")
        # F4: a fail-open bypass must not read as a clean merge to stdout-JSON
        # automation. Emit a gate_escape (reason=other, the documented catch-all)
        # so retro/audit rank the skipped fence as autonomy debt. Telemetry only.
        try:
            from fno.events.gate_escape import emit_gate_escape

            emit_gate_escape(
                "other",
                pr=pr_number,
                detail=f"incarnation-fence check crashed ({type(_exc).__name__}: {_exc}); merge proceeded fail-open",
                cwd=repo,
            )
        except Exception:  # noqa: BLE001 - telemetry never blocks the merge
            pass
    if _blocked:
        _emit(pr_number, "blocked", _fence_reason, "none", err=True)
        return 2

    # (0) Stub-manifest hold: a `contract`-tier dependent's PR must not merge
    # while it carries an unreconciled stub-manifest (mocks would ship). Checked
    # BEFORE the auto_merge gate so auto-merge cannot bypass it (AC7-EDGE), and
    # it no-ops for every non-contract PR so the default `hard` path is unchanged
    # (AC6-EDGE).
    try:
        from fno.stub_manifest import unreconciled_manifest_for_pr

        # Resolve the repo top-level: manifests are written under the PROJECT
        # root's `.fno/`, so a merge invoked from a subdirectory must not look
        # under that subdir (codex P2). Falls back to `repo` if git can't say.
        top = _git(["rev-parse", "--show-toplevel"], repo)
        root = top.stdout.strip() if top.ok and top.stdout.strip() else repo
        held = unreconciled_manifest_for_pr(pr_number, root)
    except Exception:
        held = None  # never let the guard's own failure block a normal merge
    if held:
        if held.get("_malformed"):
            detail = "malformed stub-manifest (cannot prove stubs are gone)"
        else:
            detail = f"unreconciled stub-manifest ({len(held.get('stubs', []))} stub(s))"
        _emit(
            pr_number,
            "held",
            f"contract dependent {held.get('_node')} carries a {detail}; "
            "reconcile before merge",
            "none",
            err=False,
        )
        return 2

    # (1) Short-circuit if auto-merge is disabled. The who-may-merge gate
    # (--invoker + allowed_invokers) was removed (x-04ab): auto-merge is gated
    # by `enabled` plus the CI-green / external-review / stub-manifest guards.
    auto_merge = _load_auto_merge()
    if not auto_merge.enabled:
        _emit(
            pr_number,
            "skipped",
            "auto_merge disabled",
            "none",
            err=False,
        )
        return 2

    # (1b) Honor THIS run's resolved decision, not just the project policy.
    # `auto_merge.enabled` is standing policy; the manifest's
    # `auto_merge_approved` is what init resolved after folding in the per-run
    # modifiers, and a per-run `no-merge` (which `/target bg` injects by
    # default, via harness_map._AUTONOMOUS_COMMAND) sets it false while
    # `enabled` stays true. That fold is one-directional by design: the
    # `auto-merge` token grants nothing on its own. Without this the
    # sanctioned verb is a WEAKER gate than raw `gh pr merge`, which the
    # git-protection hook already guards on this same field.
    # Absent manifest or absent field -> proceed: a manual `fno pr merge`
    # outside a target session is legitimate and must not start refusing.
    approved = _read_state_field(
        os.path.join(_repo_state_dir(repo), "target-state.md"),
        "auto_merge_approved",
    )
    if approved and approved.strip().lower() not in ("true", "yes", "1"):
        # Name WHICH input set the posture (x-9d11): the operator's first
        # question on this refusal is "what layer said no". A pre-provenance
        # manifest carries no source; that reads as unknown, never a guess.
        source = (_read_state_field(
            os.path.join(_repo_state_dir(repo), "target-state.md"),
            "auto_merge_source",
        ) or "").strip() or "unknown (pre-provenance manifest)"
        _emit(
            pr_number,
            "skipped",
            f"per-run no-merge (manifest auto_merge_approved is not true; "
            f"auto_merge_source: {source})",
            "none",
            err=False,
        )
        return 2

    # (2) gh must be installed.
    if shutil.which("gh") is None:
        _emit(pr_number, "failed", "gh CLI not installed", "none", err=True)
        return 127

    # (2a) Coverage guard (x-0eaf): the sanctioned merge must not land a PR
    # nothing reviewed. Consume the review_coverage event loop-check emits
    # (Ownership: Rust computes, Python reads); missing/stale/zero/unknown
    # refuses (fail closed). Runs only when auto_merge is enabled (step 1), so a
    # manual `gh pr merge` on a non-auto-merge repo is untouched - the
    # discriminator is auto_merge.enabled, not attendance. After the gh check so
    # a missing gh still reports its own exit 127. Skipped when no review lane is
    # configured (x-0eaf boundary: a stock install opted out of review).
    # Cache both checks: coverage answers whether anyone reviewed, while the
    # local-attestation check preserves a specifically required code-review.
    review_lane = _review_lane_configured(repo, pr_number)
    code_review_required = _code_review_attestation_required(repo, pr_number)
    # Head fetched up front (x-3a3f): the recompute below needs it to pin the
    # emitted event to what would actually merge, and the staleness comparison
    # needs it anyway. A failed fetch returns None and neither consumer can
    # act on it - same best-effort stance as before, one round trip earlier.
    head: Optional[str] = _pr_head_oid(pr_number, repo) if review_lane else None
    # Same lane guard as the head fetch: with no lane configured `cov` is never
    # consulted (`covered` short-circuits, `covered_head` is lane-gated), and
    # the read is no longer free - a missing row would fire the 120s recompute
    # subprocess and append coverage rows nobody on this path asked for.
    cov, recompute_note = (
        _review_coverage_for_pr(pr_number, repo, head) if review_lane else (None, "")
    )
    covered = (
        not review_lane
        or (
            cov is not None
            and cov.get("coverage") == "covered"
            and _safe_int(cov.get("reviewed_count"), 0) > 0
            and (
                not code_review_required
                or _coverage_has_local_pass(cov, "code-review")
            )
        )
    )
    if covered and cov is not None and review_lane:
        # Staleness: the event pins a head; if the PR head moved after the gate
        # eval, the coverage no longer describes what would merge. A recompute
        # ran against this same head when the row was stale or missing, so a
        # mismatch here means the recompute's own output disagrees with the PR
        # - still a confirmed mismatch, still refuses.
        ev_head = cov.get("head_sha") if cov else None
        if head and ev_head and head != ev_head:
            covered = False
    if not covered:
        if code_review_required and not _coverage_has_local_pass(cov, "code-review"):
            refusal = (
                "required code-review has no head-pinned local pass attestation; "
                "run the harness review verb at HEAD, then emit the code-review attestation"
            )
        else:
            refusal = _coverage_refused_reason(
                cov, head, _coverage_sources(repo) if cov is None else None
            )
        # Name the recompute and its outcome: a refusal reporting only a count
        # is what taught two workers to design around a gate that was green
        # somewhere else (x-3a3f). Bracket append, never paren-splice surgery
        # on a builder's output: a reason whose trailing paren closes an inner
        # clause (a searched list, a truncated sha) would swallow the note
        # into the wrong parenthetical.
        if recompute_note:
            refusal = f"{refusal} [{recompute_note}]"
        _emit(
            pr_number,
            "blocked",
            f"unreviewed merge refused: {refusal}",
            "none",
            err=True,
        )
        return 2

    # The covered head pins the merge so a racing push after the coverage check
    # cannot land an unreviewed head via `--auto`'s queue (x-0eaf TOCTOU). The
    # staleness check above already refused a current mismatch; this makes gh
    # itself refuse if the head moves between here and the merge.
    covered_head = (cov.get("head_sha") or "") if cov and review_lane else ""

    # (2c) Plan fidelity guard (x-cbab): the inverse of the coverage guard on the
    # ownership axis - review_coverage is Rust-computed/Python-read; plan fidelity
    # is Python-computed (fno.plan.fidelity)/read here. A plan whose declared
    # deliverables did not all ship refuses the merge unless each shortfall
    # carries a carveout (a PR-body sentence is not one). Skipped when the PR
    # carries no plan (no denominator) or is not a code payload. The join is
    # plan-grain, so an inline run with no separate planning thread has zero
    # planned rows and passes; this catches an orphan plan that never shipped.
    # A merge gate is required because a stop-gate-only check is skipped by a
    # direct `fno pr merge`; the stop gate (loopcheck.rs) holds the other path.
    _plan_path = _plan_path_for_pr(pr_number, repo)
    if _plan_path and _pr_payload_is_code(repo, pr_number):
        from fno.plan.fidelity import compute_plan_fidelity

        try:
            _fid = compute_plan_fidelity(plan_path=_plan_path)
        except Exception as exc:  # noqa: BLE001 - fail OPEN: a broken probe must not wedge a green merge
            _fid = {"refused": False, "reason": f"fidelity probe degraded: {exc}"}
        if _fid.get("refused"):
            _emit(
                pr_number,
                "blocked",
                f"plan fidelity refused: {_fid.get('reason', 'uncovered shortfall')}",
                "none",
                err=True,
            )
            return 2

    # (2b) Merge serialization + stale-base hold (parallel mode G4, LD#9).
    # Builds run parallel; merges run one at a time, and while lanes are live a
    # PR whose head is behind its base is held for `fno pr rebase` first, so a
    # lane never merges a stale base. Both checks run UNDER the lock: a peer
    # merge landing between the freshness read and our merge is exactly the
    # race the lock exists to close. Sequential runs (no live lanes) skip the
    # freshness hold and see only an uncontended lock - behavior unchanged.
    with _merge_lock() as lock:
        if lock == "held":
            _emit(
                pr_number,
                "held",
                "merge serialized: another merge holds the lock; retry",
                "none",
                err=False,
            )
            return 2
        if _live_lane_count() > 0:
            behind = _behind_by(pr_number, repo)
            if behind > 0:
                _emit(
                    pr_number,
                    "held",
                    f"stale base: head is {behind} commit(s) behind base with "
                    "parallel lanes live; run fno pr rebase, then retry",
                    "none",
                    err=False,
                )
                return 2
        # (2c) Stacked-base guard: a base branch that no longer leads to the
        # default branch merges green and ships nothing. Inside the lock because
        # the event that kills a base IS a peer merge, which is what the lock
        # serializes. `_behind_by` above cannot see this: it compares head to
        # base, and a PR stacked on an already-landed base is 0 behind it.
        # A refusal needs a retarget, so it is `blocked` (an operator action),
        # never `held` (retry the same command). An unevaluated probe proceeds
        # with a breadcrumb, matching `_behind_by`: our own read failing must
        # not wedge a merge.
        from fno.pr import _base_lineage

        verdict, why = _base_lineage.lineage_verdict(pr_number, repo)
        if verdict == "stale":
            if _base_lineage.bypassed():
                _base_lineage.emit_bypass_escape(pr_number, repo, why)
                sys.stderr.write(
                    f"pr-merge: stacked-base guard bypassed "
                    f"({_base_lineage.BYPASS_ENV}); {why}\n"
                )
            else:
                _emit(pr_number, "blocked", f"stacked base refused: {why}", "none", err=True)
                return 2
        elif verdict == "unknown":
            sys.stderr.write(
                f"pr-merge: stacked-base probe unavailable ({why}); "
                "merging without the lineage guard\n"
            )
        return _do_merge(pr_number, auto_merge, repo, covered_head)


def _checks_verdict(pr_number: int, repo: str) -> tuple[str, dict, str]:
    """CI verdict for the PR plus the head it describes.

    Borrows `verdict_for` rather than hand-rolling a statusCheckRollup read: a
    second opinion on what "green" means is how two surfaces drift apart. The
    fetch goes through this module's own `_gh` so it sits on the same process
    seam as every other call here. An unreadable rollup is ``unknown``, which
    the caller treats as not-green (fail closed).

    ``headRefOid`` rides along because a verdict is only meaningful for the SHA
    it was computed on: the caller pins the merge to that SHA so a push landing
    between this read and the merge cannot slip an unverified head through.
    """
    from fno.pr._status import verdict_for

    def _miss(why: str) -> tuple[str, dict, str]:
        # Named, like _behind_by's own miss path: "checks are unknown" alone
        # cannot tell a broken gh from a PR that simply has no checks, and the
        # operator only ever sees the emitted reason.
        return ("unknown", {"why": why}, "")

    # ToolMissing is deliberately NOT caught: the module contract reserves 127
    # for a missing gh, and both sibling handlers emit it. Swallowing it here
    # would demote that to a generic exit-1 "checks are unknown".
    res = _gh(
        ["pr", "view", str(pr_number), "--json", "state,statusCheckRollup,headRefOid"],
        repo,
    )
    if not res.ok:
        return _miss(f"gh exited {res.returncode}")
    if not (res.stdout or "").strip():
        return _miss("gh returned no output")
    try:
        data = json.loads(res.stdout)
    except json.JSONDecodeError:
        return _miss("gh returned unparseable JSON")
    rollup = data.get("statusCheckRollup") or []
    # Whole-rollup semantics: with require_checks_pass, every check must pass.
    # A required-vs-optional split would need branch-protection context that
    # `gh pr view` does not expose - its statusCheckRollup entries carry no
    # isRequired key (live-probed on gh 2.95.0, export_pr.go emits none), so a
    # filter keyed on that annotation can never fire. Held is the fail-safe
    # verdict for an optional check that never settles.
    verdict, _exit, counts = verdict_for(rollup)
    return (verdict, counts, (data.get("headRefOid") or "").strip())


def _already_armed(pr_number: int, repo: str) -> bool:
    """True when GitHub's native auto-merge queue owns this PR (finalize armed
    it). ONE probe argv shared by every executor that must stand down (x-9d11
    AC5-CON), so a probe-shape change cannot drift between merge paths."""
    res = _gh(
        ["pr", "view", str(pr_number), "--json", "autoMergeRequest",
         "-q", ".autoMergeRequest.enabled"],
        repo,
    )
    return res.ok and res.stdout.strip() == "true"


def _do_merge(pr_number: int, auto_merge, repo: str, covered_head: str = "") -> int:
    """Steps (3)-(4): build + run the gh merge and classify the outcome."""
    # AC5-CON (x-9d11): exactly one arming path. finalize.rs owns GitHub's
    # auto-merge queue; if the PR is already armed there, merging here would
    # race the queue. Say so and stand down - the queue merges it when checks
    # pass, so the merge is not being lost, just not duplicated.
    try:
        armed = _already_armed(pr_number, repo)
    except ToolMissing:
        # _already_armed calls _gh, which can raise ToolMissing same as the
        # sibling checks/merge calls below - it owes the same handler (review
        # round 12).
        _emit(pr_number, "failed", "gh CLI not installed", "none", err=True)
        return 127
    if armed:
        _emit(
            pr_number,
            "skipped",
            "PR already armed in GitHub's auto-merge queue (armed by fno-agents "
            "finalize at the terminal); the queue merges it when checks pass",
            auto_merge.merge_strategy,
            err=False,
        )
        return 2

    # (3) Build command.
    strategy = auto_merge.merge_strategy
    cmd: List[str] = ["pr", "merge", str(pr_number), f"--{strategy}"]
    # Deliberately NO --delete-branch (x-9d11): it makes gh delete the LOCAL
    # branch too, which fails "is already used by worktree" from inside the
    # worktree and can make a landed merge report failed. Remote cleanup runs
    # post-merge via _post_merge_remote_delete; the local branch/worktree is
    # archive-worktree.sh's lifecycle.
    # Deliberately NO --auto either (x-9d11): `--auto` was the second arming
    # path (finalize.rs arms GitHub's queue at the terminal; this verb queued
    # its own), and a repo without the auto-merge feature rejected the flag
    # outright. One arming path: this verb EXECUTES. require_checks_pass is
    # enforced here, before the merge call, instead of delegated to the queue
    # (x-8543: an already-green PR merges without the repo having the feature).
    # Set when THIS process is the one vouching for the checks; the worktree
    # recovery path reads it so its server-side merge is pinned to the same
    # SHA the verdict came from.
    verified_head = ""
    if auto_merge.require_checks_pass:
        try:
            verdict, counts, head_read = _checks_verdict(pr_number, repo)
        except ToolMissing:
            _emit(pr_number, "failed", "gh CLI not installed", "none", err=True)
            return 127
        verified_head = head_read
        if verdict != "green":
            # pending = wait for required checks (retry when green). unknown =
            # no rollup at all (gh failure, or a repo with no CI): retry-later,
            # never a failed-ship stamp - a PR with no checks configured needs
            # require_checks_pass=false, not a red graph status (round 7).
            _emit(
                pr_number,
                "held" if verdict in ("pending", "unknown") else "failed",
                f"checks are {verdict} ({counts}); "
                f"require_checks_pass forbids merging without green",
                strategy,
                err=verdict not in ("pending", "unknown"),
            )
            if verdict not in ("pending", "unknown"):
                # Match the pre-existing failure path: a node left reading
                # `queued` from an earlier attempt would otherwise stay queued
                # after a red refusal, and the scoreboard consumes that field.
                _sync_graph_merge_status("failed", pr_number)
            return 2 if verdict in ("pending", "unknown") else 1
        if not verified_head:
            _emit(
                pr_number,
                "failed",
                "checks read green but the PR head SHA was unreadable; refusing "
                "to merge a head the verdict cannot be pinned to",
                strategy,
                err=True,
            )
            _sync_graph_merge_status("failed", pr_number)
            return 1
        # A verdict belongs to the SHA it was computed on. Between that read and
        # this merge, another actor can push, and nothing upstream re-checks on
        # our behalf. Pin the merge to the verified head so a racing push makes
        # gh refuse instead of merging an unverified (and possibly red) commit.
        # Server-side required-check rules would cover this too, but
        # require_checks_pass exists precisely for repos without them.
    # x-0eaf: pin the merge to the covered head so a racing push cannot land an
    # unreviewed commit. gh refuses if the head moved.
    if covered_head:
        cmd += ["--match-head-commit", covered_head]
    elif verified_head:
        cmd += ["--match-head-commit", verified_head]

    # (4) Run + classify.
    try:
        res = _gh(cmd, repo)
    except ToolMissing:
        _emit(pr_number, "failed", "gh CLI not installed", "none", err=True)
        return 127

    output = (res.stdout or "") + (res.stderr or "")
    if res.ok:
        _emit(
            pr_number,
            "merged",
            "merged immediately",
            strategy,
            err=False,
            cleanup=_post_merge_remote_delete(pr_number, repo, auto_merge),
        )
        _on_confirmed_merge(pr_number, repo)
        _run_post_merge_followups(pr_number, strategy, repo)
        return 0

    # Failure path. A worktree-local post-merge step can fail even though the
    # SERVER-SIDE merge already landed (recurring PR #393/#395 bite).
    first_line = output.splitlines()[0][:200] if output.strip() else ""
    # ALWAYS re-read the merge state before reporting failure. `gh pr merge
    # --delete-branch` exits non-zero whenever a POST-merge step fails after a
    # successful server-side merge - the local branch delete (worktree-held), the
    # base-branch checkout (main held by the canonical worktree in a worktree-first
    # repo), a remote delete, a sync - and the error phrasing varies across git
    # versions and failure points. Matching phrasings ages badly; the durable
    # signal is the PR's merged state. If it landed, report merged with the
    # cleanup failure in its own field - never failed. An autonomous caller keying
    # off `outcome` then gets merge truth regardless of what post-merge cleanup did.
    view = _gh(["pr", "view", str(pr_number), "--json", "mergedAt", "-q", ".mergedAt"], repo)
    if view.ok:
        landed = view.stdout.strip()
        if landed and landed != "null":
            _git(["fetch", "origin"], repo)
            # The gh step already reported a cleanup-shaped failure; run the
            # remote delete anyway and surface both in the cleanup field.
            remote = _post_merge_remote_delete(pr_number, repo, auto_merge)
            cleanup = "failed" if not remote else f"failed; remote delete {remote}"
            _emit(
                pr_number,
                "merged",
                f"merged server-side; post-merge cleanup failed: {first_line}",
                strategy,
                err=False,
                cleanup=cleanup,
            )
            # Arm the post-merge sentinels: the merge landed, so retro/triage and
            # the memory-pass fire as on a clean merge. On a rare autonomous retry
            # against an already-merged PR this re-arms them (sentinel churn, not
            # corruption - graph writes are idempotent); the "already merged"
            # discriminator is not reliably in the error text, so the guard does
            # not try to suppress it. This is a deliberate behavior (arm for the
            # landed merge), not a cosmetic side effect.
            _on_confirmed_merge(pr_number, repo)
            _run_post_merge_followups(pr_number, strategy, repo)
            return 0

    # The merge did NOT land. gh can refuse before merging when the branch is
    # checked out in another worktree (the checkout-refused phrasing "is already
    # used by worktree" / "already checked out"); recover via the server-side API
    # so a worktree-held branch still merges. Landed cases already returned above,
    # so reaching here means not-merged - this is the API fallback, not a recovery
    # for a post-merge cleanup failure.
    if re.search(r"is already used by worktree|already checked out", output, re.IGNORECASE):
        # Carry the head pin when we have one. Reaching here from the no-auto
        # fallback means THIS process vouched for the checks at a specific SHA,
        # and this API call would otherwise merge whatever the head is now -
        # silently undoing the pin on the `--match-head-commit` retry, in the
        # very path a worktree run takes. `sha` is the endpoint's equivalent
        # guard: the merge is refused unless the head still matches.
        api_args = [
            "api",
            "--method",
            "PUT",
            f"repos/{{owner}}/{{repo}}/pulls/{pr_number}/merge",
            "-f",
            f"merge_method={strategy}",
        ]
        # covered_head first: the review lane sets it when require_checks_pass
        # is off, leaving verified_head empty - dropping it here (round 7)
        # reopened the x-0eaf TOCTOU on exactly the worktree path every run
        # takes.
        _pinned_head = covered_head or verified_head
        if _pinned_head:
            api_args += ["-f", f"sha={_pinned_head}"]
        api = _gh(api_args, repo)
        if api.ok:
            _git(["fetch", "origin"], repo)
            _emit(
                pr_number,
                "merged",
                "merged server-side (worktree fallback)",
                strategy,
                err=False,
                cleanup=_post_merge_remote_delete(pr_number, repo, auto_merge),
            )
            _on_confirmed_merge(pr_number, repo)
            _run_post_merge_followups(pr_number, strategy, repo)
            return 0

    # Merge state never became readable: `gh pr view` itself failed, so we cannot
    # tell a landed merge from a failed one. Reporting `failed` here would assert
    # merge truth we do not have - the same defect as the phrasing match this
    # guard replaced, one level up - and an autonomous caller keying on `outcome`
    # would retry a merge that may already have landed. Report `held` (exit 2,
    # the established retry-later signal) so the uncertainty stays IN the receipt
    # instead of being flattened into a false negative. A retry whose view read
    # succeeds then reports the truth either way.
    if not view.ok:
        _emit(
            pr_number,
            "held",
            "merge state unreadable (gh pr view failed: "
            f"{(view.stderr or '').splitlines()[0][:120] if view.stderr.strip() else 'no error output'}); "
            "cannot confirm whether the merge landed - retry",
            strategy,
            err=False,
        )
        return 2

    # Unrecovered failure: classify and report.
    reason = first_line
    if re.search(r"protected", output, re.IGNORECASE):
        reason = "branch protected"
    elif re.search(r"not mergeable", output, re.IGNORECASE):
        reason = "not mergeable (conflicts or base changed)"
    elif re.search(r"required review", output, re.IGNORECASE):
        reason = "required review pending"
    _emit(pr_number, "failed", reason, strategy, err=True)
    _sync_graph_merge_status("failed", pr_number)
    return 1

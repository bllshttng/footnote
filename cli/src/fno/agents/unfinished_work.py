"""The unfinished-work report: the operator question the fleet watchdog answers.

The verdict classifier in :mod:`fno.agents.watchdog` stays the internal
recovery engine (wake, reroute, reap, retire). This module answers the
outcome question the operator actually asks: was work started and never
finished? Four dimensions, each finding naming the one verb that clears it:

- ``started_free_claim``: an in_progress node whose claim is free, ranked by
  idle age, with the branch's commits ahead of ``origin/main`` where a
  worktree resolves. Clear: ``/fno:target <node>``.
- ``done_ahead_of_main``: a done node whose worktree branch still carries
  commits ahead of a freshly fetched ``origin/main``. Clear: the stranded
  recovery verb scoped to the repository.
- ``dirty_ownerless_worktree``: a worktree with uncommitted paths and no
  authoritative live owner. Clear: adopt or finish it.
- ``open_pr_ownerless``: a PR open past 24h whose node has no live owner.
  Clear: ``/fno:pr check <number>``.

Liveness is read ONLY from pid incarnation and transcript truth (the
authorities the fleet already trusts). A stored status word, registry
absence, or display name contributes no liveness verdict: unreadable
evidence preserves the candidate as unmeasurable (the dimension reads
unknown, never clean) because the cost of guessing wrong is somebody's
uncommitted work.

The commit metric is ``git rev-list --count origin/main..HEAD`` after one
``git fetch origin main`` per repository. The upstream tracking ref is not a
substitute on this path: a stale remote-tracking ref inflated a measured
count to 936 against a true 8, and a report that can be wrong by two orders
of magnitude on its first line is untrustworthy. The stranded-worktree
module's own unpushed probe is untouched; it protects destructive cleanup
and answers a different question.

The main worktree of each repository is excluded from the dirty dimension:
the canonical checkout is a shared surface with transient tenants (operator
scratch files read as dirt), so the owner join cannot answer for it.

``classify()`` is pure over injected observations; ``collect_observations()``
is the IO seam; ``build_report()`` is the one producer both the manual verb
and the scheduled tick consume.
"""
from __future__ import annotations

import json
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional, Sequence

from fno.worktree_stranded import resolve_node_id

# --- kinds and dimension vocabulary ----------------------------------------

KIND_STARTED = "started_free_claim"
KIND_DONE_AHEAD = "done_ahead_of_main"
KIND_DIRTY = "dirty_ownerless_worktree"
KIND_PR = "open_pr_ownerless"

#: Canonical order = actionable severity order: work stopped mid-node first,
#: then possibly-stranded committed work, then unowned uncommitted work, then
#: stale PRs. Findings sort by this, then age descending, then subject.
DIMENSIONS = (KIND_STARTED, KIND_DONE_AHEAD, KIND_DIRTY, KIND_PR)
_SEVERITY = {kind: i for i, kind in enumerate(DIMENSIONS)}

MEASURED = "measured"
UNKNOWN_DIM = "unknown"

#: Transcript activity fresher than this window proves an owner live. Matches
#: the fleet's idle threshold semantics (recovery.idle_threshold_seconds).
DEFAULT_LIVE_ACTIVITY_S = 900.0

#: A PR is reportable only when strictly older than this.
PR_STALE_AFTER_S = 24 * 3600


# --- the finding and the snapshot -------------------------------------------


@dataclass(frozen=True)
class Finding:
    kind: str
    subject: str
    basis: str
    clear_command: str
    node_id: Optional[str] = None
    pr_number: Optional[int] = None
    pr_url: Optional[str] = None
    cwd: Optional[str] = None
    age_s: Optional[float] = None
    ahead_count: Optional[int] = None
    dirty_count: Optional[int] = None


def finding_identity(finding: Finding) -> str:
    """Stable identity: outcome kind plus the exact node/path/PR it names.
    Age and counts are measurements, not identity, so a finding that only got
    older does not re-mail or re-escalate."""
    return f"{finding.kind}:{finding.subject}"


@dataclass(frozen=True)
class DimensionState:
    state: str  # MEASURED | UNKNOWN_DIM
    count: int  # findings when measured; meaningless when unknown
    warning: Optional[str] = None

    def to_payload(self) -> dict:
        return {"state": self.state, "count": self.count, "warning": self.warning}


@dataclass(frozen=True)
class Snapshot:
    generated_at: str
    findings: tuple
    dimensions: dict
    warnings: tuple[str, ...]
    complete: bool


# --- owner liveness ---------------------------------------------------------


@dataclass(frozen=True)
class OwnerProbe:
    """One owner candidate (a session handle) with its readable evidence.

    ``pid_alive``: True/False only from a positive process probe; None when
    no pid was recorded or the probe could not answer. ``transcript_age_s``:
    seconds since the session's transcript last moved, None when unreadable.
    ``claim_state``: the claim view this handle holds, when it holds one.
    ``stored_exited``: the ONE stored status that is itself a probe result
    (reconcile writes it only after confirming the child was gone)."""

    handle: str
    pid_alive: Optional[bool] = None
    transcript_age_s: Optional[float] = None
    claim_state: Optional[str] = None
    stored_exited: bool = False


LIVE = "live"
GONE = "gone"
OWNER_UNKNOWN = "unknown"


def owner_verdict(
    probe: OwnerProbe, *, live_activity_s: float = DEFAULT_LIVE_ACTIVITY_S
) -> str:
    """``live`` | ``gone`` | ``unknown`` for one owner candidate.

    Positive evidence only, in both directions. Live: a live pid, or a
    transcript that moved inside the activity window. Gone: a positively dead
    pid, an expired lease, or the confirmed-exit stamp. Everything else,
    including every unreadable read, is unknown, and unknown never reads as
    ownerless."""
    if probe.pid_alive is True:
        return LIVE
    if probe.transcript_age_s is not None and probe.transcript_age_s <= live_activity_s:
        return LIVE
    if probe.pid_alive is False:
        return GONE
    # Only TTL expiry (stale) proves a lease dead: suspect keeps TTL
    # protection, and the claims machinery itself refuses to steal it.
    if probe.claim_state == "stale":
        return GONE
    if probe.stored_exited:
        return GONE
    return OWNER_UNKNOWN


def _owner_set_verdict(probes: Sequence[OwnerProbe], sources_ok: bool) -> str:
    """Verdict over every candidate owner of one subject. No candidates at
    all is ownerless only when the stores that would carry candidates read
    successfully; a failed read is the unmeasurable case, not the empty one."""
    if not probes:
        return GONE if sources_ok else OWNER_UNKNOWN
    verdicts = [owner_verdict(p) for p in probes]
    if LIVE in verdicts:
        return LIVE
    if all(v == GONE for v in verdicts):
        return GONE
    return OWNER_UNKNOWN


# --- injected observations ---------------------------------------------------


@dataclass(frozen=True)
class NodeObs:
    node_id: str
    status: str
    touched_at_epoch: Optional[float] = None
    cwd: Optional[str] = None
    worktree_path: Optional[str] = None
    ahead_count: Optional[int] = None  # origin/main..HEAD where a worktree resolves
    claim: Optional[dict] = None  # {"state", "holder", "pid", ...} or None
    owner_probes: tuple = ()


@dataclass(frozen=True)
class WorktreeObs:
    path: str
    repo_root: str
    branch: Optional[str] = None
    dirty_count: Optional[int] = None  # None = the git read failed
    ahead_count: Optional[int] = None  # None = unmeasured or failed
    node_id: Optional[str] = None
    owner_probes: tuple = ()


@dataclass(frozen=True)
class PrObs:
    pr_number: int
    pr_url: Optional[str]
    node_id: Optional[str]
    state: Optional[str] = None  # None = the GitHub read failed
    opened_at_epoch: Optional[float] = None
    owner_probes: tuple = ()


@dataclass(frozen=True)
class Observations:
    now_epoch: float
    graph_ok: bool
    claims_ok: bool
    registry_ok: bool
    github_ok: bool
    nodes: tuple
    worktrees: tuple
    prs: tuple
    unscanned_roots: tuple = ()
    prs_unscanned: bool = False
    warnings: tuple[str, ...] = ()


# --- helpers -----------------------------------------------------------------


def _hours(seconds: Optional[float]) -> str:
    if seconds is None:
        return "unknown age"
    return f"{int(seconds // 3600)}h"


def _node_idle_age(node: NodeObs, now_epoch: float) -> Optional[float]:
    if node.touched_at_epoch is None:
        return None
    return max(0.0, now_epoch - node.touched_at_epoch)


def _checked(finding: Optional[Finding], warnings: list) -> Optional[Finding]:
    """A finding without a clearing verb is not a report, it is noise: it is
    dropped with a scan warning naming it, never emitted as a row."""
    if finding is None:
        return None
    if not (finding.subject and finding.basis and finding.clear_command):
        warnings.append(
            f"finding without a clearing verb withheld: {finding_identity(finding)}"
        )
        return None
    return finding


def _dirty_clear_command(w: WorktreeObs) -> str:
    """The verb that moves a dirty ownerless worktree out of its class. A
    resolved node resumes delivery; a node-less tree gets a live owner
    spawned into it, which is the adoption path this fleet actually has."""
    if w.node_id:
        return f"/fno:target {w.node_id}"
    return (
        f'fno agents spawn --cwd "{w.path}" '
        f'"adopt this worktree: finish or hand off the uncommitted work"'
    )


# --- the pure classifier ------------------------------------------------------


def classify(
    obs: Observations,
    *,
    live_activity_s: float = DEFAULT_LIVE_ACTIVITY_S,
    pr_stale_after_s: float = PR_STALE_AFTER_S,
) -> Snapshot:
    warnings: list[str] = list(obs.warnings)
    findings: list[Finding] = []
    unknown: dict[str, Optional[str]] = {dim: None for dim in DIMENSIONS}

    def _mark_unknown(dim: str, reason: str) -> None:
        if unknown[dim] is None:
            unknown[dim] = reason

    if not obs.graph_ok:
        _mark_unknown(KIND_STARTED, "graph unreadable")
        _mark_unknown(KIND_DONE_AHEAD, "graph unreadable")
    if not obs.claims_ok:
        _mark_unknown(KIND_STARTED, "claims unreadable")
    if not obs.registry_ok:
        # The registry is an owner-candidate source: unreadable means the
        # owner question is unanswerable for every candidate that has no
        # other evidence, which is the unknown case, never the clean one.
        _mark_unknown(KIND_STARTED, "registry unreadable")
        _mark_unknown(KIND_DIRTY, "registry unreadable")
        _mark_unknown(KIND_PR, "registry unreadable")
    if not obs.github_ok:
        _mark_unknown(KIND_PR, "github unreadable")
    if obs.prs_unscanned:
        # Budget ran out before every PR state was read: zero reads is not
        # zero findings.
        _mark_unknown(KIND_PR, "budget spent before PR states were read")
    for root in obs.unscanned_roots:
        _mark_unknown(KIND_DIRTY, f"budget spent before scanning {root}")
        _mark_unknown(KIND_DONE_AHEAD, f"budget spent before scanning {root}")

    worktree_by_node: dict[str, WorktreeObs] = {}
    for w in obs.worktrees:
        if w.node_id and w.node_id not in worktree_by_node:
            worktree_by_node[w.node_id] = w

    # Dimension 1: started nodes with free claims.
    if unknown[KIND_STARTED] is None:
        for node in obs.nodes:
            if node.status != "in_progress":
                continue
            claim_state = (node.claim or {}).get("state")
            if claim_state != "free":
                # Non-free (live, suspect, stale, corrupted) excludes the
                # node outright, whatever the graph's owner fields say.
                continue
            verdict = _owner_set_verdict(node.owner_probes, obs.registry_ok)
            if verdict == LIVE:
                continue
            if verdict == OWNER_UNKNOWN:
                _mark_unknown(
                    KIND_STARTED, f"owner liveness unreadable for {node.node_id}"
                )
                continue
            idle = _node_idle_age(node, obs.now_epoch)
            wt = worktree_by_node.get(node.node_id)
            ahead = node.ahead_count
            if ahead is None and wt is not None:
                ahead = wt.ahead_count
            ahead_clause = (
                f", {ahead} commits ahead of origin/main" if ahead is not None else ""
            )
            findings.append(
                _checked(
                    Finding(
                        kind=KIND_STARTED,
                        subject=node.node_id,
                        basis=(
                            f"in_progress, claim free, idle {_hours(idle)}"
                            f"{ahead_clause}"
                        ),
                        clear_command=f"/fno:target {node.node_id}",
                        node_id=node.node_id,
                        cwd=(wt.path if wt is not None else node.worktree_path or node.cwd),
                        age_s=idle,
                        ahead_count=ahead,
                    ),
                    warnings,
                )
            )
        findings = [f for f in findings if f]

    # Dimension 2: done nodes whose worktree branch never reached main.
    if unknown[KIND_DONE_AHEAD] is None:
        for node in obs.nodes:
            if node.status != "done":
                continue
            wt = worktree_by_node.get(node.node_id)
            if wt is None and node.worktree_path:
                wt = next(
                    (w for w in obs.worktrees if w.path == node.worktree_path), None
                )
            if wt is None:
                # No worktree resolved: nothing to measure for this node.
                continue
            if wt.ahead_count is None:
                _mark_unknown(KIND_DONE_AHEAD, f"ahead count unreadable for {wt.path}")
                continue
            if wt.ahead_count <= 0:
                continue
            findings.append(
                _checked(
                    Finding(
                        kind=KIND_DONE_AHEAD,
                        subject=node.node_id,
                        basis=(
                            f"done, worktree {wt.path} is {wt.ahead_count} "
                            f"commits ahead of fresh origin/main; a squash "
                            f"merge explains it innocently, nothing else does"
                        ),
                        clear_command=(
                            f"cd {wt.repo_root} && fno workspace worktree "
                            f"stranded --apply"
                        ),
                        node_id=node.node_id,
                        cwd=wt.path,
                        ahead_count=wt.ahead_count,
                    ),
                    warnings,
                )
            )
        findings = [f for f in findings if f]

    # Dimension 3: dirty worktrees with no authoritative live owner.
    if unknown[KIND_DIRTY] is None:
        for w in obs.worktrees:
            if w.dirty_count is None:
                _mark_unknown(KIND_DIRTY, f"git status failed for {w.path}")
                continue
            if w.dirty_count == 0:
                continue
            verdict = _owner_set_verdict(w.owner_probes, obs.registry_ok)
            if verdict == LIVE:
                continue
            if verdict == OWNER_UNKNOWN:
                _mark_unknown(KIND_DIRTY, f"owner liveness unreadable for {w.path}")
                continue
            findings.append(
                _checked(
                    Finding(
                        kind=KIND_DIRTY,
                        subject=w.path,
                        basis=(
                            f"{w.dirty_count} dirty path(s), no live owner, "
                            + (
                                f"node {w.node_id}"
                                if w.node_id
                                else "no resolved node"
                            )
                        ),
                        clear_command=_dirty_clear_command(w),
                        node_id=w.node_id,
                        cwd=w.path,
                        dirty_count=w.dirty_count,
                    ),
                    warnings,
                )
            )
        findings = [f for f in findings if f]

    # Dimension 4: ownerless open PRs older than the ceiling.
    if unknown[KIND_PR] is None:
        for pr in obs.prs:
            if pr.state is None:
                _mark_unknown(KIND_PR, f"github read failed for pr {pr.pr_number}")
                continue
            if pr.state != "OPEN":
                continue
            if pr.opened_at_epoch is None:
                _mark_unknown(KIND_PR, f"opened_at unreadable for pr {pr.pr_number}")
                continue
            age = obs.now_epoch - pr.opened_at_epoch
            if age <= pr_stale_after_s:
                continue
            verdict = _owner_set_verdict(pr.owner_probes, obs.registry_ok)
            if verdict == LIVE:
                continue
            if verdict == OWNER_UNKNOWN:
                _mark_unknown(
                    KIND_PR, f"owner liveness unreadable for pr {pr.pr_number}"
                )
                continue
            findings.append(
                _checked(
                    Finding(
                        kind=KIND_PR,
                        subject=f"pr-{pr.pr_number}",
                        basis=(
                            f"open {_hours(age)} with no live owner"
                            + (f", node {pr.node_id}" if pr.node_id else "")
                            + (f", {pr.pr_url}" if pr.pr_url else "")
                        ),
                        clear_command=f"/fno:pr check {pr.pr_number}",
                        node_id=pr.node_id,
                        pr_number=pr.pr_number,
                        pr_url=pr.pr_url,
                        age_s=age,
                    ),
                    warnings,
                )
            )
        findings = [f for f in findings if f]

    findings.sort(key=lambda f: (_SEVERITY[f.kind], -(f.age_s or 0), f.subject))

    dimensions: dict[str, DimensionState] = {}
    for dim in DIMENSIONS:
        if unknown[dim] is not None:
            dimensions[dim] = DimensionState(UNKNOWN_DIM, 0, unknown[dim])
            warnings.append(f"{dim}: {unknown[dim]}")
        else:
            dimensions[dim] = DimensionState(
                MEASURED, sum(1 for f in findings if f.kind == dim), None
            )

    complete = all(d.state == MEASURED for d in dimensions.values())
    return Snapshot(
        generated_at=datetime.fromtimestamp(obs.now_epoch, tz=timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        findings=tuple(findings),
        dimensions=dimensions,
        warnings=tuple(warnings),
        complete=complete,
    )


# --- rendering: digest, signature, payload -----------------------------------


def snapshot_digest(snapshot: Snapshot, limit: int = 8) -> str:
    """One-screen human digest, house style: one physical line per
    paragraph, findings as list items (a list marker starts a legal block;
    bare lines under a paragraph fail the mail style gate)."""
    lines = [f"unfinished work: {len(snapshot.findings)} finding(s)"]
    for finding in snapshot.findings[:limit]:
        lines.append(
            f"- {finding.kind} {finding.subject}: {finding.basis}"
            f" -> clear: {finding.clear_command}"
        )
    more = len(snapshot.findings) - limit
    if more > 0:
        lines.append(f"- {more} more finding(s) not shown")
    lines.append("")
    for dim in DIMENSIONS:
        state = snapshot.dimensions[dim]
        if state.state == MEASURED:
            lines.append(f"{dim}={state.count} {MEASURED}")
        else:
            lines.append(f"{dim}={UNKNOWN_DIM} ({state.warning or 'unreadable'})")
    return "\n".join(lines)


def snapshot_signature(snapshot: Snapshot) -> str:
    """Stable identity of the finding set: two sweeps that agree on the
    outcomes produce one signature, so mail and escalation speak on change
    and a finding that only aged does not re-speak."""
    return ";".join(sorted(finding_identity(f) for f in snapshot.findings))


def fresh_identities(snapshot: Snapshot, prev_signature: str) -> set:
    """Finding identities the event lane has not already published."""
    prev = set(filter(None, prev_signature.split(";")))
    return {
        finding_identity(f) for f in snapshot.findings if finding_identity(f) not in prev
    }


def snapshot_payload(snapshot: Snapshot) -> dict:
    return {
        "generated_at": snapshot.generated_at,
        "findings": [
            {
                "kind": f.kind,
                "subject": f.subject,
                "basis": f.basis,
                "clear_command": f.clear_command,
                "node_id": f.node_id,
                "pr_number": f.pr_number,
                "pr_url": f.pr_url,
                "cwd": f.cwd,
                "age_s": None if f.age_s is None else int(f.age_s),
                "ahead_count": f.ahead_count,
                "dirty_count": f.dirty_count,
            }
            for f in snapshot.findings
        ],
        "dimensions": {dim: d.to_payload() for dim, d in snapshot.dimensions.items()},
        "counts": {
            dim: (
                snapshot.dimensions[dim].count
                if snapshot.dimensions[dim].state == MEASURED
                else None
            )
            for dim in DIMENSIONS
        },
        "warnings": list(snapshot.warnings),
        "complete": snapshot.complete,
    }


# --- the git metric: fresh origin/main, counted per worktree ------------------


def _run_git(argv: list, *, cwd: Path, runner=None) -> subprocess.CompletedProcess:
    run = runner or subprocess.run
    return run(argv, cwd=str(cwd), capture_output=True, text=True, check=False)


def fetch_origin_main(repo: Path, *, runner=None) -> bool:
    """One ``git fetch origin main`` per repository. Failure is a verdict
    (the metric for that repository reads unknown), never a guessed count."""
    proc = _run_git(["git", "fetch", "origin", "main"], cwd=repo, runner=runner)
    return proc.returncode == 0


def ahead_of_main(worktree: Path, *, runner=None) -> Optional[int]:
    """Commits on the worktree's HEAD not on ``origin/main``. None when the
    read fails or answers anything but a plain integer."""
    proc = _run_git(
        ["git", "rev-list", "--count", "origin/main..HEAD"],
        cwd=worktree,
        runner=runner,
    )
    if proc.returncode != 0:
        return None
    text = (proc.stdout or "").strip()
    if not text.isdigit():
        return None
    return int(text)


def dirty_path_count(worktree: Path, *, runner=None) -> Optional[int]:
    """Number of porcelain entries in the worktree. None when the read
    fails: a failed read is unknown dirt, never zero dirt."""
    proc = _run_git(["git", "status", "--porcelain"], cwd=worktree, runner=runner)
    if proc.returncode != 0:
        return None
    lines = [ln for ln in (proc.stdout or "").splitlines() if ln.strip()]
    return len(lines)


# --- the IO seam ---------------------------------------------------------------


def _default_truth(handle: str) -> Optional[float]:
    from fno.agents.session_truth import resolve_session_truth

    result = resolve_session_truth(handle)
    age = result.get("last_activity_age_s")
    return float(age) if isinstance(age, (int, float)) else None


def _default_pid_alive(pid: Optional[int]) -> Optional[bool]:
    if pid is None:
        return None
    from fno.agents.spawn_gate import _pid_alive

    try:
        return _pid_alive(pid)
    except Exception:  # noqa: BLE001 - a broken probe is unreadable, not dead
        return None


def _read_registry_rows(path: Optional[Path] = None) -> tuple[dict, bool]:
    """cwd -> [registry row, ...] plus an ok flag. A missing registry is a
    legitimate empty fleet and is ok; one that exists and fails to parse is
    a genuine read failure, which reads every candidate unmeasurable."""
    import os

    override = os.environ.get("WORKTREE_STATUS_REGISTRY")
    target = (
        Path(override)
        if override
        else (path or Path.home() / ".fno" / "agents" / "registry.json")
    )
    if not target.exists():
        return {}, True
    try:
        data = json.loads(target.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return {}, False
    if not isinstance(data, dict):
        return {}, False
    by_cwd: dict[str, list[dict]] = {}
    for row in data.get("agents", []):
        if not isinstance(row, dict):
            continue
        cwd = row.get("cwd") or ""
        if not cwd:
            continue
        by_cwd.setdefault(str(Path(cwd)), []).append(row)
    return by_cwd, True


def _session_handle(row: dict) -> Optional[str]:
    return row.get("harness_session_id") or row.get("short_id") or None


def _parse_iso_epoch(stamp) -> Optional[float]:
    if not stamp or not isinstance(stamp, str):
        return None
    try:
        return datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def collect_observations(
    roots: Sequence[Path],
    *,
    now_s: Optional[float] = None,
    runner=None,
    truth_resolver: Optional[Callable[[str], Optional[float]]] = None,
    claim_status_fn: Optional[Callable[[str], dict]] = None,
    graph_entries: Optional[list] = None,
    registry_rows: Optional[tuple] = None,
    pr_candidates: Optional[list] = None,
    pr_state_reader: Optional[Callable] = None,
    deadline_monotonic: Optional[float] = None,
) -> Observations:
    """Gather every observation the classifier needs. Read-only apart from
    one ``git fetch origin main`` per repository and the GitHub PR reads."""
    now_s = now_s if now_s is not None else datetime.now(timezone.utc).timestamp()
    truth = truth_resolver or _default_truth
    warnings: list[str] = []

    def _budget_left() -> Optional[float]:
        if deadline_monotonic is None:
            return None
        return deadline_monotonic - time.monotonic()

    # Graph: global, one strict read.
    if graph_entries is not None:
        entries = list(graph_entries)
        graph_ok = True
    else:
        from fno.graph.store import GraphMalformedRootError, GraphUnreadableError, read_graph_strict

        try:
            entries = read_graph_strict()
            graph_ok = True
        except (GraphUnreadableError, GraphMalformedRootError) as exc:
            entries = []
            graph_ok = False
            warnings.append(f"graph unreadable: {exc}")
    entries_by_id = {
        str(e.get("id")): e for e in entries if isinstance(e, dict) and e.get("id")
    }

    registry_by_cwd, registry_ok = (
        (dict(registry_rows[0]), bool(registry_rows[1]))
        if registry_rows is not None
        else _read_registry_rows()
    )
    if not registry_ok:
        warnings.append("registry unreadable")

    # Worktrees: enumerate once per root, fetch once per root.
    worktree_rows: list[tuple[str, str, Optional[str]]] = []  # path, repo, branch
    fetched_ok: dict[str, bool] = {}
    unscanned: list[str] = []
    from fno.worktree_stranded import _worktrees

    for root in roots:
        left = _budget_left()
        if left is not None and left <= 0:
            unscanned.append(str(root))
            continue
        try:
            listed = _worktrees(Path(root))
        except Exception as exc:  # noqa: BLE001 - a failed listing is a warning
            warnings.append(f"worktree list failed for {root}: {exc}")
            listed = []
        fetched_ok[str(root)] = fetch_origin_main(Path(root), runner=runner)
        if not fetched_ok[str(root)]:
            warnings.append(f"git fetch origin main failed for {root}")
        for branch, path in listed:
            if str(Path(path)) == str(Path(root)):
                continue  # the main worktree: shared surface, excluded
            if not Path(path).is_dir():
                warnings.append(f"worktree vanished before scan: {path}")
                continue
            worktree_rows.append((path, str(root), branch))

    # Claims per node.
    claim_fn = claim_status_fn
    if claim_fn is None:
        from fno.claims.core import claim_status
        from fno.claims.io import claims_root_for

        def claim_fn(node_id: str) -> dict:
            key = f"node:{node_id}"
            root = claims_root_for(key)
            if root is None:
                raise ValueError(f"claims root unresolved for {key}")
            return claim_status(key, root=root)

    claims_ok = True
    claim_views: dict[str, dict] = {}
    for node_id in entries_by_id:
        try:
            claim_views[node_id] = claim_fn(node_id)
        except Exception:  # noqa: BLE001 - one unreadable claim store condemns the dim
            claims_ok = False
            claim_views[node_id] = {}

    # Owner probes, one per unique handle.
    probe_cache: dict[str, OwnerProbe] = {}

    def _probe_for(handle: Optional[str], *, claim_view: Optional[dict] = None) -> Optional[OwnerProbe]:
        if not handle:
            return None
        if handle in probe_cache:
            return probe_cache[handle]
        claim_state = (claim_view or {}).get("state")
        stored_exited = any(
            row.get("status") == "exited"
            for rows in registry_by_cwd.values()
            for row in rows
            if _session_handle(row) == handle
        )
        pid = (claim_view or {}).get("pid")
        probe = OwnerProbe(
            handle=handle,
            pid_alive=_default_pid_alive(pid),
            transcript_age_s=truth(handle),
            claim_state=claim_state,
            stored_exited=stored_exited,
        )
        probe_cache[handle] = probe
        return probe

    def _registry_handles(paths: Sequence[str]) -> list[str]:
        handles: list[str] = []
        for p in paths:
            for row in registry_by_cwd.get(str(Path(p)), []):
                handle = _session_handle(row)
                if handle and handle not in handles:
                    handles.append(handle)
        return handles

    # Worktree observations.
    worktrees: list[WorktreeObs] = []
    for path, repo, branch in worktree_rows:
        node_id, _entry = resolve_node_id(path, branch, entries_by_id) if entries_by_id else (None, None)
        dirty = dirty_path_count(Path(path), runner=runner)
        ahead = (
            ahead_of_main(Path(path), runner=runner)
            if fetched_ok.get(repo)
            else None
        )
        claim_view = claim_views.get(node_id) if node_id else None
        handles = _registry_handles([path])
        if claim_view and claim_view.get("holder") and claim_view.get("state") in (
            "live", "suspect", "stale",
        ):
            holder = claim_view.get("holder")
            if holder not in handles:
                handles.append(holder)
        probes = tuple(
            p for p in (
                _probe_for(
                    h,
                    claim_view=(
                        claim_view
                        if claim_view and h == claim_view.get("holder")
                        else None
                    ),
                )
                for h in handles
            )
            if p is not None
        )
        worktrees.append(
            WorktreeObs(
                path=path,
                repo_root=repo,
                branch=branch,
                dirty_count=dirty,
                ahead_count=ahead,
                node_id=node_id,
                owner_probes=probes,
            )
        )

    worktree_by_path = {w.path: w for w in worktrees}
    wt_by_node: dict[str, WorktreeObs] = {}
    for w in worktrees:
        if w.node_id and w.node_id not in wt_by_node:
            wt_by_node[w.node_id] = w

    # Node observations.
    nodes: list[NodeObs] = []
    for node_id, entry in entries_by_id.items():
        claim_view = claim_views.get(node_id)
        wt = wt_by_node.get(node_id)
        node_cwd = entry.get("_resolved_cwd") or entry.get("cwd")
        worktree_path = wt.path if wt is not None else None
        if worktree_path is None and node_cwd and node_cwd in worktree_by_path:
            worktree_path = node_cwd
        candidate_paths = [p for p in (worktree_path, node_cwd) if p]
        handles = _registry_handles(candidate_paths)
        if claim_view and claim_view.get("holder") and claim_view.get("state") in (
            "live", "suspect", "stale",
        ):
            holder = claim_view.get("holder")
            if holder not in handles:
                handles.append(holder)
        probes = tuple(
            p for p in (
                _probe_for(
                    h,
                    claim_view=(
                        claim_view
                        if claim_view and h == claim_view.get("holder")
                        else None
                    ),
                )
                for h in handles
            )
            if p is not None
        )
        nodes.append(
            NodeObs(
                node_id=node_id,
                status=str(entry.get("status") or ""),
                touched_at_epoch=_parse_iso_epoch(entry.get("touched_at")),
                cwd=node_cwd,
                worktree_path=worktree_path,
                ahead_count=wt.ahead_count if wt is not None else None,
                claim=claim_view,
                owner_probes=probes,
            )
        )

    # PR observations through the canonical reader.
    prs: list[PrObs] = []
    github_ok = True
    prs_unscanned = False
    candidates = pr_candidates
    if candidates is None:
        from fno.pr_watch._discover import discover_open_prs

        # max_age_days generous: a PR open three weeks on a done node is
        # exactly the row this dimension exists to surface, so the watcher's
        # polling grace must not bound the report.
        candidates = discover_open_prs(entries, max_age_days=3650)
    node_probes: dict[str, tuple] = {n.node_id: n.owner_probes for n in nodes}
    for candidate in candidates:
        left = _budget_left()
        if left is not None and left <= 0:
            # Out of budget mid-scan: the unread candidates stay unread, and
            # the dimension reads unknown rather than counting only what
            # fit before the deadline.
            prs_unscanned = True
            break
        node_id = getattr(candidate, "node_id", None)
        number = getattr(candidate, "pr_number", None)
        state: Optional[str] = None
        opened_epoch: Optional[float] = None
        try:
            reader = pr_state_reader
            if reader is None:
                from fno.pr_watch._discover import read_pr_state

                def reader(cand, *, reviewers):
                    return read_pr_state(cand, reviewers=reviewers)

            observation = reader(candidate, reviewers=[])
            state = str(getattr(observation, "state", "") or "") or None
            opened_epoch = _parse_iso_epoch(getattr(observation, "opened_at", None))
        except Exception as exc:  # noqa: BLE001 - one failed read degrades the dim
            github_ok = False
            warnings.append(f"pr read failed for {number}: {exc}")
        prs.append(
            PrObs(
                pr_number=int(number) if number is not None else 0,
                pr_url=getattr(candidate, "pr_url", None),
                node_id=node_id,
                state=state,
                opened_at_epoch=opened_epoch,
                owner_probes=node_probes.get(node_id, ()),
            )
        )

    return Observations(
        now_epoch=now_s,
        graph_ok=graph_ok,
        claims_ok=claims_ok,
        registry_ok=registry_ok,
        github_ok=github_ok,
        nodes=tuple(nodes),
        worktrees=tuple(worktrees),
        prs=tuple(prs),
        unscanned_roots=tuple(unscanned),
        prs_unscanned=prs_unscanned,
        warnings=tuple(warnings),
    )


def build_report(
    roots: Sequence[Path],
    *,
    now_s: Optional[float] = None,
    **kwargs,
) -> Snapshot:
    """The one producer: collect then classify. Both the manual verb and the
    scheduled tick call this, so their reports cannot diverge."""
    obs = collect_observations(roots, now_s=now_s, **kwargs)
    return classify(obs)


def publish_report(
    snapshot: Snapshot,
    *,
    source: str,
    now_s: float,
    mail_to: str,
    log: Optional[Callable[[str], None]] = None,
) -> dict:
    """The one publish sequence, shared by the manual verb and the tick:
    mail (change-gated, push not pull), sweep-file stamps (the positive
    complete marker only on a fully measured scan), the scan event plus
    fresh finding events, and the deduplicated durable question.

    Returns the payload dict callers render or JSON-emit. ``log`` receives
    human-status lines (stderr for the CLI, logging for the tick)."""
    from fno.agents import watchdog as wd

    note = log or (lambda _line: None)
    payload = snapshot_payload(snapshot)
    signature = ""

    # Mail before the sweep-file write: the change gate compares against the
    # PREVIOUS sweep's stamps, and only a settled-ok mail advances them.
    try:
        ok, receipt, signature = wd.unfinished_mail_gate(snapshot, mail_to or "")
        if not ok:
            note(f"watchdog mail: {receipt}")
    except Exception as exc:  # noqa: BLE001 - mail never breaks the report
        note(f"watchdog mail failed: {exc}")

    prev_events_sig = wd._last_unfinished_signature()
    wd.write_sweep_file(
        source,
        dict(payload["counts"]),
        now_s,
        signature,
        events_signature=snapshot_signature(snapshot),
        unfinished={
            "counts": payload["counts"],
            "complete": payload["complete"],
            "signature": signature,
        },
    )

    wd.emit_event(
        "watchdog_unfinished_work_scan",
        {
            "complete": payload["complete"],
            "finding_count": len(payload["findings"]),
            **{
                dim: payload["counts"][dim]
                for dim in DIMENSIONS
                if payload["counts"][dim] is not None
            },
            "unknown_dimensions": [
                dim
                for dim in DIMENSIONS
                if payload["dimensions"][dim]["state"] == UNKNOWN_DIM
            ],
            "warnings": payload["warnings"],
        },
    )
    fresh = fresh_identities(snapshot, prev_events_sig)
    for finding in snapshot.findings:
        if finding_identity(finding) in fresh:
            wd.emit_event(
                "watchdog_unfinished_work_finding",
                {
                    "kind": finding.kind,
                    "subject": finding.subject,
                    "basis": finding.basis,
                    "clear_command": finding.clear_command,
                    "node": finding.node_id,
                    "pr_number": finding.pr_number,
                    "cwd": finding.cwd,
                    "age_s": None if finding.age_s is None else int(finding.age_s),
                },
            )

    try:
        from fno.agents.stale_escalate import escalate_unfinished
        from fno.carveout.core import resolve_carveout_root, resolve_session_id
        from fno.paths import resolve_repo_root

        try:
            session_id = resolve_session_id(resolve_repo_root())
        except Exception:  # noqa: BLE001 - an unbound sweep still records the ask
            session_id = None
        outcome, qid = escalate_unfinished(
            list(snapshot.findings),
            root=resolve_carveout_root(),
            session_id=session_id,
            cwd=Path.cwd(),
        )
        if outcome == "none":
            note("watchdog escalation: no unfinished-work findings")
        else:
            note(
                f"watchdog escalation: {outcome} {qid} "
                f"({len(snapshot.findings)} finding(s))"
            )
    except Exception as exc:  # noqa: BLE001 - named, never fatal to the report
        note(f"watchdog escalation failed: {exc}")

    return payload

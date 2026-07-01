"""Batch-lane state primitive (Wave 1).

Coalesce N same-domain ready nodes onto one branch off origin/main, opened as
a single PR when the batch closes — cutting GitHub Actions runs ~N× (the cost
driver is PR *volume*, not bad merges).

State lives in `.fno/batches/<domain>.json`: **one open batch per domain**. The
JSON file is the durable, cross-session state — a batch survives the session
that opened it and is re-joined by domain, never by session id. Mutations are
flock-guarded (the same OS primitive `fno claim` / the capture tier use) so two
sessions joining the same domain serialize instead of clobbering each other.

This module is pure state. Policy (join-or-start, close condition) lives with
the auto-continue selection path in Wave 2; per-batch ship in Wave 3. v1 is
opt-in via `config.batch.enabled` (default false).
"""
from __future__ import annotations

import fcntl
import json
import logging
import os
import re
import secrets
import subprocess
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Iterator, Literal, Optional

import typer

_LOG = logging.getLogger(__name__)

BATCHES_DIRNAME = ".fno/batches"


class BatchError(RuntimeError):
    """Base for batch-state failures."""


class BatchExists(BatchError):
    """An open batch already exists for this domain."""


class NoOpenBatch(BatchError):
    """No open batch exists for this domain."""


class BatchFull(BatchError):
    """The open batch has reached its max_nodes ceiling."""


class BatchValidationError(ValueError):
    """Inputs to a verb failed validation."""


# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------


def batches_dir(root: Path) -> Path:
    return Path(root) / BATCHES_DIRNAME


def batch_path(domain: str, root: Path) -> Path:
    return batches_dir(root) / f"{_safe(domain)}.json"


def _lock_path(domain: str, root: Path) -> Path:
    return batches_dir(root) / f"{_safe(domain)}.lock"


def _safe(domain: str) -> str:
    d = (domain or "").strip()
    if not d or "/" in d or d in (".", ".."):
        raise BatchValidationError(f"invalid domain: {domain!r}")
    return d


def _now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


# ---------------------------------------------------------------------------
# Low-level IO (atomic write + flock-guarded read-modify-write)
# ---------------------------------------------------------------------------


def _atomic_write(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + f".tmp.{os.getpid()}")
    tmp.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, path)


@contextmanager
def _locked(domain: str, root: Path) -> Iterator[None]:
    """Serialize mutations to one domain's batch file across processes/threads."""
    lock = _lock_path(domain, root)
    lock.parent.mkdir(parents=True, exist_ok=True)
    fd = os.open(str(lock), os.O_CREAT | os.O_RDWR, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        try:
            fcntl.flock(fd, fcntl.LOCK_UN)
        finally:
            os.close(fd)


def read_batch(domain: str, root: Path) -> Optional[dict]:
    """Return the batch record for a domain, or None if no file exists."""
    p = batch_path(domain, root)
    if not p.exists():
        return None
    return json.loads(p.read_text(encoding="utf-8"))


def list_batches(root: Path) -> list[dict]:
    d = batches_dir(root)
    if not d.exists():
        return []
    out: list[dict] = []
    for p in sorted(d.glob("*.json")):
        try:
            out.append(json.loads(p.read_text(encoding="utf-8")))
        except (json.JSONDecodeError, UnicodeDecodeError, OSError) as e:
            # Name the corrupt file rather than silently hiding it from the
            # status view (the mutation path errors on it; status must not lie).
            # UnicodeDecodeError (invalid UTF-8) subclasses ValueError, not
            # OSError, so it must be listed explicitly (gemini).
            _LOG.warning("skipping unreadable batch file %s: %s", p, e)
            continue
    return out


# ---------------------------------------------------------------------------
# Predicates
# ---------------------------------------------------------------------------


def member_ids(batch: dict) -> list[str]:
    return [m["node_id"] for m in batch.get("members", [])]


def is_full(batch: dict) -> bool:
    return len(batch.get("members", [])) >= int(batch.get("max_nodes", 3))


def _is_open(batch: Optional[dict]) -> bool:
    return bool(batch) and batch.get("status") == "open"


# ---------------------------------------------------------------------------
# Verbs
# ---------------------------------------------------------------------------


def open_batch(
    *,
    domain: str,
    branch: str,
    worktree: str,
    max_nodes: int = 3,
    root: Path,
) -> dict:
    """Start a new open batch for a domain. Fails if one is already open.

    A closed/abandoned batch file for the same domain is replaced (start fresh).
    """
    _safe(domain)
    # A max_nodes < 1 makes is_full() true from the start, so no node could ever
    # join — reject it at the primitive rather than silently create a dead batch
    # (config.batch.max_nodes is already coerced >=1, but open_batch is callable
    # directly via `--max-nodes`) (gemini).
    if int(max_nodes) < 1:
        raise BatchValidationError(f"max_nodes must be >= 1, got {max_nodes}")
    with _locked(domain, root):
        existing = read_batch(domain, root)
        if _is_open(existing):
            raise BatchExists(f"an open batch already exists for domain {domain!r}")
        batch = {
            "batch_id": f"batch-{secrets.token_hex(4)}",
            "domain": domain,
            "branch": branch,
            "worktree": worktree,
            "status": "open",
            "max_nodes": int(max_nodes),
            "created_at": _now(),
            "closed_at": None,
            "pr_url": None,
            "members": [],
        }
        _atomic_write(batch_path(domain, root), batch)
        return batch


def join_batch(
    *,
    domain: str,
    node_id: str,
    summary: str = "",
    root: Path,
) -> dict:
    """Append a node to the open batch for a domain. Idempotent per node_id."""
    _safe(domain)
    with _locked(domain, root):
        batch = read_batch(domain, root)
        if not _is_open(batch):
            raise NoOpenBatch(f"no open batch for domain {domain!r}")
        assert batch is not None
        if node_id in member_ids(batch):
            return batch  # idempotent re-join
        if is_full(batch):
            raise BatchFull(
                f"batch {batch['batch_id']} is full ({batch['max_nodes']} nodes)"
            )
        batch["members"].append({"node_id": node_id, "summary": summary})
        _atomic_write(batch_path(domain, root), batch)
        return batch


def close_batch(*, domain: str, pr_url: Optional[str] = None, root: Path) -> dict:
    """Mark the open batch closed (shipped) and return it with its members."""
    _safe(domain)
    with _locked(domain, root):
        batch = read_batch(domain, root)
        if not _is_open(batch):
            raise NoOpenBatch(f"no open batch for domain {domain!r}")
        assert batch is not None
        batch["status"] = "closed"
        batch["closed_at"] = _now()
        if pr_url is not None:
            batch["pr_url"] = pr_url
        _atomic_write(batch_path(domain, root), batch)
        return batch


def abandon_batch(*, domain: str, root: Path) -> dict:
    """Abandon the open batch; return it so members can be requeued individually.

    v1 failure policy: any FAILED/BLOCKED member or a non-green batch PR abandons
    the whole batch. The members are the caller's to requeue as individual PRs.
    """
    _safe(domain)
    with _locked(domain, root):
        batch = read_batch(domain, root)
        if not _is_open(batch):
            raise NoOpenBatch(f"no open batch for domain {domain!r}")
        assert batch is not None
        batch["status"] = "abandoned"
        batch["closed_at"] = _now()
        _atomic_write(batch_path(domain, root), batch)
        return batch


# ---------------------------------------------------------------------------
# Per-batch ship (Wave 3): one PR for the whole batch, on close
# ---------------------------------------------------------------------------
#
# The daemon (active_backlog.rs) calls `fno backlog batch ship --domain <d>` when
# `should_close` trips (batch full / next node is a different domain / drain).
# ship_batch opens ONE PR for the shared batch branch and records the shared PR
# ref (pr_number/pr_url) on every member node. It does NOT mark members `done`:
# the PR is only just created (CI pending), so completion happens at merge, when
# `fno backlog reconcile` closes each member independently by its own pr_number
# (Locked Decision 5 - a shared URL is just N identical pr_url values, which the
# existing per-node close already handles).
#
# v1 failure policy (Locked Decision 2): any failure to open the PR abandons the
# batch and clears every member's `batch` mark, so they resurface in `next` and
# ship as individual PRs (today's behavior) - never worse than no batching.


# subprocess seam: tests inject a fake to avoid real git/gh calls.
Runner = Callable[..., "subprocess.CompletedProcess"]


def _run(cmd: list[str], *, cwd: Optional[str] = None) -> "subprocess.CompletedProcess":
    return subprocess.run(cmd, capture_output=True, text=True, cwd=cwd, timeout=600)


def _extract_pr_number(url_or_output: str) -> Optional[int]:
    """PR number from a GitHub URL (or a bare number). Mirrors worker/ship.py."""
    m = re.search(r"/pull/(\d+)", url_or_output or "")
    if m:
        return int(m.group(1))
    s = (url_or_output or "").strip()
    return int(s) if s.isdigit() else None


ShipAction = Literal["shipped", "abandoned", "noop"]


@dataclass
class ShipResult:
    action: ShipAction
    domain: str
    reason: str = ""
    pr_url: Optional[str] = None
    pr_number: Optional[int] = None
    members: list[str] = field(default_factory=list)

    def to_dict(self) -> dict:
        return {
            "action": self.action,
            "domain": self.domain,
            "reason": self.reason,
            "pr_url": self.pr_url,
            "pr_number": self.pr_number,
            "members": self.members,
        }


def _batch_pr_body(batch: dict) -> str:
    """PR body listing the batch members + their one-line summaries.

    Keeps the reviewer oriented on a multi-node diff. The domain boundary
    (Locked Decision 4) already caps this to same-domain work.
    """
    lines = [
        f"Batch **{batch.get('batch_id', '?')}** (domain `{batch.get('domain', '?')}`) "
        f"coalesces {len(batch.get('members', []))} node(s) into one PR to cut CI runs.",
        "",
        "## Members",
    ]
    for m in batch.get("members", []):
        summary = (m.get("summary") or "").strip()
        lines.append(f"- `{m['node_id']}`" + (f" - {summary}" if summary else ""))
    return "\n".join(lines) + "\n"


def _set_member_pr_refs(
    member_ids: list[str], *, pr_url: str, pr_number: Optional[int], root: Path
) -> None:
    """Record the shared batch PR ref on every member, in one locked mutation.

    Members are NOT closed here (the PR is not merged yet); the ref lets
    merge-time `fno backlog reconcile` close each member by its own pr_number.
    """
    from fno.graph._intake import _find_node
    from fno.graph.store import locked_mutate_graph
    from fno.paths import graph_json

    ids = set(member_ids)

    def mutator(entries):
        for nid in ids:
            node = _find_node(entries, nid)
            if node is None:
                continue
            node["pr_url"] = pr_url
            if pr_number is not None:
                node["pr_number"] = pr_number
        return entries

    locked_mutate_graph(graph_json(), mutator)


def _clear_member_batch_marks(member_ids: list[str], *, root: Path) -> None:
    """Requeue every member as an individual PR: clear the `batch` mark AND release
    its `node:<id>` claim.

    Releasing the claim is load-bearing (codex P2): a DoneBatched member's
    `node:<id>` claim is refreshed to a 2h TTL by MegawalkQueue's park path, so
    clearing `batch` alone would NOT resurface it in `fno backlog next` (the live
    claim still filters it) until the TTL expired - delaying the promised
    individual-PR requeue by up to 2h. The batched worker has already exited when
    an abandon runs (the daemon is sequential), so a non-strict release is safe.
    """
    from fno.graph._intake import _find_node
    from fno.graph.store import locked_mutate_graph
    from fno.paths import graph_json

    ids = set(member_ids)

    def mutator(entries):
        for nid in ids:
            node = _find_node(entries, nid)
            if node is not None:
                node["batch"] = None
        return entries

    locked_mutate_graph(graph_json(), mutator)

    # Release each member's node claim so the requeue is immediate, not TTL-gated.
    # force_release (not release_claim): the claim's holder is the exited worker's
    # session key, and a non-strict release with a different holder is a no-op -
    # only the administrative override actually drops it.
    try:
        from fno.claims.core import force_release_claim
        from fno.claims.io import claims_root_for
    except Exception:  # noqa: BLE001 - claims layer unavailable; mark-clear alone still requeues (eventually)
        return
    for nid in ids:
        key = f"node:{nid}"
        try:
            force_release_claim(key, "batch abandoned; requeue member", root=claims_root_for(key))
        except Exception as e:  # noqa: BLE001 - best-effort; a held claim only delays this one requeue
            _LOG.debug("batch abandon: node claim force-release failed for %s: %s", nid, e)


def ship_batch(
    *,
    domain: str,
    root: Path,
    base: str = "main",
    title: Optional[str] = None,
    run: Runner = _run,
) -> ShipResult:
    """Open ONE PR for the open batch, record the shared ref on members.

    Idempotent on the PR: an existing PR for the batch branch is reused rather
    than duplicated. Any failure to open the PR abandons the batch and requeues
    its members as individual PRs (v1 failure policy).
    """
    _safe(domain)
    batch = read_batch(domain, root)
    if not _is_open(batch):
        return ShipResult("noop", domain, reason="no open batch")
    assert batch is not None
    members = member_ids(batch)
    worktree = batch.get("worktree")
    branch = batch.get("branch")
    if not members:
        # An empty open batch has nothing to ship; abandon it so it does not
        # linger and block a fresh batch for the domain.
        abandon_batch(domain=domain, root=root)
        return ShipResult("abandoned", domain, reason="empty batch")
    if not worktree or not branch:
        _abandon_and_requeue(domain, members, root)
        return ShipResult("abandoned", domain, reason="batch missing worktree/branch", members=members)

    pr_title = title or f"batch({domain}): {len(members)} nodes"
    body = _batch_pr_body(batch)

    # Idempotency: reuse an existing PR for the branch before creating one.
    pr_url: Optional[str] = None
    pr_number: Optional[int] = None
    lst = run(["gh", "pr", "list", "--head", branch, "--json", "number,url"], cwd=worktree)
    if lst.returncode == 0 and (lst.stdout or "").strip():
        try:
            existing = json.loads(lst.stdout)
        except json.JSONDecodeError:
            existing = []
        if existing:
            pr_url = existing[0].get("url")
            pr_number = existing[0].get("number")

    if pr_url is None:
        # Push the batch branch first. `fno worktree ensure` creates only a LOCAL
        # branch and the batched worker commits locally, so `gh pr create --head`
        # (which does NOT push) would fail on an unpublished branch and abandon
        # the batch (codex P1). Push explicitly, then create.
        push = run(["git", "push", "-u", "origin", branch], cwd=worktree)
        if push.returncode != 0:
            _abandon_and_requeue(domain, members, root)
            return ShipResult(
                "abandoned", domain,
                reason=f"git push failed: {(push.stderr or push.stdout or '').strip()[:200]}",
                members=members,
            )
        cr = run(
            ["gh", "pr", "create", "--title", pr_title, "--body", body,
             "--base", base, "--head", branch],
            cwd=worktree,
        )
        if cr.returncode != 0:
            _abandon_and_requeue(domain, members, root)
            return ShipResult(
                "abandoned", domain,
                reason=f"gh pr create failed: {(cr.stderr or cr.stdout or '').strip()[:200]}",
                members=members,
            )
        pr_url = (cr.stdout or "").strip()
        pr_number = _extract_pr_number(pr_url)

    if not pr_url:
        _abandon_and_requeue(domain, members, root)
        return ShipResult("abandoned", domain, reason="no PR url from gh", members=members)

    # Record the shared ref on members BEFORE marking the batch closed. If the
    # graph write fails, the batch stays `open`, so a later ship-closeable tick
    # re-runs: `gh pr list --head` reuses the existing PR (idempotent) and retries
    # the ref write + close. Closing first would strand members `batch`-marked
    # with no pr_number (excluded from next forever, unclosable by reconcile).
    _set_member_pr_refs(members, pr_url=pr_url, pr_number=pr_number, root=root)
    close_batch(domain=domain, pr_url=pr_url, root=root)
    return ShipResult("shipped", domain, pr_url=pr_url, pr_number=pr_number, members=members)


def _abandon_and_requeue(domain: str, members: list[str], root: Path) -> None:
    """v1 failure path: abandon the batch and clear member marks (individual ship)."""
    try:
        abandon_batch(domain=domain, root=root)
    except NoOpenBatch:
        pass
    _clear_member_batch_marks(members, root=root)


# ---------------------------------------------------------------------------
# Daemon-facing verbs (Wave 2 wiring): prepare (launch) + ship-closeable (close)
# ---------------------------------------------------------------------------
#
# The active-backlog daemon (active_backlog.rs) is thin: it shells `batch
# prepare` before dispatch (to learn solo-vs-batched + the shared worktree) and
# `batch ship-closeable` after each tick (to ship any batch whose close
# condition tripped). All the policy lives here in Python where it is testable.


def _get_node(node_id: str, run: Runner) -> Optional[dict]:
    """Fetch a node dict via `fno backlog get`, or None on any failure."""
    try:
        p = run(["fno", "backlog", "get", node_id])
        if p.returncode == 0 and (p.stdout or "").strip():
            return json.loads(p.stdout)
    except Exception as e:  # noqa: BLE001
        _LOG.warning("batch prepare: fno backlog get %s failed: %s", node_id, e)
    return None


class PeekError(RuntimeError):
    """`fno backlog next` could not be read (distinct from a genuine drain)."""


def _peek_next(
    project: Optional[str], run: Runner, mission: Optional[str] = None
) -> Optional[dict]:
    """The next ready node (post batch-member exclusion), or None on genuine drain.

    Raises PeekError on ANY failure (non-zero exit, unparseable output). A drain
    (exit 0 + `null`/empty) MUST stay distinct from an error: `should_close`
    treats next=None as "drain -> close every open batch", so silently mapping a
    transient `fno backlog next` hiccup to None would ship every open batch as-is
    (1-node batches included) on one bad tick. The caller skips the tick on
    PeekError instead.

    `mission` scopes the peek to the same candidate set the daemon dispatches
    (MegawalkQueue::with_mission): without it, a same-domain ready node OUTSIDE
    the mission would keep a mission batch open forever (codex P2).
    """
    cmd = ["fno", "backlog", "next"]
    if project:
        cmd += ["--project", project]
    if mission:
        cmd += ["--mission", mission]
    try:
        p = run(cmd)
    except Exception as e:  # noqa: BLE001
        raise PeekError(f"fno backlog next spawn failed: {e}") from e
    if p.returncode != 0:
        raise PeekError(f"fno backlog next exited {p.returncode}: {(p.stderr or '').strip()[:160]}")
    out = (p.stdout or "").strip()
    if not out or out == "null":
        return None  # genuine drain
    try:
        node = json.loads(out)
    except json.JSONDecodeError as e:
        raise PeekError(f"fno backlog next returned non-JSON: {e}") from e
    return node if isinstance(node, dict) else None


def prepare_batch(
    *, node_id: str, repo: str, root: Path, run: Runner = _run
) -> dict:
    """Decide solo-vs-batched for a candidate node and (on batch) resolve the
    shared worktree, opening a new batch if needed.

    Returns one of:
      {"mode": "solo", "reason": ...}                          -> dispatch /target no-merge
      {"mode": "batched", "domain", "worktree", "branch", "batch_id"}

    Fail-safe: ANY error (node lookup, worktree ensure, disabled) degrades to
    solo, so a broken batch setup never blocks or mis-dispatches a node.
    """
    node = _get_node(node_id, run)
    if node is None:
        return {"mode": "solo", "reason": "node lookup failed"}
    decision = decide_batch_action(node, enabled=_load_batch_enabled(root), root=root)
    if decision.action == "ship_solo":
        return {"mode": "solo", "reason": decision.reason}

    domain = decision.domain
    if decision.action == "start":
        # Unique branch/worktree per batch: a fixed per-domain name would let a
        # NEW same-domain batch, started after the previous batch opened its PR
        # but before it merged, reuse the branch - `gh pr list --head` would then
        # fold the new members into the stale PR and blow past max_nodes. The
        # random suffix guarantees one branch per batch (codex P2).
        name = f"batch-{_safe(domain)}-{secrets.token_hex(3)}"
        branch = f"feature/{name}"
        we = run(["fno", "worktree", "ensure", "--repo", repo, "--name", name, "--branch", branch])
        worktree = (we.stdout or "").strip()
        if we.returncode != 0 or not worktree:
            return {"mode": "solo", "reason": f"worktree ensure failed: {(we.stderr or '').strip()[:160]}"}
        # `fno worktree ensure` is mechanism-only: it does NOT link the shared
        # `.fno/` state a worktree needs. Without setup, the batched worker's
        # session state + events would live in an unlinked worktree-local `.fno`
        # while the daemon polls the canonical journal, so the member reads as
        # no-progress and state fragments (codex P1). Link it before dispatch;
        # if setup fails, degrade to solo rather than batch into a broken tree.
        setup = Path(repo) / "scripts" / "setup" / "setup-worktree.sh"
        if setup.exists():
            sr = run(["bash", str(setup)], cwd=worktree)
            if sr.returncode != 0:
                return {"mode": "solo", "reason": f"setup-worktree failed: {(sr.stderr or '').strip()[:160]}"}
        try:
            b = open_batch(
                domain=domain, branch=branch, worktree=worktree,
                max_nodes=_config_max_nodes(root), root=root,
            )
        except BatchExists:
            # A peer opened the batch between decide and open; join it instead
            # (its recorded worktree/branch win; this call's fresh worktree is
            # left unused - a rare single-dispatcher race, minor disk).
            b = read_batch(domain, root)
            if not _is_open(b):
                return {"mode": "solo", "reason": "batch vanished after race"}
        assert b is not None
        return {
            "mode": "batched", "domain": domain,
            "worktree": b["worktree"], "branch": b["branch"], "batch_id": b["batch_id"],
        }

    # join: reuse the open batch's recorded worktree/branch.
    b = read_batch(domain, root)
    if not _is_open(b):
        return {"mode": "solo", "reason": "no open batch to join"}
    assert b is not None
    return {
        "mode": "batched", "domain": domain,
        "worktree": b["worktree"], "branch": b["branch"], "batch_id": b["batch_id"],
    }


def ship_closeable(
    *, project: Optional[str], root: Path, run: Runner = _run,
    mission: Optional[str] = None,
) -> list[ShipResult]:
    """Ship every open batch whose close condition has tripped.

    Called by the daemon after each tick. Peeks the next ready node once and
    evaluates `should_close` per open batch: a batch closes when it is full, the
    next ready node is a different domain (or size:L/p0), or the backlog drained
    (next is None -> close whatever is open).

    A peek FAILURE (distinct from a drain) skips the whole tick and ships nothing:
    treating a transient `fno backlog next` error as a drain would prematurely
    ship every open batch. The next healthy tick retries.

    Note: `config.batch.max_loc` is NOT enforced here in v1 - the batch state does
    not track cumulative diff LOC, so there is no `cum_loc` to compare. The knob
    stays inert (never wrongly closes) until a later wave records per-batch LOC.
    """
    try:
        next_node = _peek_next(project, run, mission=mission)
    except PeekError as e:
        _LOG.warning("batch ship-closeable: peek failed, skipping tick: %s", e)
        return []
    results: list[ShipResult] = []
    for b in list_batches(root):
        if b.get("status") != "open":
            continue
        close, _reason = should_close(b, next_node)
        if close:
            results.append(ship_batch(domain=b["domain"], root=root, run=run))
    return results


def _config_max_nodes(root: Path) -> int:
    try:
        from fno.config import load_settings_for_repo

        return int(load_settings_for_repo(Path(root)).config.batch.max_nodes)
    except Exception:  # noqa: BLE001 - default matches config coercion
        return 3


# ---------------------------------------------------------------------------
# Policy engine (Wave 2): join-or-start + close-condition, pure over inputs
# ---------------------------------------------------------------------------

# A node ships alone (never batched) when it is large or drop-everything: a big
# or urgent change deserves its own reviewable PR (Locked Decision, plan §close).
SOLO_SIZES = {"L"}
SOLO_PRIORITIES = {"p0"}


BatchAction = Literal["ship_solo", "start", "join"]


@dataclass
class BatchDecision:
    """What to do with a candidate node at selection time."""

    action: BatchAction
    domain: str
    reason: str

    def to_dict(self) -> dict:
        return {"action": self.action, "domain": self.domain, "reason": self.reason}


def _ships_alone(node: dict) -> Optional[str]:
    if (node.get("size") or "").upper() in SOLO_SIZES:
        return "size:L ships alone"
    if (node.get("priority") or "").lower() in SOLO_PRIORITIES:
        return "p0 ships alone"
    return None


def decide_batch_action(node: dict, *, enabled: bool, root: Path) -> BatchDecision:
    """Decide whether a candidate node ships solo, joins, or starts a batch.

    `enabled=False` always returns ship_solo → byte-for-byte today's
    one-PR-per-node behavior when config.batch.enabled is off (Locked Decision 3).
    """
    domain = node.get("domain") or "code"
    if not enabled:
        return BatchDecision("ship_solo", domain, "batching disabled")
    solo = _ships_alone(node)
    if solo:
        return BatchDecision("ship_solo", domain, solo)
    try:
        b = read_batch(domain, root)
    except (json.JSONDecodeError, UnicodeDecodeError, OSError) as e:
        # A corrupt batch file must not crash the live selection loop. Ship solo
        # (conservative): never pool a node into a batch we can't read (gemini).
        _LOG.warning("failed to read batch for domain %s: %s; shipping solo", domain, e)
        return BatchDecision("ship_solo", domain, f"error reading batch: {e}")
    if b and b.get("status") == "open" and not is_full(b):
        return BatchDecision("join", domain, f"join open batch {b['batch_id']}")
    return BatchDecision("start", domain, "no joinable open batch")


def should_close(
    batch: Optional[dict],
    next_node: Optional[dict],
    *,
    max_loc: Optional[int] = None,
    cum_loc: int = 0,
) -> tuple[bool, str]:
    """Close the open batch when the first close condition trips (plan §close).

    Domain boundary is the important one — it caps blast radius and keeps the
    review panel looking at a coherent diff.
    """
    if batch is None or batch.get("status") != "open":
        return (False, "no open batch")
    if is_full(batch):
        return (True, "max_nodes reached")
    if next_node is None:
        return (True, "no more ready nodes (drain)")
    if (next_node.get("domain") or "code") != batch.get("domain"):
        return (True, "next node is a different domain")
    solo = _ships_alone(next_node)
    if solo:
        return (True, f"next node {solo}")
    if max_loc and cum_loc > int(max_loc):
        return (True, "max_loc exceeded")
    return (False, "batch stays open")


# ---------------------------------------------------------------------------
# CLI: `fno backlog batch <verb>`
# ---------------------------------------------------------------------------

cli = typer.Typer(
    name="batch",
    help="Batch-lane state: coalesce same-domain nodes into one PR (opt-in).",
    no_args_is_help=True,
)


def _root_opt(root: Optional[str]) -> Path:
    """Resolve the batch-state root: explicit --root, else the CANONICAL repo root.

    Batch state (`.fno/batches/`) is cross-worktree coordination state, like
    `fno.claims`: the daemon (dispatch cwd), the batched worker (a linked batch
    worktree), and `ship-closeable` must all see the SAME open batch for
    "one open batch per domain" to hold. `setup-worktree.sh` does NOT link
    `.fno/batches/`, so a raw `Path.cwd()` default would fragment state across
    worktrees. resolve_canonical_repo_root() returns the main checkout from any
    linked worktree (the same category claims_dir() resolves to), so every
    participant converges on `<canonical>/.fno/batches/` (x-6cdf prerequisite).
    """
    if root:
        return Path(root)
    try:
        from fno.paths import resolve_canonical_repo_root

        return resolve_canonical_repo_root()
    except Exception:  # noqa: BLE001 - outside a git repo, fall back to cwd
        return Path.cwd()


def _emit(obj: dict) -> None:
    typer.echo(json.dumps(obj, indent=2, sort_keys=True))


@cli.command("open")
def cli_open(
    domain: str = typer.Option(..., "--domain", "-d", help="Batch domain (e.g. code)."),
    branch: str = typer.Option(..., "--branch", "-b", help="Batch branch name."),
    worktree: str = typer.Option(..., "--worktree", "-w", help="Batch worktree path."),
    max_nodes: int = typer.Option(3, "--max-nodes", help="Nodes before close."),
    root: Optional[str] = typer.Option(None, "--root", help="Project root (default cwd)."),
) -> None:
    """Start a new open batch for a domain."""
    try:
        _emit(open_batch(domain=domain, branch=branch, worktree=worktree,
                         max_nodes=max_nodes, root=_root_opt(root)))
    except BatchExists as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(3)
    except (BatchError, BatchValidationError) as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(1)


@cli.command("join")
def cli_join(
    domain: str = typer.Option(..., "--domain", "-d"),
    node: str = typer.Option(..., "--node", "-n", help="Node id to add."),
    summary: str = typer.Option("", "--summary", "-s"),
    root: Optional[str] = typer.Option(None, "--root"),
) -> None:
    """Add a node to the open batch for a domain (join-or-fail)."""
    try:
        _emit(join_batch(domain=domain, node_id=node, summary=summary, root=_root_opt(root)))
    except NoOpenBatch as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(2)
    except BatchFull as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(4)
    except (BatchError, BatchValidationError) as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(1)


@cli.command("close")
def cli_close(
    domain: str = typer.Option(..., "--domain", "-d"),
    pr_url: Optional[str] = typer.Option(None, "--pr-url"),
    root: Optional[str] = typer.Option(None, "--root"),
) -> None:
    """Close the open batch (mark shipped) and print its members."""
    try:
        _emit(close_batch(domain=domain, pr_url=pr_url, root=_root_opt(root)))
    except NoOpenBatch as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(2)
    except (BatchError, BatchValidationError) as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(1)


@cli.command("abandon")
def cli_abandon(
    domain: str = typer.Option(..., "--domain", "-d"),
    root: Optional[str] = typer.Option(None, "--root"),
) -> None:
    """Abandon the open batch AND requeue its members as individual PRs.

    v1 failure policy: clears every member's graph `batch` mark so they resurface
    in `fno backlog next`. The daemon calls this when a batched member fails.
    """
    r = _root_opt(root)
    try:
        batch = abandon_batch(domain=domain, root=r)
    except NoOpenBatch as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(2)
    except (BatchError, BatchValidationError) as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(1)
    _clear_member_batch_marks(member_ids(batch), root=r)
    _emit(batch)


@cli.command("ship")
def cli_ship(
    domain: str = typer.Option(..., "--domain", "-d"),
    base: str = typer.Option("main", "--base", help="Base branch for the PR."),
    title: Optional[str] = typer.Option(None, "--title", help="Override the PR title."),
    root: Optional[str] = typer.Option(None, "--root"),
) -> None:
    """Open ONE PR for the open batch and record the shared ref on members.

    The daemon calls this when `should_close` trips. On failure the batch is
    abandoned and its members requeue as individual PRs (v1 policy). Exit 0 on
    ship, 2 on abandon (members requeued), 3 on no-op (no open batch).
    """
    result = ship_batch(domain=domain, root=_root_opt(root), base=base, title=title)
    _emit(result.to_dict())
    if result.action == "abandoned":
        raise typer.Exit(2)
    if result.action == "noop":
        raise typer.Exit(3)


@cli.command("prepare")
def cli_prepare(
    node: str = typer.Option(..., "--node", "-n", help="Candidate node id."),
    repo: str = typer.Option(..., "--repo", help="Repo MAIN checkout (for worktree ensure)."),
    root: Optional[str] = typer.Option(None, "--root"),
) -> None:
    """Decide solo-vs-batched for a node; on batch, resolve the shared worktree.

    The daemon shells this before dispatch. Emits {mode: solo|batched, ...}.
    Always exit 0 (fail-safe degrades to solo); the daemon reads `mode`.
    """
    _emit(prepare_batch(node_id=node, repo=repo, root=_root_opt(root)))


@cli.command("ship-closeable")
def cli_ship_closeable(
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Scope the next-node peek."),
    mission: Optional[str] = typer.Option(None, "--mission", help="Scope the peek to a mission (match dispatch)."),
    root: Optional[str] = typer.Option(None, "--root"),
) -> None:
    """Ship every open batch whose close condition tripped (daemon calls per tick)."""
    results = ship_closeable(project=project, root=_root_opt(root), mission=mission)
    _emit({"shipped": [r.to_dict() for r in results]})


@cli.command("status")
def cli_status(
    domain: Optional[str] = typer.Option(None, "--domain", "-d"),
    root: Optional[str] = typer.Option(None, "--root"),
) -> None:
    """Show the open batch for a domain, or all batches."""
    r = _root_opt(root)
    if domain:
        b = read_batch(domain, r)
        _emit(b or {"domain": domain, "status": "none"})
    else:
        _emit({"batches": list_batches(r)})


def _load_batch_enabled(root: Optional[Path] = None) -> bool:
    """config.batch.enabled, defaulting False if settings can't be loaded.

    When a `root` is given (the policy verb's `--root`), read that repo's config
    via the repo-scoped loader rather than the cwd-cached `load_settings()`.
    Otherwise the decision and the batch STATE would read from different repos:
    an opted-in repo forced to ship_solo because the caller's cwd is disabled,
    or a non-opted repo batching because the cwd is enabled (codex P2).
    """
    try:
        if root is not None:
            from fno.config import load_settings_for_repo

            return bool(load_settings_for_repo(Path(root)).config.batch.enabled)
        from fno.config import load_settings

        return bool(load_settings().config.batch.enabled)
    except Exception as e:  # noqa: BLE001 - a bad/absent settings file must not enable
        # Fail-safe to disabled, but leave a trace: otherwise an explicit
        # `enabled: true` silenced by an unrelated settings error looks like a
        # mystery ("I turned batching on and nothing batches").
        _LOG.warning("config.batch.enabled unreadable (%s); batching disabled", e)
        return False


@cli.command("policy")
def cli_policy(
    node: str = typer.Option(..., "--node", "-n", help="Candidate node id."),
    root: Optional[str] = typer.Option(None, "--root"),
) -> None:
    """Emit the batch decision (ship_solo|start|join) for a candidate node.

    Reads config.batch.enabled and the node via `fno backlog get`, then applies
    the pure policy. The selection path (Wave 2 wiring) shells to this verb.
    """
    import subprocess

    node_dict: Optional[dict] = None
    try:
        proc = subprocess.run(
            ["fno", "backlog", "get", node], capture_output=True, text=True, timeout=30
        )
        if proc.returncode == 0 and proc.stdout.strip():
            node_dict = json.loads(proc.stdout)
        else:
            _LOG.warning(
                "fno backlog get %s failed (rc=%s): %s",
                node, proc.returncode, (proc.stderr or "").strip()[:200],
            )
    except Exception as e:  # noqa: BLE001
        _LOG.warning("fno backlog get %s errored: %s", node, e)

    if node_dict is None:
        # Could not read the node's size/priority. Ship solo — the conservative
        # direction: never pool a possibly-large (size:L) or drop-everything
        # (p0) node into a shared batch PR on missing data. Degrading to a bare
        # id would erase solo-eligibility and silently defeat the SOLO rule.
        _emit(BatchDecision("ship_solo", "", "node lookup failed; shipping solo").to_dict())
        return

    resolved_root = _root_opt(root)
    decision = decide_batch_action(
        node_dict, enabled=_load_batch_enabled(resolved_root), root=resolved_root
    )
    _emit(decision.to_dict())

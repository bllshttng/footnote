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
import secrets
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator, Literal, Optional

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
    return Path(root) if root else Path.cwd()


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
    """Abandon the open batch; print members to requeue as individual PRs."""
    try:
        _emit(abandon_batch(domain=domain, root=_root_opt(root)))
    except NoOpenBatch as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(2)
    except (BatchError, BatchValidationError) as e:
        typer.echo(str(e), err=True)
        raise typer.Exit(1)


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

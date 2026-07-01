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
import os
import secrets
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterator, Optional

import typer

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
        except (json.JSONDecodeError, OSError):
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

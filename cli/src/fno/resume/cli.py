"""fno do resume receipt - CLI surface for the durable typed resume receipt.

Producer/consumer over the immutable receipt artifact:
  fno do resume receipt write    snapshot current state -> immutable versioned file
  fno do resume receipt validate revalidate the latest receipt against live state
  fno do resume receipt show     load + print a receipt (parse-checked)

The producer is called at a phase/handoff boundary (the caller knows node,
session, generation, HEAD). The consumer is called by a successor session
before any write; it gathers live claim/git/worktree/event state, runs the
read-only revalidate gate, and prints a verdict. It never mutates claims.
"""
from __future__ import annotations

import json
import re
import subprocess
from pathlib import Path
from typing import Optional

import typer

from .receipt import (
    MalformedReceiptError,
    RevalidationResult,
    build_receipt,
    load_receipt,
    read_node_events,
    revalidate,
    write_receipt,
)
from fno.tombstones import tombstone_group_cls

cli = typer.Typer(
    name="resume",
    help=(
        "Durable typed resume receipts (evidence, never write authority). "
        "To revive an agent SESSION by id, use `fno agents resume <session-id>` "
        "(or `fno agents adopt <session-id>` to register it without resuming)."
    ),
    no_args_is_help=True,
)

receipt_app = typer.Typer(
    name="receipt",
    help="Write / validate / show a durable resume receipt",
    no_args_is_help=True,
    cls=tombstone_group_cls("resume receipt"),
)
cli.add_typer(receipt_app, name="receipt")


def _artifacts_dir(repo_root: Optional[Path] = None) -> Path:
    root = Path(repo_root) if repo_root else _resolve_repo_root()
    return root / ".fno" / "artifacts" / "handoff"


def _resolve_repo_root() -> Path:
    from fno.paths import resolve_repo_root

    return Path(resolve_repo_root())


def _now_utc() -> str:
    # RFC3339 UTC, no locale. date -u is portable on the supported hosts.
    try:
        return subprocess.run(
            ["date", "-u", "+%Y-%m-%dT%H:%M:%SZ"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
    except Exception:
        from datetime import datetime, timezone

        return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _split_list(raw: Optional[str]) -> list[str]:
    if not raw:
        return []
    return [part for part in raw.splitlines() if part.strip()]


@receipt_app.command("write")
def write_cmd(
    node: str = typer.Option(..., "--node", help="Backlog node id"),
    session_id: Optional[str] = typer.Option(None, "--session-id", help="Writing session id"),
    session_legacy: Optional[str] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    phase: str = typer.Option(..., "--phase", help="Phase/boundary (do, review, ship, wave, ...)"),
    generation: int = typer.Option(..., "--generation", help="Handoff generation (>=1)"),
    repo: str = typer.Option(..., "--repo", help="Repository name"),
    worktree: str = typer.Option(..., "--worktree", help="Absolute worktree path"),
    branch: str = typer.Option(..., "--branch", help="git branch"),
    head: str = typer.Option(..., "--head", help="git HEAD sha (the candidate)"),
    next_verb: str = typer.Option(..., "--next-verb", help="Exactly one next action verb"),
    next_target: Optional[str] = typer.Option(None, "--next-target", help="Next action target (node/task)"),
    completed_tasks: Optional[str] = typer.Option(None, "--completed-tasks", help="Newline-separated completed task ids"),
    remaining_tasks: Optional[str] = typer.Option(None, "--remaining-tasks", help="Newline-separated remaining task ids"),
    open_findings: Optional[str] = typer.Option(None, "--open-findings", help="Newline-separated open finding ids"),
    known_reds: Optional[str] = typer.Option(None, "--known-reds", help="Newline-separated known-red signals"),
    watchers: Optional[str] = typer.Option(None, "--watchers", help="Newline-separated watcher ids"),
    idempotency_keys: Optional[str] = typer.Option(None, "--idempotency-keys", help="Newline-separated external-effect keys"),
    written_at: Optional[str] = typer.Option(None, "--written-at", help="Override UTC timestamp (default: now)"),
) -> None:
    """Write an immutable versioned resume receipt (producer).

    Snapshots the supplied state into a receipt file named by identity. Refuses
    to overwrite a completed receipt for the same identity.
    """
    from fno._flag_aliases import merge_deprecated_alias

    session = merge_deprecated_alias(
        session_id, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )
    try:
        receipt = build_receipt(
            node=node,
            session=session or "",
            phase=phase,
            generation=generation,
            repo=repo,
            worktree=worktree,
            branch=branch,
            head=head,
            next_verb=next_verb,
            next_target=next_target,
            written_at=written_at or _now_utc(),
            completed_tasks=_split_list(completed_tasks),
            remaining_tasks=_split_list(remaining_tasks),
            open_findings=_split_list(open_findings),
            known_reds=_split_list(known_reds),
            watchers=_split_list(watchers),
            idempotency_keys=_split_list(idempotency_keys),
        )
        path = write_receipt(receipt, _artifacts_dir())
    except FileExistsError as exc:
        typer.echo(json.dumps({"ok": False, "reason": "already_exists", "error": str(exc)}))
        raise typer.Exit(code=1)
    except MalformedReceiptError as exc:
        typer.echo(json.dumps({"ok": False, "reason": "invalid_input", "error": str(exc)}))
        raise typer.Exit(code=2)
    typer.echo(json.dumps({"ok": True, "path": str(path), "content_sha": receipt.content_sha}))


def _find_latest_receipt(node: str, artifacts_dir: Path) -> Optional[Path]:
    """Latest receipt for a node: highest generation, then newest mtime.

    A node may carry several receipts (one per phase/generation/head). The
    successor revalidates the newest authority; older ones stay on disk as
    immutable history.

    Selection does NOT load the receipt: a corrupt latest receipt must still be
    SELECTED (so the caller reports `malformed_receipt`), not silently skipped
    in favor of an older good one. Generation is parsed from the filename's
    ``-gN-`` segment; mtime breaks ties.
    """
    candidates = sorted(artifacts_dir.glob(f"receipt-{node}-*.json"))
    if not candidates:
        return None
    best: Optional[Path] = None
    best_key: Optional[tuple] = None
    for p in candidates:
        m = re.search(r"-g(\d+)-", p.name)
        gen = int(m.group(1)) if m else 0
        try:
            mtime = p.stat().st_mtime
        except OSError:
            mtime = 0.0
        key = (gen, mtime)
        if best_key is None or key > best_key:
            best_key = key
            best = p
    return best


def _git_head_and_branch(worktree: Path) -> tuple[str, str]:
    def _run(*args: str) -> str:
        try:
            out = subprocess.run(
                ["git", *args], cwd=str(worktree), capture_output=True,
                text=True, check=False, timeout=10,
            )
            return out.stdout.strip() if out.returncode == 0 else ""
        except (OSError, subprocess.SubprocessError):
            return ""

    return _run("rev-parse", "HEAD"), _run("rev-parse", "--abbrev-ref", "HEAD")


def _live_claim_status(node: str, claims_root: Optional[Path]) -> dict:
    """Live claim status for node:<id>. Never raises.

    Returns the raw claim_status dict (state in free|live|suspect|stale|
    corrupted). The caller fails CLOSED on corrupted (an unreadable lockfile
    cannot confirm ownership) rather than collapsing it to a free claim.
    """
    from fno.claims.core import claim_status

    return claim_status(f"node:{node}", root=claims_root)


def _holder_of(status: dict) -> Optional[str]:
    if status.get("state") in {"free", "corrupted"}:
        return None
    holder = status.get("holder")
    return holder if isinstance(holder, str) and holder else None


@receipt_app.command("validate")
def validate_cmd(
    node: str = typer.Option(..., "--node", help="Backlog node id to revalidate"),
    session_id: Optional[str] = typer.Option(None, "--session-id", help="Successor (own) session id; default: receipt session"),
    session_legacy: Optional[str] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    worktree: Optional[str] = typer.Option(None, "--worktree", help="Override worktree to probe (default: receipt worktree)"),
    events_file: Optional[str] = typer.Option(None, "--events", help="Override events.jsonl path (default: <worktree>/.fno/events.jsonl)"),
    harness: Optional[str] = typer.Option(None, "--harness", help="Owning harness for generation scoping"),
    claims_root: Optional[str] = typer.Option(None, "--claims-root", help="Override claims root (default: ~/.fno)"),
) -> None:
    """Revalidate the latest receipt for a node against live state (consumer).

    Gathers live HEAD/branch, worktree existence, node-claim holder, and the
    node's journal events; runs the read-only revalidate gate; prints a JSON
    verdict. Never acquires or releases claims - on failure the caller parks
    and the predecessor's state is preserved.
    """
    from fno._flag_aliases import merge_deprecated_alias

    session = merge_deprecated_alias(
        session_id, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )
    artifacts_dir = _artifacts_dir()
    latest = _find_latest_receipt(node, artifacts_dir)
    if latest is None:
        typer.echo(json.dumps({"ok": False, "reason": "no_receipt", "node": node}))
        raise typer.Exit(code=1)
    try:
        receipt = load_receipt(latest)
    except MalformedReceiptError as exc:
        typer.echo(json.dumps({"ok": False, "reason": "malformed_receipt", "error": str(exc), "path": str(latest)}))
        raise typer.Exit(code=1)

    wt = Path(worktree) if worktree else Path(receipt.worktree)
    live_head, live_branch = _git_head_and_branch(wt) if wt.exists() else ("", "")
    croot = Path(claims_root).expanduser() if claims_root else None
    claim = _live_claim_status(node, croot)
    # Fail CLOSED on a corrupted lockfile: an unreadable claim cannot confirm
    # ownership, so it must not collapse to "free" and grant an ok verdict
    # (live-lockfile revalidation means the lockfile's own state is load-bearing).
    if claim.get("state") == "corrupted":
        typer.echo(json.dumps({
            "ok": False,
            "reason": "corrupted_claim",
            "node": node,
            "receipt": str(latest),
            "claim_error": claim.get("error"),
            "checked": {"live_claim_state": "corrupted"},
        }))
        raise typer.Exit(code=1)
    live_holder = _holder_of(claim)

    events_path = Path(events_file) if events_file else (wt / ".fno" / "events.jsonl")
    node_events = [
        e
        for e in read_node_events([events_path])
        if _event_node(e) == node
    ] if events_path.exists() else []

    res: RevalidationResult = revalidate(
        receipt,
        live_head=live_head,
        live_branch=live_branch,
        worktree_exists=wt.exists(),
        live_claim_holder=live_holder,
        own_session=session or receipt.identity.session,
        node_events=node_events,
        harness=harness,
    )
    checked = dict(res.checked)
    checked["live_claim_state"] = claim.get("state")
    out = {
        "ok": res.ok,
        "reason": res.reason,
        "node": node,
        "receipt": str(latest),
        "next_action": {"verb": receipt.next_action.verb, "target": receipt.next_action.target},
        "idempotency_keys": list(receipt.idempotency_keys),
        "checked": checked,
    }
    typer.echo(json.dumps(out))
    raise typer.Exit(code=0 if res.ok else 1)


def _event_node(e: dict) -> Optional[str]:
    for key in ("node_id", "graph_node_id"):
        v = e.get(key)
        if isinstance(v, str) and v:
            return v
    data = e.get("data")
    if isinstance(data, dict):
        for key in ("node_id", "graph_node_id"):
            v = data.get(key)
            if isinstance(v, str) and v:
                return v
    return None


@receipt_app.command("show")
def show_cmd(
    node: str = typer.Option(..., "--node", help="Backlog node id"),
    receipt_file: Optional[str] = typer.Option(None, "--path", help="Explicit receipt file (default: latest for node)"),
) -> None:
    """Load + print a receipt (parse/integrity checked). Fails on malformed."""
    path = Path(receipt_file) if receipt_file else _find_latest_receipt(node, _artifacts_dir())
    if path is None or not Path(path).exists():
        typer.echo(json.dumps({"ok": False, "reason": "no_receipt", "node": node}))
        raise typer.Exit(code=1)
    try:
        receipt = load_receipt(Path(path))
    except MalformedReceiptError as exc:
        typer.echo(json.dumps({"ok": False, "reason": "malformed_receipt", "error": str(exc)}))
        raise typer.Exit(code=1)
    typer.echo(json.dumps(receipt.to_dict(), indent=2, sort_keys=True))



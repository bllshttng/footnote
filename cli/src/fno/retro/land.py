"""Land classified candidates hybrid-by-mode.

- autonomous (megawalk/megatron): create nodes ACTIVE - the loop cannot block on
  a human ack.
- interactive (standalone /target): create nodes then QUEUE them, so a human
  acks via `fno backlog pick` before they enter active `ready` (adopt-stays-pure).
- low/nit (tier == inbox): append one inbox.md line instead of a node.

Mode is read from the trigger sentinel (captured at MERGE time). When the mode is
absent or unreadable the routine defaults to INTERACTIVE (AC4-MODE) - the safe
choice, since an erroneous active-create bypasses the human ack the interactive
path exists to preserve. All node creation goes through the locked
``locked_mutate_graph`` path (no direct graph.json writes), and nodes are scoped
to the PR's canonical repo root, never the run cwd (pitfall fu-cwd341).
"""
from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Optional

from fno.retro.dedup import trailer
from fno.retro.types import TIER_INBOX, Candidate

MODE_AUTONOMOUS = "autonomous"
MODE_INTERACTIVE = "interactive"

# Injection points (real defaults below). Kept narrow so the routing logic is
# unit-testable without a live graph.
CreateFn = Callable[..., str]  # (title, details, priority, project, cwd, domain, queued) -> node_id
InboxFn = Callable[[Candidate], None]


@dataclass
class LandResult:
    outcome: str  # "active" | "queued" | "inbox" | "failed"
    candidate: Candidate
    node_id: Optional[str] = None
    error: Optional[str] = None


def resolve_mode(sentinel: Optional[dict]) -> str:
    """Resolve landing mode from the trigger sentinel; default interactive (AC4-MODE)."""
    if not sentinel:
        return MODE_INTERACTIVE
    mode = str(sentinel.get("mode", "") or "").lower()
    return MODE_AUTONOMOUS if mode == MODE_AUTONOMOUS else MODE_INTERACTIVE


# -- real defaults: locked graph path + inbox append -----------------------

def _default_create(
    *,
    title: str,
    details: str,
    priority: str,
    project: Optional[str],
    cwd: Optional[str],
    domain: str = "code",
    queued: bool = False,
    caused_by: Optional[str] = None,
) -> str:
    """Create a backlog node through the locked path.

    When ``queued`` is True the node is created ALREADY queued (queued_at set in
    the SAME mutation), so interactive mode never has a create-succeeded-but-
    queue-failed window that would leave a node active and bypass the human ack.

    ``caused_by`` (W4 causal links) stamps the origin node id on the created
    node in the same mutation, but only when that node actually exists in the
    graph - a stale sentinel id is silently skipped rather than dangled.
    """
    from fno.graph._constants import mint_node_id
    from fno.graph.cli import _build_backlog_node, _graph_path
    from fno.graph.store import locked_mutate_graph

    new_id_holder: list[Optional[str]] = [None]

    def mutator(entries):
        new_id = mint_node_id({e.get("id") for e in entries})
        new_id_holder[0] = new_id
        node = _build_backlog_node(
            title=title,
            project=project,
            cwd=cwd,
            priority=priority,
            domain=domain,
            details=details,
        )
        node["id"] = new_id
        if queued:
            node["queued_at"] = datetime.now(timezone.utc).isoformat()
            node["queued_reason"] = "retro-triage (interactive): awaiting human ack"
        if caused_by and any(e.get("id") == caused_by for e in entries):
            node["caused_by"] = caused_by
        entries.append(node)
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    return new_id_holder[0] or ""


def _default_inbox(repo_root: Path, candidate: Candidate) -> None:
    """Append a low/nit finding through the canonical locked inbox writer.

    Routing through ``backlog.inbox.add_item`` (not a raw append) is required so
    the line is emitted in the ``fu-XXXXXX — title`` shape the inbox reader and
    ``fno backlog capture list`` expect, honors the configured inbox path, and is
    serialized by the same flock as every other inbox writer.
    """
    from fno.backlog.capture import MAX_WHY_LEN, add_item
    from fno.paths import inbox_path

    why = (candidate.finding_text or candidate.body or candidate.title).strip().replace("\n", " ")
    if len(why) > MAX_WHY_LEN:
        why = why[: MAX_WHY_LEN - 1].rstrip() + "…"
    add_item(
        inbox_path(repo_root),
        title=candidate.title,
        source=f"PR #{candidate.source_pr}" if candidate.source_pr is not None else "retro-triage",
        why=why,
        priority=candidate.priority,
    )


def land_candidates(
    candidates: list[Candidate],
    *,
    mode: str,
    repo_root: Path,
    project: Optional[str] = None,
    cwd: Optional[str] = None,
    domain: str = "code",
    create_fn: Optional[CreateFn] = None,
    inbox_fn: Optional[InboxFn] = None,
    caused_by: Optional[str] = None,
) -> list[LandResult]:
    """Land each candidate per mode/tier. Per-node failures are recorded (not raised)
    so partial progress persists and a re-run dedups what landed (AC4-FR)."""
    create_fn = create_fn or _default_create
    inbox_fn = inbox_fn or (lambda cand: _default_inbox(repo_root, cand))
    # nodes scoped to the PR's canonical root, never the run cwd.
    node_cwd = cwd if cwd is not None else str(repo_root)
    interactive = mode == MODE_INTERACTIVE

    results: list[LandResult] = []
    born_rs = None  # one shared blast-cap across the whole harvest batch (lazy)
    for c in candidates:
        if c.uncited:
            continue  # never land an uncited candidate
        if c.tier == TIER_INBOX:
            try:
                inbox_fn(c)
                results.append(LandResult("inbox", c))
            except Exception as exc:  # inbox write/validation failure -> record
                results.append(LandResult("failed", c, error=str(exc)))
            continue

        details = f"{c.body}\n\n{trailer(c.source_pr, c.content_hash)}"
        try:
            # Interactive nodes are created ALREADY queued in one mutation, so
            # there is no create-ok/queue-fail window that could leave a node
            # active and bypass the human ack (adopt-stays-pure).
            # caused_by rides only when known, so injected create_fn fakes with
            # fixed signatures (tests) stay call-compatible.
            causal_kwargs = {"caused_by": caused_by} if caused_by else {}
            node_id = create_fn(
                title=c.title,
                details=details,
                priority=c.priority,
                project=project,
                cwd=node_cwd,
                domain=domain,
                queued=interactive,
                **causal_kwargs,
            )
        except Exception as exc:  # lock timeout etc. -> record, keep going
            results.append(LandResult("failed", c, error=str(exc)))
            continue

        results.append(
            LandResult("queued" if interactive else "active", c, node_id=node_id)
        )

        # Born-with-why (v2 A1): the retro-harvest birth path is the exact gap
        # this epic fixes (x-7c38 / x-6e23 filed follow-ups with no /think). Route
        # each created node through the shared hook so its why travels forward.
        # One shared RunState bounds the whole batch's blast radius; strictly
        # non-fatal + opt-in (gate OFF => complete no-op). The hook re-reads the
        # durable node by id, so the id stub is all it needs.
        try:
            from fno.provenance.spawn_think import RunState, on_node_born

            if born_rs is None:
                born_rs = RunState()
            on_node_born({"id": node_id}, run_state=born_rs)
        except Exception:  # noqa: BLE001 - additive; never wedge the harvest
            pass

    return results


def has_failures(results: list[LandResult]) -> bool:
    return any(r.outcome == "failed" for r in results)

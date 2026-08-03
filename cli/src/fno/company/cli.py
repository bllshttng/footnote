"""Hidden company campaign inspection and proposal surface.

Read-only inspection (``show``, ``topology``, ``join``) plus a proposal command
that mutates nothing without ``--commit``. Emits a versioned ``--json`` response
so the skill layer and Rust control plane parse rather than scrape.
"""

from __future__ import annotations

import json
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

import typer

from fno.company.campaign import (
    BranchInput,
    CampaignRefusal,
    PlannedDeliverable,
    PlannedEffect,
    classify_objective,
)
from fno.company.contracts import EvidenceResult
from fno.company.coordinator import CoordinatorRefusal, commit
from fno.company.join import JoinBranch, evaluate_join
from fno.company.topology import InferenceFacts, resolve_topology
from fno.graph._intake import _find_node
from fno.graph.store import read_graph
from fno.roles.models import RoleManifest

company_app = typer.Typer(name="company", help="Company campaign inspection and proposals.")

_RESPONSE_VERSION = 1
_NON_PASSING = (EvidenceResult.FAILED, EvidenceResult.BLOCKED, EvidenceResult.UNKNOWN)


def _envelope(payload: dict[str, Any]) -> dict[str, Any]:
    return {"version": _RESPONSE_VERSION, **payload}


def _require_node(entries: list[dict], node_id: str) -> dict:
    node = _find_node(entries, node_id)
    if node is None:
        raise typer.BadParameter(f"no such node {node_id}")
    return node


def _branch_evidence(company_work: dict) -> EvidenceResult:
    """Aggregate one branch's declared evidence (failed > blocked > unknown)."""
    results = [
        EvidenceResult(item["result"])
        for item in company_work.get("evidence", [])
        if isinstance(item, dict) and "result" in item
    ]
    if results and all(r is EvidenceResult.PASSED for r in results):
        return EvidenceResult.PASSED
    for result in _NON_PASSING:
        if result in results:
            return result
    return EvidenceResult.UNKNOWN


@company_app.command("propose")
def propose_command(
    objective: str = typer.Option(..., "--objective"),
    spec: Path = typer.Option(..., "--spec", help="JSON {roles, branches} decomposition spec"),
    graph: Path = typer.Option(..., "--graph"),
    commit_flag: bool = typer.Option(False, "--commit"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Classify an objective into a proposal; commit only with --commit."""
    raw = json.loads(spec.read_text())
    roles = [RoleManifest.model_validate(r) for r in raw.get("roles", [])]
    branches = [
        BranchInput(
            role_id=b["role_id"],
            deliverables=tuple(
                PlannedDeliverable(
                    id=d["id"],
                    kind=d["kind"],
                    required_evidence_ids=tuple(d.get("required_evidence_ids", ())),
                    effect=(
                        PlannedEffect(
                            effect_class=d["effect"]["effect_class"],
                            destination=d["effect"]["destination"],
                        )
                        if d.get("effect")
                        else None
                    ),
                )
                for d in b.get("deliverables", [])
            ),
            depends_on=tuple(b.get("depends_on", ())),
            requires_iteration=bool(b.get("requires_iteration", False)),
        )
        for b in raw.get("branches", [])
    ]

    proposal = classify_objective(
        objective=objective, roles=roles, branches=branches, now=datetime.now(UTC)
    )

    if isinstance(proposal, CampaignRefusal):
        payload = _envelope({"status": "refused", "refusal": {"reason": proposal.reason.value, "detail": proposal.detail}})
        typer.echo(json.dumps(payload, separators=(",", ":")))
        raise typer.Exit(code=1)

    if commit_flag:
        result = commit(proposal, graph_path=graph, project="fno", now=datetime.now(UTC))
        if isinstance(result, CoordinatorRefusal):
            payload = _envelope({"status": "refused", "refusal": {"reason": result.reason.value, "detail": result.detail}})
            typer.echo(json.dumps(payload, separators=(",", ":")))
            raise typer.Exit(code=1)
        payload = _envelope(
            {
                "status": "committed",
                "epic_id": result.epic_id,
                "children": [c.model_dump() for c in result.children],
                "proposal": proposal.model_dump(mode="json"),
            }
        )
    else:
        payload = _envelope({"status": "proposed", "proposal": proposal.model_dump(mode="json")})

    if json_output:
        typer.echo(json.dumps(payload, separators=(",", ":")))
    else:
        typer.echo(f"company: {payload['status']}")
        typer.echo(f"- objective: {proposal.objective}")
        typer.echo(f"- topology: {proposal.topology.value}")
        for branch in proposal.branches:
            typer.echo(f"- {branch.role_id} ({branch.owner_function_id})")


@company_app.command("show")
def show_command(
    node: str = typer.Argument(...),
    graph: Path = typer.Option(..., "--graph"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Show a campaign node's company_work and children."""
    entries = read_graph(graph)
    epic = _require_node(entries, node)
    children = [e for e in entries if e.get("parent") == node]
    payload = _envelope(
        {
            "node_id": node,
            "type": epic.get("type"),
            "company_work": epic.get("company_work"),
            "children": [{"id": c.get("id"), "blocked_by": c.get("blocked_by", [])} for c in children],
        }
    )
    if json_output:
        typer.echo(json.dumps(payload, separators=(",", ":")))
    else:
        typer.echo(f"company show: {node} ({epic.get('type')})")
        for child in children:
            typer.echo(f"- {child.get('id')} blocked_by={child.get('blocked_by', [])}")


@company_app.command("topology")
def topology_command(
    node: str = typer.Argument(...),
    graph: Path = typer.Option(..., "--graph"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Resolve one work order's topology and report the deciding source."""
    entries = read_graph(graph)
    target = _require_node(entries, node)
    company_work = target.get("company_work") or {}
    deliverables = company_work.get("deliverables", []) if isinstance(company_work, dict) else []
    effects = company_work.get("effects", []) if isinstance(company_work, dict) else []
    resolution = resolve_topology(
        plan_lock=None,
        role_default=None,
        inference_facts=InferenceFacts(
            deliverable_count=len(deliverables),
            has_dependency_edges=bool(target.get("blocked_by")),
            has_iteration_evaluator=False,
            has_declared_effect=bool(effects),
        ),
    )
    shape = resolution.shape if hasattr(resolution, "shape") else None
    source = resolution.source.value if hasattr(resolution, "source") else None
    payload = _envelope({"node_id": node, "shape": shape, "source": source} if shape else {"node_id": node, "refusal": resolution.model_dump()})
    if json_output:
        typer.echo(json.dumps(payload, separators=(",", ":")))
    else:
        typer.echo(f"company topology: {node} -> {shape} ({source})")


@company_app.command("join")
def join_command(
    node: str = typer.Argument(...),
    graph: Path = typer.Option(..., "--graph"),
    json_output: bool = typer.Option(False, "--json", "-J"),
) -> None:
    """Evaluate a join over a campaign node's required branches."""
    entries = read_graph(graph)
    _require_node(entries, node)
    branches = []
    for child in (e for e in entries if e.get("parent") == node):
        company_work = child.get("company_work") or {}
        work_order = company_work.get("work_order", {}) if isinstance(company_work, dict) else {}
        branches.append(
            JoinBranch(
                work_order_id=str(work_order.get("node_id") or child.get("id") or "unknown"),
                attempt_id=str(work_order.get("attempt_id") or "attempt-1"),
                result=_branch_evidence(company_work if isinstance(company_work, dict) else {}),
            )
        )
    evaluation = evaluate_join(branches)
    payload = _envelope(
        {
            "node_id": node,
            "aggregate": evaluation.aggregate.value,
            "branches": [b.model_dump() for b in evaluation.branches],
        }
    )
    if json_output:
        typer.echo(json.dumps(payload, separators=(",", ":")))
    else:
        typer.echo(f"company join: {node} -> {evaluation.aggregate.value}")
        for row in evaluation.branches:
            typer.echo(f"- {row.work_order_id}: {row.result.value}")

from __future__ import annotations

import json
from pathlib import Path

import pytest
from pydantic import ValidationError
from typer.testing import CliRunner

from fno.company.contracts import CompanyWorkRefs, FunctionRef, RoleRef, WorkOrderRef
from fno.graph.types import Entry
from fno.plan.cli import plan_app
from fno.plan.schema import PlanFrontmatter


runner = CliRunner()


def _refs() -> CompanyWorkRefs:
    return CompanyWorkRefs(
        function=FunctionRef(id="operations"),
        role=RoleRef(id="operations-lead", function_id="operations"),
        work_order=WorkOrderRef(
            node_id="x-e9a3",
            attempt_id="attempt-1",
            role_id="operations-lead",
        ),
    )


def test_plan_frontmatter_parses_additive_company_work_refs() -> None:
    plan = PlanFrontmatter(
        node="x-e9a3",
        status="ready",
        created="2026-08-02",
        company_work=_refs().model_dump(mode="json"),
        historical_field="preserved-by-byte-writer",
    )

    assert plan.company_work == _refs()


def test_legacy_plan_frontmatter_remains_valid() -> None:
    plan = PlanFrontmatter(node="x-legacy", status="ready", created="2026-08-02")

    assert plan.company_work is None


def test_plan_rejects_company_work_for_a_different_graph_node() -> None:
    with pytest.raises(ValidationError, match="must match plan node"):
        PlanFrontmatter(
            node="x-owner",
            status="ready",
            created="2026-08-02",
            company_work=_refs(),
        )


@pytest.mark.parametrize(("work_order_node", "exit_code"), [("x-e9a3", 0), ("x-other", 1)])
def test_plan_validate_checks_company_work_from_markdown(
    tmp_path: Path, work_order_node: str, exit_code: int
) -> None:
    plan = tmp_path / "company-plan.md"
    plan.write_text(
        "---\n"
        "node: x-e9a3\n"
        "status: ready\n"
        "created: 2026-08-02\n"
        "company_work:\n"
        "  work_order:\n"
        f"    node_id: {work_order_node}\n"
        "    attempt_id: attempt-1\n"
        "---\n"
        "# Company plan\n"
    )

    result = runner.invoke(plan_app, ["validate", str(plan)])

    assert result.exit_code == exit_code
    if exit_code == 1:
        assert "must match plan node" in result.output


@pytest.mark.parametrize(("domain", "deliverable_type"), [("code", "engineering"), ("research", "research_document")])
def test_plan_validate_preserves_legacy_document_kinds(
    tmp_path: Path, domain: str, deliverable_type: str
) -> None:
    plan = tmp_path / f"{domain}-plan.md"
    plan.write_text(
        "---\n"
        f"node: x-{domain}\n"
        "status: ready\n"
        "created: 2026-08-02\n"
        f"domain: {domain}\n"
        f"deliverable_type: {deliverable_type}\n"
        "---\n"
        "# Legacy plan\n"
    )

    result = runner.invoke(plan_app, ["validate", str(plan)])

    assert result.exit_code == 0


def test_graph_entry_round_trips_company_work_and_unknown_fields() -> None:
    entry = Entry(
        id="x-e9a3",
        company_work=_refs().model_dump(mode="json"),
        future_graph_field={"kept": True},
    )

    dumped = entry.model_dump(mode="json")
    assert dumped["company_work"]["work_order"]["node_id"] == "x-e9a3"
    assert dumped["future_graph_field"] == {"kept": True}


def test_graph_entry_rejects_company_work_for_a_different_node() -> None:
    with pytest.raises(ValidationError, match="must match graph entry id"):
        Entry(id="x-owner", company_work=_refs())


def _outer_identity_mismatch() -> dict:
    company_work = _refs().model_dump(mode="json")
    company_work["work_order"]["node_id"] = "x-other"
    return company_work


def _contradictory_backlinks() -> dict:
    return {
        "work_order": {"node_id": "x-owner", "attempt_id": "attempt-1"},
        "deliverables": [
            {
                "id": "d1",
                "kind": "artifact",
                "work_order_id": "x-owner",
                "attempt_id": "attempt-1",
                "effect_id": "e1",
            },
            {
                "id": "d2",
                "kind": "artifact",
                "work_order_id": "x-owner",
                "attempt_id": "attempt-1",
            },
        ],
        "effects": [
            {
                "id": "e1",
                "work_order_id": "x-owner",
                "attempt_id": "attempt-1",
                "deliverable_id": "d2",
                "effect_class": "external-communication",
                "destination": "helpdesk://ticket/42",
            }
        ],
    }


@pytest.mark.parametrize(
    ("company_work", "error"),
    [
        (_outer_identity_mismatch(), "must match graph entry id"),
        (_contradictory_backlinks(), "conflicting deliverable"),
    ],
)
def test_graph_store_rejects_invalid_company_work_without_writing(
    tmp_path, company_work: dict, error: str
) -> None:
    from fno.graph.store import locked_mutate_graph

    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps({"entries": [{"id": "x-owner", "company_work": company_work}]})
        + "\n"
    )
    original = graph.read_bytes()

    with pytest.raises(ValueError, match=error):
        locked_mutate_graph(graph, lambda entries: entries)

    assert graph.read_bytes() == original


def test_graph_store_persists_valid_company_work_and_unknown_fields(tmp_path: Path) -> None:
    from fno.graph.store import locked_mutate_graph

    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-e9a3",
                        "company_work": _refs().model_dump(mode="json"),
                        "future_graph_field": {"kept": True},
                    }
                ]
            }
        )
        + "\n"
    )

    locked_mutate_graph(graph, lambda entries: entries)

    saved = json.loads(graph.read_text())["entries"][0]
    assert saved["company_work"]["work_order"]["node_id"] == "x-e9a3"
    assert saved["future_graph_field"] == {"kept": True}


def test_graph_store_persists_normalized_company_work(tmp_path: Path) -> None:
    from fno.graph.store import locked_mutate_graph

    company_work = _refs().model_dump(mode="json")
    company_work["work_order"]["node_id"] = " x-e9a3 "
    company_work["work_order"]["attempt_id"] = " attempt-1 "
    graph = tmp_path / "graph.json"
    graph.write_text(
        json.dumps({"entries": [{"id": "x-e9a3", "company_work": company_work}]})
        + "\n"
    )

    locked_mutate_graph(graph, lambda entries: entries)

    saved = json.loads(graph.read_text())["entries"][0]["company_work"]
    assert saved["work_order"]["node_id"] == "x-e9a3"
    assert saved["work_order"]["attempt_id"] == "attempt-1"


def test_legacy_graph_entry_remains_valid() -> None:
    entry = Entry(id="x-legacy")

    assert entry.company_work is None

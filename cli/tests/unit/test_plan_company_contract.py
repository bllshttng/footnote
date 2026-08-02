from __future__ import annotations

from fno.company.contracts import CompanyWorkRefs, FunctionRef, RoleRef, WorkOrderRef
from fno.graph.types import Entry
from fno.plan.schema import PlanFrontmatter


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


def test_graph_entry_round_trips_company_work_and_unknown_fields() -> None:
    entry = Entry(
        id="x-e9a3",
        company_work=_refs().model_dump(mode="json"),
        future_graph_field={"kept": True},
    )

    dumped = entry.model_dump(mode="json")
    assert dumped["company_work"]["work_order"]["node_id"] == "x-e9a3"
    assert dumped["future_graph_field"] == {"kept": True}


def test_legacy_graph_entry_remains_valid() -> None:
    entry = Entry(id="x-legacy")

    assert entry.company_work is None

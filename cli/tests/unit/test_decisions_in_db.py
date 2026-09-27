"""Decision records round-trip through the graph.db keeper and its node join."""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.graph import api
from tests.fixtures.graph_seed import seed_graph


pytestmark = pytest.mark.dev_build


def _graph(tmp_path: Path, decisions: list[dict] | None = None) -> Path:
    row = {
        "id": "x-decision-node",
        "slug": "x-decision-node",
        "title": "Decision node",
        "type": "feature",
        "status": "idea",
        "priority": "p2",
        "domain": "code",
        "created_at": "2026-09-16T00:00:00+00:00",
    }
    if decisions is not None:
        row["decisions"] = decisions
    path = tmp_path / "graph.json"
    seed_graph(path, {"entries": [row]})
    return path


def _event(decision_id: str = "d-wave12") -> dict:
    return {
        "ts": "2026-09-16T00:00:01Z",
        "type": "operator_decision",
        "source": "target",
        "data": {
            "decision_id": decision_id,
            "decision": "store decisions in graph.db",
            "subject": "x-decision-node",
            "authority_source": "operator",
        },
    }


def test_decision_record_round_trips_and_joins_to_subject_node(tmp_path: Path) -> None:
    graph = _graph(tmp_path)

    result = api.decision_record(_event(), path=graph)

    assert result["success"] is True
    rows = api.decisions(path=graph)
    assert [row["decision_id"] for row in rows] == ["d-wave12"]
    assert rows[0]["_event_type"] == "operator_decision"
    assert [row["decision_id"] for row in api.decisions("x-decision-node", path=graph)] == [
        "d-wave12"
    ]

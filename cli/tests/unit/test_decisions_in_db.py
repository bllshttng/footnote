"""Decision records round-trip through the graph.db keeper and its node join."""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from fno.graph import api
from fno.rust_binary import find_dev_binary


pytestmark = pytest.mark.skipif(
    find_dev_binary() is None,
    reason="compiled fno-agents binary not present",
)


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
    path.write_text(json.dumps({"entries": [row]}) + "\n", encoding="utf-8")
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


def test_import_requires_every_node_decision_to_have_a_durable_event(tmp_path: Path) -> None:
    graph = _graph(tmp_path, [{"decision_id": "d-missing", "ts": "2026-09-16T00:00:01Z"}])

    with pytest.raises(Exception, match="d-missing"):
        api.decisions(path=graph)

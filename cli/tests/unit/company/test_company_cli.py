from __future__ import annotations

import hashlib
import json
from pathlib import Path

from typer.testing import CliRunner

from fno.company.cli import company_app

runner = CliRunner()


def _role(role_id: str, function_id: str) -> dict:
    return {
        "role": {"id": role_id, "function_id": function_id},
        "function": {"id": function_id},
        "mission": "m",
        "deliverable_kinds": ["brief"],
        "authority_ceiling": "internal",
        "review_policy": {"required": True, "minimum_reviewers": 1},
        "delivery_policy": {"required_evidence": ["artifact-exists"]},
        "default_topology": "direct",
    }


def _spec(tmp_path: Path, *, depends: bool = False) -> Path:
    spec_path = tmp_path / "spec.json"
    branches = [
        {"role_id": "role-a", "deliverables": [{"id": "d-a", "kind": "brief"}]},
        {
            "role_id": "role-b",
            "deliverables": [{"id": "d-b", "kind": "brief"}],
            "depends_on": ["role-a"] if depends else [],
        },
    ]
    spec_path.write_text(
        json.dumps(
            {
                "roles": [_role("role-a", "fn-a"), _role("role-b", "fn-b")],
                "branches": branches,
            }
        )
    )
    return spec_path


def test_propose_without_commit_does_not_mutate_graph(tmp_path: Path) -> None:
    graph = tmp_path / "graph.json"
    spec = _spec(tmp_path)
    before = hashlib.sha256(graph.read_bytes()).hexdigest() if graph.exists() else "absent"
    result = runner.invoke(
        company_app,
        [
            "propose",
            "--objective",
            "Grow audience.",
            "--spec",
            str(spec),
            "--graph",
            str(graph),
            "--json",
        ],
    )
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["version"] == 1
    assert payload["status"] == "proposed"
    after = hashlib.sha256(graph.read_bytes()).hexdigest() if graph.exists() else "absent"
    assert before == after


def test_propose_commit_then_show_topology_join(tmp_path: Path) -> None:
    graph = tmp_path / "graph.json"
    spec = _spec(tmp_path, depends=True)
    result = runner.invoke(
        company_app,
        [
            "propose",
            "--objective",
            "Grow audience.",
            "--spec",
            str(spec),
            "--graph",
            str(graph),
            "--commit",
            "--json",
        ],
    )
    assert result.exit_code == 0, result.output
    payload = json.loads(result.stdout)
    assert payload["status"] == "committed"
    epic_id = payload["epic_id"]
    assert len(payload["children"]) == 2

    show = runner.invoke(company_app, ["show", epic_id, "--graph", str(graph), "--json"])
    assert show.exit_code == 0, show.output
    assert len(json.loads(show.stdout)["children"]) == 2

    child_b = next(c for c in payload["children"] if c["role_id"] == "role-b")
    topo = runner.invoke(
        company_app, ["topology", child_b["node_id"], "--graph", str(graph), "--json"]
    )
    assert topo.exit_code == 0, topo.output
    topo_payload = json.loads(topo.stdout)
    assert topo_payload["shape"] in ("direct", "loop", "squad", "pipeline")
    assert topo_payload["source"] == "inference"

    join = runner.invoke(company_app, ["join", epic_id, "--graph", str(graph), "--json"])
    assert join.exit_code == 0, join.output
    join_payload = json.loads(join.stdout)
    assert join_payload["aggregate"] == "unknown"
    assert len(join_payload["branches"]) == 2

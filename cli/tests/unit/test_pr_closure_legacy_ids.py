"""Compact legacy ids stay resolvable through PR closure consumers."""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

from fno.graph._constants import is_wellformed_node_id
from fno.graph._reconcile import bind_pr_rows
from fno.pr.closure import branch_node_ids, render_pr_closure_trailer

REPO = Path(__file__).resolve().parents[3]


def test_compact_legacy_id_is_a_valid_read_shape():
    assert is_wellformed_node_id("xd863")
    assert is_wellformed_node_id("x664b")
    assert not is_wellformed_node_id("xg863")


def test_compact_branch_candidate_is_exact_and_delimiter_bounded():
    assert branch_node_ids("feature/xd863") == ["xd863"]
    assert branch_node_ids("feature/xd863-close") == ["xd863"]
    assert branch_node_ids("feature/xd863g") == []


def test_compact_target_and_contained_child_are_rendered(monkeypatch):
    rendered = {}

    def render(ids):
        rendered["ids"] = list(ids)
        return "Fixes " + " ".join(ids)

    monkeypatch.setattr("fno.pr.closure.render_closure_trailer", render)
    entries = [
        {"id": "xd863", "contained_in": None},
        {"id": "x664b", "contained_in": "xd863"},
    ]

    assert render_pr_closure_trailer(entries, "xd863") == "Fixes xd863 x664b"
    assert rendered["ids"] == ["xd863", "x664b"]


def test_compact_closure_claims_bind_only_existing_graph_rows():
    entries = [
        {"id": "xd863", "pr_number": None, "pr_url": None, "additional_prs": []},
        {"id": "x664b", "pr_number": None, "pr_url": None, "additional_prs": []},
    ]

    result = bind_pr_rows(
        entries,
        ["xd863", "x664b"],
        pr_number=42,
        pr_url="https://github.com/example/project/pull/42",
    )

    assert result.outcome == "bound"
    assert [row["pr_number"] for row in entries] == [42, 42]


def test_closure_gate_checks_compact_branch_claims():
    env = dict(os.environ)
    env["PR_HEAD_REF"] = "feature/xd863"
    env["PR_BODY"] = "Fixes xd863 x664b"
    result = subprocess.run(
        ["bash", str(REPO / "scripts" / "ci" / "check-pr-node-closure.sh")],
        capture_output=True,
        text=True,
        check=False,
        env=env,
    )

    assert result.returncode == 0, result.stderr
    assert "all present in the exact trailer" in result.stdout

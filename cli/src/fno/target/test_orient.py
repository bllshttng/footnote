"""Tests for the target orientation report (x-a7be, change A)."""
from __future__ import annotations

import os
from pathlib import Path

from fno.target import orient


def test_render_aligns_all_six_lines() -> None:
    lines = [orient.OrientLine("node", "fresh"), orient.OrientLine("done-when", "x")]
    out = orient.render(lines)
    assert "node:" in out and "done-when:" in out
    # labels right-padded to a common width
    assert out.splitlines()[0].startswith("node:     ")


def test_node_line_no_node() -> None:
    assert orient._node_line(None, Path("/")).startswith("fresh")


def test_node_line_not_in_graph(monkeypatch) -> None:
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: None)
    line = orient._node_line("x-zzzz", Path("/"))
    assert "unknown" in line and "fno backlog get x-zzzz" in line


def test_node_line_shipped(monkeypatch) -> None:
    monkeypatch.setattr(
        orient, "_graph_entry", lambda *_: {"_status": "done", "pr_number": 42}
    )
    assert orient._node_line("x-1", Path("/"), manifest_raw={}) == "shipped (PR #42 merged)"


def test_node_line_half_done(monkeypatch) -> None:
    monkeypatch.setattr(
        orient, "_graph_entry", lambda *_: {"_status": "ready", "pr_number": 7}
    )
    assert orient._node_line("x-1", Path("/"), manifest_raw={}) == "half-done (PR #7)"


def test_node_line_in_progress_from_manifest_claim(monkeypatch) -> None:
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: {"_status": "ready"})
    raw = {"target_claim_key": "node:x-1", "target_claim_holder": "target-session:abc"}
    line = orient._node_line("x-1", Path("/"), manifest_raw=raw)
    assert "in-progress" in line and "target-session:abc" in line


def test_node_line_graph_error_degrades(monkeypatch) -> None:
    def boom(*_):
        raise RuntimeError("graph blew up")

    monkeypatch.setattr(orient, "_graph_entry", boom)
    line = orient._node_line("x-1", Path("/"), manifest_raw={})
    assert "unknown" in line and "resolve:" in line


def test_attended_line_from_manifest() -> None:
    assert orient._attended_line({"attended": True}).startswith("true")
    assert orient._attended_line({"attended": False}).startswith("false")


def test_attended_line_substrate(monkeypatch) -> None:
    monkeypatch.delenv("FNO_AGENT_SELF", raising=False)
    monkeypatch.delenv("FNO_BG", raising=False)
    monkeypatch.delenv("TARGET_UNATTENDED", raising=False)
    assert orient._attended_line(None).startswith("true")
    monkeypatch.setenv("FNO_AGENT_SELF", "worker-x")
    assert orient._attended_line(None).startswith("false")


def test_worktree_line(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(orient, "_is_linked_worktree", lambda _: False)
    line = orient._worktree_line(tmp_path, "x-9")
    assert "fno target start x-9" in line
    monkeypatch.setattr(orient, "_is_linked_worktree", lambda _: True)
    assert orient._worktree_line(tmp_path, "x-9") == str(tmp_path)


def test_tests_line_detection(tmp_path) -> None:
    assert "unknown" in orient._tests_line(tmp_path)
    (tmp_path / "pyproject.toml").write_text("[project]\n", encoding="utf-8")
    assert "pytest" in orient._tests_line(tmp_path)
    (tmp_path / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
    assert "cargo test" in orient._tests_line(tmp_path)


def test_done_when_advisory() -> None:
    assert "advisory" in orient._done_when_line({"no_ship": "true"}, Path("/"))


def test_done_when_pr_and_handoff(monkeypatch) -> None:
    monkeypatch.setattr(orient, "_required_bots", lambda _: ["codex-bot"])
    line = orient._done_when_line({"attended": False}, Path("/"))
    assert "codex-bot" in line and "hand off" in line


def test_done_when_no_review_gate(monkeypatch) -> None:
    monkeypatch.setattr(orient, "_required_bots", lambda _: [])
    line = orient._done_when_line({"attended": True}, Path("/"))
    assert "PR + CI only" in line and "hand off" not in line


def test_plan_line(tmp_path) -> None:
    assert "none" in orient._plan_line(None, tmp_path)
    plan = tmp_path / "p.md"
    plan.write_text("edits `a/b.py`\n", encoding="utf-8")
    assert "stale-reference" in orient._plan_line(str(plan), tmp_path)


def test_build_report_is_read_only_six_lines(monkeypatch, tmp_path) -> None:
    monkeypatch.setattr(orient, "_graph_entry", lambda *_: None)
    lines = orient.build_report(tmp_path, node_id="x-1", plan_path=None, manifest_raw={})
    labels = [ln.label for ln in lines]
    assert labels == ["node", "attended", "worktree", "tests", "plan", "done-when"]


def test_self_check_runs() -> None:
    orient._self_check()

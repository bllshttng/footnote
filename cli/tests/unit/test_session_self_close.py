"""`session add --ended-at` self-close: honest receipt, foreign-row guard."""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path

import pytest


NODE = "x-selfc001"

AMBIENT = "sess-ambient"
OWNER = "sess-owner"


def _make_graph(tmp_path: Path, entries: list[dict]) -> Path:
    g = tmp_path / "graph.json"
    seed_graph(g, json.dumps({"entries": entries}, indent=2) + "\n")
    return g


def _patch_graph(monkeypatch, graph_path: Path) -> None:
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", graph_path)
    monkeypatch.setattr(gc, "GRAPH_MD", graph_path.parent / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", graph_path)


def _node_with_open_do_row(session_id: str) -> dict:
    return {
        "id": NODE,
        "title": "t",
        "status": "in_progress",
        "sessions": [
            {
                "phase": "execute",
                "harness": "claude",
                "session_id": session_id,
                "started_at": "2026-09-12T00:00:00Z",
            }
        ],
    }


def _stub_slug(monkeypatch, slug):
    import fno.graph._reconcile as R

    monkeypatch.setattr(R, "resolve_current_repo_slug", lambda *a, **k: slug)


def _invoke(monkeypatch, g: Path, *args: str):
    from typer.testing import CliRunner

    import fno.graph.cli as C

    monkeypatch.setattr(C, "_graph_path", lambda: g)
    return CliRunner().invoke(C.cli, ["session", "add", NODE, *args])


def _row(g: Path) -> dict:
    from fno.graph.store import read_graph_strict

    return read_graph_strict(g)[0]["sessions"][0]


def test_ac2_hp_the_owning_session_self_closes_and_receipt_says_ended(
    tmp_path, monkeypatch
):
    g = _make_graph(tmp_path, [_node_with_open_do_row(AMBIENT)])
    _patch_graph(monkeypatch, g)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", AMBIENT)

    r = _invoke(
        monkeypatch, g, "--phase", "do", "--ended-at", "2026-09-13T12:00:00Z", "--json"
    )
    assert r.exit_code == 0, r.output
    out = json.loads(r.output)
    assert out["status"] == "ended"
    assert out["added"] is False
    row = _row(g)
    assert row["ended_at"] == "2026-09-13T12:00:00Z"


def test_ac2_err_ending_another_sessions_open_row_refuses(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [_node_with_open_do_row(OWNER)])
    _patch_graph(monkeypatch, g)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", AMBIENT)

    r = _invoke(
        monkeypatch,
        g,
        "--phase", "do",
        "--harness", "claude",
        "--session-id", OWNER,
        "--ended-at", "2026-09-13T12:00:00Z",
    )
    assert r.exit_code == 2
    assert "fno backlog session reap-open" in r.output
    assert _row(g).get("ended_at") is None


def test_ac2_edge_a_backfill_with_no_prior_row_records(tmp_path, monkeypatch):
    g = _make_graph(tmp_path, [{"id": NODE, "title": "t", "sessions": []}])
    _patch_graph(monkeypatch, g)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", AMBIENT)

    r = _invoke(
        monkeypatch,
        g,
        "--phase", "do",
        "--harness", "claude",
        "--session-id", "s-old",
        "--ended-at", "2026-09-13T12:00:00Z",
    )
    assert r.exit_code == 0, r.output
    assert "recorded execute claude:s-old" in r.output
    assert r.output.count("ended") == 0
    assert _row(g)["ended_at"] == "2026-09-13T12:00:00Z"


def test_a_reclose_of_an_already_closed_row_still_reads_duplicate(
    tmp_path, monkeypatch
):
    g = _make_graph(tmp_path, [_node_with_open_do_row(AMBIENT)])
    _patch_graph(monkeypatch, g)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", AMBIENT)

    first = _invoke(
        monkeypatch, g, "--phase", "do", "--ended-at", "2026-09-13T12:00:00Z"
    )
    assert first.exit_code == 0, first.output
    assert "ended execute" in first.output

    second = _invoke(
        monkeypatch, g, "--phase", "do", "--ended-at", "2026-09-13T13:00:00Z"
    )
    assert second.exit_code == 0, second.output
    assert "already recorded" in second.output


@pytest.mark.parametrize("argv", [
    ("--pr-number", "1500", "--phase", "do"),
])
def test_pr_mode_self_close_of_a_foreign_open_row_refuses_too(
    tmp_path, monkeypatch, argv
):
    """The --pr-number path resolves one node before the stamp, so the same
    guard fires there - no second door to another session's row."""
    from typer.testing import CliRunner

    import fno.graph.cli as C

    g = _make_graph(
        tmp_path,
        [dict(
            _node_with_open_do_row(OWNER),
            pr_number=1500,
            pr_url="https://github.com/bllshttng/footnote/pull/1500",
        )],
    )
    _stub_slug(monkeypatch, "bllshttng/footnote")
    _patch_graph(monkeypatch, g)
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", AMBIENT)
    monkeypatch.setattr(C, "_graph_path", lambda: g)

    r = CliRunner().invoke(
        C.cli,
        ["session", "add", *argv, "--harness", "claude", "--session-id", OWNER,
         "--ended-at", "2026-09-13T12:00:00Z"],
    )
    assert r.exit_code == 2
    assert "only that session ends it" in r.output

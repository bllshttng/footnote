"""Integration tests for slug resolution + display in the backlog CLI (ab-f82e8083).

Covers: `get` by slug / bare-hex, `find` high-recall + handle-leading output +
slug in JSON, `ready` slug-leading rows, and the idempotent `backfill-slugs` verb.
"""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    # Seam readers (guarded metadata/display reads) resolve paths.graph_json
    # at call time; pin the resolver to the same hermetic file.
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    seed_graph(g, json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    from fno.graph.store import read_graph_strict

    return read_graph_strict(g)


# -- ready: slug leads -------------------------------------------------------


def test_ready_rows_lead_with_slug(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "status": "ready", "domain": "code", "project": "fno", "plan_path": "p.md"},
    ])
    result = runner.invoke(app, ["backlog", "ready", "--all"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert data[0]["slug"] == "dashless-spawn"


# -- backfill-slugs ----------------------------------------------------------


# -- update --details --------------------------------------------------------


def test_roadmap_only_public_no_leaks(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-11111111", "title": "Public feature", "slug": "pub", "status": "ready",
         "priority": "p1", "size": "M", "project": "fno", "public": True,
         "plan_path": "internal/fno/plans/secret.md", "cwd": "/private/x"},
        {"id": "ab-22222222", "title": "Private thing", "slug": "priv", "status": "ready",
         "priority": "p2", "project": "fno"},  # absent public flag -> included
        {"id": "ab-22222223", "title": "Explicitly private", "slug": "private", "status": "ready",
         "priority": "p2", "project": "fno", "public": False},
        {"id": "ab-33333333", "title": "Other project pub", "slug": "op", "status": "ready",
         "priority": "p1", "project": "other", "public": True},  # wrong project -> excluded
    ])
    result = runner.invoke(app, ["backlog", "roadmap", "--project", "fno"])
    assert result.exit_code == 0, result.output
    out = result.stdout
    assert "Public feature" in out
    assert "Private thing" in out
    assert "Explicitly private" not in out
    assert "Other project pub" not in out
    # No internal fields leak.
    assert "ab-11111111" not in out
    assert "secret.md" not in out
    assert "/private/x" not in out
    # Grouped under the Now column (p1).
    assert "## Now" in out


def test_roadmap_backlog_html_hands_off_public_render(tmp_graph, tmp_path, monkeypatch):
    """--backlog-html hands the project to board-render's public mode.

    Selection and the title gate run in the Rust renderer; the request
    carries only the target path and the project scope, and a failed render
    exits 1 with the reason.
    """
    import subprocess

    _seed(tmp_graph, [
        {"id": "ab-11111111", "title": "Public feature", "slug": "pub", "status": "ready",
         "priority": "p1", "project": "fno"},
        {"id": "ab-22222223", "title": "Explicitly private", "slug": "private",
         "status": "ready", "priority": "p2", "project": "fno", "public": False},
        {"id": "ab-33333333", "title": "Leaky at /Users/bb/tmp", "slug": "leak",
         "status": "ready", "priority": "p2", "project": "fno"},
    ])
    seen: dict = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        seen["request"] = json.loads(kwargs["input"])
        return subprocess.CompletedProcess(
            argv, 0,
            stdout='{"written": [{"path": "x", "cards": 1}], "failed": []}',
            stderr="",
        )

    def refusing_run(argv, **kwargs):
        return subprocess.CompletedProcess(
            argv, 1, stdout="",
            stderr="board-render: bad request JSON: unknown field `public`",
        )

    monkeypatch.setattr(
        "fno.rust_binary.resolve_front_binary", lambda: tmp_path / "stub-front"
    )
    monkeypatch.setattr(subprocess, "run", fake_run)
    backlog = tmp_path / "backlog.html"

    result = runner.invoke(
        app,
        ["backlog", "roadmap", "--project", "fno", "--backlog-html", str(backlog)],
    )
    assert result.exit_code == 0, result.output
    assert seen["argv"][-1] == "board-render"
    assert seen["request"]["targets"] == [{"path": str(backlog)}]
    assert seen["request"]["public"] == {"project": "fno"}

    monkeypatch.setattr(subprocess, "run", refusing_run)
    failed = runner.invoke(
        app,
        ["backlog", "roadmap", "--project", "fno", "--backlog-html", str(backlog)],
    )
    assert failed.exit_code == 1
    assert "predates the public board render" in (failed.stdout + (failed.stderr or ""))


def test_roadmap_includes_archive_only_shipped_row(tmp_graph, tmp_path):
    _seed(tmp_graph, [
        {"id": "ab-live0001", "title": "Live marker", "status": "ready",
         "priority": "p1", "project": "fno"},
        {"id": "ab-done0001", "title": "Archive shipped marker", "status": "done",
         "priority": "p2", "project": "fno",
         "completed_at": "2026-08-20T00:00:00Z",
         "archived_at": "2026-08-21T00:00:00Z"},
    ])

    result = runner.invoke(app, ["backlog", "roadmap", "--project", "fno"])

    assert result.exit_code == 0, result.output
    assert "## Shipped" in result.stdout
    assert "Archive shipped marker" in result.stdout


def test_view_refuses_when_the_board_render_fails(tmp_graph, monkeypatch):
    def _failing():
        return 1

    monkeypatch.setattr("fno.graph.roadmap_public.render_local_targets", _failing)

    result = runner.invoke(app, ["backlog", "view"])

    assert result.exit_code != 0
    assert "local board render failed" in (result.stdout + (result.stderr or ""))


@pytest.mark.parametrize(
    "argv",
    [
        ["backlog", "roadmap", "--project", "fno"],
    ],
)
def test_html_views_refuse_stale_local_graph_under_external_tracker(
    tmp_graph, monkeypatch, argv
):
    _seed(tmp_graph, [
        {"id": "ab-local001", "title": "STALE-LOCAL-MARKER", "status": "ready",
         "priority": "p1", "project": "fno"},
    ])
    monkeypatch.setenv("FNO_TRACKER_BACKEND", "github")

    result = runner.invoke(app, argv)

    assert result.exit_code == 2
    output = result.stdout + (result.stderr or "")
    assert "stale local" in output.lower() or "external" in output.lower()
    assert "STALE-LOCAL-MARKER" not in output


@pytest.mark.parametrize(
    "argv",
    [
        ["backlog", "roadmap", "--project", "fno"],
    ],
)
def test_html_views_refuse_corrupt_live_graph_even_with_healthy_archive(
    tmp_graph, tmp_path, monkeypatch, argv
):
    from fno.graph import _constants as graph_constants
    from fno.graph import store as graph_store

    bad_graph = tmp_path / "corrupt-graph.json"
    bad_db = bad_graph.with_suffix(".db")
    bad_db.write_text("{broken", encoding="utf-8")
    monkeypatch.setattr(graph_constants, "GRAPH_JSON", bad_graph)
    monkeypatch.setattr(graph_store, "GRAPH_JSON", bad_graph)
    monkeypatch.setattr("fno.paths.graph_json", lambda: bad_graph)
    monkeypatch.setattr(
        graph_constants, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json"
    )
    (tmp_path / "graph-archive.json").write_text(
        json.dumps({"entries": [
            {"id": "ab-archive1", "title": "ARCHIVE-ONLY-SUCCESS-MARKER",
             "status": "done", "priority": "p2", "project": "fno"},
        ]}),
        encoding="utf-8",
    )

    result = runner.invoke(app, argv)

    assert result.exit_code != 0
    output = result.stdout + (result.stderr or "")
    assert "canonical graph read failed" in output
    assert "ARCHIVE-ONLY-SUCCESS-MARKER" not in output


def test_public_title_gate_omits_leaky_rows_and_publishes_the_rest(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-11111111", "title": "Clean row publishes", "status": "ready",
         "priority": "p1", "project": "fno"},
        # Node ids and PR numbers are ruled public; the row publishes.
        {"id": "ab-22222222", "title": "Closes PR #123 for ab-deadbeef", "status": "ready",
         "priority": "p2", "project": "fno"},
        # Machine-revealing classes cost the row, not the page.
        {"id": "ab-33333333", "title": "crashed for /Users/alice/secret ses-kv3H",
         "status": "ready", "priority": "p2", "project": "fno"},
    ])

    result = runner.invoke(app, ["backlog", "roadmap", "--project", "fno"])

    assert result.exit_code == 0, result.output
    assert "Clean row publishes" in result.stdout
    assert "Closes PR #123 for ab-deadbeef" in result.stdout
    assert "ab-33333333" not in result.stdout
    assert "/Users/alice/secret" not in result.stdout
    assert "ses-kv3H" not in result.stdout
    # One counted warning names the classes, never the offending title.
    output = result.stdout + (result.stderr or "")
    assert "omitted 1 public row" in output
    assert "home-path" in output
    assert "session-id" in output


def test_roadmap_uses_live_epic_priority_and_shared_order(
    tmp_graph, tmp_path
):
    _seed(tmp_graph, [
        {"id": "ab-live0001", "title": "Live epic", "type": "epic",
         "status": "ready", "priority": "p1", "project": "fno"},
        {"id": "ab-child001", "title": "Promoted child", "status": "in_progress",
         "priority": "p2", "project": "fno", "public": True,
         "parent": "ab-live0001"},
        {"id": "ab-loose001", "title": "Loose now", "status": "ready",
         "priority": "p1", "project": "fno", "public": True},
        {"id": "ab-dead0001", "title": "Dead epic", "type": "epic",
         "status": "superseded", "priority": "p0", "project": "fno"},
        {"id": "ab-child002", "title": "Unpromoted child", "status": "ready",
         "priority": "p2", "project": "fno", "public": True,
         "parent": "ab-dead0001"},
        {"id": "ab-active01", "title": "Active epic", "type": "epic",
         "status": "ready", "priority": "p2", "project": "fno", "public": True},
        {"id": "ab-done001", "title": "Done child", "status": "done",
         "priority": "p2", "project": "fno", "parent": "ab-active01",
         "completed_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-claimed1", "title": "Claimed later", "status": "in_progress",
         "priority": "p3", "project": "fno", "public": True},
    ])

    md = runner.invoke(app, ["backlog", "roadmap", "--project", "fno"]).stdout
    md_now = md.split("## Now", 1)[1].split("## Next", 1)[0]
    md_next = md.split("## Next", 1)[1]
    assert md_now.index("Promoted child") < md_now.index("Loose now")
    assert "Active epic" in md_now
    assert "Claimed later" in md_now
    assert "Unpromoted child" in md_next


def test_roadmap_folds_triage_into_later(tmp_graph):
    # A queued node routes to Triage internally; the public roadmap shows it
    # under Later (Triage is not a public column).
    _seed(tmp_graph, [
        {"id": "ab-77777777", "title": "Queued p1 item", "slug": "q", "status": "ready",
         "priority": "p1", "project": "fno", "public": True, "queued_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-88888888", "title": "Plain p3 item", "slug": "p3", "status": "ready",
         "priority": "p3", "project": "fno", "public": True},
    ])
    out = runner.invoke(app, ["backlog", "roadmap", "--project", "fno"]).stdout
    assert "## Later" in out
    assert "Queued p1 item" in out   # folded in despite being Triage internally
    assert "Plain p3 item" in out
    assert "## Triage" not in out    # no public Triage column

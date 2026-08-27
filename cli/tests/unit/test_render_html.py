"""Tests for the canonical local and public backlog dashboard renderer."""
from __future__ import annotations

import inspect
import re
from pathlib import Path

from fno.graph.render_html import (
    UNSCOPED_LABEL,
    _obsidian_url,
    _project_color,
    render_graph_html,
)


def _entry(eid: str, **kwargs) -> dict:
    base = {
        "id": eid,
        "title": eid,
        "type": "feature",
        "priority": "p2",
        "completed_at": None,
        "deferred_at": None,
        "session_id": None,
        "status": "ready",
        "blocked_by": [],
        "plan_path": None,
        "pr_url": None,
        "pr_number": None,
        "project": None,
        "created_at": "2026-01-01T00:00:00Z",
        "touched_at": "2026-01-02T00:00:00Z",
    }
    base.update(kwargs)
    return base


def test_canonical_dashboard_has_shared_filters_and_private_detail_data(tmp_path: Path):
    entries = [
        _entry(
            "local-marker",
            project="alpha",
            status="in_review",
            details="private detail marker",
            plan_path="/Users/me/private-plan.md",
            pr_number=1208,
            pr_url="https://github.com/acme/project/pull/1208",
        ),
        _entry("unknown-marker", project="beta", status="future_status"),
    ]
    out = tmp_path / "graph.html"

    render_graph_html(entries, out)

    text = out.read_text()
    for marker in (
        'id="stats"',
        'id="statusChips"',
        'id="projectChips"',
        'id="q"',
        'id="fromDate"',
        'id="prioSel"',
        'id="sizeSel"',
        'id="planOnly"',
        'id="prOnly"',
        'id="board"',
        'class="group"',
    ):
        assert marker in text
    assert "local-marker" in text
    assert "private detail marker" in text
    assert "/Users/me/private-plan.md" in text
    assert "future_status" in text
    assert "has a PR" in text


def test_public_and_local_dashboards_share_skeleton_but_not_private_fields(tmp_path: Path):
    entry = _entry(
        "shared-marker",
        project="alpha",
        title="shared title",
        details="private description",
        plan_path="/Users/me/private-plan.md",
    )
    local_path = tmp_path / "local.html"
    render_graph_html([entry], local_path)

    from fno.graph.roadmap_public import render_public_backlog_html

    public = render_public_backlog_html([entry], "alpha")
    local = local_path.read_text()
    for marker in ('id="stats"', 'id="statusChips"', 'id="projectChips"', 'class="group"'):
        assert marker in local and marker in public
    assert "shared title" in local and "shared title" in public
    assert "shared-marker" in local and "shared-marker" not in public
    assert "private description" in local and "private description" not in public
    assert "/Users/me/private-plan.md" in local and "/Users/me/private-plan.md" not in public


def test_project_filter_keeps_rows_in_dom_and_lists_unscoped(tmp_path: Path):
    entries = [
        _entry("alpha-card", project="alpha"),
        _entry("beta-card", project="beta"),
        _entry("unscoped-card", project=None),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()

    assert set(re.findall(r'data-project="([^"]+)"', text)) == {
        "alpha",
        "beta",
        UNSCOPED_LABEL,
    }
    assert "alpha-card" in text and "beta-card" in text and "unscoped-card" in text
    assert UNSCOPED_LABEL in text
    assert "is-hidden" in text
    assert "fno-kanban-project-state" in text
    assert "localStorage.getItem(PROJECT_KEY)" in text
    assert "localStorage.setItem(PROJECT_KEY" in text
    assert "className = 'row' + (matches(n) ? '' : ' is-hidden')" in text


def test_scoped_render_preserves_cross_project_relationship_context(tmp_path: Path):
    entries = [
        _entry("parent-card", project="alpha", title="Alpha parent"),
        _entry(
            "child-card",
            project="beta",
            title="Beta child",
            blocked_by=["parent-card"],
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out, project="beta")
    text = out.read_text()
    assert "Beta child" in text
    assert "parent-card" in text
    assert "Alpha parent" in text
    assert '"id":"parent-card"' in text
    assert '"id":"child-card"' in text


def test_public_projection_never_emits_private_plan_links():
    from fno.graph.roadmap_public import render_public_backlog_html, render_public_roadmap_html

    entry = _entry(
        "public-marker",
        project="fno",
        title="clean public title",
        details="private description",
        plan_path="/Users/me/private-plan.md",
    )
    for document in (
        render_public_roadmap_html([entry], "fno"),
        render_public_backlog_html([entry], "fno"),
    ):
        assert "clean public title" in document
        assert "public-marker" not in document
        assert "private description" not in document
        assert "/Users/me/private-plan.md" not in document
        assert "obsidian://" not in document


def test_renderer_modules_do_not_open_or_parse_graph_json_directly():
    import ast
    from fno.graph import roadmap_public, render_html

    for module in (render_html, roadmap_public):
        source = inspect.getsource(module)
        tree = ast.parse(source)
        assert not any(
            isinstance(node, (ast.Import, ast.ImportFrom))
            and any(alias.name == "json" for alias in node.names)
            for node in ast.walk(tree)
        )


def test_obsidian_url_builds_and_normalizes_internal_paths():
    expected = "obsidian://open?vault=myvault&file=internal/fno/plans/plan"
    assert _obsidian_url("myvault", "internal/fno/plans/plan.md") == expected
    assert _obsidian_url("myvault", "~/myvault/internal/fno/plans/plan.md") == expected
    assert _obsidian_url("myvault", "src/not-a-plan.py") is None


def test_project_color_is_deterministic():
    assert _project_color("gamma") == _project_color("gamma")
    assert _project_color(None) == "hsl(0, 0%, 70%)"


def test_public_rows_keep_safe_plan_and_pr_flags_without_private_values():
    from fno.graph.roadmap_public import render_public_backlog_html

    document = render_public_backlog_html(
        [
            _entry(
                "public-flags",
                project="alpha",
                plan_path="/Users/me/private-plan.md",
                pr_number=1208,
                pr_url="https://github.com/acme/project/pull/1208",
            )
        ],
        "alpha",
    )

    assert '"pl":true' in document
    assert '"pr":true' in document
    assert "/Users/me/private-plan.md" not in document
    assert "1208" not in document


def test_dashboard_date_filter_only_applies_to_closed_statuses():
    from fno.graph.render_html import _DASHBOARD_JS

    assert "n.s === 'done' || n.s === 'superseded'" in _DASHBOARD_JS
    assert "state.from" in _DASHBOARD_JS


def test_local_rows_include_reverse_successor_context(tmp_path: Path):
    entries = [
        _entry("blocker", project="alpha", title="Blocker"),
        _entry("dependent", project="alpha", title="Dependent", blocked_by=["blocker"]),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)

    text = out.read_text()
    assert '"su":[{"id":"dependent"' in text
    assert "Unblocks" in text


def test_legacy_object_project_state_preserves_true_selected_projects():
    from fno.graph.render_html import _DASHBOARD_JS

    assert "saved[p] === true" in _DASHBOARD_JS
    assert "saved[p] === false" not in _DASHBOARD_JS


def test_dashboard_contains_static_content_without_javascript(tmp_path: Path):
    out = tmp_path / "graph.html"
    render_graph_html(
        [_entry("no-js-marker", project="alpha", title="Visible without JavaScript")],
        out,
    )

    text = out.read_text()
    main = text.split('<main id="board">', 1)[1].split("</main>", 1)[0]
    assert "Visible without JavaScript" in main
    assert 'class="group"' in main
    assert 'class="fallback-detail"' in main


def test_legacy_all_hidden_project_state_remains_an_active_empty_selection():
    from fno.graph.render_html import _DASHBOARD_JS

    assert "projectFilterActive" in _DASHBOARD_JS
    assert "saved[p] === true" in _DASHBOARD_JS
    assert "state.projectFilterActive" in _DASHBOARD_JS

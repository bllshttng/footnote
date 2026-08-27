"""Tests for the canonical local and public backlog dashboard renderer."""
from __future__ import annotations

import inspect
import re
from pathlib import Path

from fno.graph.render_html import (
    UNSCOPED_LABEL,
    _dashboard_rows,
    _obsidian_url,
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
    assert "r.el.className = 'row' + (ok ? '' : ' is-hidden')" in text
    # The group header follows the same rule, over the VISIBLE rows, so a
    # fully filtered group cannot paint as an empty section claiming a count.
    assert "sec.el.className = 'group' + (vis.length ? '' : ' is-hidden')" in text
    assert "sec.count.textContent = vis.length" in text
    # The board is built once and only toggled after that, so collapsing a
    # group or opening a row detail survives the next keystroke.
    assert text.count("board.innerHTML = ''") == 1
    assert "  build();" in text


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


def test_dashboard_initially_selects_all_nonterminal_statuses():
    from fno.graph.render_html import _DASHBOARD_JS

    assert (
        "ORDER.filter(function (s) { return s !== 'superseded' && "
        "(s !== 'done' || DATA.initial_done); })"
    ) in _DASHBOARD_JS


def test_dashboard_derives_done_from_completion_fact(tmp_path: Path):
    out = tmp_path / "graph.html"
    render_graph_html(
        [_entry("completed-marker", status="ready", completed_at="2026-08-27T00:00:00Z")],
        out,
    )

    text = out.read_text()
    assert '"s":"done"' in text
    assert '"s":"ready"' not in text


def _payload(document: str) -> str:
    """The data-bearing half of a rendered board: markup plus the JSON payload.

    The inline stylesheet and the behavior script are authored constants that
    carry no node data, and a CSS hex colour such as ``#707782`` matches the
    gate's pr-reference pattern. Dropping them keeps the probe pointed at the
    only two places an entry field can reach: the static markup and the JSON.
    """
    for opening, closing in (("<style>", "</style>"), ("<script>", "</script>")):
        while opening in document and closing in document:
            head, rest = document.split(opening, 1)
            document = head + rest.split(closing, 1)[1]
    return document


def test_public_document_carries_no_leak_class_the_gate_names(tmp_path: Path):
    """The gate's own regexes, run over the WHOLE public document.

    The gate scans titles only. This scans every byte the public surface
    emits with the same LEAK_PATTERNS list, so a private field reaching the
    markup through any other route still fails here. The local render is the
    paired positive control: the same probe MUST fire on it, which is what
    proves the probe can fire at all.
    """
    from fno.graph.render_html import LEAK_PATTERNS
    from fno.graph.roadmap_public import render_public_backlog_html

    entry = _entry(
        "ab-4f470abc",
        project="fno",
        title="a clean public title",
        details="private description",
        plan_path="/Users/me/internal/fno/plans/private-plan.md",
        pr_number=1208,
        pr_url="https://github.com/o/r/pull/1208",
    )
    public = render_public_backlog_html([entry], "fno")
    fired = sorted(name for name, pattern in LEAK_PATTERNS if pattern.search(_payload(public)))
    assert fired == [], f"public document leaks {fired}"

    local_path = tmp_path / "local.html"
    render_graph_html([entry], local_path)
    local_fired = sorted(
        name
        for name, pattern in LEAK_PATTERNS
        if pattern.search(_payload(local_path.read_text()))
    )
    assert "node-id" in local_fired and "home-path" in local_fired, (
        f"probe never fires, so the public zero proves nothing: {local_fired}"
    )


def test_default_local_path_renders_the_canonical_dashboard(tmp_path: Path, monkeypatch):
    """The surface the operator names: ~/.fno/graph.html, path resolved by default.

    Every other test passes an explicit path, so none of them covers the
    lazily-resolved GRAPH_HTML default that the auto-render hook actually
    writes. Assert the canonical skeleton, and assert the replaced card
    projection is absent.
    """
    from fno.graph import _constants
    from fno.graph import render_html as module

    default = tmp_path / "graph.html"
    monkeypatch.setattr(_constants, "GRAPH_HTML", default)

    module.render_graph_html([_entry("ab-defa0117", project="fno", title="Default path")])
    text = default.read_text()
    for marker in ('id="stats"', 'id="statusChips"', 'id="projectChips"', 'id="fromDate"',
                   'id="planOnly"', 'id="prOnly"', 'class="group"'):
        assert marker in text, f"canonical dashboard marker missing: {marker}"
    assert "Default path" in text and "ab-defa0117" in text
    for card_marker in ('class="card"', 'class="lane"', 'id="show-done"'):
        assert card_marker not in text, f"replaced card projection resurfaced: {card_marker}"


def test_deselecting_every_project_chip_does_not_blank_the_board():
    """An empty selection is no filter, matching the status chips one line up.

    Toggling the last chip off used to leave projectFilterActive true over an
    empty set, so every row failed projectMatch and the board went blank with
    no chip pressed. saveProjects then persisted that state.
    """
    from fno.graph.render_html import _DASHBOARD_JS

    assert "state.projectFilterActive = state.projects.size > 0;" in _DASHBOARD_JS
    assert (
        "return !state.projectFilterActive || !state.projects.size "
        "|| state.projects.has(n.project);"
    ) in _DASHBOARD_JS
    # The one remaining unconditional activation is the ?project= override,
    # where a single named project IS the selection.
    assert _DASHBOARD_JS.count("state.projectFilterActive = true;") == 1
    assert "queryProject" in _DASHBOARD_JS


def test_unscoped_chip_sorts_last_by_the_real_label(tmp_path: Path):
    """The sort compared against 'unscoped'; the sentinel is '(unscoped)'.

    Both arms were dead, so the unscoped chip sorted first. Ship the label in
    the payload instead of repeating a literal that can drift again.
    """
    out = tmp_path / "graph.html"
    render_graph_html([_entry("ab-11111111", project="zeta"),
                       _entry("ab-22222222", project=None)], out)
    text = out.read_text()
    assert f'"unscoped_label":"{UNSCOPED_LABEL}"' in text
    assert "a === UNSCOPED ? 1 : b === UNSCOPED ? -1" in text


def test_roadmap_opens_with_shipped_visible_and_backlog_does_not(tmp_path: Path):
    """A roadmap exists to show shipped work, so it opens with done pressed.

    Every other surface opens on open work. The flag travels in the payload,
    so the one status-seed expression serves both.
    """
    from fno.graph.roadmap_public import (
        render_public_backlog_html,
        render_public_roadmap_html,
    )

    entry = _entry("ab-33333333", project="fno", title="shipped thing", status="done")
    assert '"initial_done":true' in render_public_roadmap_html([entry], "fno")
    assert '"initial_done":false' in render_public_backlog_html([entry], "fno")
    local_path = tmp_path / "local.html"
    render_graph_html([entry], local_path)
    assert '"initial_done":false' in local_path.read_text()


def test_successor_lookup_is_indexed_not_a_per_entry_rescan():
    """The auto-render hook runs on EVERY graph mutation, so this is the
    latency the operator feels on every `fno backlog` command.

    Scanning the source list per entry to find what each one unblocks is
    quadratic. Measured on the real graph shape at 4694 entries: 3.26s that
    way, 0.13s indexed, for the same 424 rows that carry a successor.

    Asserted as a RATIO against the same call with successors disabled, not
    as a wall-clock bound, so it does not go flaky on a loaded machine. A
    per-entry rescan is ~47x; an index is ~2x.
    """
    import time

    entries = [
        {
            "id": f"ab-{i:08x}",
            "title": f"node {i}",
            "status": "ready",
            "project": "fno",
            "created_at": "2026-01-01T00:00:00Z",
            "blocked_by": [f"ab-{i - 1:08x}"] if i else [],
        }
        for i in range(3000)
    ]

    start = time.perf_counter()
    local_rows = _dashboard_rows(entries, local=True, context_entries=entries)
    local_elapsed = time.perf_counter() - start

    # The public projection emits no successors at all, so it is the honest
    # floor for everything else this function does over the same entries.
    start = time.perf_counter()
    _dashboard_rows(entries, local=False, context_entries=entries)
    baseline = time.perf_counter() - start

    assert sum(1 for r in local_rows if r["su"]) == 2999, "successors still populated"
    assert local_elapsed < baseline * 10, (
        f"successor lookup looks quadratic: {local_elapsed:.3f}s local vs "
        f"{baseline:.3f}s baseline over {len(entries)} entries"
    )


def test_successor_rows_match_the_entries_that_name_the_blocker():
    """The index must agree with the scan it replaced, including the case a
    scan handled implicitly: a blocker id that no entry carries."""
    entries = [
        _entry("ab-00000001", project="fno", title="blocker"),
        _entry("ab-00000002", project="fno", title="first", blocked_by=["ab-00000001"]),
        _entry("ab-00000003", project="fno", title="second", blocked_by=["ab-00000001"]),
        _entry("ab-00000004", project="fno", title="unrelated", blocked_by=["ab-missing"]),
    ]
    rows = {r["id"]: r for r in _dashboard_rows(entries, local=True, context_entries=entries)}

    assert sorted(s["id"] for s in rows["ab-00000001"]["su"]) == [
        "ab-00000002",
        "ab-00000003",
    ]
    assert rows["ab-00000002"]["su"] == []
    assert rows["ab-00000004"]["su"] == []
    assert rows["ab-00000004"]["bb"] == [
        {"id": "ab-missing", "s": "not found", "t": ""}
    ], "an unresolvable blocker still reports itself"

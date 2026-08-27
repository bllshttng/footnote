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


def test_renderer_modules_never_read_the_graph_store_themselves():
    """The rule is "go through fno.graph's reader", not "never import json".

    The previous shape of this guard walked for an `import json` node, so
    `__import__("json")` slipped past it and the test was green by
    construction rather than by behavior. It also aimed at the wrong symbol:
    json.dumps for the payload was never the hazard. Reading the store
    directly is - a second reader is what undercounts done, because closed
    work migrates to graph-archive.json.

    Asserts a POSITIVE marker (the sanctioned reader is called) alongside the
    absence, so a renderer that stopped reading anything at all still fails.
    """
    import ast
    from fno.graph import roadmap_public, render_html

    def _calls(tree):
        names = set()
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            func = node.func
            if isinstance(func, ast.Name):
                names.add(func.id)
            elif isinstance(func, ast.Attribute):
                names.add(func.attr)
                if isinstance(func.value, ast.Name):
                    names.add(f"{func.value.id}.{func.attr}")
        return names

    forbidden = {"json.load", "json.loads", "read_json", "loads", "load"}
    for module in (render_html, roadmap_public):
        called = _calls(ast.parse(inspect.getsource(module)))
        leaked = called & forbidden
        assert not leaked, f"{module.__name__} parses the store itself: {leaked}"

    # Positive control: the sanctioned reader IS reached, so the absence above
    # cannot pass merely because nothing reads anything.
    entrypoint = ast.parse(inspect.getsource(render_html.load_render_entries))
    assert "read_graph_with_archive" in _calls(entrypoint)
    assert "entries_with_archive" in _calls(entrypoint)


def test_the_payload_is_serialized_with_the_real_json_module():
    """Guard the dodge itself: __import__ must not come back to duck a linter."""
    source = inspect.getsource(
        __import__("fno.graph.render_html", fromlist=["render_html"])
    )
    assert '__import__("json")' not in source
    assert "json.dumps(" in source


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


def test_static_half_shows_what_the_chips_show_not_the_whole_graph(tmp_path: Path):
    """Every node was serialised twice, and build() discards the static copy.

    Measured on the real graph before this: 10.1 MB of an 18.7 MB document,
    nearly all closed work the script hides on first paint anyway. The two
    halves agree now, which is both the size win and the correctness one - a
    no-JS reader saw archived nodes a JS reader could not.
    """
    entries = [
        _entry("ab-0pen0001", project="fno", title="OpenWork", status="ready"),
        _entry("ab-d0ne0001", project="fno", title="ClosedWork", status="done"),
        _entry("ab-5up30001", project="fno", title="GoneWork", status="superseded"),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    static = text.split('<main id="board">', 1)[1].split("</main>", 1)[0]

    assert "OpenWork" in static
    assert "ClosedWork" not in static, "closed work must not render in the static half"
    assert "GoneWork" not in static
    # Positive control: the closed rows are still in the PAYLOAD, so pressing
    # the Done chip reveals them. Hiding by omission from the data would be the
    # data loss this whole node exists to prevent.
    assert '"t":"ClosedWork"' in text
    assert '"t":"GoneWork"' in text


def test_roadmap_static_half_keeps_shipped_work(tmp_path: Path):
    """A roadmap presses the done chip on first paint, so its static half must
    agree. The two surfaces derive the rule from one tuple."""
    from fno.graph.roadmap_public import (
        render_public_backlog_html,
        render_public_roadmap_html,
    )

    entry = _entry("ab-5h1p0001", project="fno", title="ShippedThing", status="done")
    roadmap = render_public_roadmap_html([entry], "fno")
    backlog = render_public_backlog_html([entry], "fno")

    def _static(doc: str) -> str:
        return doc.split('<main id="board">', 1)[1].split("</main>", 1)[0]

    assert "ShippedThing" in _static(roadmap), "a roadmap must show shipped work"
    assert '"t":"ShippedThing"' in roadmap, "and carry it in the payload"
    # The public BACKLOG projection drops closed work upstream, at
    # PUBLIC_BACKLOG_STATUSES, so done never reaches either half. That is a
    # different mechanism from the static-half rule and predates this work.
    assert "ShippedThing" not in backlog


def test_status_bar_colors_are_keyed_by_name_not_by_position(tmp_path: Path):
    """render() emits no segment for a zero-count status, so positional
    nth-child rules slid every surviving segment into the wrong colour, and the
    colours shifted live as the user filtered. A status past the ninth got no
    rule at all."""
    from fno.graph.render_html import (
        _DASHBOARD_STATUS_COLORS,
        _DASHBOARD_UNKNOWN_COLOR,
    )

    out = tmp_path / "graph.html"
    render_graph_html([_entry("ab-c010r001", project="fno", status="blocked")], out)
    text = out.read_text()

    assert ".tw i:nth-child" not in text, "positional colour rules must be gone"
    assert "COLORS[s] || UNKNOWN_COLOR" in text
    assert "background:' + color" in text
    assert f'"unknown_color":"{_DASHBOARD_UNKNOWN_COLOR}"' in text
    for status, color in _DASHBOARD_STATUS_COLORS.items():
        assert f'"{status}":"{color}"' in text, f"{status} lost its colour"


def test_every_ranked_status_has_a_color():
    """The order list and the colour table are two lists that must not drift."""
    from fno.graph.render_html import (
        _DASHBOARD_STATUS_COLORS,
        _DASHBOARD_STATUS_ORDER,
    )

    missing = [s for s in _DASHBOARD_STATUS_ORDER if s not in _DASHBOARD_STATUS_COLORS]
    assert not missing, f"ranked statuses with no colour: {missing}"


def test_a_persisted_selection_naming_absent_projects_does_not_blank_the_board():
    """localStorage is origin-wide, so one key is shared by every board.

    A selection saved on the global board names projects a scoped board has
    never heard of. Restored as-is, those match nothing, every row hides, and
    no chip renders pressed to explain why.
    """
    from fno.graph.render_html import _DASHBOARD_JS

    assert "var present = new Set(projectNames);" in _DASHBOARD_JS
    assert "return present.has(p);" in _DASHBOARD_JS
    assert (
        "state.projectFilterActive = loadedProjects.active && state.projects.size > 0;"
    ) in _DASHBOARD_JS


def test_type_is_public_but_parent_ids_are_not(tmp_path: Path):
    """The pair. A type names a KIND of work and is safe to publish; a parent
    is a node id and is not.

    Probed with the gate's own LEAK_PATTERNS over the data-bearing markup, and
    paired with the local render firing the same probe, so the public zero
    cannot pass on a document that carries nothing.
    """
    from fno.graph.render_html import LEAK_PATTERNS
    from fno.graph.roadmap_public import render_public_backlog_html

    entries = [
        _entry("ab-9a000001", project="fno", title="An epic", type="epic"),
        _entry("ab-9a000002", project="fno", title="A bug", type="bug",
               parent="ab-9a000001"),
    ]
    public = render_public_backlog_html(entries, "fno")
    local_path = tmp_path / "local.html"
    render_graph_html(entries, local_path)
    local = local_path.read_text()

    assert '"ty":"epic"' in public and '"ty":"bug"' in public, "type is public"
    assert '"pa":' not in public, "a parent id must never reach a public board"
    assert "ab-9a000001" not in _payload(public)

    fired = sorted(n for n, p in LEAK_PATTERNS if p.search(_payload(public)))
    assert fired == [], f"public leaks {fired}"
    local_fired = sorted(n for n, p in LEAK_PATTERNS if p.search(_payload(local)))
    assert "node-id" in local_fired, "the probe never fires, so the zero proves nothing"
    assert '"pa":"ab-9a000001"' in local, "the local board DOES carry ancestry"


def test_the_design_tokens_survived_the_port():
    """The port shipped 10 custom properties against the original's 25, which
    is why the first board read grey: every status foreground had a paired
    background and only the foregrounds came across.

    Counts the tokens and names the ones that carry the design, rather than
    counting rules, which says nothing about WHICH rules.
    """
    import re

    from fno.graph.render_html import _DASHBOARD_CSS as css

    tokens = set(re.findall(r"(--[a-z0-9-]+)\s*:", css))
    assert len(tokens) >= 25, f"only {len(tokens)} design tokens: {sorted(tokens)}"
    for required in (
        "--ready-bg", "--done-bg", "--idea-bg", "--sup-bg", "--prog-bg",
        "--defer-bg", "--blocked-bg", "--accent-soft", "--shadow", "--ink-2",
    ):
        assert required in tokens, f"{required} did not survive the port"

    # The five losses named on the node, each asserted by its own marker.
    assert "prefers-color-scheme" in css, "no dark mode"
    assert ':root[data-theme="dark"]' in css, "no explicit dark override"
    assert ".rmain:hover" in css, "rows do not respond to the pointer"
    assert "position:sticky" in css, "the filter bar scrolls away"
    assert "box-shadow" in css, "nothing has depth"
    assert ".row[data-s=done] .rt" in css, "closed work is not de-emphasized"


def test_every_status_and_type_has_a_class_the_stylesheet_defines():
    """Two lists that must not drift: the vocabulary the JS emits, and the
    rules the stylesheet carries. A pill with no rule renders unstyled."""
    from fno.graph.render_html import (
        _DASHBOARD_CSS,
        _DASHBOARD_STATUS_COLORS,
        _DASHBOARD_STATUS_ORDER,
        _DASHBOARD_TYPE_CLASSES,
        _DASHBOARD_TYPE_FALLBACK,
    )

    for status in _DASHBOARD_STATUS_ORDER:
        assert f".s-{status}" in _DASHBOARD_CSS, f"status {status} has no pill rule"
        assert status in _DASHBOARD_STATUS_COLORS, f"status {status} has no bar colour"
    for cls in list(_DASHBOARD_TYPE_CLASSES.values()) + [_DASHBOARD_TYPE_FALLBACK]:
        assert f".{cls}" in _DASHBOARD_CSS, f"type class {cls} has no rule"
    # Every colour the bar can emit must name a token the stylesheet defines.
    for status, colour in _DASHBOARD_STATUS_COLORS.items():
        token = colour.removeprefix("var(").removesuffix(")")
        assert f"{token}:" in _DASHBOARD_CSS, f"{status} points at undefined {token}"


def test_the_live_claim_is_keyed_on_local_not_on_projection(tmp_path: Path):
    """A local board IS re-rendered on every graph mutation; a published one is
    a snapshot at publish time. Saying either of the other is a lie about how
    fresh the page is.

    Keyed on `local`, because `render_graph_html` passes local=True and leaves
    projection at its default. Keying on projection labelled the operator's own
    live board a snapshot, which is the same defect pointed the other way, so
    this asserts BOTH directions rather than only the published one.
    """
    from fno.graph.roadmap_public import render_public_backlog_html

    entries = [_entry("ab-14000001", project="fno", title="A node")]
    local_path = tmp_path / "local.html"
    render_graph_html(entries, local_path)
    local = local_path.read_text()
    public = render_public_backlog_html(entries, "fno")

    assert "re-rendered on every graph mutation" in local
    assert "snapshot" not in local.split("</header>")[0]
    assert "snapshot" in public.split("</header>")[0]
    assert "re-rendered on every graph mutation" not in public


def test_the_light_palette_meets_wcag_aa_on_small_text():
    """Every pill, chip and badge on this board is 10.5px to 12.5px, so 4.5:1
    is the bar for all of them.

    The port shipped four pairs below it: ready 3.43, accent 3.99, muted 4.03
    and deferred 4.20. Every status chip is pressed on first paint, so the
    failing state was the board's default rather than an edge case.
    """
    import re

    from fno.graph.render_html import _DASHBOARD_CSS

    def _lum(value: str) -> float:
        h = value.lstrip("#")
        parts = []
        for i in (0, 2, 4):
            c = int(h[i : i + 2], 16) / 255
            parts.append(c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4)
        return 0.2126 * parts[0] + 0.7152 * parts[1] + 0.0722 * parts[2]

    def _ratio(fg: str, bg: str) -> float:
        a, b = _lum(fg), _lum(bg)
        return (max(a, b) + 0.05) / (min(a, b) + 0.05)

    light = _DASHBOARD_CSS.split(":root {", 1)[1].split("}", 1)[0]
    tok = dict(re.findall(r"(--[a-z0-9-]+):\s*(#[0-9a-fA-F]{6})", light))

    pairs = [
        ("--ready", "--ready-bg"),
        ("--done", "--done-bg"),
        ("--idea", "--idea-bg"),
        ("--sup", "--sup-bg"),
        ("--prog", "--prog-bg"),
        ("--defer", "--defer-bg"),
        ("--blocked", "--blocked-bg"),
        ("--bug", "--bug-bg"),
        ("--epic", "--epic-bg"),
        ("--accent", "--accent-soft"),
        ("--muted", "--surface-2"),
        ("--ink-2", "--surface-2"),
        ("--muted", "--surface"),
    ]
    failures = []
    for fg, bg in pairs:
        assert fg in tok and bg in tok, f"{fg} or {bg} vanished from the palette"
        got = _ratio(tok[fg], tok[bg])
        if got < 4.5:
            failures.append(f"{fg} on {bg} = {got:.2f}")
    assert not failures, "below 4.5:1 on small text: " + ", ".join(failures)


def test_the_public_board_removes_the_id_track_not_just_its_width():
    """A zero-width grid column still takes the gap between it and the next.

    Collapsing the id column to 0 left an 11px indent on every public row
    rather than none, because `gap` applies between all tracks including
    zero-width ones. The track has to go, not its width.
    """
    from fno.graph.render_html import _DASHBOARD_CSS as css

    assert 'body[data-local="false"] .rid { display:none }' in css, (
        "the empty id span still occupies a grid cell on public boards"
    )
    assert 'body[data-local="false"] .rmain { grid-template-columns:1fr auto auto }' in css
    assert "grid-template-columns:0 1fr" not in css, (
        "a zero-width track still costs its gap; remove the track instead"
    )
    # The narrow layout re-points meta and dot, which would otherwise address
    # a column that no longer exists on a public board.
    assert 'body[data-local="false"] .meta, body[data-local="false"] .dot { grid-column:1 }' in css


def test_a_child_completed_at_beats_its_stale_open_status_in_the_parent_rollup():
    """A legacy child carrying completed_at beside an open status must summarise
    as done in the parent's `ki`, the same rule the child's own row already uses.
    Without it kidBar() counts a finished child as open, so parent progress
    contradicts the child row rendered directly beneath it."""
    entries = [
        _entry("ab-00000010", project="fno", title="parent"),
        _entry(
            "ab-00000011",
            project="fno",
            title="legacy child",
            parent="ab-00000010",
            status="ready",
            completed_at="2026-01-01T00:00:00Z",
        ),
    ]
    rows = {r["id"]: r for r in _dashboard_rows(entries, local=True, context_entries=entries)}

    assert rows["ab-00000011"]["s"] == "done", "control: the child's own row"
    assert rows["ab-00000010"]["ki"] == [
        {"id": "ab-00000011", "s": "done", "t": "legacy child", "ty": "feature"}
    ], "the parent rollup must agree with the child's own row"


def test_completed_at_beats_a_stale_status_in_every_relation_projection():
    """The rule the row's own status uses must hold in all four projections.
    Fixing `ki` alone left `su`, `bb` and `sb` reading status raw, so a finished
    dependency still rendered open inside the red Dependencies box and the node
    read as blocked by work that was done."""
    entries = [
        _entry(
            "ab-00000020",
            project="fno",
            title="finished blocker",
            status="ready",
            completed_at="2026-01-01T00:00:00Z",
        ),
        _entry(
            "ab-00000021",
            project="fno",
            title="dependent",
            blocked_by=["ab-00000020"],
            superseded_by="ab-00000020",
        ),
    ]
    rows = {r["id"]: r for r in _dashboard_rows(entries, local=True, context_entries=entries)}

    assert rows["ab-00000020"]["s"] == "done", "control: the blocker's own row"
    assert rows["ab-00000021"]["bb"] == [
        {"id": "ab-00000020", "s": "done", "t": "finished blocker"}
    ], "Dependencies must not show a finished blocker as open"
    assert rows["ab-00000021"]["sb"] == {
        "id": "ab-00000020",
        "s": "done",
        "t": "finished blocker",
    }, "Superseded by must agree with the row"
    assert rows["ab-00000020"]["su"] == [
        {"id": "ab-00000021", "s": "ready", "t": "dependent"}
    ], "an unfinished successor still reads its own stored status"


def test_an_unresolvable_relation_id_still_reports_not_found():
    """The shared helper must keep the missing-row fallback the projections had:
    an id absent from the index is 'not found', never 'unknown'."""
    entries = [_entry("ab-00000030", project="fno", blocked_by=["ab-missing1"])]

    rows = {r["id"]: r for r in _dashboard_rows(entries, local=True, context_entries=entries)}

    assert rows["ab-00000030"]["bb"] == [{"id": "ab-missing1", "s": "not found", "t": ""}]


def test_a_legacy_deferred_sentinel_in_completed_at_is_not_read_as_done():
    """A pre-migration row encodes deferral INSIDE completed_at as a `deferred:`
    sentinel, and deferral is a returnable rung. The render path reads through
    read_graph_with_archive, which does not run the recompute migration, so the
    sentinel arrives intact. A bare truthiness test relabels that work done: the
    parent shows a full kidBar and a deferred blocker reads as finished inside
    the Dependencies box, so the node looks unblocked."""
    entries = [
        _entry("ab-00000040", project="fno", title="parent"),
        _entry(
            "ab-00000041",
            project="fno",
            title="legacy deferred",
            parent="ab-00000040",
            status="deferred",
            completed_at="deferred:2026-01-01T00:00:00Z",
        ),
        _entry(
            "ab-00000042",
            project="fno",
            title="really done",
            parent="ab-00000040",
            status="ready",
            completed_at="2026-01-01T00:00:00Z",
        ),
    ]
    rows = {r["id"]: r for r in _dashboard_rows(entries, local=True, context_entries=entries)}

    assert rows["ab-00000041"]["s"] == "deferred", "the sentinel is not a completion"
    assert rows["ab-00000042"]["s"] == "done", "control: a real timestamp still closes"
    assert sorted((k["id"], k["s"]) for k in rows["ab-00000040"]["ki"]) == [
        ("ab-00000041", "deferred"),
        ("ab-00000042", "done"),
    ], "the parent rollup must not count deferred work as finished"


def test_a_terminal_status_without_a_timestamp_keeps_its_stored_status():
    """`superseded` carrying no completed_at reads as itself, not as done."""
    entries = [_entry("ab-00000050", project="fno", status="superseded")]

    rows = {r["id"]: r for r in _dashboard_rows(entries, local=True, context_entries=entries)}

    assert rows["ab-00000050"]["s"] == "superseded"

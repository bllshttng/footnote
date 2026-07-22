"""Unit tests for fno.graph.render_html - HTML kanban rendering."""
from __future__ import annotations

from pathlib import Path

from fno.graph.render_html import (
    UNSCOPED_LABEL,
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
        "project": None,
        "created_at": "2026-01-01T00:00:00Z",
    }
    base.update(kwargs)
    return base


def test_card_flags_queued(tmp_path: Path):
    """A queued node gets the green 'queued' flag chip and card class."""
    entries = [
        _entry(
            "ab-flagged0",
            project="fno",
            status="ready",
            priority="p3",
            queued_at="2026-05-12T12:00:00Z",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "flag-queued" in text
    assert ">queued<" in text


def test_card_flags_claimed(tmp_path: Path):
    """An in-session card gets the in-session flag chip and card class."""
    entries = [
        _entry(
            "ab-flagged1",
            project="fno",
            status="in_progress",
            session_id="some-session",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "flag-claimed" in text
    assert ">in session<" in text


def test_card_flags_blocked_shows_count(tmp_path: Path):
    """Blocked card surfaces the blocker count as a flag (not a column override)."""
    entries = [
        _entry(
            "ab-flagged2",
            project="fno",
            status="blocked",
            blocked_by=["ab-aaaa0001", "ab-aaaa0002"],
            priority="p1",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "flag-blocked" in text
    assert ">blocked (2)<" in text


def test_card_flags_idea_needs_plan(tmp_path: Path):
    """Idea (no plan) cards get a 'needs plan' chip; status doesn't pin them to a column."""
    entries = [
        _entry("ab-flagged3", project="fno", status="idea", priority="p2"),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "flag-idea" in text
    assert ">needs plan<" in text


def test_render_html_columns_are_collapsible_details(tmp_path: Path):
    """Each column is a <details data-col=...> so users can tap to collapse."""
    entries = [
        _entry("ab-c0011111", project="fno", status="ready", priority="p1"),
        _entry(
            "ab-c0022222",
            project="fno",
            completed_at="2026-05-01T00:00:00Z",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    # All columns render as <details class="col col-...">, incl. Triage.
    for col_lower in ("now", "next", "later", "triage", "done"):
        assert f'class="col col-{col_lower}"' in text
        assert f'data-col="{col_lower.capitalize()}"' in text
    # Done + Triage ship CLOSED by default; others ship OPEN.
    assert '<details class="col col-done" data-col="Done">' in text
    assert '<details class="col col-triage" data-col="Triage">' in text
    assert '<details class="col col-now" data-col="Now" open>' in text
    # localStorage persistence key + capture-phase toggle listener present.
    assert "fno-kanban-col-state" in text
    assert "addEventListener('toggle'" in text


def test_render_html_basic(tmp_path: Path):
    entries = [
        _entry("ab-aaaa1111", status="ready", project="alpha"),
        _entry(
            "ab-bbbb2222",
            status="ready",
            project="beta",
            completed_at="2026-05-01T00:00:00Z",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)

    assert out.exists()
    text = out.read_text()
    assert "<html" in text
    assert "ab-aaaa1111" in text
    assert "ab-bbbb2222" in text
    # 3 section-level <details> (master + 2 projects) + 5 column-level
    # <details> inside each (Now/Next/Later/Triage/Done) = 3 + 3*5 = 18 total.
    assert text.count("<details ") == 18
    assert ">alpha<" in text and ">beta<" in text
    # Master section is a <details> with id="master" so JS can collapse on mobile.
    assert 'id="master"' in text
    # All four columns rendered in the master board.
    for col in ("Now", "Next", "Later", "Done"):
        assert f">{col} <" in text


def test_obsidian_url_builds_for_internal_plan_paths():
    from fno.graph.render_html import _obsidian_url
    url = _obsidian_url("myvault", "internal/fno/plans/2026-05-12-graph-html-kanban.md")
    assert url == (
        "obsidian://open?vault=myvault"
        "&file=internal/fno/plans/2026-05-12-graph-html-kanban"
    )


def test_obsidian_url_returns_none_for_non_internal_paths():
    from fno.graph.render_html import _obsidian_url
    assert _obsidian_url("myvault", "src/some/code.py") is None
    assert _obsidian_url("myvault", "") is None
    assert _obsidian_url("myvault", None) is None  # type: ignore[arg-type]


def test_obsidian_url_normalizes_tilde_and_absolute_prefixes():
    """Vault-relative form is recovered regardless of how plan_path was stored."""
    from fno.graph.render_html import _obsidian_url
    expected = (
        "obsidian://open?vault=myvault"
        "&file=internal/etl/plans/2026-04-23-ny-records-scraper"
    )
    # tilde prefix
    assert _obsidian_url("myvault", "~/myvault/internal/etl/plans/2026-04-23-ny-records-scraper.md") == expected
    # absolute prefix
    assert _obsidian_url("myvault", "/Users/me/myvault/internal/etl/plans/2026-04-23-ny-records-scraper.md") == expected
    # worktree prefix
    assert _obsidian_url(
        "myvault",
        "~/conductor/workspaces/example-pipeline/tyler-v1/internal/etl/plans/2026-04-23-ny-records-scraper.md",
    ) == expected
    # already-canonical (no change)
    assert _obsidian_url("myvault", "internal/etl/plans/2026-04-23-ny-records-scraper.md") == expected


def test_canonicalize_plan_path():
    from fno.graph.render_html import _canonicalize_plan_path
    # already-canonical form
    assert _canonicalize_plan_path("internal/etl/plans/X.md") == "internal/etl/plans/X.md"
    # vault-prefixed needs a vault hint
    assert _canonicalize_plan_path("~/myvault/internal/etl/plans/X.md", vault="myvault") == "internal/etl/plans/X.md"
    assert _canonicalize_plan_path("/Users/me/myvault/internal/etl/plans/X.md", vault="myvault") == "internal/etl/plans/X.md"
    # worktree-rooted falls back to last /internal/ even without vault hint
    assert _canonicalize_plan_path("~/conductor/wt/x/internal/etl/plans/X/") == "internal/etl/plans/X/"
    # deprecated dev/ is no longer recognized as canonical
    assert _canonicalize_plan_path("dev/fno/plans/X.md") is None
    assert _canonicalize_plan_path("~/myvault/dev/fno/plans/X", vault="myvault") is None
    # unrecognizable paths
    assert _canonicalize_plan_path("src/code.py") is None
    assert _canonicalize_plan_path(None) is None
    assert _canonicalize_plan_path("") is None


def test_render_html_emits_copy_button_with_data_copy_attr(tmp_path: Path):
    """Each card's eid becomes a <button data-copy="ab-xxxx"> for tap-to-copy."""
    entries = [_entry("ab-cccc9999", project="fno")]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert 'data-copy="ab-cccc9999"' in text
    assert 'aria-label="Copy ab-cccc9999 to clipboard"' in text
    # JS handler is present.
    assert "navigator.clipboard" in text
    assert ".eid[data-copy]" in text


def test_obsidian_url_non_markdown_path_returns_none():
    """A directory or non-.md path has nothing addressable; returns None."""
    from fno.graph.render_html import _obsidian_url
    assert _obsidian_url("myvault", "internal/fno/plans/2026-05-08-feature/") is None
    assert _obsidian_url("myvault", "internal/fno/plans/2026-05-08-feature") is None


def test_obsidian_url_file_plan_drops_md():
    """File plans drop the .md extension for the obsidian file param."""
    from fno.graph.render_html import _obsidian_url
    url = _obsidian_url("myvault", "internal/fno/plans/2026-05-12-quick.md")
    assert url == "obsidian://open?vault=myvault&file=internal/fno/plans/2026-05-12-quick"


def test_render_html_renders_obsidian_link_when_vault_set(tmp_path: Path, monkeypatch):
    """When config.obsidian.vault resolves, plan_path becomes a tappable obsidian:// link."""
    import fno.graph.render_html as rh
    monkeypatch.setattr(rh, "_load_obsidian_vault", lambda: "myvault")
    entries = [
        _entry(
            "ab-pppp1111",
            project="fno",
            plan_path="internal/fno/plans/2026-05-12-feature.md",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "obsidian://open?vault=myvault" in text
    assert "internal/fno/plans/2026-05-12-feature" in text
    assert "<a href=\"obsidian:" in text


def test_render_html_skips_obsidian_link_when_vault_unset(tmp_path: Path, monkeypatch):
    """When vault is None, plan_path renders as plain text (no tappable link)."""
    import fno.graph.render_html as rh
    monkeypatch.setattr(rh, "_load_obsidian_vault", lambda: None)
    entries = [
        _entry(
            "ab-pppp2222",
            project="fno",
            plan_path="internal/fno/plans/2026-05-12-feature.md",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "obsidian://" not in text
    # Plan path still rendered as text in the .meta.plan div.
    assert "internal/fno/plans/2026-05-12-feature.md" in text


def test_render_html_mobile_viewport_and_breakpoint(tmp_path: Path):
    """Mobile-friendliness: viewport meta tag is present and CSS stacks columns by default."""
    entries = [_entry("ab-ccc11111", project="alpha")]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    # Without the viewport meta tag, mobile browsers render at desktop width.
    assert '<meta name="viewport" content="width=device-width, initial-scale=1">' in text
    # Mobile-first: cols default to one-column; desktop breakpoint upgrades to
    # one track per column (len(COLUMNS), now 5 with Triage).
    assert ".cols { display: grid; grid-template-columns: 1fr;" in text
    assert "@media (min-width: 768px)" in text
    assert "grid-template-columns: repeat(5, 1fr)" in text
    # The grid count is derived from COLUMNS, not hardcoded - no stale token.
    assert "__NCOLS__" not in text


def test_render_html_done_hidden_default(tmp_path: Path):
    """Done column ships closed (no `open` attribute on its <details>)."""
    entries = [
        _entry("ab-cccc3333", project="alpha", completed_at="2026-05-01T00:00:00Z"),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    # Done <details> rendered WITHOUT the `open` attribute so it starts collapsed.
    assert '<details class="col col-done" data-col="Done">' in text
    # Other columns (Now/Next/Later) ship with `open`.
    assert '<details class="col col-now" data-col="Now" open>' in text


def test_render_html_project_color_stable(tmp_path: Path):
    entries = [_entry("ab-dddd4444", project="gamma")]
    out_a = tmp_path / "a.html"
    out_b = tmp_path / "b.html"
    render_graph_html(entries, out_a)
    render_graph_html(entries, out_b)
    color = _project_color("gamma")
    a = out_a.read_text()
    b = out_b.read_text()
    assert a.count(color) == b.count(color)
    assert a.count(color) >= 1


def test_render_html_pr_url_javascript_uri_blocked(tmp_path: Path):
    entries = [
        _entry(
            "ab-gggg7777",
            project="alpha",
            completed_at="2026-05-01T00:00:00Z",
            pr_url="javascript:alert(1)",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    # No anchor href should carry the javascript: scheme.
    assert 'href="javascript' not in text
    # The text is still rendered (escaped) so the entry isn't silently dropped.
    assert "javascript:alert(1)" in text


def test_render_html_non_ascii_content(tmp_path: Path):
    entries = [
        _entry(
            "ab-hhhh8888",
            project="réalité",
            title="émoji 🎯 entry — supports diacritics",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text(encoding="utf-8")
    assert "réalité" in text
    assert "émoji 🎯" in text


def test_render_html_unscoped_grouped_last(tmp_path: Path):
    entries = [
        _entry("ab-eeee5555", project="alpha"),
        _entry("ab-ffff6666", project=None),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    # Both project sections present; unscoped label appears.
    assert ">alpha<" in text
    assert UNSCOPED_LABEL in text
    # alpha summary should appear before the unscoped summary in source order.
    assert text.index(">alpha<") < text.index(UNSCOPED_LABEL)


# ---------------------------------------------------------------------------
# Obsidian vault is loaded from the GLOBAL settings file (~/.fno/...).
# render_html.py reads it directly via _global_settings_path(), bypassing
# load_settings()'s project-local-first lookup so a project whose own
# .fno/settings.yaml lacks an obsidian block can't shadow the vault
# on auto-render (graph.html is a global artifact and must read global config).
# ---------------------------------------------------------------------------


def _make_global_settings(tmp_path: Path, *, enabled: bool, vault: str | None = None) -> Path:
    """Write a minimal global-shape settings.yaml under tmp_path."""
    cfg_lines = ["config:\n", "  obsidian:\n", f"    enabled: {str(enabled).lower()}\n"]
    if vault:
        cfg_lines.append(f"    vault: {vault}\n")
    settings_path = tmp_path / "settings.yaml"
    settings_path.write_text("".join(cfg_lines), encoding="utf-8")
    return settings_path


def test_render_html_loads_vault_from_global_settings(tmp_path: Path, monkeypatch):
    """When the global settings file has obsidian.enabled=true and a vault,
    plan_path links render as obsidian:// URLs."""
    settings_path = _make_global_settings(tmp_path, enabled=True, vault="myvault")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(settings_path))

    entries = [
        _entry(
            "ab-settings1",
            project="fno",
            plan_path="internal/fno/plans/2026-05-14-test.md",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "obsidian://open?vault=myvault" in text


def test_render_html_no_link_when_global_obsidian_disabled(tmp_path: Path, monkeypatch):
    """When the global settings file has obsidian.enabled=false, plan_path
    renders as plain text with no obsidian deep link."""
    settings_path = _make_global_settings(tmp_path, enabled=False)
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(settings_path))

    entries = [
        _entry(
            "ab-settings2",
            project="fno",
            plan_path="internal/fno/plans/2026-05-14-test.md",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "obsidian://" not in text
    assert "internal/fno/plans/2026-05-14-test.md" in text


# ---------------------------------------------------------------------------
# Within-column per-project sub-lanes + soft WIP cap header (ab-95a4a479).
# Both read from the GLOBAL settings file directly (defensive, never raise in
# the auto-render path which fires inside locked_mutate_graph).
# ---------------------------------------------------------------------------


def _make_kanban_settings(tmp_path: Path, wip_caps_yaml: str) -> Path:
    """Write a global-shape settings.yaml with a config.kanban.wip_caps block."""
    settings_path = tmp_path / "settings.yaml"
    settings_path.write_text(
        "config:\n  kanban:\n    wip_caps:\n" + wip_caps_yaml, encoding="utf-8"
    )
    return settings_path


def test_ac2_hp_master_sublanes_group_by_project(tmp_path: Path):
    """AC2-HP: the master board emits a per-project sub-lane divider for each
    project in a multi-project column."""
    entries = [
        _entry("ab-sl000001", project="web", priority="p1"),
        _entry("ab-sl000002", project="etl", priority="p1"),
        _entry("ab-sl000003", project="fno", priority="p1"),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    # Master board renders before the per-project <details class="project"> sections.
    master = text.split('<details class="project"', 1)[0]
    assert 'class="lane"' in master
    for proj in ("web", "etl", "fno"):
        assert f">{proj}</span>" in master  # lane-chip label present in master


def test_ac2_edge_single_project_column_no_sublane(tmp_path: Path):
    """AC2-EDGE: a column whose cards are all one project emits no lane divider."""
    entries = [
        _entry("ab-sl000010", project="web", priority="p1"),
        _entry("ab-sl000011", project="web", priority="p1"),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert 'class="lane"' not in text  # no redundant divider for a single project


def test_ac2_err_malformed_node_does_not_break_grouping(tmp_path: Path):
    """AC2-ERR: a node missing most fields is tolerated; the render completes
    and the well-formed cards still render."""
    entries = [
        _entry("ab-sl000020", project="web", priority="p1"),
        {"id": "ab-sl000021"},  # malformed: only an id
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)  # must not raise
    text = out.read_text()
    assert "ab-sl000020" in text
    assert "ab-sl000021" in text  # the bad node is tolerated, not dropped


def test_ac3_hp_wip_count_and_cap_shown(tmp_path: Path, monkeypatch):
    """AC3-HP: with a configured cap, the master column header shows count / cap."""
    settings = _make_kanban_settings(tmp_path, "      now: 20\n")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(settings))
    entries = [_entry("ab-wc000001", project="web", priority="p1")]  # 1 card in Now
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "1 / 20" in text


def test_ac3_err_malformed_cap_degrades_safely(tmp_path: Path, monkeypatch):
    """AC3-ERR: a string/negative cap renders the plain count and never raises."""
    for bad in ('      now: "lots"\n', "      now: -3\n", "      now: 0\n"):
        settings = _make_kanban_settings(tmp_path, bad)
        monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(settings))
        entries = [_entry("ab-wc000010", project="web", priority="p1")]
        out = tmp_path / "graph.html"
        render_graph_html(entries, out)  # must not raise
        text = out.read_text()
        # Plain, uncapped count for Now (no "/ cap").
        assert '>Now <span class="count">1</span>' in text


def test_ac3_ui_overflow_styled(tmp_path: Path, monkeypatch):
    """AC3-UI: when the count exceeds the cap, the count carries an overflow style."""
    settings = _make_kanban_settings(tmp_path, "      now: 1\n")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(settings))
    entries = [
        _entry("ab-wc000020", project="web", priority="p1"),
        _entry("ab-wc000021", project="web", priority="p1"),
    ]  # 2 cards, cap 1 -> overflow
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert 'class="count over"' in text
    assert "2 / 1" in text


def test_ac3_edge_uncapped_column_omits_cap(tmp_path: Path, monkeypatch):
    """AC3-EDGE: a column with no cap entry shows the plain count (no / n)."""
    settings = _make_kanban_settings(tmp_path, "      now: 20\n")  # only now capped
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(settings))
    entries = [_entry("ab-wc000030", project="web", priority="p3")]  # Later, uncapped
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert '>Later <span class="count">1</span>' in text


def test_render_html_project_local_settings_cannot_shadow_global_vault(
    tmp_path: Path, monkeypatch
):
    """Regression: render_graph_html must read vault from the GLOBAL settings
    file even when the cwd contains a project-local .fno/settings.yaml
    that lacks an obsidian block.

    Reproduces the bug where any backlog mutation fired from a project whose
    own settings.yaml had no obsidian block would auto-rerender the global
    graph.html with vault=None, wiping every Obsidian deep link.
    """
    # Global settings: obsidian enabled with vault.
    global_dir = tmp_path / "global"
    global_dir.mkdir()
    global_settings = _make_global_settings(global_dir, enabled=True, vault="myvault")
    monkeypatch.setenv("FNO_GLOBAL_SETTINGS_PATH", str(global_settings))

    # Project-local settings: no obsidian block at all. If render_html went
    # through load_settings() this would shadow the global config and zero
    # out the vault on the model-default obsidian.enabled=false.
    project_root = tmp_path / "project"
    fno_dir = project_root / ".fno"
    fno_dir.mkdir(parents=True)
    (fno_dir / "settings.yaml").write_text(
        "config:\n  budget_cap: 100\n", encoding="utf-8"
    )
    monkeypatch.chdir(project_root)
    monkeypatch.setenv("FNO_REPO_ROOT", str(project_root))

    entries = [
        _entry(
            "ab-shadow1",
            project="fno",
            plan_path="internal/fno/plans/2026-05-14-shadow.md",
        ),
    ]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    text = out.read_text()
    assert "obsidian://open?vault=myvault" in text, (
        "Project-local config without obsidian block must NOT shadow global vault"
    )


def test_html_overlay_live_claim_bucketed_now(monkeypatch):
    """x-4845: the HTML board's _bucket consults live_claimed_node_ids so a
    lockfile-held node lands in Now even at p3, without a graph session_id."""
    from fno.graph.render_html import _bucket

    monkeypatch.setattr("fno.graph.render_html.live_claimed_node_ids", lambda: {"x-live"})
    entries = [_entry("x-live", priority="p3")]
    cols = _bucket(entries)
    assert any(e["id"] == "x-live" for e in cols["Now"])
    assert all(e["id"] != "x-live" for e in cols["Later"])


def test_html_overlay_degrades_on_empty_claims(tmp_path: Path, monkeypatch):
    """Claims unreadable -> empty overlay, HTML render still succeeds."""
    monkeypatch.setattr("fno.graph.render_html.live_claimed_node_ids", lambda: set())
    entries = [_entry("x-none", priority="p2")]
    out = tmp_path / "graph.html"
    render_graph_html(entries, out)
    assert out.exists()

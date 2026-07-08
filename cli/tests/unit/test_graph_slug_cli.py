"""Integration tests for slug resolution + display in the backlog CLI (ab-f82e8083).

Covers: `get` by slug / bare-hex, `find` high-recall + handle-leading output +
slug in JSON, `ready` slug-leading rows, and the idempotent `backfill-slugs` verb.
"""
from __future__ import annotations

import json
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.cli import app

runner = CliRunner()


@pytest.fixture
def tmp_graph(tmp_path, monkeypatch) -> Path:
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _seed(g: Path, entries: list[dict]) -> None:
    g.write_text(json.dumps({"entries": entries}, indent=2) + "\n")


def _read(g: Path) -> list[dict]:
    return json.loads(g.read_text()).get("entries", [])


# -- get by slug / bare-hex --------------------------------------------------


def test_get_by_slug_resolves_to_node(tmp_graph):
    # AC1-HP: `get <slug>` resolves to the node's ab-id.
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["backlog", "get", "dashless-spawn"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert data["id"] == "ab-994222ee"
    assert data["slug"] == "dashless-spawn"


def test_get_by_bare_hex_reprefixes(tmp_graph):
    # AC4-HP: `get 1234abcd` (no ab-, no hyphen) re-prefixes and resolves.
    _seed(tmp_graph, [
        {"id": "ab-1234abcd", "title": "Billing", "slug": "billing",
         "_status": "ready", "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "get", "1234abcd"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["id"] == "ab-1234abcd"


def test_get_unknown_target_exits_1(tmp_graph):
    # AC1-FR-ish: a target that matches no id/slug/bare-hex fails loud.
    _seed(tmp_graph, [
        {"id": "ab-aaaaaaaa", "title": "Thing", "slug": "thing",
         "_status": "ready", "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "get", "nonsense-slug"])
    assert result.exit_code == 1
    combined = result.stdout + (result.stderr or "")
    assert "nonsense-slug" in combined


def test_get_field_works_with_slug_input(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["backlog", "get", "dashless-spawn", "--field", "id"])
    assert result.exit_code == 0, result.output
    assert result.stdout.strip() == "ab-994222ee"


# -- get --strict: the router's contract binding (x-4af4) --------------------


def test_get_strict_resolves_exact_forms(tmp_graph):
    # T1: --strict resolves the exact forms (id, slug, bare-hex) the router seeds
    # a design from - identical to the default, but pinned as the stable surface.
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno"},
    ])
    for token in ("ab-994222ee", "dashless-spawn", "994222ee"):
        result = runner.invoke(app, ["backlog", "get", token, "--strict"])
        assert result.exit_code == 0, (token, result.output)
        assert json.loads(result.stdout)["id"] == "ab-994222ee"


def test_get_strict_does_not_fuzzy_fall_through(tmp_graph):
    # AC1-EDGE + kill_criteria: a token that only describe-it fuzzy matching would
    # resolve (a mistyped mode keyword near a real slug) must NOT resolve under
    # --strict. `find` (the fuzzy surface) DOES match it - proving strict != fuzzy.
    _seed(tmp_graph, [
        {"id": "ab-aaaaaaaa", "title": "panel mode debate", "slug": "panel-mode",
         "_status": "ready", "domain": "code", "project": "p"},
    ])
    strict = runner.invoke(app, ["backlog", "get", "panle", "--strict"])
    assert strict.exit_code == 1, strict.output
    fuzzy = runner.invoke(app, ["backlog", "find", "panel"])
    assert fuzzy.exit_code == 0 and "ab-aaaaaaaa" in fuzzy.stdout


# -- find: high recall + handle display --------------------------------------


def test_find_matches_details_high_recall(tmp_graph):
    # AC2-HP recall: the search term lives only in details, not the title.
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "mobile node-id entry", "slug": "mobile-entry",
         "details": "iOS autocorrect mangles ab- prefixes on a phone",
         "_status": "ready", "domain": "code", "project": "p"},
        {"id": "ab-bbbbbbbb", "title": "unrelated", "slug": "unrelated",
         "_status": "ready", "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "find", "ios autocorrect"])
    assert result.exit_code == 0, result.output
    assert "ab-994222ee" in result.stdout
    assert "ab-bbbbbbbb" not in result.stdout


def test_find_human_output_leads_with_handle(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["backlog", "find", "dashless"])
    assert result.exit_code == 0, result.output
    # The row leads with `slug (ab-id)`.
    assert "dashless-spawn (ab-994222ee)" in result.stdout


def test_find_resolves_ab_prefixed_slug(tmp_graph):
    # codex P2: a slug that itself starts with `ab-` must resolve via find, the
    # same node `get` resolves - it must not be mis-routed to the id path.
    _seed(tmp_graph, [
        {"id": "ab-77777777", "title": "AB test cleanup", "slug": "ab-test-cleanup",
         "_status": "ready", "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "find", "ab-test-cleanup"])
    assert result.exit_code == 0, result.output
    assert "ab-77777777" in result.stdout
    assert "ab-test-cleanup (ab-77777777)" in result.stdout


def test_find_json_includes_slug(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno"},
    ])
    result = runner.invoke(app, ["backlog", "find", "dashless", "--json"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert data[0]["slug"] == "dashless-spawn"


# -- ready: slug leads -------------------------------------------------------


def test_ready_rows_lead_with_slug(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-994222ee", "title": "Dashless spawn", "slug": "dashless-spawn",
         "_status": "ready", "domain": "code", "project": "fno", "plan_path": "p.md"},
    ])
    result = runner.invoke(app, ["backlog", "ready", "--all"])
    assert result.exit_code == 0, result.output
    data = json.loads(result.stdout)
    assert data[0]["slug"] == "dashless-spawn"


# -- backfill-slugs ----------------------------------------------------------


def test_backfill_slugs_assigns_and_is_idempotent(tmp_graph):
    # AC5-EDGE: legacy nodes (no slug) get one; re-running changes nothing.
    _seed(tmp_graph, [
        {"id": "ab-aaaaaaaa", "title": "First thing", "_status": "ready",
         "domain": "code", "project": "p"},
        {"id": "ab-bbbbbbbb", "title": "Second thing", "_status": "ready",
         "domain": "code", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "backfill-slugs"])
    assert result.exit_code == 0, result.output
    assert json.loads(result.stdout)["slugs_assigned"] == 2
    by_id = {e["id"]: e for e in _read(tmp_graph)}
    assert by_id["ab-aaaaaaaa"]["slug"] == "first-thing"
    assert by_id["ab-bbbbbbbb"]["slug"] == "second-thing"

    # Re-run is a no-op.
    result2 = runner.invoke(app, ["backlog", "backfill-slugs"])
    assert result2.exit_code == 0, result2.output
    assert json.loads(result2.stdout)["slugs_assigned"] == 0


# -- update --details --------------------------------------------------------


def test_update_details_sets_and_clears(tmp_graph):
    # `update --details` edits rationale in place (no recreate-via-idea dupe).
    _seed(tmp_graph, [
        {"id": "ab-deadbeef", "title": "Thing", "slug": "thing", "_status": "ready",
         "domain": "code", "project": "p", "details": None},
    ])
    result = runner.invoke(app, ["backlog", "update", "ab-deadbeef", "--details", "the full rationale"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0]["details"] == "the full rationale"

    # `null` clears it; --description is an accepted alias.
    result = runner.invoke(app, ["backlog", "update", "ab-deadbeef", "--description", "null"])
    assert result.exit_code == 0, result.output
    assert _read(tmp_graph)[0]["details"] is None


def test_update_domain_size_type(tmp_graph):
    # Create-only fields are now editable, so a mistake never forces a recreate.
    _seed(tmp_graph, [
        {"id": "ab-feedface", "title": "Thing", "slug": "thing", "_status": "ready",
         "domain": "code", "type": "feature", "project": "p"},
    ])
    result = runner.invoke(app, ["backlog", "update", "ab-feedface",
                                 "--domain", "design", "--size", "l", "--type", "epic"])
    assert result.exit_code == 0, result.output
    node = _read(tmp_graph)[0]
    assert node["domain"] == "design"
    assert node["size"] == "L"  # normalized to uppercase
    assert node["type"] == "epic"


def test_update_rejects_bad_size_and_type(tmp_graph):
    # Validation guards against storing garbage (gemini HIGH on PR #48).
    _seed(tmp_graph, [
        {"id": "ab-feedface", "title": "Thing", "slug": "thing", "_status": "ready",
         "domain": "code", "type": "feature", "project": "p"},
    ])
    bad_size = runner.invoke(app, ["backlog", "update", "ab-feedface", "--size", "foo"])
    assert bad_size.exit_code == 1
    bad_type = runner.invoke(app, ["backlog", "update", "ab-feedface", "--type", "widget"])
    assert bad_type.exit_code == 1
    # 'null' still clears size, and roadmap is a valid type.
    assert runner.invoke(app, ["backlog", "update", "ab-feedface", "--size", "null"]).exit_code == 0
    assert runner.invoke(app, ["backlog", "update", "ab-feedface", "--type", "roadmap"]).exit_code == 0


# -- public roadmap ----------------------------------------------------------


def test_roadmap_only_public_no_leaks(tmp_graph):
    _seed(tmp_graph, [
        {"id": "ab-11111111", "title": "Public feature", "slug": "pub", "_status": "ready",
         "priority": "p1", "size": "M", "project": "fno", "public": True,
         "plan_path": "internal/fno/plans/secret.md", "cwd": "/private/x"},
        {"id": "ab-22222222", "title": "Private thing", "slug": "priv", "_status": "ready",
         "priority": "p2", "project": "fno"},  # not public -> excluded
        {"id": "ab-33333333", "title": "Other project pub", "slug": "op", "_status": "ready",
         "priority": "p1", "project": "other", "public": True},  # wrong project -> excluded
    ])
    result = runner.invoke(app, ["backlog", "roadmap", "--project", "fno"])
    assert result.exit_code == 0, result.output
    out = result.stdout
    assert "Public feature" in out
    assert "Private thing" not in out
    assert "Other project pub" not in out
    # No internal fields leak.
    assert "ab-11111111" not in out
    assert "secret.md" not in out
    assert "/private/x" not in out
    # Grouped under the Now column (p1).
    assert "## Now" in out


def test_roadmap_html_escapes_and_filters(tmp_graph, tmp_path):
    _seed(tmp_graph, [
        {"id": "ab-44444444", "title": "Shipped <b>X</b>", "slug": "sx",
         "_status": "ready", "priority": "p1", "project": "fno", "public": True,
         "completed_at": "2026-01-01T00:00:00Z"},
    ])
    hp = tmp_path / "roadmap.html"
    result = runner.invoke(app, ["backlog", "roadmap", "--project", "fno", "--html", str(hp)])
    assert result.exit_code == 0, result.output
    body = hp.read_text()
    assert "Shipped &lt;b&gt;X&lt;/b&gt;" in body  # escaped, not raw HTML
    assert "Shipped" in body  # Done column relabeled


def test_roadmap_html_omits_internal_status_flags(tmp_graph, tmp_path):
    # Public HTML must not leak live-board workflow flags (codex P2 on PR #48):
    # a blocked / plan-less node would otherwise render `blocked`/`needs plan`.
    _seed(tmp_graph, [
        {"id": "ab-aaaa0001", "title": "Blocker", "slug": "blk", "_status": "ready",
         "priority": "p1", "project": "fno", "completed_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-aaaa0002", "title": "Public blocked plan-less", "slug": "pbp",
         "priority": "p1", "project": "fno", "public": True,
         "blocked_by": ["ab-aaaa0003"]},  # open blocker -> would flag "blocked"
        {"id": "ab-aaaa0003", "title": "Open dep", "slug": "dep", "_status": "ready",
         "priority": "p2", "project": "fno"},
    ])
    hp = tmp_path / "roadmap.html"
    result = runner.invoke(app, ["backlog", "roadmap", "--project", "fno", "--html", str(hp)])
    assert result.exit_code == 0, result.output
    body = hp.read_text().lower()
    assert "public blocked plan-less" in body  # the node still shows
    # Check rendered markup, not the shared CSS (which still *defines* the
    # .flag-* selectors). A leak is a flag badge element or a flag-tagged card.
    assert '<span class="flag' not in body, "flag badge element leaked"
    assert 'class="card flag' not in body, "card tagged with internal flag class"


def test_roadmap_folds_triage_into_later(tmp_graph):
    # A queued node routes to Triage internally; the public roadmap shows it
    # under Later (Triage is not a public column).
    _seed(tmp_graph, [
        {"id": "ab-77777777", "title": "Queued p1 item", "slug": "q", "_status": "ready",
         "priority": "p1", "project": "fno", "public": True, "queued_at": "2026-01-01T00:00:00Z"},
        {"id": "ab-88888888", "title": "Plain p3 item", "slug": "p3", "_status": "ready",
         "priority": "p3", "project": "fno", "public": True},
    ])
    out = runner.invoke(app, ["backlog", "roadmap", "--project", "fno"]).stdout
    assert "## Later" in out
    assert "Queued p1 item" in out   # folded in despite being Triage internally
    assert "Plain p3 item" in out
    assert "## Triage" not in out    # no public Triage column

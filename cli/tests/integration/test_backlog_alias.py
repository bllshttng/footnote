"""Phase 01 + 02a + 03: backlog sub-app + verb aliases + triage nesting.

Verifies dual registration: `backlog` is canonical, `graph` is a
hidden deprecated alias. Both must resolve to the same Typer app so
every verb and help output is byte-identical. Also verifies that the
canonical `intake` verb is registered (and the deprecated `adopt`
alias is gone), the `done` verb works, and the nested `triage`
sub-app surface is present.
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
    """A fresh empty graph.json routed to tmp_path via monkeypatch."""
    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "GRAPH_ARCHIVE_JSON", tmp_path / "graph-archive.json")
    monkeypatch.setattr(gc, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_LOCK_FILE", tmp_path / "graph.lock")
    return g


def _invoke(*args, input=None):
    return runner.invoke(app, list(args), input=input, catch_exceptions=False)


# ---------------------------------------------------------------------------
# Phase 01 - backlog sub-app + graph alias
# ---------------------------------------------------------------------------


def test_ac1_hp_backlog_help_lists_verbs():
    """`fno backlog --help` lists the advertised verbs (x-71b6 tiering).

    `ready`/`intake` are hidden now; the advertised menu leads with add/next/get.
    """
    r = _invoke("backlog", "--help")
    assert r.exit_code == 0, r.output
    for verb in ("add", "next", "get", "find", "done"):
        assert verb in r.output, f"verb {verb!r} missing from backlog help"


def test_ac1_hp_graph_alias_help_identical_to_backlog():
    """`fno graph --help` produces the same verb surface as `fno backlog --help`."""
    r_backlog = _invoke("backlog", "--help")
    r_graph = _invoke("graph", "--help")
    assert r_backlog.exit_code == 0 and r_graph.exit_code == 0
    # Every advertised verb listed in backlog must also be listed in graph.
    for verb in ("add", "next", "get", "find", "done"):
        assert verb in r_graph.output, f"verb {verb!r} missing from graph alias help"


def test_ac1_hp_top_level_help_hides_graph_shows_backlog():
    """`fno --help` lists `backlog` but not `graph` (graph is hidden)."""
    r = _invoke("--help")
    assert r.exit_code == 0
    assert "backlog" in r.output, "backlog should appear in top-level help"
    # Line-level check: graph should not appear as a command entry.
    # Typer help lists commands with a leading space/bullet; a bare substring
    # check would false-positive on docstrings. Look for the command row.
    command_lines = [ln for ln in r.output.splitlines() if ln.strip().startswith("graph ")]
    assert not command_lines, (
        f"graph should be hidden from top-level help, found lines: {command_lines}"
    )


def test_ac2_hp_backlog_and_graph_share_behavior(tmp_graph):
    """`fno backlog add X` and `fno graph add X` produce the same node schema."""
    r_b = _invoke("--json", "backlog", "add", "FeatureB")
    r_g = _invoke("--json", "graph", "add", "FeatureG")
    assert r_b.exit_code == 0, r_b.output
    assert r_g.exit_code == 0, r_g.output
    node_b = json.loads(r_b.stdout)
    node_g = json.loads(r_g.stdout)
    # Both should have the same keys (same schema)
    assert set(node_b.keys()) == set(node_g.keys())
    assert node_b["title"] == "FeatureB"
    assert node_g["title"] == "FeatureG"


# ---------------------------------------------------------------------------
# Phase 02a - intake (canonical) + done verb
# ---------------------------------------------------------------------------


def test_ac1_hp_intake_adopts_plan(tmp_graph, tmp_path):
    """`fno backlog intake <plan>` intakes the plan as a new node.

    Asserts the user-visible verb AND the on-disk writer flip so a
    regression that flips ``_build_intake_node`` back to ``"adopt"`` is
    caught by the integration suite. This pairs with the unit test on
    ``INTAKE_SOURCE_VALUES`` membership; the writer must emit the new
    canonical value, not the legacy one.
    """
    plan = tmp_path / "fake-plan.md"
    plan.write_text("---\ntitle: My Plan\n---\n# Body\n")
    r = _invoke("backlog", "intake", str(plan))
    assert r.exit_code == 0, r.output
    assert "intake ab-" in r.output or "ab-" in r.output

    graph = json.loads(tmp_graph.read_text())
    entries = graph["entries"]
    assert len(entries) == 1, f"expected 1 entry, got {entries!r}"
    assert entries[0]["source"] == "intake", (
        f"writer must emit source: 'intake', got {entries[0].get('source')!r}"
    )


def test_adopt_alias_is_gone(tmp_graph, tmp_path):
    """`fno backlog adopt <plan>` exits non-zero (alias removed); `intake` is in --help."""
    plan = tmp_path / "fake-plan2.md"
    plan.write_text("---\ntitle: My Plan 2\n---\n# Body\n")

    r = _invoke("backlog", "adopt", str(plan))
    assert r.exit_code != 0, (
        f"adopt alias should be gone, but exited 0 with output: {r.output!r}"
    )
    # The deprecation forwarder is gone, so the 'deprecated' string from the
    # old alias must not appear anywhere in stdout/stderr.
    assert "deprecated" not in r.output.lower(), (
        f"adopt should fail with 'no such command', not the old deprecation forwarder: {r.output!r}"
    )
    # Lock in Typer's native rejection path: a future regression that swaps
    # the alias for a different non-zero-exit shim would slip past the two
    # assertions above. "no such command" only comes from Typer when the
    # subcommand is genuinely unregistered.
    assert "no such command" in r.output.lower(), (
        f"expected Typer 'No such command' rejection, got: {r.output!r}"
    )

    # The canonical verb must still be registered and invocable. `intake` is
    # display-hidden under the x-71b6 In-N-Out tiering, so it no longer appears
    # in `backlog --help`; its own --help still works (hiding != removal).
    hi = _invoke("backlog", "intake", "--help")
    assert hi.exit_code == 0, f"intake must stay invocable: {hi.output!r}"
    # adopt must not appear as a visible command row either
    h = _invoke("backlog", "--help")
    assert h.exit_code == 0
    command_lines = [
        ln for ln in h.output.splitlines()
        if ln.strip().startswith("adopt ") or ln.strip() == "adopt"
    ]
    assert not command_lines, (
        f"adopt should not appear in backlog help, found lines: {command_lines}"
    )


def test_ac1_hp_done_marks_node_completed(tmp_graph):
    """`fno backlog done <id>` sets completed_at and _status derives to done."""
    add = _invoke("--json", "backlog", "add", "DoneTest")
    assert add.exit_code == 0
    node_id = json.loads(add.stdout)["id"]

    r = _invoke("backlog", "done", node_id)
    assert r.exit_code == 0, r.output

    # Fetch and assert completed_at is set
    get = _invoke("--json", "backlog", "get", node_id)
    assert get.exit_code == 0
    node = json.loads(get.stdout)
    assert node.get("completed_at"), "completed_at must be set"
    # _status is derived by recompute_statuses; it may not be in the JSON
    # serialization but the completed_at presence is the canonical signal.


def test_ac3_edge_done_is_idempotent(tmp_graph):
    """Running `done` on an already-done node is a safe no-op (exit 0)."""
    add = _invoke("--json", "backlog", "add", "IdemTest")
    node_id = json.loads(add.stdout)["id"]
    _invoke("backlog", "done", node_id)
    r2 = _invoke("backlog", "done", node_id)
    assert r2.exit_code == 0, r2.output
    assert "already" in r2.output.lower() or "done" in r2.output.lower()


def test_ac4_err_done_rejects_invalid_id(tmp_graph):
    """`fno backlog done not-a-real-id` exits non-zero with a clear error."""
    r = _invoke("backlog", "done", "not-a-real-id")
    assert r.exit_code != 0, "done should reject invalid IDs"


# ---------------------------------------------------------------------------
# Phase 03 - triage sub-app surface
# ---------------------------------------------------------------------------


def test_ac1_hp_triage_sub_app_registered():
    """`fno backlog triage --help` lists the five triage verbs."""
    r = _invoke("backlog", "triage", "--help")
    assert r.exit_code == 0, r.output
    for verb in ("context", "propose", "validate", "apply", "projects"):
        assert verb in r.output, f"triage verb {verb!r} missing from help"


def test_ac2_edge_triage_in_backlog_help():
    """`fno backlog --help` lists `triage` as a nested sub-app."""
    r = _invoke("backlog", "--help")
    assert r.exit_code == 0
    assert "triage" in r.output


def test_ac1_hp_triage_context_emits_candidates(tmp_graph):
    """`fno backlog triage context` emits JSON with a `candidates` key."""
    r = _invoke("backlog", "triage", "context", "--all")
    assert r.exit_code == 0, r.output
    data = json.loads(r.stdout)
    assert "candidates" in data, f"expected 'candidates' key, got {list(data.keys())}"


def test_ac1_hp_triage_projects_empty_graph(tmp_graph):
    """`fno backlog triage projects` on an empty graph returns an empty projects list.

    Shape must be ``{"projects": [{"name", "pending_count"}, ...]}`` so the
    /triage skill's ``each`` iterator can read counts for its banner — a
    flat list would strip that context.
    """
    r = _invoke("backlog", "triage", "projects")
    assert r.exit_code == 0, r.output
    data = json.loads(r.stdout)
    assert data == {"projects": []}


# ---------------------------------------------------------------------------
# x-71b6 In-N-Out tiering: `fno backlog --help` advertises 11 verbs; the rest
# are hidden but invocable, with sibling pointers on the advertised verbs.
# ---------------------------------------------------------------------------

_ADVERTISED_BACKLOG_VERBS = {
    "add", "idea", "get", "update", "view", "find", "next", "done", "defer",
    "rank", "triage",
}


def test_backlog_help_advertises_only_the_menu():
    """AC1-HP: `fno backlog --help` lists exactly the advertised verbs (<=12)."""
    import click
    import typer.main

    from fno.graph.cli import cli as backlog_app

    group = typer.main.get_command(backlog_app)
    ctx = click.Context(group)
    listed = [
        name
        for name in group.list_commands(ctx)
        if not (cmd := group.get_command(ctx, name)) or not cmd.hidden
    ]
    assert set(listed) == _ADVERTISED_BACKLOG_VERBS, (
        f"advertised backlog verbs drifted: {sorted(listed)}"
    )
    assert len(listed) <= 12


@pytest.mark.parametrize(
    "verb",
    ["decompose", "intake", "undefer", "reconcile", "maintain", "roadmap", "supersede"],
)
def test_hidden_backlog_verbs_stay_invocable(verb):
    """AC2-ERR: a hidden backlog verb still runs its own --help (exit 0)."""
    r = _invoke("backlog", verb, "--help")
    assert r.exit_code == 0, f"hidden verb {verb!r} --help failed: {r.output!r}"


def test_advertised_verbs_point_at_hidden_siblings():
    """AC3-UI: paired-verb pointers keep the hidden sibling discoverable."""
    d = _invoke("backlog", "defer", "--help")
    assert d.exit_code == 0 and "undefer" in d.output, d.output
    dn = _invoke("backlog", "done", "--help")
    assert dn.exit_code == 0 and "reconcile" in dn.output, dn.output


def test_ac2_hp_triage_propose_dry_run_emits_template(tmp_graph):
    """`fno backlog triage propose --dry-run` emits a stub proposal."""
    r = _invoke("backlog", "triage", "propose", "--dry-run", "--all")
    assert r.exit_code == 0, r.output
    data = json.loads(r.stdout)
    for key in ("dependencies", "priority_changes", "duplicates"):
        assert key in data, f"proposal template missing {key!r}"


def test_ac1_hp_triage_validate_passes_clean_proposal(tmp_graph, tmp_path):
    """A proposal with no edges / cycles passes through validate unchanged."""
    prop = tmp_path / "prop.json"
    prop.write_text(json.dumps({
        "dependencies": [],
        "priority_changes": [],
        "duplicates": [],
    }))
    r = _invoke("backlog", "triage", "validate", str(prop))
    assert r.exit_code == 0, r.output
    data = json.loads(r.stdout)
    assert data.get("dependencies") == []


# ---------------------------------------------------------------------------
# Phase 03 - intake --project flag + settings-workspace warning
# ---------------------------------------------------------------------------


def test_intake_project_flag_overrides_frontmatter(tmp_graph, tmp_path):
    """--project beats frontmatter beats cwd inference."""
    plan = tmp_path / "plan-A.md"
    plan.write_text(
        "---\nproject: from-frontmatter\n---\n# title\nbody\n"
    )
    r = _invoke("--json", "backlog", "intake", str(plan), "--project", "from-flag")
    assert r.exit_code == 0, r.output
    g = json.loads(tmp_graph.read_text())
    nodes = g.get("entries") or []
    assert any(n.get("project") == "from-flag" for n in nodes), (
        f"expected node with project=from-flag, got: {[n.get('project') for n in nodes]}"
    )


def test_intake_warns_when_project_not_in_settings(tmp_graph, tmp_path, monkeypatch):
    """Resolved project not in any settings workspace -> stderr warning, exit 0."""
    plan = tmp_path / "plan-B.md"
    plan.write_text(
        "---\nproject: brand-new-project\n---\n# title\n"
    )

    # Project-local settings declares only 'existing-project'
    settings = tmp_path / ".fno" / "settings.yaml"
    settings.parent.mkdir(parents=True, exist_ok=True)
    settings.write_text(
        "work:\n  workspaces:\n    main:\n      projects:\n"
        "        - name: existing-project\n          path: ~/code/existing-project\n"
    )
    # Isolate HOME so the user's real ~/.fno/settings.yaml is not consulted
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setenv("HOME", str(fake_home))
    monkeypatch.chdir(tmp_path)

    r = _invoke("backlog", "intake", str(plan))
    assert r.exit_code == 0, r.output
    # CliRunner captures combined output by default. Look for the warning text.
    out = r.output
    assert "brand-new-project" in out, out
    assert "not declared in any settings.yaml workspace" in out, out


def test_intake_empty_project_flag_errors(tmp_graph, tmp_path):
    """--project '' is rejected with exit 1."""
    plan = tmp_path / "plan-C.md"
    plan.write_text("---\nproject: foo\n---\n")

    r = _invoke("backlog", "intake", str(plan), "--project", "")
    assert r.exit_code == 1, r.output
    assert "must be a non-empty string" in r.output, r.output


# ---------------------------------------------------------------------------
# Phase 04 - cmd_update --project / --cwd, validator warn, list_misscoped
# ---------------------------------------------------------------------------


def _seed_node(tmp_graph: Path, node: dict) -> None:
    g = json.loads(tmp_graph.read_text())
    g["entries"].append(node)
    tmp_graph.write_text(json.dumps(g))


def test_cmd_update_project_repoints_node(tmp_graph):
    """fno backlog update <id> --project <name> updates the project field."""
    _seed_node(tmp_graph, {
        "id": "ab-12345678", "project": "myvault", "cwd": "/old/cwd",
        "title": "x", "type": "feature",
    })
    r = _invoke("backlog", "update", "ab-12345678", "--project", "example-pipeline")
    assert r.exit_code == 0, r.output

    g = json.loads(tmp_graph.read_text())
    node = next(e for e in g["entries"] if e["id"] == "ab-12345678")
    assert node["project"] == "example-pipeline"
    assert node["cwd"] == "/old/cwd"


def test_cmd_update_project_and_cwd_atomic(tmp_graph):
    """--project and --cwd together update both in one locked-mutate write."""
    _seed_node(tmp_graph, {
        "id": "ab-aaaaaaaa", "project": "myvault", "cwd": "/home/user/myvault",
        "title": "y", "type": "feature",
    })
    r = _invoke(
        "backlog", "update", "ab-aaaaaaaa",
        "--project", "example-pipeline",
        "--cwd", "/tmp/example-pipeline",
    )
    assert r.exit_code == 0, r.output

    g = json.loads(tmp_graph.read_text())
    node = next(e for e in g["entries"] if e["id"] == "ab-aaaaaaaa")
    assert node["project"] == "example-pipeline"
    assert node["cwd"] == "/tmp/example-pipeline"


def test_cmd_update_empty_project_errors(tmp_graph):
    """--project '' is rejected with exit 1."""
    _seed_node(tmp_graph, {
        "id": "ab-bbbbbbbb", "project": "myvault", "cwd": "/old",
        "title": "z", "type": "feature",
    })
    r = _invoke("backlog", "update", "ab-bbbbbbbb", "--project", "")
    assert r.exit_code == 1, r.output
    assert "must be a non-empty string" in r.output


def test_cmd_update_empty_cwd_errors(tmp_graph):
    """--cwd '' is rejected with exit 1."""
    _seed_node(tmp_graph, {
        "id": "ab-cccccccc", "project": "myvault", "cwd": "/old",
        "title": "z", "type": "feature",
    })
    r = _invoke("backlog", "update", "ab-cccccccc", "--cwd", "")
    assert r.exit_code == 1, r.output
    assert "must be a non-empty string" in r.output


def test_cmd_update_cwd_expands_tilde(tmp_graph, monkeypatch):
    """--cwd '~/foo' is expanded to /home/foo."""
    monkeypatch.setenv("HOME", "/Users/testuser")
    _seed_node(tmp_graph, {
        "id": "ab-dddddddd", "project": "foo", "cwd": "/old",
        "title": "z", "type": "feature",
    })
    r = _invoke("backlog", "update", "ab-dddddddd", "--cwd", "~/code/foo")
    assert r.exit_code == 0, r.output

    g = json.loads(tmp_graph.read_text())
    node = next(e for e in g["entries"] if e["id"] == "ab-dddddddd")
    assert node["cwd"] == "/Users/testuser/code/foo"


def test_cmd_update_project_on_missing_node_errors(tmp_graph):
    r = _invoke("backlog", "update", "ab-deadbeef", "--project", "foo")
    assert r.exit_code == 1, r.output
    assert "not found" in r.output


def test_intake_routes_to_frontmatter_project_end_to_end(tmp_graph, tmp_path, monkeypatch):
    """Journey 1: shared-vault intake routes to frontmatter project regardless of cwd.

    Without --project, a plan whose frontmatter declares project: foo must
    land in graph.json with project=foo even when cwd resolves to a different
    git repo. This is the core bug the plan fixes.
    """
    # Plan declares project: from-frontmatter
    plan = tmp_path / "plan-shared-vault.md"
    plan.write_text(
        "---\nproject: from-frontmatter\n---\n# title\nbody\n"
    )
    # Isolate from any settings.yaml that would emit a workspace warning
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setenv("HOME", str(fake_home))

    r = _invoke("backlog", "intake", str(plan))
    assert r.exit_code == 0, r.output

    g = json.loads(tmp_graph.read_text())
    nodes = g.get("entries") or []
    assert len(nodes) == 1
    assert nodes[0]["project"] == "from-frontmatter", (
        f"expected node project=from-frontmatter, got: {nodes[0].get('project')}"
    )

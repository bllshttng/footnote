"""Tests for the plan-claims feature.

Three layers covered:
1. Plan frontmatter ``claims: ab-XXX`` (declarative).
2. CLI flag ``--claims ab-XXX`` (runtime override; beats frontmatter).
3. Title-similarity warning at intake when no claim was declared.

The intake handler updates an existing idea-state node in place when a claim
resolves; refuses non-idea targets; appends a fresh node when no claim is
declared. The similarity scan runs only on the append path.
"""
from __future__ import annotations

import json
import re
from pathlib import Path
from unittest.mock import patch

import click.exceptions
import pytest

from fno.graph._intake import _resolve_claim, _warn_similar_idea_titles
from fno.graph.cli import _intake_impl


# -- fixtures --


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "parent": None,
        "title": "default-title",
        "type": "feature",
        "project": "fno",
        "cwd": "/tmp/abilities",
        "priority": "p2",
        "domain": "code",
        "blocked_by": [],
        "session_id": None,
        "claimed_at": None,
        "completed_at": None,
        "has_brief": False,
        "compacted": False,
        "roadmap_id": None,
        "vision_path": None,
        "details": None,
        "size": None,
        "batch": None,
        "cost_usd": None,
        "cost_sessions": [],
        "plan_path": None,
        "pr_number": None,
        "pr_url": None,
        "merge_status": None,
        "artifact_url": None,
        "completion_note": None,
        "status": "idea",
        "created_at": "2026-01-01T00:00:00+00:00",
    }
    base.update(overrides)
    return base


@pytest.fixture
def fixture_graph(tmp_path: Path):
    """Create a temp graph.json and patch _graph_path to return it."""
    entries = [
        _node(
            "ab-1dea1234",
            title="Backlog intake honors plan claims",
            status="idea",
            details="three-layer claim resolution",
        ),
        _node(
            "ab-d0ne5678",
            title="Already shipped feature",
            status="done",
            completed_at="2026-04-01T00:00:00+00:00",
            plan_path="plans/already.md",
        ),
        _node(
            "ab-0fff9999",
            title="Provider rotation substrate",
            status="idea",
        ),
    ]
    graph_file = tmp_path / "graph.json"
    graph_file.write_text(json.dumps({"entries": entries}) + "\n")

    with patch("fno.graph.cli._graph_path", return_value=graph_file), \
         patch("fno.graph._intake._git_repo_root", return_value=str(tmp_path)):
        yield graph_file


def _read_entries(graph_file: Path) -> list[dict]:
    return json.loads(graph_file.read_text())["entries"]


def _write_quick_plan(
    tmp_path: Path,
    title: str = "New plan title",
    *,
    claims: str | None = None,
) -> Path:
    plan = tmp_path / "plan.md"
    fm_lines = ["---"]
    if claims:
        fm_lines.append(f"claims: {claims}")
    fm_lines += ["created: 2026-05-05T04:35", "---"]
    body = [f"# {title}", "", "Body."]
    plan.write_text("\n".join(fm_lines + [""] + body) + "\n")
    return plan


# -- _resolve_claim --


def test_resolve_claim_returns_none_when_no_claim_anywhere(fixture_graph, tmp_path):
    plan = _write_quick_plan(tmp_path)
    entries = _read_entries(fixture_graph)
    node, source = _resolve_claim(None, str(plan), entries)
    assert node is None
    assert source is None


def test_resolve_claim_reads_frontmatter(fixture_graph, tmp_path):
    plan = _write_quick_plan(tmp_path, claims="ab-1dea1234")
    entries = _read_entries(fixture_graph)
    node, source = _resolve_claim(None, str(plan), entries)
    assert node is not None
    assert node["id"] == "ab-1dea1234"
    assert source == "frontmatter"


def test_resolve_claim_cli_wins_over_frontmatter(fixture_graph, tmp_path):
    plan = _write_quick_plan(tmp_path, claims="ab-1dea1234")
    entries = _read_entries(fixture_graph)
    # CLI claim names a different node; CLI wins.
    node, source = _resolve_claim("ab-0fff9999", str(plan), entries)
    assert node is not None
    assert node["id"] == "ab-0fff9999"
    assert source == "cli"


def test_resolve_claim_unknown_id_raises(fixture_graph, tmp_path):
    plan = _write_quick_plan(tmp_path)
    entries = _read_entries(fixture_graph)
    with pytest.raises(ValueError, match="not found on graph"):
        _resolve_claim("ab-99999999", str(plan), entries)


def test_resolve_claim_invalid_format_raises(fixture_graph, tmp_path):
    plan = _write_quick_plan(tmp_path)
    entries = _read_entries(fixture_graph)
    with pytest.raises(ValueError, match="invalid claims value"):
        _resolve_claim("not-an-id", str(plan), entries)


def test_resolve_claim_invalid_frontmatter_value_ignored(fixture_graph, tmp_path):
    """Malformed frontmatter values are treated as 'no claim' rather than crash.

    Only the CLI path raises on malformed input; frontmatter is best-effort
    parsed and a non-matching value falls through to the no-claim path.
    """
    plan = _write_quick_plan(tmp_path, claims="not-an-id")
    entries = _read_entries(fixture_graph)
    node, source = _resolve_claim(None, str(plan), entries)
    assert node is None
    assert source is None


# -- _warn_similar_idea_titles --


def test_similar_titles_warning_fires_above_threshold(fixture_graph, capsys):
    entries = _read_entries(fixture_graph)
    _warn_similar_idea_titles(
        new_title="Backlog intake honors plan claims to existing idea nodes",
        new_id="ab-7ew15h01",
        entries=entries,
    )
    err = capsys.readouterr().err
    assert "ab-1dea1234" in err
    assert "--claims ab-1dea1234" in err


def test_similar_titles_warning_skipped_below_threshold(fixture_graph, capsys):
    entries = _read_entries(fixture_graph)
    _warn_similar_idea_titles(
        new_title="Authentication middleware rewrite",
        new_id="ab-7ew15h01",
        entries=entries,
    )
    err = capsys.readouterr().err
    assert "ab-1dea1234" not in err
    assert "ab-0fff9999" not in err


def test_similar_titles_warning_excludes_self(fixture_graph, capsys):
    entries = list(_read_entries(fixture_graph))
    # Add a node whose title matches the candidate exactly.
    entries.append(
        _node(
            "ab-5e1f0001",
            title="Backlog intake honors plan claims",
            status="idea",
        )
    )
    _warn_similar_idea_titles(
        new_title="Backlog intake honors plan claims",
        new_id="ab-5e1f0001",
        entries=entries,
    )
    err = capsys.readouterr().err
    # The self-id is excluded; ab-1dea1234 (ratio 1.0 with the new title) IS reported.
    assert "ab-5e1f0001" not in err
    assert "ab-1dea1234" in err


def test_similar_titles_warning_skips_non_idea_states(fixture_graph, capsys):
    entries = _read_entries(fixture_graph)
    _warn_similar_idea_titles(
        new_title="Already shipped feature",  # exact match to ab-d0ne5678
        new_id="ab-7ew15h01",
        entries=entries,
    )
    err = capsys.readouterr().err
    # ab-d0ne5678 is `status: done`, not idea — skipped.
    assert "ab-d0ne5678" not in err


# -- _intake_impl: end-to-end claim mutation --


def test_intake_with_frontmatter_claim_updates_idea_node(fixture_graph, tmp_path, capsys):
    plan = _write_quick_plan(
        tmp_path,
        title="Backlog intake honors plan claims (final)",
        claims="ab-1dea1234",
    )
    _intake_impl(plan_paths=[str(plan)])
    out = capsys.readouterr().out
    assert "claimed ab-1dea1234" in out
    entries = _read_entries(fixture_graph)
    # No new node appended.
    assert len(entries) == 3
    target = next(e for e in entries if e["id"] == "ab-1dea1234")
    assert target["plan_path"] == str(plan)
    assert target["title"] == "Backlog intake honors plan claims (final)"
    # claimed_at is reset to None as part of the idea -> ready promotion.
    assert target["claimed_at"] is None


def test_intake_with_cli_claim_wins_over_frontmatter(fixture_graph, tmp_path, capsys):
    plan = _write_quick_plan(
        tmp_path,
        title="Some plan",
        claims="ab-1dea1234",  # frontmatter says 1234
    )
    _intake_impl(plan_paths=[str(plan)], claims="ab-0fff9999")
    out = capsys.readouterr().out
    assert "claimed ab-0fff9999" in out
    entries = _read_entries(fixture_graph)
    # ab-0fff9999 updated; ab-1dea1234 untouched.
    assert next(e for e in entries if e["id"] == "ab-0fff9999")["plan_path"] == str(plan)
    assert next(e for e in entries if e["id"] == "ab-1dea1234")["plan_path"] is None


def test_intake_refuses_non_idea_target(fixture_graph, tmp_path, capsys):
    plan = _write_quick_plan(tmp_path)
    with pytest.raises((SystemExit, click.exceptions.Exit)) as exc_info:
        _intake_impl(plan_paths=[str(plan)], claims="ab-d0ne5678")
    assert getattr(exc_info.value, "exit_code", getattr(exc_info.value, "code", 0)) != 0
    err = capsys.readouterr().err
    assert "ab-d0ne5678" in err
    # Graph unchanged.
    entries = _read_entries(fixture_graph)
    assert next(e for e in entries if e["id"] == "ab-d0ne5678")["plan_path"] == "plans/already.md"


def test_intake_invalid_claim_value_exits_nonzero(fixture_graph, tmp_path, capsys):
    plan = _write_quick_plan(tmp_path)
    with pytest.raises((SystemExit, click.exceptions.Exit)) as exc_info:
        _intake_impl(plan_paths=[str(plan)], claims="not-an-id")
    assert getattr(exc_info.value, "exit_code", getattr(exc_info.value, "code", 0)) != 0
    err = capsys.readouterr().err
    assert "invalid claims value" in err.lower() or "not-an-id" in err


def test_intake_unknown_claim_id_exits_nonzero(fixture_graph, tmp_path, capsys):
    plan = _write_quick_plan(tmp_path)
    with pytest.raises((SystemExit, click.exceptions.Exit)) as exc_info:
        _intake_impl(plan_paths=[str(plan)], claims="ab-99999999")
    assert getattr(exc_info.value, "exit_code", getattr(exc_info.value, "code", 0)) != 0
    err = capsys.readouterr().err
    assert "ab-99999999" in err


def test_intake_no_claim_appends_and_warns_on_similar_title(
    fixture_graph, tmp_path, capsys,
):
    plan = _write_quick_plan(
        tmp_path,
        title="Backlog intake honors plan claims to existing idea nodes",
    )
    _intake_impl(plan_paths=[str(plan)])
    err = capsys.readouterr().err
    # A fresh node was appended — original idea nodes still present.
    entries = _read_entries(fixture_graph)
    assert len(entries) == 4
    # Warning names the closest candidate.
    assert "ab-1dea1234" in err
    assert re.search(r"--claims\s+ab-1dea1234", err)


def test_intake_refuses_claim_when_plan_already_adopted_as_different_node(
    fixture_graph, tmp_path, capsys,
):
    """Repair-path safety: if the plan was already intaked as a fresh node
    (e.g. by an older /spec without --claims), the user must supersede that
    duplicate before claiming the original idea node.
    """
    plan = _write_quick_plan(tmp_path, title="Some plan")
    # First intake: no claim, creates a fresh node.
    _intake_impl(plan_paths=[str(plan)])
    capsys.readouterr()
    # Second intake: now declare a claim against ab-1dea1234, but the plan
    # is already adopted as the freshly-created node. We refuse rather than
    # silently double-applying.
    with pytest.raises((SystemExit, click.exceptions.Exit)):
        _intake_impl(plan_paths=[str(plan)], claims="ab-1dea1234")
    err = capsys.readouterr().err
    assert "supersede" in err
    assert "ab-1dea1234" in err


def test_cli_runner_intake_with_claims_flag(tmp_path, monkeypatch):
    """End-to-end CliRunner test for the real Typer command wiring.

    Catches option-name regressions (e.g. someone renaming `--claims` to
    `--claim-id` and forgetting to update `_intake_impl`'s signature) and
    Typer parameter-binding drift. The unit tests above call _intake_impl
    directly so they bypass this layer.
    """
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    entries = [_node("ab-1dea1234", title="Idea title", status="idea")]
    g.write_text(json.dumps({"entries": entries}) + "\n")
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    plan = _write_quick_plan(tmp_path, title="Real Typer plan")
    runner = CliRunner()
    result = runner.invoke(
        app, ["backlog", "intake", str(plan), "--claims", "ab-1dea1234"]
    )
    assert result.exit_code == 0, result.output
    assert "claimed ab-1dea1234 via cli" in result.output
    after = json.loads(g.read_text())["entries"]
    target = next(e for e in after if e["id"] == "ab-1dea1234")
    assert target["plan_path"] == str(plan)
    assert target["title"] == "Real Typer plan"


def test_cli_runner_intake_unknown_claim_exits_nonzero(tmp_path, monkeypatch):
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    g.write_text('{"entries": []}\n')
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    plan = _write_quick_plan(tmp_path)
    runner = CliRunner()
    result = runner.invoke(
        app, ["backlog", "intake", str(plan), "--claims", "ab-99999999"]
    )
    assert result.exit_code != 0
    assert "ab-99999999" in result.output


def test_intake_no_claim_no_similar_title_no_warning(fixture_graph, tmp_path, capsys):
    plan = _write_quick_plan(
        tmp_path,
        title="Completely unrelated authentication overhaul",
    )
    _intake_impl(plan_paths=[str(plan)])
    err = capsys.readouterr().err
    # No similarity warning for unrelated titles.
    assert "ab-1dea1234" not in err
    assert "ab-0fff9999" not in err


# -- claim path: project/cwd backfill --


@pytest.fixture
def unscoped_node_graph(tmp_path: Path):
    """Graph where the target idea node was created via ``fno backlog new``
    without a project (project=null, cwd=null). Simulates the workflow where
    someone runs ``fno backlog new "title"`` first, then writes a plan that
    claims that node."""
    entries = [
        _node(
            "ab-aaa00001",
            title="An unscoped idea created via fno backlog new",
            status="idea",
            project=None,
            cwd=None,
        ),
    ]
    graph_file = tmp_path / "graph.json"
    graph_file.write_text(json.dumps({"entries": entries}) + "\n")
    with patch("fno.graph.cli._graph_path", return_value=graph_file), \
         patch("fno.graph._intake._git_repo_root", return_value=str(tmp_path)):
        yield graph_file


def test_intake_claim_backfills_project_and_cwd_when_null(
    unscoped_node_graph, tmp_path, capsys
):
    """When a claim resolves to a node with project=null/cwd=null
    (typical of ``fno backlog new`` followed by a plan write), the intake
    backfills both fields from the plan path's git root. This eliminates
    the manual ``fno backlog update --project --cwd`` step."""
    plan = _write_quick_plan(
        tmp_path,
        title="Plan that claims an unscoped node",
        claims="ab-aaa00001",
    )
    _intake_impl(plan_paths=[str(plan)])

    entries = _read_entries(unscoped_node_graph)
    target = next(e for e in entries if e["id"] == "ab-aaa00001")
    # Project derived from git root basename (the tmp_path's basename).
    assert target["project"] == tmp_path.name
    # Cwd points at the git root (the tmp_path itself).
    assert target["cwd"] == str(tmp_path)
    # Plan + title landed too (the existing claim behavior, regression check).
    assert target["plan_path"] == str(plan)
    assert target["title"] == "Plan that claims an unscoped node"


def test_intake_claim_preserves_existing_project_and_cwd(
    fixture_graph, tmp_path, capsys
):
    """When a claim resolves to a node that already has project/cwd set
    (e.g. created via ``fno backlog new --project X``), the backfill
    must NOT overwrite those values. Backfill is null-only."""
    plan = _write_quick_plan(
        tmp_path,
        title="Plan that claims an already-scoped node",
        claims="ab-1dea1234",
    )
    _intake_impl(plan_paths=[str(plan)])

    entries = _read_entries(fixture_graph)
    target = next(e for e in entries if e["id"] == "ab-1dea1234")
    # Original project/cwd values preserved verbatim.
    assert target["project"] == "fno"
    assert target["cwd"] == "/tmp/abilities"


def test_intake_claim_backfills_only_null_fields(tmp_path, capsys):
    """When exactly one of project/cwd is null, only that one is backfilled.
    The other keeps its existing value. Verifies field-level granularity."""
    entries = [
        _node(
            "ab-aaa00002",
            title="An idea node with project but no cwd",
            status="idea",
            project="some-other-project",  # set
            cwd=None,                       # null
        ),
    ]
    graph_file = tmp_path / "graph.json"
    graph_file.write_text(json.dumps({"entries": entries}) + "\n")

    with patch("fno.graph.cli._graph_path", return_value=graph_file), \
         patch("fno.graph._intake._git_repo_root", return_value=str(tmp_path)):
        plan = _write_quick_plan(
            tmp_path,
            title="Plan that claims a half-scoped node",
            claims="ab-aaa00002",
        )
        _intake_impl(plan_paths=[str(plan)])

        result_entries = json.loads(graph_file.read_text())["entries"]
        target = next(e for e in result_entries if e["id"] == "ab-aaa00002")
        # project preserved (was non-null).
        assert target["project"] == "some-other-project"
        # cwd backfilled (was null).
        assert target["cwd"] == str(tmp_path)


# -- canonical-cwd recording (ab-932357c7) --
#
# Durable artifacts (backlog nodes, think/blueprint intake) must record the
# canonical main checkout, not the creating worktree. We simulate a linked
# worktree by patching `_git_repo_root` (canonical) and `os.getcwd` (worktree)
# to DIVERGENT paths, then assert the recorded cwd is the canonical one.


def test_resolve_git_roots_records_canonical_not_worktree(tmp_path):
    """AC1/AC4: resolve_git_roots() returns the canonical main checkout as the
    cwd element, derived from git primitives only, never the worktree path."""
    from fno.graph._intake import resolve_git_roots

    canonical = tmp_path / "canonical"
    worktree = tmp_path / "worktree-linked"
    canonical.mkdir()
    worktree.mkdir()

    with patch("fno.graph._intake._git_repo_root", return_value=str(canonical)), \
         patch("os.getcwd", return_value=str(worktree)):
        derived_name, cwd_root = resolve_git_roots()

    assert cwd_root == str(canonical)
    assert cwd_root != str(worktree)
    # project name is still the canonical basename, shared across worktrees.
    assert derived_name == "canonical"


def test_intake_records_canonical_cwd_from_linked_worktree(tmp_path):
    """AC1: a node intaken from inside a linked worktree (no frontmatter cwd)
    records the canonical checkout, so the reference survives worktree archival."""
    from fno.graph._intake import resolve_node_project_and_cwd

    canonical = tmp_path / "canonical"
    worktree = tmp_path / "worktree-linked"
    canonical.mkdir()
    worktree.mkdir()
    plan = _write_quick_plan(worktree, title="Plan filed from a worktree")

    with patch("fno.graph._intake._git_repo_root", return_value=str(canonical)), \
         patch("os.getcwd", return_value=str(worktree)):
        project, node_cwd, _ = resolve_node_project_and_cwd(str(plan), None, [])

    assert node_cwd == str(canonical)
    assert node_cwd != str(worktree)
    assert project == "canonical"


def test_intake_frontmatter_cwd_still_wins(tmp_path):
    """AC2: an explicit `cwd:` in plan frontmatter is preserved verbatim and is
    NOT canonicalized away from the author's intent."""
    from fno.graph._intake import resolve_node_project_and_cwd

    canonical = tmp_path / "canonical"
    canonical.mkdir()
    explicit = tmp_path / "explicit-target"
    plan = canonical / "plan.md"
    plan.write_text(
        "---\n"
        f"cwd: {explicit}\n"
        "created: 2026-05-24T00:00\n"
        "---\n\n# Plan with explicit cwd\n\nBody.\n"
    )

    with patch("fno.graph._intake._git_repo_root", return_value=str(canonical)):
        _, node_cwd, _ = resolve_node_project_and_cwd(str(plan), None, [])

    assert node_cwd == str(explicit)


def test_backlog_add_records_canonical_cwd(tmp_path):
    """AC1 + AC2 + AC3 for `fno backlog add`: default cwd is the canonical
    checkout; an explicit --cwd is preserved; in the canonical checkout
    (canonical == cwd) behavior is unchanged."""
    from typer.testing import CliRunner
    from fno.graph.cli import cli

    canonical = tmp_path / "canonical"
    worktree = tmp_path / "worktree-linked"
    canonical.mkdir()
    worktree.mkdir()
    graph_file = tmp_path / "graph.json"
    graph_file.write_text(json.dumps({"entries": []}) + "\n")

    runner = CliRunner()
    with patch("fno.graph.cli._graph_path", return_value=graph_file), \
         patch("fno.graph._intake._git_repo_root", return_value=str(canonical)), \
         patch("os.getcwd", return_value=str(worktree)):
        # AC1: no --cwd -> canonical, not the worktree.
        result = runner.invoke(cli, ["add", "Durable node"])
        assert result.exit_code == 0, result.output
        node = _read_entries(graph_file)[-1]
        assert node["cwd"] == str(canonical)
        assert node["cwd"] != str(worktree)

        # AC2: explicit --cwd preserved verbatim (abspath of the caller intent).
        result = runner.invoke(cli, ["add", "Pinned node", "--cwd", str(worktree)])
        assert result.exit_code == 0, result.output
        node = _read_entries(graph_file)[-1]
        assert node["cwd"] == str(worktree)


def test_backlog_idea_has_add_flag_parity(tmp_path):
    """`idea` is sugar for `add`: it accepts the same parent/size/domain flags,
    so a fresh idea need not be patched with a follow-up `fno backlog update`."""
    from typer.testing import CliRunner
    from fno.graph.cli import cli

    graph_file = tmp_path / "graph.json"
    graph_file.write_text(json.dumps({"entries": []}) + "\n")

    runner = CliRunner()
    with patch("fno.graph.cli._graph_path", return_value=graph_file), \
         patch("fno.graph._intake._git_repo_root", return_value=str(tmp_path)):
        result = runner.invoke(
            cli,
            ["idea", "t", "--parent", "ab-1234abcd", "--size", "M", "--domain", "infra"],
        )
        assert result.exit_code == 0, result.output

    node = _read_entries(graph_file)[-1]
    assert node["parent"] == "ab-1234abcd"
    assert node["size"] == "M"
    assert node["domain"] == "infra"

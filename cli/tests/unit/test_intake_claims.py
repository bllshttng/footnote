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

from fno.graph._intake import (
    _refuse_surfaceless_intake, _resolve_claim, _warn_similar_nodes,
)
from fno.graph.cli import _create_node_impl, _do_intake_multi, _intake_impl


# -- fixtures --


def _node(node_id: str, **overrides) -> dict:
    base = {
        "id": node_id,
        "parent": None,
        "title": "default-title",
        "type": "feature",
        "project": "fno",
        "cwd": "/tmp/fno",
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
         patch("fno.graph._intake._git_repo_root", return_value=str(tmp_path)), \
         patch("fno.paths.graph_archive_json", return_value=tmp_path / "graph-archive.json"):
        yield graph_file


def _read_entries(graph_file: Path) -> list[dict]:
    return json.loads(graph_file.read_text())["entries"]


# Every plan fixture in this file carries a file surface: intake refuses a
# surface-less plan by default, so a realistic default fixture states
# one and the refusal tests opt OUT via a body without this section.
_SURFACE = (
    "## Files to Modify\n\n| File | Action |\n|---|---|\n"
    "| `cli/src/fno/example.py` | modify |"
)


def _write_quick_plan(
    tmp_path: Path,
    title: str = "New plan title",
    *,
    claims: str | None = None,
    difficulty: str | None = None,
) -> Path:
    plan = tmp_path / "plan.md"
    fm_lines = ["---"]
    if claims:
        fm_lines.append(f"claims: {claims}")
    if difficulty:
        fm_lines.append(f"difficulty: {difficulty}")
    fm_lines += ["created: 2026-05-05T04:35", "---"]
    body = [f"# {title}", "", _SURFACE, "", "Body."]
    plan.write_text("\n".join(fm_lines + [""] + body) + "\n")
    return plan


# -- cross-roadmap plan ownership (x-d9a4, codex P1) --


def test_prepare_intake_refuses_a_plan_owned_under_another_roadmap(tmp_path):
    """plan == PR == node is roadmap-agnostic: a plan owned under one roadmap must
    not bind to a second node under another, or both dispatch concurrently.
    _match_plan_in_graph only catches the same-roadmap "already intaked" case, so
    the cross-roadmap refusal lives in _prepare_intake (the shared prep for the
    claim, create, and multi lanes)."""
    from fno.graph._intake import _prepare_intake

    plan = _write_quick_plan(tmp_path)
    owner = _node(
        "ab-0wn0001",
        title="Owner under roadmap A",
        status="ready",
        plan_path=str(plan),
        roadmap_id="roadmap-a",
    )

    with pytest.raises(ValueError) as exc:
        _prepare_intake(
            str(plan), [owner],
            roadmap_id="roadmap-B",  # different roadmap -> same-roadmap match misses
            cli_title=None, cli_priority=None, cli_deps=[], cli_points=None,
        )
    assert "ab-0wn0001" in str(exc.value)
    assert "one PR is one node" in str(exc.value)


def test_prepare_intake_allows_a_plan_owned_under_the_same_roadmap(tmp_path):
    """The same-roadmap 'already intaked' path is untouched: it returns the
    existing node rather than refusing, so cross-roadmap guard must not fire."""
    from fno.graph._intake import _prepare_intake

    plan = _write_quick_plan(tmp_path)
    owner = _node(
        "ab-0wn0001",
        title="Owner under roadmap A",
        status="ready",
        plan_path=str(plan),
        roadmap_id="roadmap-a",
    )

    result = _prepare_intake(
        str(plan), [owner],
        roadmap_id="roadmap-a",  # same roadmap -> friendly 'already intaked'
        cli_title=None, cli_priority=None, cli_deps=[], cli_points=None,
    )
    assert result["status"] == "already"
    assert result["id"] == "ab-0wn0001"


def test_prepare_intake_maps_legacy_plan_priority_vocabulary(tmp_path):
    """A plan frontmatter priority in the legacy severity vocabulary maps to
    its pN band instead of landing raw, where the ordering silently degrades
    it to p2 while the board still displays the raw value."""
    from fno.graph._intake import _prepare_intake

    plan = _write_quick_plan(tmp_path)
    plan.write_text(plan.read_text().replace("created:", "priority: high\ncreated:"))

    result = _prepare_intake(
        str(plan), [],
        roadmap_id=None, cli_title=None, cli_priority=None, cli_deps=[], cli_points=None,
    )
    assert result["status"] == "ready"
    assert result["node_spec"]["priority"] == "p1"


def test_prepare_intake_refuses_out_of_vocabulary_plan_priority(tmp_path):
    """A plan priority no vocabulary maps refuses loudly at the write boundary
    rather than storing a value every ordering quietly treats as p2."""
    from fno.graph._intake import _prepare_intake

    plan = _write_quick_plan(tmp_path)
    plan.write_text(plan.read_text().replace("created:", "priority: urgent\ncreated:"))

    with pytest.raises(ValueError) as exc:
        _prepare_intake(
            str(plan), [],
            roadmap_id=None, cli_title=None, cli_priority=None, cli_deps=[], cli_points=None,
        )
    assert "invalid priority" in str(exc.value)


# -- mandatory file surface at intake --


def _surfaceless_plan(tmp_path: Path, title: str = "No surface plan") -> Path:
    plan = tmp_path / "no-surface.md"
    plan.write_text(f"---\ncreated: 2026-05-05T04:35\n---\n# {title}\n\nBody.\n")
    return plan


def test_intake_refuses_surfaceless_plan(fixture_graph, tmp_path, capsys):
    """A plan whose Files to Modify parses empty is refused (exit 2) naming the
    plan and --allow-no-surface, with the graph unchanged."""
    plan = _surfaceless_plan(tmp_path)
    with pytest.raises((SystemExit, click.exceptions.Exit)) as exc_info:
        _intake_impl(plan_paths=[str(plan)])
    code = getattr(exc_info.value, "exit_code", getattr(exc_info.value, "code", 0))
    assert code == 2
    err = capsys.readouterr().err
    assert str(plan) in err
    assert "--allow-no-surface" in err
    assert len(_read_entries(fixture_graph)) == 3  # nothing written


def test_intake_allow_no_surface_flag_admits_the_plan(fixture_graph, tmp_path, capsys):
    plan = _surfaceless_plan(tmp_path)
    _intake_impl(plan_paths=[str(plan)], allow_no_surface=True)
    out = capsys.readouterr().out
    assert "intake " in out
    assert len(_read_entries(fixture_graph)) == 4


def test_surfaceful_intake_creates_node_without_surface_message(
    fixture_graph, tmp_path, capsys
):
    """A plan stating a surface intakes exactly as before - no surface noise."""
    plan = _write_quick_plan(tmp_path, title="Surfaceful intake plan omega")
    _intake_impl(plan_paths=[str(plan)])
    captured = capsys.readouterr()
    assert "no comparable file surface" not in captured.err
    assert len(_read_entries(fixture_graph)) == 4


def test_multi_intake_refuses_whole_batch_on_surfaceless_plan(
    fixture_graph, tmp_path, capsys
):
    """The multi lane refuses the BATCH, not the file: a surface-less plan
    aborts before any write, so the good plan does not land either."""
    from types import SimpleNamespace

    (tmp_path / "good").mkdir()
    good = _write_quick_plan(tmp_path / "good", title="Good surface plan")
    bad = _surfaceless_plan(tmp_path)
    args = SimpleNamespace(
        deps=None, priority=None, points=None, project=None, title=None, force_new_roadmap=False,
    )
    with pytest.raises((SystemExit, click.exceptions.Exit)) as exc_info:
        _do_intake_multi(
            args, [str(good), str(bad)], roadmap_id=None, dry_run=False
        )
    code = getattr(exc_info.value, "exit_code", getattr(exc_info.value, "code", 0))
    assert code == 2
    err = capsys.readouterr().err
    assert str(bad) in err
    titles = [e.get("title") for e in _read_entries(fixture_graph)]
    assert not any("Good surface plan" in (t or "") for t in titles)
    assert len(_read_entries(fixture_graph)) == 3  # batch left the graph alone


def test_intake_missing_plan_file_refused_as_not_found(fixture_graph, tmp_path, capsys):
    """A missing plan file gets its own not-found refusal, not the misleading
    'add rows' message (and never mints a node pointing at nothing)."""
    with pytest.raises((SystemExit, click.exceptions.Exit)) as exc_info:
        _intake_impl(plan_paths=[str(tmp_path / "typo.md")])
    code = getattr(exc_info.value, "exit_code", getattr(exc_info.value, "code", 0))
    assert code == 2
    err = capsys.readouterr().err
    assert "plan file not found" in err
    assert "path spelling" in err
    assert len(_read_entries(fixture_graph)) == 3


def test_surface_refusal_reads_cli_paths_cwd_relative(tmp_path, monkeypatch):
    """The probe reads the CLI-given spelling (Path, not a git-root join), so a
    ../path from a subdirectory does not false-refuse a valid surface."""
    (tmp_path / "sub").mkdir()
    plan = tmp_path / "plan.md"
    plan.write_text(f"# Title\n\n{_SURFACE}\n")
    monkeypatch.chdir(tmp_path / "sub")
    _refuse_surfaceless_intake(["../plan.md"], allow_no_surface=False)  # no raise
    with pytest.raises((SystemExit, click.exceptions.Exit)):
        _refuse_surfaceless_intake(["../missing.md"], allow_no_surface=False)


def test_surface_refusal_expands_tilde_paths(tmp_path, monkeypatch):
    """A quoted tilde path to an existing, surfaceful plan must not take the
    missing-file branch: the collision resolver expands tilde at dispatch, so
    refusing it here would break a plan that works end to end."""
    (tmp_path / "vault").mkdir()
    plan = tmp_path / "vault" / "plan.md"
    plan.write_text(f"# Title\n\n{_SURFACE}\n")
    monkeypatch.setenv("HOME", str(tmp_path))
    _refuse_surfaceless_intake(["~/vault/plan.md"], allow_no_surface=False)


def test_multi_intake_exempts_already_intaked_surfaceless_plan(
    fixture_graph, tmp_path, capsys
):
    """An already-bound surface-less plan is a no-op for this verb (the single
    lane prints 'already intaked' and exits 0), so re-running a batch that
    contains one must not refuse the rest."""
    from types import SimpleNamespace

    bound = _surfaceless_plan(tmp_path, title="Bound surfaceless")
    entries = _read_entries(fixture_graph)
    entries.append(
        _node("ab-b0nd0001", title="Owner of the surfaceless plan",
              plan_path=str(bound), status="ready")
    )
    fixture_graph.write_text(json.dumps({"entries": entries}) + "\n")
    (tmp_path / "fresh").mkdir()
    fresh = _write_quick_plan(tmp_path / "fresh", title="Fresh surface plan")

    args = SimpleNamespace(
        deps=None, priority=None, points=None, project=None, title=None, force_new_roadmap=False,
    )
    _do_intake_multi(
        args, [str(bound), str(fresh)], roadmap_id=None, dry_run=False
    )
    captured = capsys.readouterr()
    assert "already intaked ab-b0nd0001" in captured.out
    titles = [e.get("title") for e in _read_entries(fixture_graph)]
    assert any("Fresh surface plan" in (t or "") for t in titles)


def test_multi_intake_exempts_bound_surfaceless_plan_across_roadmaps(
    fixture_graph, tmp_path, capsys
):
    """A surface-less plan owned under ANY roadmap is exempt from the batch
    refusal: the mutator skips it per file with the accurate owner-conflict
    error instead of blaming a file table the owner never reads."""
    from types import SimpleNamespace

    bound = _surfaceless_plan(tmp_path, title="Bound surfaceless")
    entries = _read_entries(fixture_graph)
    entries.append(
        _node("ab-r0ad0001", title="Owner under another roadmap",
              plan_path=str(bound), status="ready", roadmap_id="roadmap-a")
    )
    fixture_graph.write_text(json.dumps({"entries": entries}) + "\n")
    (tmp_path / "fresh2").mkdir()
    fresh = _write_quick_plan(tmp_path / "fresh2", title="Cross roadmap fresh")

    args = SimpleNamespace(
        deps=None, priority=None, points=None, project=None, title=None, force_new_roadmap=False,
    )
    _do_intake_multi(
        args, [str(bound), str(fresh)], roadmap_id=None, dry_run=False
    )
    captured = capsys.readouterr()
    # The bound plan resolves per file to the owner conflict, not a refusal.
    assert "delivery unit" in captured.err
    titles = [e.get("title") for e in _read_entries(fixture_graph)]
    assert any("Cross roadmap fresh" in (t or "") for t in titles)


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


# -- _warn_similar_nodes (filing-time dedup net, plan x-6ac7) --


def _dedup_entries():
    """Synthetic candidates: idea1 is the exact-match top; done1/rev1 carry
    pr_number; sup1 is superseded and must be excluded."""
    return [
        _node("idea1", title="dedup gate for backlog filings", status="idea"),
        _node("done1", title="dedup gate for backlog filings extra tokens", status="done", pr_number=42),
        _node("rev1", title="dedup net for backlog node birth", status="in_review", pr_number=77),
        _node("sup1", title="dedup gate for backlog filings twin", status="superseded"),
    ]


def test_warn_similar_nodes_names_done_and_in_review_with_pr_number(capsys):
    # AC5-EDGE: done AND in_review both surface, each with PR#N; superseded excluded.
    new = _node("new", title="dedup gate for backlog node filings", status="idea")
    _warn_similar_nodes(new, _dedup_entries(), intake_hint=False)
    captured = capsys.readouterr()
    assert "dedup:" in captured.err
    assert "done1" in captured.err and "done" in captured.err and "PR#42" in captured.err
    assert "rev1" in captured.err and "in_review" in captured.err and "PR#77" in captured.err
    assert "sup1" not in captured.err  # superseded never a candidate
    # AC4-UI: receipt is stderr-only; stdout stays the machine-readable payload.
    assert "dedup:" not in captured.out


def test_warn_similar_nodes_silent_when_no_candidates(capsys):
    new = _node("new", title="completely unrelated novel topic here", status="idea")
    _warn_similar_nodes(new, _dedup_entries(), intake_hint=False)
    assert capsys.readouterr().err == ""


def test_warn_similar_nodes_intake_hint_names_idea_state_top_candidate(capsys):
    new = _node("new", title="dedup gate for backlog node filings", status="idea")
    _warn_similar_nodes(new, _dedup_entries(), intake_hint=True)
    err = capsys.readouterr().err
    # The top candidate (idea1) is idea-state, so its id is in the --claims remedy.
    assert re.search(r"--claims\s+idea1", err)


def test_warn_similar_nodes_intake_hint_omits_claims_when_top_not_idea(capsys):
    # Top candidate is done (not claimable upstream); the intake remedy informs
    # only, no --claims hint.
    done_top = _node("done1", title="already shipped feature exactly twin", status="done", pr_number=9)
    new = _node("new", title="already shipped feature exactly", status="idea")
    _warn_similar_nodes(new, [done_top], intake_hint=True)
    err = capsys.readouterr().err
    assert "--claims" not in err
    assert "supersede" in err.lower()


def test_warn_similar_nodes_without_intake_hint_omits_claims(capsys):
    new = _node("new", title="dedup gate for backlog filings", status="idea")
    _warn_similar_nodes(new, _dedup_entries(), intake_hint=False)
    err = capsys.readouterr().err
    assert "--claims" not in err
    assert "supersede" in err.lower()


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


def test_intake_claim_records_blueprint_difficulty(fixture_graph, tmp_path, capsys):
    """AC1-HP: blueprint intake revises the filed estimate with attribution."""
    plan = _write_quick_plan(
        tmp_path,
        title="Difficulty revision",
        claims="ab-1dea1234",
        difficulty="high",
    )
    _intake_impl(plan_paths=[str(plan)])
    capsys.readouterr()
    target = next(e for e in _read_entries(fixture_graph) if e["id"] == "ab-1dea1234")
    assert target["difficulty"] == "high"
    assert target["difficulty_history"][-1]["source"] == "blueprint"


def test_intake_claim_carries_p0_acknowledgment(fixture_graph, tmp_path, capsys):
    """Claimed intake preserves the plan's validated p0 acknowledgment."""
    plan = tmp_path / "p0-claim.md"
    plan.write_text(
        "---\nclaims: ab-1dea1234\npriority: p0\nblocks_everything: true\n---\n"
        f"# Broken service\n\n{_SURFACE}\n"
    )
    _intake_impl(plan_paths=[str(plan)])
    capsys.readouterr()
    target = next(e for e in _read_entries(fixture_graph) if e["id"] == "ab-1dea1234")
    assert target["priority"] == "p0"
    assert target["blocks_everything"] is True


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
    assert "dedup:" in err
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


def test_claim_lane_flows_declared_type_doc_to_graph(tmp_path, monkeypatch):
    """The claim lane reads `type` doc->graph, same as the create lane.

    Intake has two lanes. `_build_intake_node` reads the doc's `type`; if this
    one did not, the flow would be a guard on one of N reachable paths - a doc
    declaring `type: bug` claimed onto an existing node would leave the graph
    at its mint-time default and the two would silently disagree.
    """
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": [_node("ab-1dea1234", title="Idea", status="idea")]}) + "\n")
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    plan = tmp_path / "plan.md"
    plan.write_text(
        f"---\ncreated: 2026-05-05T04:35\ntype: bug\n---\n# Bug plan\n\n{_SURFACE}\n\nBody.\n"
    )

    result = CliRunner().invoke(
        app, ["backlog", "intake", str(plan), "--claims", "ab-1dea1234"]
    )
    assert result.exit_code == 0, result.output
    after = json.loads(g.read_text())["entries"]
    assert next(e for e in after if e["id"] == "ab-1dea1234")["type"] == "bug"


def test_claim_lane_respects_the_epic_nesting_cap(tmp_path, monkeypatch):
    """A doc-declared `type: epic` cannot mint a third epic level.

    `add` and `update` both refuse this; a frontmatter lane that skipped the cap
    would be a guard on one of N reachable paths. This lane SKIPS with a warning
    rather than refusing - the frontmatter is advisory and the claim is valid.
    """
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    # mission (epic) -> nested (epic) -> target. Promoting target = 3rd level.
    entries = [
        _node("ab-m1111111", title="Mission", type="epic"),
        _node("ab-e2222222", title="Nested epic", type="epic", parent="ab-m1111111"),
        _node("ab-1dea1234", title="Idea", status="idea", parent="ab-e2222222"),
    ]
    g.write_text(json.dumps({"entries": entries}) + "\n")
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    plan = tmp_path / "plan.md"
    plan.write_text(
        f"---\ncreated: 2026-05-05T04:35\ntype: epic\n---\n# Epic plan\n\n{_SURFACE}\n\nBody.\n"
    )

    result = CliRunner().invoke(
        app, ["backlog", "intake", str(plan), "--claims", "ab-1dea1234"]
    )
    assert result.exit_code == 0, result.output  # the claim itself still lands
    after = json.loads(g.read_text())["entries"]
    target = next(e for e in after if e["id"] == "ab-1dea1234")
    assert target["type"] == "feature"  # promotion refused
    assert target["plan_path"] == str(plan)  # the rest of the claim applied
    assert "epic-nesting cap" in result.output


def test_claim_lane_allows_an_epic_under_a_top_level_mission(tmp_path, monkeypatch):
    """The cap is two levels, so one epic under a top-level mission is legal."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    entries = [
        _node("ab-m1111111", title="Mission", type="epic"),
        _node("ab-1dea1234", title="Idea", status="idea", parent="ab-m1111111"),
    ]
    g.write_text(json.dumps({"entries": entries}) + "\n")
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    plan = tmp_path / "plan.md"
    plan.write_text(
        f"---\ncreated: 2026-05-05T04:35\ntype: epic\n---\n# Epic plan\n\n{_SURFACE}\n\nBody.\n"
    )

    result = CliRunner().invoke(
        app, ["backlog", "intake", str(plan), "--claims", "ab-1dea1234"]
    )
    assert result.exit_code == 0, result.output
    after = json.loads(g.read_text())["entries"]
    assert next(e for e in after if e["id"] == "ab-1dea1234")["type"] == "epic"


def test_claim_lane_leaves_type_alone_when_doc_declares_none(tmp_path, monkeypatch):
    """A doc with no node-kind `type` never demotes an operator-set node type."""
    from typer.testing import CliRunner
    from fno.cli import app
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    seeded = _node("ab-1dea1234", title="Idea", status="idea")
    seeded["type"] = "epic"  # an operator set this explicitly
    g.write_text(json.dumps({"entries": [seeded]}) + "\n")
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    plan = _write_quick_plan(tmp_path, title="Untyped plan")
    result = CliRunner().invoke(
        app, ["backlog", "intake", str(plan), "--claims", "ab-1dea1234"]
    )
    assert result.exit_code == 0, result.output
    after = json.loads(g.read_text())["entries"]
    assert next(e for e in after if e["id"] == "ab-1dea1234")["type"] == "epic"


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
    assert target["cwd"] == "/tmp/fno"


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
                ["idea", "t", "--parent", "ab-1234abcd", "--size", "M", "--domain", "infra", "--difficulty", "low"],
        )
        assert result.exit_code == 0, result.output

    node = _read_entries(graph_file)[-1]
    assert node["parent"] == "ab-1234abcd"
    assert node["size"] == "M"
    assert node["domain"] == "infra"


# -- birth-path dedup parity (plan x-6ac7 task 1.3: AC1/AC2/AC3/AC6-FR) --
# Every node-birth path runs the SAME dedup net. A net on 2 of 3 paths is
# decorative (pitfalls corpus entry 1), so these exercise all three through
# their real entry points and pin that the old idea-state-only net is gone.


def test_idea_birth_path_warns_on_near_duplicate(fixture_graph, capsys):
    # AC1-HP + AC4-UI: the idea/add path (completely unguarded before this)
    # files a near-dup of ab-1dea1234; receipt on stderr names id + status +
    # 2-decimal score; stdout stays the JSON payload.
    _create_node_impl(
        title="Backlog intake plan-claim resolution net",
        details="three-layer claim resolution for backlog intake",
    )
    captured = capsys.readouterr()
    assert len(_read_entries(fixture_graph)) == 4  # new node persisted (exit 0)
    assert "dedup:" in captured.err
    assert "ab-1dea1234" in captured.err
    assert re.search(r"ab-1dea1234\s+idea\s+0\.\d{2}", captured.err)
    assert "dedup:" not in captured.out


def test_idea_birth_path_warns_on_near_duplicate_of_done(fixture_graph, capsys):
    # The exact regression class this PR fixes: a shipped `done` node is the
    # answer to a duplicate filing, and the old idea-only net could not see it.
    # Pins the wiring end-to-end (unit tests cover the scorer/helper in isolation).
    _create_node_impl(
        title="Already shipped feature refactor", details="feature shipped already"
    )
    captured = capsys.readouterr()
    assert "dedup:" in captured.err
    assert "ab-d0ne5678" in captured.err
    assert re.search(r"ab-d0ne5678\s+done\s+0\.\d{2}", captured.err)


def test_idea_birth_path_silent_on_clean_filing(fixture_graph, capsys):
    # AC2-HP: an unrelated filing warns nothing.
    _create_node_impl(title="Completely unrelated novel topic", details="unique content here")
    assert "dedup:" not in capsys.readouterr().err


def test_multi_intake_birth_path_warns_on_near_duplicate(fixture_graph, tmp_path, capsys):
    # AC6-FR (batch leg): the multi-intake path warns per intaked node.
    from types import SimpleNamespace

    def _plan(path: Path, title: str) -> Path:
        path.write_text(
            "\n".join(
                ["---", "created: 2026-05-05T04:35", "---", "", f"# {title}", "",
                 _SURFACE, "", "Body.", ""]
            )
            + "\n"
        )
        return path

    plan_a = _plan(tmp_path / "plan_a.md", "Backlog intake honors plan claims alpha")
    plan_b = _plan(tmp_path / "plan_b.md", "Backlog intake honors plan claims beta")
    args = SimpleNamespace(
        deps=None, priority="p2", points=None, project=None, title=None, force_new_roadmap=False,
    )
    _do_intake_multi(args, [str(plan_a), str(plan_b)], roadmap_id=None, dry_run=False)
    err = capsys.readouterr().err
    assert err.count("dedup:") == 2  # one receipt per intaked node
    assert "ab-1dea1234" in err


def test_multi_intake_skips_refused_plan_cleanly(fixture_graph, tmp_path, capsys):
    """A refusal (bad priority vocabulary, owner conflict) names ONE file: the
    batch intakes the rest and reports the refusal as a clean per-file error,
    never a traceback out of the locked mutator."""
    from types import SimpleNamespace

    def _plan(path: Path, title: str, priority: str | None = None) -> Path:
        fm = ["---"]
        if priority:
            fm.append(f"priority: {priority}")
        fm += ["created: 2026-05-05T04:35", "---"]
        path.write_text(
            "\n".join(fm + ["", f"# {title}", "", _SURFACE, "", "Body.", ""]) + "\n"
        )
        return path

    good = _plan(tmp_path / "good.md", "Batch intake good plan gamma")
    bad = _plan(tmp_path / "bad.md", "Batch intake bad plan delta", priority="urgent")
    args = SimpleNamespace(
        # priority None so the PLAN frontmatter supplies it (a cli priority
        # would outrank the frontmatter and the bad value would never be read).
        deps=None, priority=None, points=None, project=None, title=None, force_new_roadmap=False,
    )
    _do_intake_multi(args, [str(good), str(bad)], roadmap_id=None, dry_run=False)
    captured = capsys.readouterr()
    assert "invalid priority" in captured.err
    assert "skipped" in captured.err
    assert "refused" in captured.out
    titles = [e.get("title") for e in _read_entries(fixture_graph)]
    assert any("good plan gamma" in (t or "") for t in titles)
    assert not any("bad plan delta" in (t or "") for t in titles)
    assert len(_read_entries(fixture_graph)) == 4  # 3 seeded + the 1 good plan


def test_multi_intake_dry_run_previews_refusals_not_would_intake(
    fixture_graph, tmp_path, capsys
):
    """The multi dry-run previews validate-then, mirroring the single-plan dry
    run: a plan the real run would refuse (bad priority) or skip as
    already-intaked previews as exactly that - never as would-intake."""
    from types import SimpleNamespace

    def _plan(path: Path, title: str, priority: str | None = None) -> Path:
        fm = ["---"]
        if priority:
            fm.append(f"priority: {priority}")
        fm += ["created: 2026-05-05T04:35", "---"]
        path.write_text(
            "\n".join(fm + ["", f"# {title}", "", _SURFACE, "", "Body.", ""]) + "\n"
        )
        return path

    good = _plan(tmp_path / "good.md", "Batch intake good plan gamma")
    bad = _plan(tmp_path / "bad.md", "Batch intake bad plan delta", priority="urgent")
    already = _plan(tmp_path / "already.md", "Batch intake already plan epsilon")
    # A graph node already owns `already`'s plan_path: the real run reports
    # "already intaked ab-alr0001" and intakes nothing for it.
    entries = _read_entries(fixture_graph)
    entries.append(_node("ab-alr0001", title="Owner of the already plan", plan_path=str(already)))
    fixture_graph.write_text(json.dumps({"entries": entries}) + "\n")

    args = SimpleNamespace(
        # priority None so the PLAN frontmatter supplies it (a cli priority
        # would outrank the frontmatter and the bad value would never be read).
        deps=None, priority=None, points=None, project=None, title=None, force_new_roadmap=False,
    )
    _do_intake_multi(
        args, [str(good), str(bad), str(already)], roadmap_id=None, dry_run=True
    )
    captured = capsys.readouterr()

    # The preview ran and resolved the files: the good plan previews as intake.
    would_lines = [ln for ln in captured.out.splitlines() if "would intake" in ln]
    assert any("good plan gamma" in ln for ln in would_lines)
    # The refusable and the already-intaked plans never preview as would-intake.
    assert not any("bad plan delta" in ln for ln in would_lines)
    assert not any("already plan epsilon" in ln for ln in would_lines)
    # They preview as the outcome the real run would produce instead.
    assert "would skip" in captured.err and "invalid priority" in captured.err
    assert "already intaked ab-alr0001" in captured.out
    # The count names the plans that would actually land: one, not three.
    assert "1 plans would be intaked" in captured.out


def test_multi_intake_dry_run_grows_preview_graph_for_batch_duplicates(
    fixture_graph, tmp_path, capsys
):
    """A path passed twice (the caller's comma-split can produce this) previews
    once as would-intake and once as already intaked by this batch - the real
    run intakes the first and reports the second as already - never twice as
    would-intake."""
    from types import SimpleNamespace

    plan = tmp_path / "dupe.md"
    plan.write_text(
        "\n".join(["---", "created: 2026-05-05T04:35", "---", "", "# Dupe plan zeta", "", _SURFACE, "", "Body.", ""])
        + "\n"
    )
    args = SimpleNamespace(
        deps=None, priority=None, points=None, project=None, title=None, force_new_roadmap=False,
    )
    _do_intake_multi(args, [str(plan), str(plan)], roadmap_id=None, dry_run=True)
    captured = capsys.readouterr()

    would_lines = [ln for ln in captured.out.splitlines() if "would intake" in ln]
    assert len(would_lines) == 1
    assert "Dupe plan zeta" in would_lines[0]
    # The second occurrence previews the real run's outcome: already intaked
    # by this batch, with the plan named.
    assert "already intaked" in captured.out
    assert str(plan) in [ln for ln in captured.out.splitlines() if "already intaked" in ln][0]
    assert "1 plans would be intaked" in captured.out


def test_multi_intake_build_refusal_skips_one_file_not_batch(
    fixture_graph, tmp_path, capsys
):
    """A build-time refusal (invalid difficulty frontmatter) names ONE file:
    the batch lands the rest and skips the file with a clean per-file error.
    Uncaught, the ValueError escaped the lock and persisted NOTHING."""
    from types import SimpleNamespace

    def _plan(path: Path, title: str, difficulty: str | None = None) -> Path:
        fm = ["---"]
        if difficulty:
            fm.append(f"difficulty: {difficulty}")
        fm += ["created: 2026-05-05T04:35", "---"]
        path.write_text(
            "\n".join(fm + ["", f"# {title}", "", _SURFACE, "", "Body.", ""]) + "\n"
        )
        return path

    good = _plan(tmp_path / "good.md", "Build refusal batch good eta")
    bad = _plan(tmp_path / "baddiff.md", "Build refusal batch bad theta", difficulty="bogus")
    args = SimpleNamespace(
        deps=None, priority=None, points=None, project=None, title=None, force_new_roadmap=False,
    )
    _do_intake_multi(args, [str(good), str(bad)], roadmap_id=None, dry_run=False)
    captured = capsys.readouterr()

    assert "invalid difficulty" in captured.err
    assert "skipped" in captured.err
    assert "1 refused" in captured.out
    # The batch landed: the good plan persisted, no node for the refused one.
    entries = _read_entries(fixture_graph)
    assert len(entries) == 4
    assert any("good eta" in (e.get("title") or "") for e in entries)
    assert not any("bad theta" in (e.get("title") or "") for e in entries)


def test_multi_intake_claim_updates_idea_node_in_place(fixture_graph, tmp_path, capsys):
    """A frontmatter claim in a multi batch claims the EXISTING idea node in
    place - plan attached, idea promoted - exactly like the single-plan lane.
    Never a fresh duplicate node beside an unclaimed idea."""
    from types import SimpleNamespace

    plan = tmp_path / "claimed.md"
    plan.write_text(
        "\n".join(
            ["---", "claims: ab-1dea1234", "created: 2026-05-05T04:35", "---",
             "", "# Claimed by multi batch iota", "", _SURFACE, "", "Body.", ""]
        ) + "\n"
    )
    args = SimpleNamespace(
        deps=None, priority=None, points=None, project=None, title=None, force_new_roadmap=False,
    )
    _do_intake_multi(args, [str(plan)], roadmap_id=None, dry_run=False)
    captured = capsys.readouterr()

    # No duplicate: the graph still holds exactly the three seeded nodes.
    entries = _read_entries(fixture_graph)
    assert len(entries) == 3
    claimed = next(e for e in entries if e["id"] == "ab-1dea1234")
    assert claimed["plan_path"] == str(plan)
    assert claimed["claimed_at"] is None
    assert "Claimed by multi batch iota" in claimed["title"]
    assert "claim ab-1dea1234" in captured.out
    assert "1 claimed" in captured.out


def test_multi_intake_dry_run_previews_claim_and_build_refusal(
    fixture_graph, tmp_path, capsys
):
    """Preview parity for both per-file outcomes: a claims plan previews as
    would claim (naming the claimed node), and a plan whose build would raise
    (invalid difficulty) previews as would skip - never would-intake."""
    from types import SimpleNamespace

    claimy = tmp_path / "claimy.md"
    claimy.write_text(
        "\n".join(
            ["---", "claims: ab-1dea1234", "created: 2026-05-05T04:35", "---",
             "", "# Preview claim kappa", "", _SURFACE, "", "Body.", ""]
        ) + "\n"
    )
    baddiff = tmp_path / "baddiff.md"
    baddiff.write_text(
        "\n".join(
            ["---", "difficulty: bogus", "created: 2026-05-05T04:35", "---",
             "", "# Preview refusal lambda", "", _SURFACE, "", "Body.", ""]
        ) + "\n"
    )
    args = SimpleNamespace(
        deps=None, priority=None, points=None, project=None, title=None, force_new_roadmap=False,
    )
    _do_intake_multi(
        args, [str(claimy), str(baddiff)], roadmap_id=None, dry_run=True
    )
    captured = capsys.readouterr()

    assert "would claim" in captured.out
    assert "(claims ab-1dea1234)" in captured.out
    assert "would skip" in captured.err
    assert "invalid difficulty" in captured.err
    would_lines = [ln for ln in captured.out.splitlines() if "would intake" in ln]
    assert not any("kappa" in ln for ln in would_lines)
    assert not any("lambda" in ln for ln in would_lines)
    assert "0 plans would be intaked" in captured.out
    # The preview mutated nothing: the idea node is unclaimed, no nodes added.
    entries = _read_entries(fixture_graph)
    assert len(entries) == 3
    assert next(e for e in entries if e["id"] == "ab-1dea1234")["plan_path"] is None


def test_old_idea_title_warning_function_is_gone():
    # AC6-FR: the difflib idea-state-only net cannot survive as a second path.
    import fno.graph._intake as _intake

    assert not hasattr(_intake, "_warn_similar_idea_titles")


def test_idea_path_survives_scorer_failure_exit_zero(tmp_path, monkeypatch):
    # AC3-ERR: a raising scorer leaves exit 0, the node persisted, and exactly
    # one warning line. Real Typer exit code read in-process via CliRunner (no
    # shell pipe - pitfalls corpus entry 3).
    from typer.testing import CliRunner
    from fno.graph.cli import cli
    import fno.graph.relatedness as rel
    import fno.graph._constants as gc
    import fno.graph.store as gs

    g = tmp_path / "graph.json"
    g.write_text(json.dumps({"entries": []}) + "\n")
    ledger = tmp_path / "ledger.json"
    ledger.write_text('{"entries": []}\n')
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gc, "GRAPH_MD", tmp_path / "graph.md")
    monkeypatch.setattr(gc, "LEDGER_JSON", ledger)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)

    def boom(entry, entries, k=3):
        raise RuntimeError("scorer exploded")

    monkeypatch.setattr(rel, "similar_nodes", boom)

    result = CliRunner().invoke(cli, ["idea", "Backlog dedup gate filings", "--difficulty", "low"])
    assert result.exit_code == 0, result.output
    assert len(json.loads(g.read_text())["entries"]) == 1  # node persisted
    # CliRunner mixes stderr into output; pin the dedup warning text (not just
    # any single warning line) and that it is the only one.
    assert "post-file dedup check skipped" in result.output
    assert result.output.count("warning:") == 1


def test_warn_similar_nodes_includes_archived_nodes(monkeypatch, tmp_path, capsys):
    # codex P2: a shipped-and-archived node is the answer to a duplicate filing,
    # but once `archive --apply` moves it to graph-archive.json the working graph
    # alone no longer sees it. The dedup scan must read the archive too.
    archive = tmp_path / "graph-archive.json"
    archive.write_text(
        json.dumps({"entries": [
            _node("arch1", title="dedup gate for backlog filings", status="done", pr_number=99),
        ]})
        + "\n"
    )
    monkeypatch.setattr("fno.paths.graph_archive_json", lambda: archive)
    # Working graph is empty; the only candidate lives in the archive.
    new = _node("new", title="dedup gate for backlog node filings", status="idea")
    _warn_similar_nodes(new, [], intake_hint=False)
    err = capsys.readouterr().err
    assert "dedup:" in err
    assert "arch1" in err
    assert "done" in err
    assert "PR#99" in err


def test_new_birth_path_warns_on_near_duplicate(tmp_path, monkeypatch):
    # codex P2: `fno new` is a reachable plan-less birth path with its own
    # mutator; it must run the same dedup net as idea/add/intake.
    from typer.testing import CliRunner
    from fno.graph.cli import cli
    import fno.graph._constants as gc

    g = tmp_path / "graph.json"
    g.write_text(
        json.dumps({"entries": [
            _node("ab-d0ne5678", title="Already shipped feature", status="done"),
        ]})
        + "\n"
    )
    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    # Isolate from the real archive so only the working-graph candidate is seen.
    monkeypatch.setattr("fno.paths.graph_archive_json", lambda: tmp_path / "no-archive.json")
    result = CliRunner().invoke(
        cli, ["new", "Already shipped feature refactor", "--unscoped", "--force-domain"],
    )
    assert result.exit_code == 0, result.output
    # CliRunner mixes stderr into output; the receipt names the done candidate.
    assert "dedup:" in result.output
    assert "ab-d0ne5678" in result.output


def test_safe_stderr_warn_swallows_broken_stream(monkeypatch):
    # codex P2: the dedup fallback runs after the node committed, so a
    # closed/broken stderr must not escape and fail the filing.
    from fno.graph.cli import _safe_stderr_warn

    def boom(_msg):
        raise OSError("stderr broken")

    monkeypatch.setattr("sys.stderr.write", boom)
    _safe_stderr_warn("warning: must not escape\n")  # must not raise


def test_entries_with_archive_degrades_when_archive_read_raises(monkeypatch, tmp_path):
    # codex P2: a malformed/unreadable archive must not suppress scoring of the
    # valid working graph; the contract is best-effort degrade.
    from fno.graph import store

    bad_archive = tmp_path / "graph-archive.json"
    bad_archive.write_text('{"entries": []}\n')
    monkeypatch.setattr("fno.paths.graph_archive_json", lambda: bad_archive)

    def boom(_path=None):
        raise RuntimeError("archive unreadable")

    monkeypatch.setattr(store, "read_graph", boom)
    working = [_node("live1", title="some live node", status="idea")]
    assert store.entries_with_archive(working) == working  # degraded, did not raise

"""Design-stage probe: the `design` rung of the derived lifecycle ladder."""
from __future__ import annotations

import pytest

from fno.graph.ladder import is_design_stage


DESIGN_FM = "---\nstatus: design\n---\n\n# Doc\n"


def _plan(tmp_path, body: str, name: str = "p.md") -> dict:
    """A node entry carrying an absolute plan_path (the simplest form)."""
    target = tmp_path / name
    target.write_text(body)
    return {"id": "x-test", "plan_path": str(target)}


def test_relative_plan_path_resolves_against_node_cwd(tmp_path):
    """The majority form on the live graph: repo-relative path + node `cwd`.

    Resolving against the calling process's cwd instead silently no-ops the
    gate for every foreign node - the daemon selects across projects.
    """
    (tmp_path / "plans").mkdir()
    (tmp_path / "plans" / "d.md").write_text(DESIGN_FM)
    entry = {"id": "x-test", "plan_path": "plans/d.md", "cwd": str(tmp_path)}
    assert is_design_stage(entry)


def test_fragment_plan_path_strips_anchor(tmp_path):
    """`<doc>#group-<slug>` paths are not literal filenames."""
    (tmp_path / "d.md").write_text(DESIGN_FM)
    entry = {"id": "x-test", "plan_path": f"{tmp_path / 'd.md'}#group-foo"}
    assert is_design_stage(entry)


def test_tilde_plan_path_expands(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    (tmp_path / "d.md").write_text(DESIGN_FM)
    assert is_design_stage({"id": "x-test", "plan_path": "~/d.md"})


def test_relative_path_without_cwd_stays_armed(tmp_path, monkeypatch):
    """No `cwd` to resolve against: fail open rather than guess."""
    monkeypatch.chdir(tmp_path)
    (tmp_path / "d.md").write_text(DESIGN_FM)
    assert not is_design_stage({"id": "x-test", "plan_path": "nope/d.md"})


def test_folder_plan_stays_armed(tmp_path):
    """A directory plan_path has no frontmatter to read - documented gap."""
    (tmp_path / "planfolder").mkdir()
    assert not is_design_stage({"id": "x-test", "plan_path": str(tmp_path / "planfolder")})


def test_design_frontmatter_is_design_stage(tmp_path):
    assert is_design_stage(_plan(tmp_path, DESIGN_FM))


@pytest.mark.parametrize("status", ["ready", "in_progress", "shipped", "done", "archived"])
def test_blueprinted_and_beyond_are_armed(tmp_path, status):
    assert not is_design_stage(_plan(tmp_path, f"---\nstatus: {status}\n---\n"))


def test_quick_plan_without_execution_strategy_is_armed(tmp_path):
    """`/blueprint quick` omits `## Execution Strategy` by design.

    Probing for that heading (rather than frontmatter) misread every
    quick-plan as unfinished - the regression this test pins.
    """
    body = "---\nstatus: ready\nkind: quick-plan\n---\n\n## Changes\n\n## Verification\n"
    assert not is_design_stage(_plan(tmp_path, body))


def test_quoted_and_cased_status_still_reads_design(tmp_path):
    assert is_design_stage(_plan(tmp_path, "---\nstatus: 'Design'\n---\n"))


@pytest.mark.parametrize(
    "body",
    [
        "# No frontmatter at all\n",
        "---\ntitle: no status key\n---\n",
        "---\nstatus: [unclosed\n",  # malformed YAML
    ],
)
def test_unparseable_plan_stays_armed(tmp_path, body):
    """Fail OPEN: only positive `status: design` evidence demotes a node."""
    assert not is_design_stage(_plan(tmp_path, body))


def test_missing_file_stays_armed(tmp_path):
    """A symlinked vault that is not mounted must never quarantine the backlog."""
    assert not is_design_stage({"id": "x-test", "plan_path": str(tmp_path / "absent.md")})


@pytest.mark.parametrize(
    "entry",
    [
        None,
        "not-an-entry",
        {},                                   # no plan_path (an `idea` node)
        {"plan_path": None},
        {"plan_path": ""},
        {"plan_path": 42},                    # non-string survives the graph's tolerance
    ],
)
def test_malformed_entries_stay_armed(entry):
    assert not is_design_stage(entry)


def test_starvation_receipt_names_design_not_quarantined(tmp_path):
    """A design-stage node is a lifecycle rung, not starvation.

    Reporting it as the generic `quarantined` would read as a stuck node and
    send an operator hunting for a problem that isn't there.
    """
    from datetime import datetime, timezone

    from fno.graph.cli import _starvation_receipts

    plan = tmp_path / "d.md"
    plan.write_text("---\nstatus: design\n---\n")
    node = {
        "id": "x-aaaa",
        "_status": "ready",
        "plan_path": str(plan),
        "created_at": datetime.now(timezone.utc).isoformat(),
    }
    out = _starvation_receipts(
        [node], None, True, None, set(), datetime.now(timezone.utc), 21
    )
    assert out == [("x-aaaa", "design")]

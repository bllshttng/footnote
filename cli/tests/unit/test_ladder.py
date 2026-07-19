"""Design-stage probe: the `design` rung of the derived lifecycle ladder."""
from __future__ import annotations

import pytest

from fno.graph.ladder import is_design_stage


def _plan(tmp_path, body: str, name: str = "p.md") -> str:
    target = tmp_path / name
    target.write_text(body)
    return str(target)


def test_design_frontmatter_is_design_stage(tmp_path):
    assert is_design_stage(_plan(tmp_path, "---\nstatus: design\n---\n\n# Doc\n"))


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
    assert not is_design_stage(str(tmp_path / "absent.md"))


@pytest.mark.parametrize("value", [None, "", 0, [], {"status": "design"}])
def test_non_path_inputs_stay_armed(value):
    assert not is_design_stage(value)

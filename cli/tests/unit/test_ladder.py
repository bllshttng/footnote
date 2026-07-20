"""Design-stage probe: the `design` rung of the derived lifecycle ladder."""
from __future__ import annotations

import os

import pytest

from fno.graph.ladder import is_design_stage


DESIGN_FM = "---\nstatus: design\n---\n\n# Doc\n"


def _now() -> str:
    from datetime import datetime, timezone

    return datetime.now(timezone.utc).isoformat()


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


def test_relative_path_without_cwd_does_not_use_process_cwd(tmp_path, monkeypatch):
    """No `cwd` to resolve against: fail open rather than guess.

    The file deliberately EXISTS at that relative path in the process cwd - an
    earlier cut returned the bare relative path and would have design-gated an
    unrelated node off a coincidentally-matching local doc.
    """
    monkeypatch.chdir(tmp_path)
    (tmp_path / "d.md").write_text(DESIGN_FM)
    assert not is_design_stage({"id": "x-test", "plan_path": "d.md"})


def test_undecodable_plan_stays_armed_without_raising(tmp_path):
    """A binary file at the plan path must not escape as an exception.

    `detect_stale_ready` has no outer catch, so a read error escaping here
    would abort an entire `maintain` run.
    """
    binary = tmp_path / "d.md"
    binary.write_bytes(b"\xff\xfe\x00\x80 not utf-8")
    assert not is_design_stage({"id": "x-test", "plan_path": str(binary)})


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


def test_design_node_is_never_stale_ready(tmp_path):
    """Quarantine must not reach a node that is unarmed on purpose.

    Pinned on `is_stale_ready` itself rather than `detect_stale_ready`, because
    `maintain --apply` re-runs the predicate directly under the lock.
    """
    from datetime import datetime, timedelta, timezone

    from fno.graph.maintain import detect_stale_ready, is_stale_ready

    now = datetime.now(timezone.utc)
    plan = tmp_path / "d.md"
    plan.write_text(DESIGN_FM)
    os.utime(plan, (0, 0))  # ancient mtime: no movement signal
    node = {
        "id": "x-old",
        "_status": "ready",
        "plan_path": str(plan),
        "created_at": (now - timedelta(days=400)).isoformat(),
    }
    assert not is_stale_ready(node, now, 21)
    assert detect_stale_ready([node], 21, now) == []


def test_recompute_persists_the_design_rung(tmp_path):
    """The rung is persisted so every reader sees it, including the Rust mux."""
    from fno.graph.statuses import recompute_statuses

    design = tmp_path / "d.md"
    design.write_text(DESIGN_FM)
    blueprint = tmp_path / "b.md"
    blueprint.write_text("---\nstatus: ready\n---\n")
    entries = [
        {"id": "x-i", "plan_path": None},
        {"id": "x-d", "plan_path": str(design)},
        {"id": "x-r", "plan_path": str(blueprint)},
        {"id": "x-p", "plan_path": str(design), "locked_by": "w", "claimed_at": _now()},
    ]
    got = {e["id"]: e["_status"] for e in recompute_statuses(entries)}
    assert got == {"x-i": "idea", "x-d": "design", "x-r": "ready", "x-p": "in_progress"}


def test_legacy_claimed_status_migrates_on_read(tmp_path):
    """A row persisted before the rename still reads as the current vocabulary."""
    from fno.graph.store import _apply_graph_defaults

    entries = _apply_graph_defaults([{"id": "x-a", "_status": "claimed"}])
    assert entries[0]["_status"] == "in_progress"


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

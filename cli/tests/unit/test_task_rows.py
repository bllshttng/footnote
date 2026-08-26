"""Task-grain rows and the task claim on status transition (epic x-09d7, group 3).

Acceptance contract: a task row transitions to in_progress AND the claim
lockfile for that exact task key exists; a second claimant is refused BY NAME;
the claim is released on completion; a give-back returns the row to pending;
an over-long task key is refused at validation, never truncated into a
colliding filename. Every assertion names a POSITIVE marker - the lockfile
path, the holder string, the archived corpse under .expired/ - never a bare
absence, and each refusal test carries a positive control proving the same
instrument succeeds on the healthy path.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest
from typer.testing import CliRunner

from fno.claims.core import claim_status
from fno.claims.io import claim_path, encode_key
from fno.claims.tasks import task_key

runner = CliRunner()

SID_A = "2782a6e1-aaaa-4bbb-8ccc-000000000001"
SID_B = "2782a6e1-aaaa-4bbb-8ccc-000000000002"

PLAN_TMPL = """---
title: rows
status: ready
---

# rows

## Execution Strategy

```yaml
tasks:
  - id: "1.1"
    title: first
    surface: []
    acceptance: ["ac"]
  - id: "1.2"
    title: second
    surface: []
    acceptance: ["ac"]
{extra}
```
"""


def _plan_with(tmp_path: Path, extra: str = "") -> Path:
    plan = tmp_path / "plan.md"
    plan.write_text(PLAN_TMPL.format(extra=extra), encoding="utf-8")
    return plan


@pytest.fixture
def claims_root(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Hermetic claims root: every claim read/write lands under tmp_path."""
    root = tmp_path / "claims"
    monkeypatch.setenv("FNO_CLAIMS_ROOT", str(root))
    return root


@pytest.fixture
def tmp_graph(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, claims_root: Path) -> Path:
    """A two-node scratch graph: one with a bound plan, one without."""
    plan = _plan_with(tmp_path)
    g = tmp_path / "graph.json"
    g.write_text(
        json.dumps(
            {
                "entries": [
                    {
                        "id": "x-t1",
                        "slug": "task-rows",
                        "status": "ready",
                        "title": "rows",
                        "plan_path": str(plan),
                    },
                    {"id": "x-t2", "slug": "no-plan", "status": "ready", "title": "np"},
                ]
            }
        )
        + "\n",
        encoding="utf-8",
    )
    import fno.graph._constants as gc
    import fno.graph.store as gs

    monkeypatch.setattr(gc, "GRAPH_JSON", g)
    monkeypatch.setattr(gs, "GRAPH_JSON", g)
    monkeypatch.setattr("fno.paths.graph_json", lambda: g)
    monkeypatch.setattr("fno.paths.graph_archive_json", lambda: tmp_path / "ga.json")
    return g


def _node_row(graph: Path, node_id: str, task_id: str) -> dict:
    entries = json.loads(graph.read_text(encoding="utf-8"))["entries"]
    node = next(e for e in entries if e.get("id") == node_id)
    return next(r for r in node["tasks"] if r["id"] == task_id)


def _live_pid() -> int:
    """This test process's pid: provably live for the claim's duration."""
    return os.getpid()


def _dead_pid() -> int:
    """A pid that was alive and is now gone (dead-pid recovery input)."""
    proc = subprocess.Popen(["/bin/sleep", "0.01"])
    proc.wait()
    return proc.pid


def _task_update(monkeypatch, pid: int, *args: str):
    from fno.graph import cli as graph_cli

    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_pid", lambda *a, **k: pid
    )
    return runner.invoke(graph_cli.task_app, ["update", *args])


# -- AC1: rows materialize from the plan --


def test_task_list_materializes_pending_rows(tmp_graph: Path):
    """AC1-HP: first read writes a pending ownerless row per plan task."""
    from fno.graph import cli as graph_cli

    result = runner.invoke(
        graph_cli.task_app, ["list", "x-t1", "--json"]
    )
    assert result.exit_code == 0, result.output
    rows = json.loads(result.output)["tasks"]
    assert rows == [
        {"id": "1.1", "status": "pending", "owner": None},
        {"id": "1.2", "status": "pending", "owner": None},
    ]
    # Positive persistence marker: the rows are in the graph file, not just
    # the echoed payload.
    row = _node_row(tmp_graph, "x-t1", "1.1")
    assert row["status"] == "pending" and row["owner"] is None


def test_task_list_no_plan_refuses_and_writes_nothing(
    tmp_graph: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC1-ERR: no plan bound -> exit 2, message names the node, graph intact.

    The byte-identical check is only half the proof; the paired positive
    control (a node WITH a plan gets rows written by the same verb) proves the
    instrument was willing to write, so the untouched bytes mean refusal, not a
    no-op instrument.
    """
    from fno.graph import cli as graph_cli

    before = tmp_graph.read_text(encoding="utf-8")
    result = runner.invoke(graph_cli.task_app, ["list", "x-t2"])
    assert result.exit_code == 2
    assert "no plan bound to x-t2" in result.output
    assert tmp_graph.read_text(encoding="utf-8") == before

    ok = runner.invoke(graph_cli.task_app, ["list", "x-t1", "--json"])
    assert ok.exit_code == 0, ok.output
    assert json.loads(ok.output)["tasks"], "positive control: the verb does write rows"


def test_unknown_task_id_exits_2_naming_plan_ids(
    tmp_graph: Path, monkeypatch: pytest.MonkeyPatch
):
    from fno.graph import cli as graph_cli

    result = _task_update(
        monkeypatch, _live_pid(), "x-t1", "9.9", "--status", "in_progress",
        "--owner", SID_A,
    )
    assert result.exit_code == 2
    assert "task '9.9' not in plan" in result.output
    assert "1.1" in result.output and "1.2" in result.output, "names the plan's ids"


# -- AC2: the claim IS the transition --


def test_in_progress_claims_and_second_claimant_refused_by_name(
    tmp_graph: Path, claims_root: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC2-HP + AC2-ERR: transition writes row + lockfile; peer exit 3 names A."""
    key = task_key("x-t1", "1.1")
    lock = claim_path(key, root=claims_root)

    got = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    )
    assert got.exit_code == 0, got.output
    # Positive markers: the exact task key's lockfile exists and carries A.
    assert lock.exists(), f"claim lockfile for {key} must exist"
    status = claim_status(key, root=claims_root)
    assert status["holder"] == SID_A
    assert status["state"] in ("live", "suspect")
    row = _node_row(tmp_graph, "x-t1", "1.1")
    assert row == {
        "id": "1.1",
        "status": "in_progress",
        "owner": SID_A,
        "claimed_at": row["claimed_at"],
    } and row["claimed_at"], "row carries status, owner, claimed_at"

    refused = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_B,
    )
    assert refused.exit_code == 3
    assert SID_A in refused.output, "refusal names the holder"
    # The row and the lockfile still belong to A (re-read, not absence).
    assert _node_row(tmp_graph, "x-t1", "1.1")["owner"] == SID_A
    assert claim_status(key, root=claims_root)["holder"] == SID_A


def test_in_progress_without_session_id_exits_4(
    tmp_graph: Path, monkeypatch: pytest.MonkeyPatch
):
    """Exit 4 (not a silent claim under an unprovable holder)."""
    from types import SimpleNamespace

    from fno.graph import cli as graph_cli

    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda *a, **k: SimpleNamespace(session_id=None, harness="claude"),
    )
    result = runner.invoke(
        graph_cli.task_app,
        ["update", "x-t1", "1.1", "--status", "in_progress"],
    )
    assert result.exit_code == 4
    assert "pass --owner <full-session-id>" in result.output


def test_unprovable_pid_refuses_rather_than_degrading(
    tmp_graph: Path, claims_root: Path, monkeypatch: pytest.MonkeyPatch
):
    """A pid-less acquire would anchor to the dying CLI process and leave the
    claim instantly stealable; the verb refuses (exit 4) and writes nothing."""
    result = _task_update(
        monkeypatch, None, "x-t1", "1.1", "--status", "in_progress", "--owner", SID_A,
    )
    assert result.exit_code == 4
    assert "FNO_SESSION_PID" in result.output
    # Positive absence proof: the healthy pid on the SAME path creates the
    # lockfile, so the instrument ran and the refusal was the pid guard.
    ok = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    )
    assert ok.exit_code == 0, ok.output
    assert claim_path(task_key("x-t1", "1.1"), root=claims_root).exists()


def test_claim_contention_exits_3_within_the_waves_contract(
    tmp_graph: Path, monkeypatch: pytest.MonkeyPatch
):
    """ClaimContended (recovery-mutex exhaustion) must not escape as a
    traceback; exit 3 keeps the documented 0/3/4 contract for the waves flow."""
    from fno.claims.core import ClaimContended

    def boom(*a, **k):
        raise ClaimContended("recovery mutex busy")

    monkeypatch.setattr("fno.claims.tasks.acquire_task", boom)
    result = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    )
    assert result.exit_code == 3
    assert "contention" in result.output


def test_unreadable_plan_is_a_named_refusal(
    tmp_path: Path, tmp_graph: Path, monkeypatch: pytest.MonkeyPatch
):
    """A stale plan_path (file gone) exits 1 naming the path, never a
    FileNotFoundError traceback; the graph is untouched."""
    from fno.graph import cli as graph_cli

    entries = json.loads(tmp_graph.read_text(encoding="utf-8"))["entries"]
    entries[0]["plan_path"] = str(tmp_path / "gone.md")
    tmp_graph.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    before = tmp_graph.read_text(encoding="utf-8")

    result = runner.invoke(graph_cli.task_app, ["list", "x-t1"])
    assert result.exit_code == 1
    assert "not readable" in result.output and "gone.md" in result.output
    assert tmp_graph.read_text(encoding="utf-8") == before


def test_second_list_read_is_read_only(
    tmp_graph: Path, monkeypatch: pytest.MonkeyPatch
):
    """Once every plan id has a row, listing re-prints them without a graph
    write: the file content is byte-identical after the steady-state read."""
    from fno.graph import cli as graph_cli

    first = runner.invoke(graph_cli.task_app, ["list", "x-t1", "--json"])
    assert first.exit_code == 0, first.output
    settled = tmp_graph.read_text(encoding="utf-8")

    second = runner.invoke(graph_cli.task_app, ["list", "x-t1", "--json"])
    assert second.exit_code == 0, second.output
    assert json.loads(second.output) == json.loads(first.output)
    assert tmp_graph.read_text(encoding="utf-8") == settled


# -- AC3: dead-pid recovery --


def test_dead_holder_is_archived_and_reclaimed(
    tmp_graph: Path, claims_root: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC3-HP: a dead-pid claim is swept to .expired/ and B takes the task."""
    key = task_key("x-t1", "1.1")
    dead = _dead_pid()

    got = _task_update(
        monkeypatch, dead, "x-t1", "1.1", "--status", "in_progress", "--owner", SID_A,
    )
    assert got.exit_code == 0, got.output
    assert claim_status(key, root=claims_root)["holder"] == SID_A

    taken = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_B,
    )
    assert taken.exit_code == 0, taken.output
    # Positive markers: B owns row + live claim, and A's corpse is archived.
    assert _node_row(tmp_graph, "x-t1", "1.1")["owner"] == SID_B
    assert claim_status(key, root=claims_root)["holder"] == SID_B
    expired_dir = claim_path(key, root=claims_root).parent / ".expired"
    archived = list(expired_dir.glob(f"{encode_key(key)}.*.lock"))
    assert archived, f"the dead claim must be archived under {expired_dir}"


# -- completion and give-back --


def test_done_releases_the_claim(
    tmp_graph: Path, claims_root: Path, monkeypatch: pytest.MonkeyPatch
):
    _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    )
    key = task_key("x-t1", "1.1")

    done = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "done", "--owner", SID_A,
    )
    assert done.exit_code == 0, done.output
    assert _node_row(tmp_graph, "x-t1", "1.1")["status"] == "done"
    # Positive verdict from the instrument: state reads free, file gone.
    assert claim_status(key, root=claims_root)["state"] == "free"
    assert not claim_path(key, root=claims_root).exists()


def test_pending_give_back_is_holder_only(
    tmp_graph: Path, claims_root: Path, monkeypatch: pytest.MonkeyPatch
):
    """AC4-ERR shape at the verb level: give-back by the holder returns the
    task to pending with no owner and frees the claim; anyone else exits 3."""
    _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    )
    key = task_key("x-t1", "1.1")

    stranger = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "pending", "--owner", SID_B,
    )
    assert stranger.exit_code == 3
    assert SID_A in stranger.output

    holder = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "pending", "--owner", SID_A,
    )
    assert holder.exit_code == 0, holder.output
    row = _node_row(tmp_graph, "x-t1", "1.1")
    assert row["status"] == "pending" and row["owner"] is None
    assert "claimed_at" not in row
    assert claim_status(key, root=claims_root)["state"] == "free"


# -- key validation: refused, never truncated --


def test_done_row_cannot_be_reopened(
    tmp_graph: Path, claims_root: Path, monkeypatch: pytest.MonkeyPatch
):
    """A finished task is shipped work: the in_progress transition refuses
    (exit 3) instead of re-dispatching it from stale per-checkout state."""
    _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    )
    _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "done", "--owner", SID_A,
    )

    reopened = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_B,
    )
    assert reopened.exit_code == 3
    assert "is done" in reopened.output
    # Positive markers: the row stays done and no claim is left behind.
    assert _node_row(tmp_graph, "x-t1", "1.1")["status"] == "done"
    assert claim_status(task_key("x-t1", "1.1"), root=claims_root)["state"] == "free"


def test_done_by_non_holder_refused(
    tmp_graph: Path, claims_root: Path, monkeypatch: pytest.MonkeyPatch
):
    """A peer cannot mark a live worker's task done: the row is holder-guarded
    like the give-back, so the board never reports in-flight work finished."""
    _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    )

    stranger = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "done", "--owner", SID_B,
    )
    assert stranger.exit_code == 3
    assert SID_A in stranger.output
    row = _node_row(tmp_graph, "x-t1", "1.1")
    assert row["status"] == "in_progress" and row["owner"] == SID_A


def test_exited_graph_write_releases_the_claim(
    tmp_graph: Path, claims_root: Path, monkeypatch: pytest.MonkeyPatch
):
    """locked_mutate_graph sys.exit()s (does not raise) on a corrupt graph;
    the transition must release the claim on that path too, or it stays held
    by the long-lived session pid until the whole session dies."""
    import fno.graph.store as store

    def _wedge(*a, **k):
        raise SystemExit(1)

    monkeypatch.setattr(store, "locked_mutate_graph", _wedge)
    result = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    )
    assert result.exit_code == 1
    assert claim_status(task_key("x-t1", "1.1"), root=claims_root)["state"] == "free"


def test_malformed_plan_is_a_named_refusal(
    tmp_path: Path, tmp_graph: Path
):
    """A broken Execution Strategy fence refuses by name, never a parser
    traceback (the stop-not-traceback contract of the task verbs)."""
    from fno.graph import cli as graph_cli

    plan = tmp_path / "broken.md"
    plan.write_text(
        "---\ntitle: broken\nstatus: ready\n---\n\n# broken\n\n"
        "## Execution Strategy\n\n```yaml\ntasks: [oops\n```\n",
        encoding="utf-8",
    )
    entries = json.loads(tmp_graph.read_text(encoding="utf-8"))["entries"]
    entries[0]["plan_path"] = str(plan)
    tmp_graph.write_text(json.dumps({"entries": entries}), encoding="utf-8")

    result = runner.invoke(graph_cli.task_app, ["list", "x-t1"])
    assert result.exit_code == 1
    assert "plan parse failed" in result.output
    assert "Traceback" not in result.output


def test_idless_plan_poll_never_takes_the_lock(
    tmp_path: Path, tmp_graph: Path
):
    """A plan with no Execution Strategy and no rows exits 2 WITHOUT the
    locked write: a polling fleet must not churn the graph mutation pipeline
    on every poll of a task-less node."""
    from fno.graph import cli as graph_cli

    plan = tmp_path / "empty.md"
    plan.write_text(
        "---\ntitle: empty\nstatus: ready\n---\n\n# empty\n", encoding="utf-8"
    )
    entries = json.loads(tmp_graph.read_text(encoding="utf-8"))["entries"]
    entries[0]["plan_path"] = str(plan)
    tmp_graph.write_text(json.dumps({"entries": entries}), encoding="utf-8")
    before = tmp_graph.read_text(encoding="utf-8")

    result = runner.invoke(graph_cli.task_app, ["list", "x-t1"])
    assert result.exit_code == 2
    assert "no tasks declared" in result.output
    assert tmp_graph.read_text(encoding="utf-8") == before


def test_overlong_task_key_refused_at_validation(
    tmp_path: Path, tmp_graph: Path, claims_root: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """A plan task id whose key exceeds MAX_KEY_LENGTH is refused (exit 2)
    with no lockfile; the sibling normal task through the SAME verb creates
    its lockfile, proving validation ran rather than the call being skipped."""
    plan = tmp_path / "plan.md"
    huge_id = "z" * 260  # task:x-t1:<260 chars> > MAX_KEY_LENGTH (256)
    plan.write_text(
        PLAN_TMPL.format(
            extra=f'  - id: "{huge_id}"\n    title: huge\n    surface: []\n'
            '    acceptance: ["ac"]'
        ),
        encoding="utf-8",
    )
    bad_lock = claim_path(task_key("x-t1", huge_id), root=claims_root)

    refused = _task_update(
        monkeypatch, _live_pid(), "x-t1", huge_id, "--status", "in_progress",
        "--owner", SID_A,
    )
    assert refused.exit_code == 2
    assert "exceeds" in refused.output
    assert not bad_lock.exists(), "a refused key must leave no lockfile"

    ok = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    )
    assert ok.exit_code == 0, ok.output
    assert claim_path(task_key("x-t1", "1.1"), root=claims_root).exists(), (
        "positive control: the same verb creates the healthy key's lockfile"
    )


# -- review round 3: plan_path forms, done-row give-back, graph-unreadable exit --


def _rebind_plan(graph: Path, node_id: str, plan_path: str) -> None:
    """Rewrite one node's stored plan_path, leaving the rest of the graph."""
    data = json.loads(graph.read_text(encoding="utf-8"))
    for e in data["entries"]:
        if e.get("id") == node_id:
            e["plan_path"] = plan_path
    graph.write_text(json.dumps(data) + "\n", encoding="utf-8")


def test_tilde_plan_path_resolves(
    tmp_graph: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """A `~`-prefixed plan_path is a form the graph really stores.

    Read raw through Path() it is a relative directory named "~", so the verb
    refused exit 1 and the caller dispatched unclaimed. The assertion is the
    materialized row list, a marker only a resolved read produces.
    """
    from fno.graph import cli as graph_cli

    home = tmp_path / "home"
    home.mkdir()
    (home / "plan.md").write_text(
        (tmp_path / "plan.md").read_text(encoding="utf-8"), encoding="utf-8"
    )
    monkeypatch.setenv("HOME", str(home))
    _rebind_plan(tmp_graph, "x-t1", "~/plan.md")

    result = runner.invoke(graph_cli.task_app, ["list", "x-t1", "--json"])
    assert result.exit_code == 0, result.output
    assert [r["id"] for r in json.loads(result.output)["tasks"]] == ["1.1", "1.2"]


def test_unreadable_plan_still_refuses(tmp_graph: Path):
    """Positive control for the test above: resolution did not turn the
    unreadable-plan refusal into a silent pass."""
    from fno.graph import cli as graph_cli

    _rebind_plan(tmp_graph, "x-t1", "~/no-such-plan.md")
    result = runner.invoke(graph_cli.task_app, ["list", "x-t1", "--json"])
    assert result.exit_code == 1
    assert "is not readable" in result.output


def test_give_back_refuses_a_done_row(
    tmp_graph: Path, claims_root: Path, monkeypatch: pytest.MonkeyPatch
):
    """`done` leaves `owner` set, so the holder's own later give-back passed
    the owner check and reopened shipped work. It is refused by status."""
    assert _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "in_progress",
        "--owner", SID_A,
    ).exit_code == 0
    assert _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "done", "--owner", SID_A
    ).exit_code == 0

    refused = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.1", "--status", "pending",
        "--owner", SID_A,
    )
    assert refused.exit_code == 3
    assert "is done" in refused.output
    assert _node_row(tmp_graph, "x-t1", "1.1")["status"] == "done"

    ok = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.2", "--status", "in_progress",
        "--owner", SID_A,
    )
    assert ok.exit_code == 0, ok.output
    given = _task_update(
        monkeypatch, _live_pid(), "x-t1", "1.2", "--status", "pending",
        "--owner", SID_A,
    )
    assert given.exit_code == 0, (
        "positive control: the same verb still gives a live row back"
    )
    assert _node_row(tmp_graph, "x-t1", "1.2")["status"] == "pending"


def test_unreadable_graph_does_not_collide_with_held(tmp_graph: Path):
    """A corrupt graph must not exit 3, which waves.md 3e reads as "a peer
    holds it, skip this round" - that skips every task and names a holder
    that does not exist."""
    from fno.graph import cli as graph_cli

    tmp_graph.write_text("{ not json", encoding="utf-8")
    result = runner.invoke(graph_cli.task_app, ["list", "x-t1", "--json"])
    assert result.exit_code == graph_cli.TASK_GRAPH_UNREADABLE_EXIT
    assert result.exit_code != 3, "3 means a peer holds the task"


def test_duplicate_plan_ids_make_one_row(tmp_path: Path):
    """Every row writer breaks on the first id match, so a second row for the
    same id is orphaned at pending forever. Reachable: PyYAML reads
    `id: 2.10` as the float 2.1, colliding with 2.1."""
    from fno.graph.tasks import ensure_task_rows

    plan = _plan_with(
        tmp_path,
        extra='  - id: "1.1"\n    title: dupe\n    surface: []\n'
        '    acceptance: ["ac"]',
    )
    entry: dict = {"id": "x-t1"}
    rows = ensure_task_rows(entry, plan)
    assert [r["id"] for r in rows] == ["1.1", "1.2"]

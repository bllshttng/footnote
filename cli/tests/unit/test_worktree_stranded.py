"""The three-way stranded-worktree classifier.

`classify()` is pure, so the fixture table drives it directly with no git or
subprocess involved. The one thing that must NOT be pure-fixture-tested is
the unsound-probe regression: a prior sweep proved that a branch existing on
origin at an OLDER sha reads as healthy under a name-existence check, so
that one drives a real temp git repo the same way test_worktree_reapable.py
does, exercising the actual `wt_unpushed_count` shell function classify()'s
git input comes from.
"""
import json
import subprocess
from pathlib import Path

import pytest

from fno.worktree_stranded import (
    ABANDONED,
    CLEAN,
    LIVE,
    PR_OPEN,
    SHIPPED,
    STRANDED,
    UNKNOWN,
    Row,
    _unpushed_batch,
    _worktrees,
    act_on_stranded,
    apply_sweep,
    classify,
    record_unknown,
    resolve_node_id,
)

REPO_ROOT = Path(__file__).resolve().parents[3]


def _base_kwargs(**overrides) -> dict:
    kwargs = dict(
        path="/wt/x-abcd",
        branch="feature/x-abcd",
        unpushed=3,
        unpushed_ok=True,
        node="x-abcd",
        node_entry={"status": "ready"},
        graph_ok=True,
        registry_status=None,
        registry_ok=True,
    )
    kwargs.update(overrides)
    return kwargs


# --- the seven classes, fixture table ---------------------------------


@pytest.mark.parametrize(
    "overrides,expected",
    [
        pytest.param({"unpushed": 0}, CLEAN, id="clean-zero-unpushed"),
        pytest.param({"node": None, "node_entry": None}, UNKNOWN, id="unknown-node-unresolved"),
        pytest.param(
            {"node_entry": {"status": "done"}}, SHIPPED, id="shipped-done-status"
        ),
        pytest.param(
            {"node_entry": {"status": "superseded"}}, ABANDONED, id="abandoned-superseded"
        ),
        pytest.param(
            {"node_entry": {"status": "deferred"}}, ABANDONED, id="abandoned-deferred"
        ),
        pytest.param({"registry_status": "busy"}, LIVE, id="live-busy"),
        pytest.param({"registry_status": "idle"}, LIVE, id="live-idle"),
        pytest.param(
            {"node_entry": {"status": "ready", "pr_number": 42}}, PR_OPEN, id="pr-open"
        ),
        pytest.param({}, STRANDED, id="stranded-otherwise"),
    ],
)
def test_classify_seven_classes(overrides, expected):
    row = classify(**_base_kwargs(**overrides))
    assert row.klass == expected


def test_live_outranks_pr_open():
    """The epic king's correction: a live-fleet row must win even when the
    same node also carries an open PR, or a minutes-old branch could get
    acted on because PR_OPEN was checked first."""
    row = classify(**_base_kwargs(registry_status="busy", node_entry={"status": "ready", "pr_number": 7}))
    assert row.klass == LIVE


# --- both fail-open paths -----------------------------------------------


@pytest.mark.parametrize(
    "overrides",
    [
        pytest.param({"unpushed_ok": False}, id="git-read-failed"),
        pytest.param({"graph_ok": False}, id="graph-read-failed"),
        pytest.param({"registry_ok": False}, id="fleet-read-failed"),
    ],
)
def test_any_failed_input_is_unknown(overrides):
    row = classify(**_base_kwargs(**overrides))
    assert row.klass == UNKNOWN
    assert "read failed" in row.facts["reason"]


def test_unresolved_node_is_unknown_not_stranded():
    row = classify(**_base_kwargs(node=None, node_entry=None))
    assert row.klass == UNKNOWN


def test_resolved_id_with_no_graph_row_is_unknown():
    """A directory/branch shaped like a node id whose id no longer exists in
    the graph (deleted, archived) must not fall through to STRANDED."""
    row = classify(**_base_kwargs(node="x-ghost", node_entry=None))
    assert row.klass == UNKNOWN


# --- node resolution: each of the three sources in isolation ------------


def test_resolve_by_directory_basename():
    entries = {"x-ab12": {"id": "x-ab12", "status": "ready"}}
    node, entry = resolve_node_id("/repo/.claude/worktrees/x-ab12", "unrelated-branch-name", entries)
    assert node == "x-ab12"
    assert entry == entries["x-ab12"]


def test_resolve_by_branch_when_basename_misses():
    entries = {"x-50a6": {"id": "x-50a6", "status": "ready"}}
    node, entry = resolve_node_id("/repo/some-unrelated-dir", "feature/x-50a6", entries)
    assert node == "x-50a6"
    assert entry == entries["x-50a6"]


def test_resolve_by_state_file_when_dir_and_branch_miss(tmp_path):
    entries = {"x-9ab2": {"id": "x-9ab2", "status": "ready"}}
    state_dir = tmp_path / ".fno"
    state_dir.mkdir()
    (state_dir / "target-state.md").write_text("---\nsome: frontmatter\n---\ngraph_node_id: x-9ab2\n")
    node, entry = resolve_node_id(str(tmp_path), "some-unrelated-branch", entries)
    assert node == "x-9ab2"
    assert entry == entries["x-9ab2"]


def test_resolve_state_file_null_is_unresolved(tmp_path):
    state_dir = tmp_path / ".fno"
    state_dir.mkdir()
    (state_dir / "target-state.md").write_text("graph_node_id: null\n")
    node, entry = resolve_node_id(str(tmp_path), None, {})
    assert node is None
    assert entry is None


def test_resolve_nothing_matches():
    node, entry = resolve_node_id("/repo/nope", None, {"x-1": {"id": "x-1"}})
    assert node is None
    assert entry is None


def test_worktrees_excludes_a_bare_entry(monkeypatch):
    """A code-review finding: a `bare` porcelain entry (the main admin
    directory of a bare-repo-as-worktree-container setup) carries neither a
    `branch` nor a `detached` line, so without an explicit check it falls
    through the same path as a real detached worktree and misreports as
    one. It has no working tree to inspect and must not appear at all -
    matching the pre-detached-fix behavior, which also never emitted a row
    for it."""
    porcelain = (
        "worktree /repo/bare-admin\nbare\n\n"
        "worktree /repo/normal\nHEAD abc123\nbranch refs/heads/main\n\n"
        "worktree /repo/detached-wt\nHEAD def456\ndetached\n"
    )

    def fake_run(args, **kwargs):
        return subprocess.CompletedProcess(args, 0, stdout=porcelain, stderr="")

    monkeypatch.setattr("fno.worktree_stranded.subprocess.run", fake_run)

    rows = _worktrees(Path("/repo"))
    assert rows == [("main", "/repo/normal"), (None, "/repo/detached-wt")]


# --- UNKNOWN never pushes -------------------------------------------------


def test_unknown_row_never_pushes(monkeypatch):
    calls = []

    def fake_run(args, **kwargs):
        calls.append(args)
        return subprocess.CompletedProcess(args, 0, stdout="{}", stderr="")

    monkeypatch.setattr("fno.worktree_stranded.subprocess.run", fake_run)

    row = Row(UNKNOWN, "x-abcd", 3, "1 hour ago", {"path": "/wt/x-abcd", "branch": "feature/x-abcd", "reason": "test"})
    outcome = record_unknown(row)

    assert outcome["acts"] == [{"act": "event_emit", "ok": True}]
    assert not any("push" in args for args in calls)


def test_apply_sweep_only_acts_on_stranded_and_unknown(monkeypatch):
    calls = []

    def fake_run(args, **kwargs):
        calls.append(args)
        return subprocess.CompletedProcess(args, 0, stdout="{}", stderr="")

    monkeypatch.setattr("fno.worktree_stranded.subprocess.run", fake_run)

    rows = [
        Row(CLEAN, "x-1", 0, "now", {"path": "/a", "branch": "b1"}),
        Row(SHIPPED, "x-2", 5, "now", {"path": "/b", "branch": "b2"}),
        Row(LIVE, "x-3", 5, "now", {"path": "/c", "branch": "b3"}),
        Row(UNKNOWN, "x-4", 5, "now", {"path": "/d", "branch": "b4", "reason": "r"}),
    ]
    outcomes = apply_sweep(rows)

    assert len(outcomes) == 1
    assert outcomes[0]["node"] == "x-4"
    assert not any("push" in args for args in calls)


def test_apply_sweep_wake_false_still_records_unknown_but_skips_stranded(monkeypatch):
    """A code-review finding: config.recovery.watchdog="report" must still
    classify (so counts stay honest) but never push or file - the same
    wake vs report split the fleet watchdog leg draws at apply_verdict.
    Recording UNKNOWN is reporting, not acting, so it stays unconditional."""
    calls = []

    def fake_run(args, **kwargs):
        calls.append(args)
        return subprocess.CompletedProcess(args, 0, stdout="{}", stderr="")

    monkeypatch.setattr("fno.worktree_stranded.subprocess.run", fake_run)

    rows = [
        Row(STRANDED, "x-5", 5, "now", {"path": "/e", "branch": "b5"}),
        Row(UNKNOWN, "x-4", 5, "now", {"path": "/d", "branch": "b4", "reason": "r"}),
    ]
    outcomes = apply_sweep(rows, wake=False)

    assert len(outcomes) == 1
    assert outcomes[0]["node"] == "x-4"
    assert not any("push" in args for args in calls)


def test_act_on_stranded_stops_at_first_failure(monkeypatch):
    """A failed push must never reach the backlog-update or event acts -
    the next tick retries the whole row from scratch."""
    calls = []

    def fake_run(args, **kwargs):
        calls.append(args)
        if "push" in args:
            return subprocess.CompletedProcess(args, 1, stdout="", stderr="remote rejected")
        return subprocess.CompletedProcess(args, 0, stdout="{}", stderr="")

    monkeypatch.setattr("fno.worktree_stranded.subprocess.run", fake_run)

    row = Row(STRANDED, "x-abcd", 3, "1 hour ago", {"path": "/wt/x-abcd", "branch": "feature/x-abcd"})
    outcome = act_on_stranded(row)

    assert outcome["stopped_at"] == "push"
    assert len(outcome["acts"]) == 1
    assert not any("backlog" in args for args in calls)


# --- regression: the unsound name-existence probe -----------------------


def _git(cwd: Path, *args: str) -> str:
    r = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    assert r.returncode == 0, f"git {' '.join(args)} failed: {r.stderr}"
    return r.stdout


def test_remote_branch_at_older_sha_still_reads_unpushed(tmp_path):
    """A branch existing on origin is not proof the local HEAD's commits
    are on it. A branch-name-exists probe reads this worktree as healthy;
    the sound probe (rev-list --not --remotes, what classify()'s unpushed
    input actually comes from) must not."""
    remote = tmp_path / "remote.git"
    subprocess.run(["git", "init", "--bare", "-q", str(remote)], check=True)

    work = tmp_path / "work"
    work.mkdir()
    _git(work, "init", "-q")
    _git(work, "config", "user.email", "t@t.co")
    _git(work, "config", "user.name", "t")
    _git(work, "remote", "add", "origin", str(remote))
    (work / "f.txt").write_text("one\n")
    _git(work, "add", "f.txt")
    _git(work, "commit", "-q", "-m", "first")
    _git(work, "push", "-q", "origin", "HEAD:refs/heads/feature/x-ab12")

    # Local moves ahead; origin/feature/x-ab12 stays at the older sha.
    (work / "f.txt").write_text("two\n")
    _git(work, "add", "f.txt")
    _git(work, "commit", "-q", "-m", "second, never pushed")

    counts = _unpushed_batch([str(work)])
    count, ok, age = counts[str(work)]
    assert ok is True
    assert count > 0
    assert age != ""


def test_worktree_stranded_never_imports_resolve_repo_root():
    """A code-review finding, confirmed by three independent finder angles:
    the script's own path must never come from resolve_repo_root(), a
    process-cached, cwd-dependent read with no idea which of possibly many
    swept repos (pr-watch's multi-repo tick loop) is in play. The module no
    longer imports it at all - repo resolution for the interactive default
    (worktree_cli/cli.py's `stranded` command) is the CLI layer's job, not
    this module's, so there is nothing here left to decoy."""
    import fno.worktree_stranded as ws

    assert not hasattr(ws, "resolve_repo_root")


def test_unpushed_batch_ignores_cwd(tmp_path, monkeypatch):
    """Point cwd at garbage and confirm the batch still finds the real
    script (resolved from this module's own __file__, never cwd) and runs."""
    remote = tmp_path / "remote.git"
    subprocess.run(["git", "init", "--bare", "-q", str(remote)], check=True)
    work = tmp_path / "work"
    work.mkdir()
    _git(work, "init", "-q")
    _git(work, "config", "user.email", "t@t.co")
    _git(work, "config", "user.name", "t")
    _git(work, "remote", "add", "origin", str(remote))
    (work / "f.txt").write_text("one\n")
    _git(work, "add", "f.txt")
    _git(work, "commit", "-q", "-m", "unpushed")

    decoy = tmp_path / "decoy"
    decoy.mkdir()
    monkeypatch.chdir(decoy)

    counts = _unpushed_batch([str(work)])
    count, ok, _age = counts[str(work)]
    assert ok is True
    assert count > 0

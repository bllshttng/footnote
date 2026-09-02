"""Worktree pruning in ``fno agents rm``, through the reapable gate only.

(x-d545) A human removed ONE named row: its worktree goes with it, but a row
removal must never become a fourth door around the three buckets in
``.claude/rules/worktrees.md``. Every decision routes through the
``worktree_reapable`` classifier (the verb ``fno agents workspace worktree
reapable`` serves), and the row is removed either way.

ACs:
- AC5-HP : a gate-clean worktree is removed; the receipt names it.
- AC6-EDGE: a gate-refused worktree survives untouched, the receipt names the
           path and the gate's reason, and the ROW is still removed.
- AC7-ERR: a probe that cannot answer keeps the tree; the receipt says so.
- AC2-HP : a row with no linked worktree prunes nothing and rm succeeds.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from fno.agents import dispatch as dispatch_mod
from fno.agents.dispatch import rm_agent
from fno.agents.registry import load_registry, update_registry
from fno.worktree_reapable import Verdict


@pytest.fixture
def isolated_state(tmp_path: Path, monkeypatch) -> Path:
    from fno import paths

    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setattr(paths, "agents_registry_path", lambda: tmp_path / "registry.jsonl")
    monkeypatch.setattr(paths, "state_dir", lambda: tmp_path / "state")
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    return tmp_path


def _linked_worktree(tmp_path: Path, name: str) -> Path:
    """A dir with the linked-worktree shape (a `.git` FILE), no real repo."""
    wt = tmp_path / "worktrees" / name
    wt.mkdir(parents=True)
    (wt / ".git").write_text("gitdir: /elsewhere/x.git\n", encoding="utf-8")
    return wt


def _entry(name: str, cwd: Path, tmp_path: Path):
    from fno.agents.registry import AgentEntry

    return AgentEntry(
        name=name,
        harness="claude",
        cwd=str(cwd),
        log_path=str(tmp_path / f"{name}.log"),
        short_id="deadbee1",
        harness_session_id="deadbee1-1111-2222-3333-444444444444",
    )


@pytest.fixture
def claude_available(monkeypatch):
    monkeypatch.setattr(dispatch_mod, "is_provider_available", lambda _: True)
    monkeypatch.setattr(
        "fno.agents.harnesses.claude.claude_rm", lambda short_id, timeout=None: (0, "")
    )


def test_rm_removes_a_gate_clean_worktree(isolated_state, claude_available, capsys):
    """AC5-HP."""
    tmp_path = isolated_state
    wt = _linked_worktree(tmp_path, "clean")
    update_registry(lambda entries: entries + [_entry("w1", wt, tmp_path)])
    monkey_clean = {"called": False}

    import fno.worktree_reapable as wr

    original = wr.reapable

    def fake_reapable(path):
        monkey_clean["called"] = True
        return Verdict(reapable=True, reason="clean", recoverable_deletions=0)

    wr.reapable = fake_reapable
    git_calls: list[list[str]] = []
    git_kwds: list[dict] = []

    def fake_run(cmd, **kwargs):
        git_calls.append(list(cmd))
        git_kwds.append(kwargs)
        return type("R", (), {"returncode": 0, "stderr": ""})()

    original_run = dispatch_mod.subprocess.run
    dispatch_mod.subprocess.run = fake_run
    try:
        result = rm_agent("w1")
    finally:
        dispatch_mod.subprocess.run = original_run
        wr.reapable = original

    assert result.registry_changed is True
    assert result.worktree_receipt == f"worktree removed: {wt}"
    assert monkey_clean["called"], "the gate was consulted"
    assert git_calls and git_calls[0][:3] == ["git", "worktree", "remove"]
    assert git_kwds[0].get("cwd") == str(wt), "git ran from the leaf, not the caller's cwd"
    out = capsys.readouterr().out
    assert f"worktree removed: {wt}" in out


def test_rm_keeps_a_refused_worktree_and_still_removes_the_row(
    isolated_state, claude_available, capsys, monkeypatch
):
    """AC6-EDGE."""
    tmp_path = isolated_state
    wt = _linked_worktree(tmp_path, "dirty")
    update_registry(lambda entries: entries + [_entry("w2", wt, tmp_path)])

    import fno.worktree_reapable as wr

    monkeypatch.setattr(
        wr,
        "reapable",
        lambda path: Verdict(reapable=False, reason="modified-tracked", detail="src/x.py"),
    )
    removed: list[list[str]] = []
    monkeypatch.setattr(
        dispatch_mod.subprocess,
        "run",
        lambda cmd, **kwargs: removed.append(list(cmd))
        or type("R", (), {"returncode": 0, "stderr": ""})(),
    )

    result = rm_agent("w2")

    assert result.registry_changed is True, "a protected worktree never wedges the row"
    assert [e.name for e in load_registry()] == []
    assert result.worktree_receipt == (
        f"worktree kept: {wt} (the gate said no: modified-tracked)"
    )
    assert removed == [], "the gate said no: git must never run"
    assert f"worktree kept: {wt}" in capsys.readouterr().out


def test_rm_keeps_the_tree_when_the_probe_cannot_answer(
    isolated_state, claude_available, capsys, monkeypatch
):
    """AC7-ERR: removal never guesses."""
    tmp_path = isolated_state
    wt = _linked_worktree(tmp_path, "mute")
    update_registry(lambda entries: entries + [_entry("w3", wt, tmp_path)])

    import fno.worktree_reapable as wr

    monkeypatch.setattr(
        wr,
        "reapable",
        lambda path: Verdict(reapable=False, reason="probe-failed", detail="git-error: boom"),
    )
    result = rm_agent("w3")

    assert result.worktree_receipt == (
        f"worktree kept: {wt} (the reapable probe could not answer: git-error: boom)"
    )
    assert f"could not answer" in capsys.readouterr().out


def test_rm_without_a_worktree_is_a_clean_noop(isolated_state, claude_available, capsys):
    """AC2-HP for the prune: a plain cwd owns nothing removable."""
    tmp_path = isolated_state
    plain = tmp_path / "plain-cwd"
    plain.mkdir()
    update_registry(lambda entries: entries + [_entry("w4", plain, tmp_path)])

    result = rm_agent("w4")

    assert result.registry_changed is True
    assert result.worktree_receipt is None
    assert "worktree" not in capsys.readouterr().out

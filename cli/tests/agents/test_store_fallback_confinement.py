"""Project confinement for harness-store adoption (defect 1, ruled into x-8f8c).

A bare handle from a foreign repo used to heal into the caller's scope and get
woken as a side effect. Adoption is now confined to the caller's project; an
out-of-project hit is refused. Membership is the shared ``git-common-dir`` (NOT
``--show-toplevel``): footnote is worktree-first, so toplevel differs per
worktree and would refuse canonical->worktree traffic, while the common-dir is
identical across a project's whole worktree family.

The bidirectional guarantee is the load-bearing test: same-project (canonical vs
one of its worktrees) RESOLVES, and a genuinely foreign repo REFUSES. A test that
only proved the foreign-refuse case would have shipped the worktree bug.
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

from fno.agents.store_fallback import (
    StoreHit,
    _same_project,
    heal_from_harness_store,
)


def _git_repo(path: Path) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-q"], cwd=path, check=True)
    subprocess.run(["git", "config", "user.email", "t@t.t"], cwd=path, check=True)
    subprocess.run(["git", "config", "user.name", "t"], cwd=path, check=True)
    (path / "x").write_text("x")
    subprocess.run(["git", "add", "-A"], cwd=path, check=True)
    subprocess.run(["git", "commit", "-q", "-m", "i"], cwd=path, check=True)
    return path


def _git_worktree(main: Path, dest: Path) -> Path:
    subprocess.run(
        ["git", "-C", str(main), "worktree", "add", "-q", str(dest)],
        check=True,
    )
    return dest


@pytest.fixture
def two_repos(tmp_path):
    """A main repo, a worktree of it, and an unrelated foreign repo."""
    main = _git_repo(tmp_path / "footnote")
    worktree = _git_worktree(main, tmp_path / "wt")
    foreign = _git_repo(tmp_path / "regready")
    return main, worktree, foreign


# ---------------------------------------------------------------------------
# Membership: same project across a worktree; foreign differs
# ---------------------------------------------------------------------------


def test_same_project_canonical_to_worktree_resolves(two_repos):
    # The fail-before-pass case: under a --show-toplevel rule this would be False
    # (different toplevels), refusing most footnote mail. git-common-dir matches.
    main, worktree, _ = two_repos
    assert _same_project(str(main), str(worktree)) is True
    assert _same_project(str(worktree), str(main)) is True


def test_foreign_repo_refuses(two_repos):
    main, _, foreign = two_repos
    assert _same_project(str(main), str(foreign)) is False
    assert _same_project(str(foreign), str(main)) is False


def test_bidirectional_membership_in_one_assertion(two_repos):
    """Both directions, one test -- the required shape (defect 1 ruling)."""
    main, worktree, foreign = two_repos
    # Same project (canonical <-> worktree): resolves.
    assert _same_project(str(main), str(worktree)) is True
    # Foreign project: refuses.
    assert _same_project(str(main), str(foreign)) is False


# ---------------------------------------------------------------------------
# heal_from_harness_store: confine adoption to the caller's project
# ---------------------------------------------------------------------------


def _stub_hits(monkeypatch, hits):
    monkeypatch.setattr(
        "fno.agents.store_fallback.complete_store_hits",
        lambda token, **kw: list(hits),
    )


def test_heal_refuses_cross_project_and_adopts_same(two_repos, monkeypatch, tmp_path):
    from fno.agents.registry import AgentResolutionError

    main, worktree, foreign = two_repos
    sid = "ab12cdef-0000-0000-0000-000000000000"
    reg = tmp_path / "registry.json"  # isolate the adoption write

    # A foreign-repo session: refused from the footnote scope.
    _stub_hits(monkeypatch, [StoreHit("claude", sid, str(foreign))])
    with pytest.raises(AgentResolutionError) as exc:
        heal_from_harness_store("ab12cdef", scope_cwd=str(main), registry_path=reg)
    assert "cross-project" in str(exc.value).lower()

    # A same-project worktree session: adopted.
    _stub_hits(monkeypatch, [StoreHit("claude", sid, str(worktree))])
    entry = heal_from_harness_store(
        "ab12cdef", scope_cwd=str(main), registry_path=reg
    )
    assert entry is not None
    assert entry.harness_session_id == sid


def test_heal_cross_project_flag_adopts_foreign(two_repos, monkeypatch, tmp_path):
    main, _, foreign = two_repos
    sid = "ab12cdef-0000-0000-0000-000000000001"
    _stub_hits(monkeypatch, [StoreHit("claude", sid, str(foreign))])
    entry = heal_from_harness_store(
        "ab12cdef", scope_cwd=str(main), cross_project=True,
        registry_path=tmp_path / "registry.json",
    )
    assert entry is not None
    assert entry.harness_session_id == sid


def test_heal_unresolvable_hit_named_honestly_not_cross_project(
    two_repos, monkeypatch, tmp_path
):
    """A hit whose cwd never resolved (empty) is refused, but the message must
    NOT call it cross-project: membership was undeterminable, not foreign.
    Calling an unresolvable hit cross-project sends the operator looking in the
    wrong place."""
    from fno.agents.registry import AgentResolutionError

    main, _, _ = two_repos
    sid = "ab12cdef-0000-0000-0000-000000000002"
    # Empty cwd: a transcript that never recorded one, or a reaped worktree.
    _stub_hits(monkeypatch, [StoreHit("claude", sid, "")])
    with pytest.raises(AgentResolutionError) as exc:
        heal_from_harness_store(
            "ab12cdef", scope_cwd=str(main),
            registry_path=tmp_path / "registry.json",
        )
    msg = str(exc.value).lower()
    assert "could not be determined" in msg
    # The REASON must not mislabel an undeterminable hit as cross-project. (The
    # trailing "pass cross-project" hint legitimately contains the substring, so
    # assert against the reason phrase, not the whole message.)
    assert "cross-project candidate" not in msg


# ---------------------------------------------------------------------------
# Binary: the exec'd `fno agents heal-token` refuses cross-project (defect 1
# ruling: prove the guard on the exec'd path, not only in-process)
# ---------------------------------------------------------------------------


def _seed_claude_transcript(projects_dir: Path, session_id: str, cwd: Path) -> None:
    """Drop a claude-shaped transcript whose recorded cwd is ``cwd``."""
    slot = projects_dir / "footnote-test-project"
    slot.mkdir(parents=True, exist_ok=True)
    (slot / f"{session_id}.jsonl").write_text(
        json.dumps({"cwd": str(cwd)}) + "\n"
    )


def test_binary_heal_token_refuses_cross_project(two_repos, tmp_path, monkeypatch):
    main, worktree, foreign = two_repos
    sid = "ab12cdef-0000-0000-0000-000000000009"
    projects = tmp_path / "projects"
    _seed_claude_transcript(projects, sid, foreign)

    env = {
        **os.environ,
        # Point the probe at the seeded transcript; isolate the registry.
        "FNO_CLAUDE_PROJECTS_DIR": str(projects),
        "FNO_STATE_DIR": str(tmp_path / "fno-state"),
        # Keep the host's real sessions/codex/opencode stores out of the probe.
        "FNO_CLAUDE_SESSIONS_DIR": str(tmp_path / "sessions-empty"),
        "FNO_CODEX_SESSIONS_DIR": str(tmp_path / "codex-empty"),
    }
    for k in ("FNO_AGENTS_HOME",):
        env.pop(k, None)
    (tmp_path / "fno-state").mkdir(parents=True, exist_ok=True)
    (tmp_path / "sessions-empty").mkdir(exist_ok=True)
    (tmp_path / "codex-empty").mkdir(exist_ok=True)

    cli_dir = Path(__file__).resolve().parents[2]  # .../cli (the uv project)

    def _run(cwd: Path):
        # fno-py is the Python CLI entrypoint (imports the editable source); the
        # Rust `fno` mux dispatches to a separately-deployed fno-py, so it would
        # test the deployed copy, not this worktree's fix.
        return subprocess.run(
            ["uv", "run", "--project", str(cli_dir), "fno-py",
             "agents", "heal-token", "ab12cdef"],
            cwd=str(cwd), env=env, capture_output=True, text=True,
        )

    # From the footnote scope: a foreign-repo session is REFUSED (exit 3).
    out_main = _run(main)
    assert out_main.returncode == 3, out_main.stderr + out_main.stdout
    assert "cross-project" in (out_main.stderr + out_main.stdout).lower()

    # From the foreign repo itself: the same session is in-project -> resolves
    # (exit 0), proving the guard discriminates rather than refusing everything.
    out_foreign = _run(foreign)
    assert out_foreign.returncode == 0, out_foreign.stderr + out_foreign.stdout

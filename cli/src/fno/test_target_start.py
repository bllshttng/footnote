"""Tests for `fno do target start` - the one-verb cold-start (x-d91b).

Covers the pure name sanitizer plus the four command branches with the
subprocess + setup-hook stubbed so no real worktree/state is created:
  * already-isolated -> no-op, nothing spawned (Boundary).
  * happy path -> ensure + setup-hook + init, receipt `node=claimed`.
  * existing manifest -> idempotent skip, init NOT re-run (Invariant).
  * ensure failure -> loud non-zero, init never reached (Errors).
"""
from __future__ import annotations

import json
import subprocess
import time
import pytest
import typer
from pathlib import Path
from types import SimpleNamespace

from typer.testing import CliRunner

from fno import target_cli
from fno.target_cli import _wt_name, target_app

runner = CliRunner()


# ----------------------------- pure sanitizer ----------------------------- #
def test_wt_name_node_id_roundtrips():
    assert _wt_name("x-d91b") == "x-d91b"


def test_wt_name_slugifies_free_text():
    assert _wt_name("Fix the Login Bug!") == "fix-the-login-bug"


def test_wt_name_never_empty():
    assert _wt_name("///") == "target"


def test_wt_name_bounded():
    assert len(_wt_name("a" * 200)) == 60


def test_wt_name_no_trailing_hyphen_after_truncation():
    # Truncation lands on the hyphen at index 59 -> must be stripped (gemini #114).
    out = _wt_name("a" * 59 + "-bug")
    assert not out.endswith("-")
    assert out == "a" * 59


# --------------------------- slug -> id resolution ------------------------ #
def test_resolve_node_id_upgrades_slug(monkeypatch):
    from types import SimpleNamespace

    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    monkeypatch.setattr("fno.graph.load.load_graph", lambda p: [])
    monkeypatch.setattr(
        "fno.graph.fuzzy.resolve_node",
        lambda q, e: SimpleNamespace(kind="exact", id="ab-1a2b3c4d"),
    )
    assert target_cli._resolve_node_id("dashless-spawn") == "ab-1a2b3c4d"


def test_resolve_node_id_freetext_fallthrough(monkeypatch):
    from types import SimpleNamespace

    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    monkeypatch.setattr("fno.graph.load.load_graph", lambda p: [])
    monkeypatch.setattr(
        "fno.graph.fuzzy.resolve_node",
        lambda q, e: SimpleNamespace(kind="none", id=None),
    )
    assert target_cli._resolve_node_id("fix the login bug") == "fix the login bug"


# ----------------------- Codex Desktop native handoff --------------------- #
def _write_codex_rollout(path: Path, *, session_id: str, cwd: Path, originator: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "type": "session_meta",
                "payload": {
                    "id": session_id,
                    "cwd": str(cwd),
                    "originator": originator,
                },
            }
        )
        + "\n"
    )


def _write_codex_project_assignment(
    codex_home: Path, *, session_id: str, canonical: Path, cwd: Path
) -> None:
    state_path = codex_home / ".codex-global-state.json"
    state = json.loads(state_path.read_text()) if state_path.exists() else {}
    project_id = "local-footnote"
    state.setdefault("local-projects", {})[project_id] = {
        "name": canonical.name,
        "rootPaths": [str(canonical)],
    }
    state.setdefault("thread-project-assignments", {})[session_id] = {
        "projectKind": "local",
        "projectId": project_id,
        "cwd": str(cwd),
    }
    codex_home.mkdir(parents=True, exist_ok=True)
    state_path.write_text(json.dumps(state))


def test_codex_session_originator_reads_exact_rollout(monkeypatch, tmp_path):
    rollout = tmp_path / "rollout-thread-desktop.jsonl"
    _write_codex_rollout(
        rollout,
        session_id="thread-desktop",
        cwd=Path("/canonical/footnote"),
        originator="Codex Desktop",
    )
    monkeypatch.setattr(
        "fno.agents.discover.codex_rollout_for_session",
        lambda session_id: rollout if session_id == "thread-desktop" else None,
    )

    meta = target_cli._codex_session_meta("thread-desktop")

    assert meta is not None
    assert meta["originator"] == "Codex Desktop"
    assert meta["cwd"] == "/canonical/footnote"


def test_codex_session_originator_refuses_mismatched_rollout(monkeypatch, tmp_path):
    rollout = tmp_path / "rollout-other.jsonl"
    _write_codex_rollout(
        rollout,
        session_id="other-thread",
        cwd=Path("/canonical/footnote"),
        originator="Codex Desktop",
    )
    monkeypatch.setattr(
        "fno.agents.discover.codex_rollout_for_session", lambda session_id: rollout
    )

    assert target_cli._codex_session_meta("thread-desktop") is None


def test_desktop_canonical_start_requests_native_handoff_without_side_effects(
    monkeypatch, tmp_path
):
    canonical = tmp_path / "footnote"
    canonical.mkdir()
    monkeypatch.chdir(canonical)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-desktop")
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli,
        "_git_out",
        lambda cwd, *args: (
            str(canonical)
            if args == ("rev-parse", "--show-toplevel")
            else "base-sha"
            if args == ("rev-parse", "--verify", "origin/main^{commit}")
            else "base-sha"
            if args == ("merge-base", "HEAD", "origin/main")
            else None
        ),
    )
    monkeypatch.setattr(
        target_cli,
        "_codex_session_meta",
        lambda session_id: {
            "id": session_id,
            "cwd": str(canonical),
            "originator": "Codex Desktop",
        },
    )
    monkeypatch.setattr(
        "fno.worktree_paths.resolve_worktree_policy",
        lambda repo, harness: SimpleNamespace(
            policy="external",
            requested_policy="harness-native",
            degraded=True,
            project="footnote",
        ),
    )
    monkeypatch.setattr(
        target_cli, "_codex_project_assignment", lambda *args: True
    )
    monkeypatch.setattr(target_cli, "_foreign_live_holder", lambda node: None)
    calls = []
    monkeypatch.setattr(
        target_cli.subprocess,
        "run",
        lambda *args, **kwargs: calls.append((args, kwargs))
        or subprocess.CompletedProcess([], 0, stdout="", stderr=""),
    )

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == target_cli._CODEX_NATIVE_HANDOFF_EXIT
    assert "native-handoff=required" in result.output
    assert "project=footnote" in result.output
    assert "/worktree" in result.output
    assert "fno do target start x-0b3f" in result.output
    # "No side effects" means nothing MUTATING ran, not "no subprocess ran".
    # Counting every call made this order-dependent: `fno.paths.resolve_repo_root`
    # is @cache'd and shells `git worktree list` on its first use in a process,
    # so whichever test ran first paid that read. The count was 1 with the file
    # and 3 in isolation, which is why CI's sharding could fail it alone.
    argvs = [c[0][0] for c in calls]
    fetches = [a for a in argvs if a[:5] == ["git", "-C", str(canonical), "fetch", "--quiet"]]
    assert len(fetches) == 1, argvs
    mutating = {"add", "checkout", "branch", "commit", "push", "worktree"}
    for argv in argvs:
        verbs = [t for t in argv if t in mutating]
        # `worktree list` is a read; `worktree add` is not.
        assert not verbs or argv[argv.index(verbs[0]) : argv.index(verbs[0]) + 2] == [
            "worktree",
            "list",
        ], f"mutating call during a native-handoff start: {argv}"


def test_codex_tui_canonical_start_does_not_request_desktop_handoff(
    monkeypatch, tmp_path
):
    canonical = tmp_path / "footnote"
    wt = tmp_path / "fallback"
    canonical.mkdir()
    wt.mkdir()
    monkeypatch.chdir(canonical)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-tui")
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    def _git_out_stub(cwd, *args):
        if args == ("rev-parse", "--show-toplevel"):
            return str(canonical)
        if args == ("rev-parse", "--verify", "origin/main^{commit}"):
            # The ordinary-path base_label resolution verifies the ref ensure
            # built the worktree from.
            return "base-sha-0001"
        return None

    monkeypatch.setattr(target_cli, "_git_out", _git_out_stub)
    monkeypatch.setattr(
        target_cli,
        "_codex_session_meta",
        lambda session_id: {
            "id": session_id,
            "cwd": str(canonical),
            "originator": "codex-tui",
        },
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, path: (0, "")
    )

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 0, stdout=str(wt), stderr="")
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == 0, result.output
    assert "native-handoff=required" not in result.output
    assert f"worktree={wt}" in result.output


def test_desktop_canonical_start_refuses_without_project_assignment(
    monkeypatch, tmp_path
):
    canonical = tmp_path / "footnote"
    canonical.mkdir()
    monkeypatch.chdir(canonical)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-desktop")
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda node: node)
    monkeypatch.setattr(
        target_cli,
        "_git_out",
        lambda cwd, *args: str(canonical)
        if args == ("rev-parse", "--show-toplevel")
        else None,
    )
    monkeypatch.setattr(
        target_cli,
        "_codex_session_meta",
        lambda session_id: {
            "id": session_id,
            "cwd": str(canonical),
            "originator": "Codex Desktop",
        },
    )
    monkeypatch.setattr(
        target_cli, "_codex_project_assignment", lambda *args: False
    )
    calls = []
    monkeypatch.setattr(
        target_cli.subprocess,
        "run",
        lambda *args, **kwargs: calls.append((args, kwargs)),
    )

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == 1
    assert "no verified assignment" in result.output
    assert all(
        "ensure" not in call[0][0] and "init" not in call[0][0]
        for call in calls
    )


def _git_ok(cwd: Path, *args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=cwd, check=True, capture_output=True, text=True
    ).stdout.strip()


def _native_git_fixture(tmp_path: Path) -> tuple[Path, Path, Path]:
    canonical = tmp_path / "canonical" / "footnote"
    origin = tmp_path / "origin.git"
    codex_home = tmp_path / ".codex"
    native = codex_home / "worktrees" / "abcd" / "footnote"
    canonical.mkdir(parents=True)
    _git_ok(canonical, "init", "-q", "-b", "main")
    _git_ok(canonical, "config", "user.email", "test@example.com")
    _git_ok(canonical, "config", "user.name", "Test")
    (canonical / "README.md").write_text("base\n")
    _git_ok(canonical, "add", "README.md")
    _git_ok(canonical, "commit", "-qm", "base")
    subprocess.run(
        ["git", "init", "-q", "--bare", "-b", "main", str(origin)], check=True
    )
    _git_ok(canonical, "remote", "add", "origin", str(origin))
    _git_ok(canonical, "push", "--no-verify", "-q", "origin", "main")
    native.parent.mkdir(parents=True)
    _git_ok(canonical, "worktree", "add", "--detach", str(native), "origin/main")
    return canonical, native, codex_home


def test_codex_native_repo_requires_desktop_registered_worktree(monkeypatch, tmp_path):
    canonical, native, codex_home = _native_git_fixture(tmp_path)
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-desktop")
    monkeypatch.setattr(
        target_cli,
        "_codex_session_meta",
        lambda session_id: {
            "id": session_id,
            "cwd": str(canonical),
            "originator": "Codex Desktop",
        },
    )
    _write_codex_project_assignment(
        codex_home,
        session_id="thread-desktop",
        canonical=canonical,
        cwd=native,
    )

    assert target_cli._codex_native_repo(native) == canonical.resolve()
    assert target_cli._codex_native_repo(canonical) is None


def test_codex_native_repo_rejects_unassigned_external_worktree(monkeypatch, tmp_path):
    canonical, native, codex_home = _native_git_fixture(tmp_path)
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-desktop")
    monkeypatch.setattr(
        target_cli,
        "_codex_session_meta",
        lambda session_id: {
            "id": session_id,
            "cwd": str(canonical),
            "originator": "Codex Desktop",
        },
    )

    assert target_cli._codex_native_repo(native) is None


def test_codex_project_assignments_roll_distinct_worktrees_to_one_project(tmp_path):
    canonical = tmp_path / "canonical" / "footnote"
    codex_home = tmp_path / ".codex"
    first = codex_home / "worktrees" / "one" / "footnote"
    second = codex_home / "worktrees" / "two" / "footnote"
    canonical.mkdir(parents=True)
    first.mkdir(parents=True)
    second.mkdir(parents=True)
    _write_codex_project_assignment(
        codex_home, session_id="thread-one", canonical=canonical, cwd=first
    )
    _write_codex_project_assignment(
        codex_home, session_id="thread-two", canonical=canonical, cwd=second
    )

    assert target_cli._codex_project_assignment(
        "thread-one", first, canonical, codex_home=codex_home
    )
    assert target_cli._codex_project_assignment(
        "thread-two", second, canonical, codex_home=codex_home
    )


def test_codex_native_repo_rejects_worktree_not_handed_off_from_canonical(
    monkeypatch, tmp_path
):
    canonical, native, codex_home = _native_git_fixture(tmp_path)
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-desktop")
    monkeypatch.setattr(
        target_cli,
        "_codex_session_meta",
        lambda session_id: {
            "id": session_id,
            "cwd": str(native),
            "originator": "Codex Desktop",
        },
    )

    assert target_cli._codex_native_repo(native) is None


def test_codex_native_repo_rejects_tui_even_under_codex_home(monkeypatch, tmp_path):
    _canonical, native, codex_home = _native_git_fixture(tmp_path)
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-tui")
    monkeypatch.setattr(
        target_cli,
        "_codex_session_meta",
        lambda session_id: {"id": session_id, "originator": "codex-tui"},
    )

    assert target_cli._codex_native_repo(native) is None


def test_prepare_codex_native_branch_uses_remote_main(tmp_path):
    _canonical, native, _codex_home = _native_git_fixture(tmp_path)
    remote_head = _git_ok(native, "rev-parse", "origin/main")

    base = target_cli._prepare_codex_native_branch(native, "x-0b3f")

    assert base == f"origin/main@{remote_head[:12]}"
    assert _git_ok(native, "branch", "--show-current") == "feature/x-0b3f"
    assert _git_ok(native, "rev-parse", "HEAD") == remote_head


def test_prepare_codex_native_branch_refuses_dirty_detached_worktree(tmp_path):
    _canonical, native, _codex_home = _native_git_fixture(tmp_path)
    (native / "dirty.txt").write_text("dirty\n")

    with pytest.raises(typer.Exit) as exc:
        target_cli._prepare_codex_native_branch(native, "x-0b3f")

    assert exc.value.exit_code == 1
    assert _git_ok(native, "branch", "--show-current") == ""


def test_prepare_codex_native_branch_refuses_preexisting_detached_target(tmp_path):
    canonical, native, _codex_home = _native_git_fixture(tmp_path)
    _git_ok(canonical, "branch", "feature/x-0b3f", "origin/main")

    with pytest.raises(typer.Exit) as exc:
        target_cli._prepare_codex_native_branch(native, "x-0b3f")

    assert exc.value.exit_code == 1
    assert _git_ok(native, "branch", "--show-current") == ""


def test_prepare_codex_native_branch_refuses_unmanifested_divergent_resume(tmp_path):
    _canonical, native, _codex_home = _native_git_fixture(tmp_path)
    target_cli._prepare_codex_native_branch(native, "x-0b3f")
    (native / "diverged.txt").write_text("diverged\n")
    _git_ok(native, "add", "diverged.txt")
    _git_ok(native, "commit", "-qm", "diverged")

    with pytest.raises(typer.Exit) as exc:
        target_cli._prepare_codex_native_branch(native, "x-0b3f")

    assert exc.value.exit_code == 1


def test_native_codex_retry_initializes_in_app_owned_worktree(monkeypatch, tmp_path):
    canonical = tmp_path / "canonical" / "footnote"
    native = tmp_path / ".codex" / "worktrees" / "abcd" / "footnote"
    canonical.mkdir(parents=True)
    native.mkdir(parents=True)
    monkeypatch.chdir(native)
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_foreign_live_holder", lambda node: None)
    monkeypatch.setattr(target_cli, "_codex_native_repo", lambda cwd: canonical)
    monkeypatch.setattr(
        target_cli, "_prepare_codex_native_branch", lambda cwd, node: "origin/main"
    )
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda node, explicit=None, provider=None, **_kw: (None, "provider-default"),
    )
    setup_calls = []
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook",
        lambda repo, wt: setup_calls.append((repo, wt)) or (0, ""),
    )
    init_calls = []

    def fake_run(args, **kwargs):
        if "init" in args:
            init_calls.append((list(args), kwargs.get("cwd")))
            (native / ".fno").mkdir()
            (native / ".fno" / "target-state.md").write_text(
                "graph_node_id: x-0b3f\n"
            )
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    monkeypatch.setattr(
        target_cli, "_classify_node_claim", lambda node: ("ours", {"state": "live"})
    )

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == 0, result.output
    assert "app-owned=codex" in result.output
    assert "base=origin/main" in result.output
    assert "node=claimed" in result.output
    assert setup_calls == [(canonical, native)]
    assert len(init_calls) == 1
    assert init_calls[0][1] == str(native)


def test_native_codex_initial_free_text_receipt_is_unclaimed(
    monkeypatch, tmp_path, capsys
):
    canonical = tmp_path / "canonical"
    native = tmp_path / "native"
    canonical.mkdir()
    native.mkdir()
    monkeypatch.setattr(
        target_cli, "_prepare_codex_native_branch", lambda cwd, node: "origin/main@abc"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, wt: (0, "")
    )
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda node, explicit=None, provider=None, **_kw: (None, "provider-default"),
    )

    def fake_run(args, **kwargs):
        (native / ".fno").mkdir()
        (native / ".fno" / "target-state.md").write_text("graph_node_id: null\n")
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)

    target_cli._start_codex_native(
        canonical=canonical,
        cwd=native,
        node="some feature text",
        plan_path=None,
        size=None,
        model=None,
        harness=None,
        beastmode=False,
        no_merge=False,
    )

    assert "node=unclaimed" in capsys.readouterr().out


def test_desktop_explicit_external_policy_skips_assignment_proof(
    monkeypatch, tmp_path
):
    canonical = tmp_path / "canonical"
    canonical.mkdir()
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-desktop")
    monkeypatch.setattr(
        target_cli,
        "_codex_session_meta",
        lambda session_id: {
            "id": session_id,
            "cwd": str(canonical),
            "originator": "Codex Desktop",
        },
    )
    monkeypatch.setattr(
        "fno.worktree_paths.resolve_worktree_policy",
        lambda repo, harness: SimpleNamespace(
            policy="external",
            requested_policy="external",
            degraded=False,
            project="footnote",
        ),
    )
    monkeypatch.setattr(
        target_cli,
        "_codex_project_assignment",
        lambda *args: pytest.fail("explicit external policy needs no app assignment"),
    )

    assert target_cli._codex_desktop_handoff_policy(canonical) is None


def test_unverified_codex_worktree_refuses_instead_of_noop(monkeypatch, tmp_path):
    codex_home = tmp_path / ".codex"
    native = codex_home / "worktrees" / "abcd" / "footnote"
    native.mkdir(parents=True)
    monkeypatch.chdir(native)
    monkeypatch.setenv("CODEX_HOME", str(codex_home))
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda node: node)
    monkeypatch.setattr(target_cli, "_foreign_live_holder", lambda node: None)
    monkeypatch.setattr(target_cli, "_codex_native_repo", lambda cwd: None)

    result = runner.invoke(target_app, ["start", "x-0b3f"])

    assert result.exit_code == 1
    assert "native ownership could not be verified" in result.output
    assert "already isolated" not in result.output


def test_native_codex_retry_fails_closed_when_setup_fails(monkeypatch, tmp_path):
    canonical = tmp_path / "canonical"
    native = tmp_path / "native"
    canonical.mkdir()
    native.mkdir()
    monkeypatch.setattr(
        target_cli, "_prepare_codex_native_branch", lambda cwd, node: "origin/main"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, wt: (2, "broken")
    )
    init_calls = []
    monkeypatch.setattr(
        target_cli.subprocess,
        "run",
        lambda *args, **kwargs: init_calls.append((args, kwargs)),
    )

    with pytest.raises(typer.Exit) as exc:
        target_cli._start_codex_native(
            canonical=canonical,
            cwd=native,
            node="x-0b3f",
            plan_path=None,
            size=None,
            model=None,
            harness=None,
            beastmode=False,
            no_merge=False,
        )

    assert exc.value.exit_code == 2
    assert init_calls == []


def test_native_codex_resume_refuses_manifest_without_exact_node(monkeypatch, tmp_path):
    canonical = tmp_path / "canonical"
    native = tmp_path / "native"
    canonical.mkdir()
    (native / ".fno").mkdir(parents=True)
    (native / ".fno" / "target-state.md").write_text("graph_node_id: null\n")
    monkeypatch.setattr(
        target_cli, "_prepare_codex_native_branch", lambda cwd, node: "origin/main"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, wt: (0, "")
    )
    monkeypatch.setattr(
        target_cli, "_classify_node_claim", lambda node: ("ours", {"state": "live"})
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda node: {"id": node})

    with pytest.raises(typer.Exit) as exc:
        target_cli._start_codex_native(
            canonical=canonical,
            cwd=native,
            node="x-0b3f",
            plan_path=None,
            size=None,
            model=None,
            harness=None,
            beastmode=False,
            no_merge=False,
        )

    assert exc.value.exit_code == 1


@pytest.mark.parametrize(
    "verdict,expected",
    [("ours", "already-claimed"), ("dead_predecessor", "reacquired")],
)
def test_native_codex_resume_preserves_exact_node_claim(
    monkeypatch, tmp_path, capsys, verdict, expected
):
    canonical = tmp_path / "canonical"
    native = tmp_path / "native"
    canonical.mkdir()
    (native / ".fno").mkdir(parents=True)
    (native / ".fno" / "target-state.md").write_text("graph_node_id: x-0b3f\n")
    monkeypatch.setattr(
        target_cli, "_prepare_codex_native_branch", lambda cwd, node: "origin/main@abc"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, wt: (0, "")
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda node: {"id": node})
    monkeypatch.setattr(
        target_cli,
        "_classify_node_claim",
        lambda node: (verdict, {"state": "stale", "holder": "old"}),
    )
    reacquired = []
    monkeypatch.setattr(
        target_cli,
        "_reacquire_node_claim",
        lambda node, cwd, info: reacquired.append(node) or "holder",
    )
    def _no_init(cmd, *args, **kwargs):
        # `cmd` is the argv LIST, not the *args tuple: `"init" in list(args)`
        # compares against the list itself and never matches, which silently
        # retired this guard.
        argv = list(cmd) if isinstance(cmd, (list, tuple)) else [str(cmd)]
        if "init" in argv:
            pytest.fail("native resume must not rerun init")
        return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", _no_init)

    target_cli._start_codex_native(
        canonical=canonical,
        cwd=native,
        node="x-0b3f",
        plan_path=None,
        size=None,
        model=None,
        harness=None,
        beastmode=False,
        no_merge=False,
    )

    assert expected in capsys.readouterr().out
    assert reacquired == (["x-0b3f"] if verdict == "dead_predecessor" else [])


def test_native_codex_resume_refuses_foreign_exact_node_claim(monkeypatch, tmp_path):
    canonical = tmp_path / "canonical"
    native = tmp_path / "native"
    canonical.mkdir()
    (native / ".fno").mkdir(parents=True)
    (native / ".fno" / "target-state.md").write_text("graph_node_id: x-0b3f\n")
    monkeypatch.setattr(
        target_cli, "_prepare_codex_native_branch", lambda cwd, node: "origin/main@abc"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, wt: (0, "")
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda node: {"id": node})
    monkeypatch.setattr(
        target_cli,
        "_classify_node_claim",
        lambda node: (
            "foreign_live",
            {"state": "live", "holder": "other", "pid": 42, "host": "host"},
        ),
    )

    with pytest.raises(typer.Exit) as exc:
        target_cli._start_codex_native(
            canonical=canonical,
            cwd=native,
            node="x-0b3f",
            plan_path=None,
            size=None,
            model=None,
            harness=None,
            beastmode=False,
            no_merge=False,
        )

    assert exc.value.exit_code == 1


def test_native_codex_free_text_resume_stays_unclaimed(monkeypatch, tmp_path, capsys):
    canonical = tmp_path / "canonical"
    native = tmp_path / "native"
    canonical.mkdir()
    (native / ".fno").mkdir(parents=True)
    (native / ".fno" / "target-state.md").write_text("graph_node_id: null\n")
    monkeypatch.setattr(
        target_cli, "_prepare_codex_native_branch", lambda cwd, node: "origin/main@abc"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, wt: (0, "")
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda node: None)
    monkeypatch.setattr(
        target_cli,
        "_reacquire_node_claim",
        lambda *args: pytest.fail("free-text target must remain unclaimed"),
    )

    target_cli._start_codex_native(
        canonical=canonical,
        cwd=native,
        node="some feature text",
        plan_path=None,
        size=None,
        model=None,
        harness=None,
        beastmode=False,
        no_merge=False,
    )

    assert "node=unclaimed" in capsys.readouterr().out


def test_native_codex_free_text_resume_refuses_malformed_manifest(
    monkeypatch, tmp_path
):
    canonical = tmp_path / "canonical"
    native = tmp_path / "native"
    canonical.mkdir()
    (native / ".fno").mkdir(parents=True)
    (native / ".fno" / "target-state.md").write_text("truncated: true\n")
    monkeypatch.setattr(
        target_cli, "_prepare_codex_native_branch", lambda cwd, node: "origin/main@abc"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, wt: (0, "")
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda node: None)

    with pytest.raises(typer.Exit) as exc:
        target_cli._start_codex_native(
            canonical=canonical,
            cwd=native,
            node="some feature text",
            plan_path=None,
            size=None,
            model=None,
            harness=None,
            beastmode=False,
            no_merge=False,
        )

    assert exc.value.exit_code == 1


# ------------------------------- no-op branch ----------------------------- #
def test_already_isolated_is_noop(monkeypatch):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_foreign_live_holder", lambda nid: None)
    spawned = []
    monkeypatch.setattr(
        target_cli.subprocess, "run", lambda *a, **k: spawned.append(a) or None
    )
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert "already isolated" in result.stdout
    assert spawned == []  # nothing created


def test_start_refuses_dispatch_hold_before_worktree_ensure(monkeypatch):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *a: "/canonical/repo")
    monkeypatch.setattr(target_cli, "_find_node", lambda node: {"id": node})
    seen = []

    def refuse(node):
        seen.append(node["id"])
        raise typer.Exit(code=2)

    monkeypatch.setattr(target_cli, "_refuse_dispatch_hold", refuse, raising=False)
    spawned = []
    monkeypatch.setattr(
        target_cli.subprocess, "run", lambda *a, **k: spawned.append(a) or None
    )
    result = runner.invoke(target_app, ["start", "x-5a5c"])
    assert result.exit_code == 2
    assert seen == ["x-5a5c"]
    assert spawned == []


# --------------------------- happy path + idempotency --------------------- #
def _wire_happy(monkeypatch, wt_path: Path, *, manifest_exists: bool):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli, "_git_out", lambda cwd, *a: "/canonical/repo"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda r, w: (0, "")
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda *a, **k: SimpleNamespace(
            harness="codex", session_id="test-successor-session", disposition="single"
        ),
    )
    if manifest_exists:
        (wt_path / ".fno").mkdir(parents=True, exist_ok=True)
        (wt_path / ".fno" / "target-state.md").write_text("session_id: x\n")

    init_calls = []

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 0, stdout=str(wt_path), stderr="")
        if "init" in args:
            init_calls.append(kwargs.get("cwd"))
            return subprocess.CompletedProcess(args, 0)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    return init_calls


def test_happy_path_claims_and_prints_receipt(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=False)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert f"worktree={wt}" in result.stdout
    assert "base=origin/main" in result.stdout
    assert "node=claimed" in result.stdout
    # init ran exactly once, from inside the worktree (binds owner_cwd).
    assert init_calls == [str(wt)]


def test_start_forwards_model_provider_to_init(monkeypatch, tmp_path):
    """--model/--harness ride through to the composed `fno do target init` call."""
    wt = tmp_path / "wt"
    wt.mkdir()
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *a: "/canonical/repo")
    monkeypatch.setattr("fno.worktree._run_setup_worktree_hook", lambda r, w: (0, ""))

    init_args = {}

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 0, stdout=str(wt), stderr="")
        if "init" in args:
            init_args["args"] = list(args)
            return subprocess.CompletedProcess(args, 0)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    result = runner.invoke(
        target_app, ["start", "x-d91b", "--model", "glm-4.7", "--harness", "codex"]
    )
    assert result.exit_code == 0, result.stdout
    a = init_args["args"]
    assert a[a.index("--model") + 1] == "glm-4.7"
    assert a[a.index("--harness") + 1] == "codex"


def test_existing_manifest_is_idempotent(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=True)
    # Same-session re-run: the live claim is ours, so start is idempotent
    # (no re-acquire, no double-init). Without this patch a stale-free claim now
    # reads as a successor takeover (reacquire), not already-claimed.
    monkeypatch.setattr(
        target_cli, "_classify_node_claim", lambda n: ("ours", {"state": "live"})
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda n: {"id": n})
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert "node=already-claimed" in result.stdout
    assert init_calls == []  # invariant: never double-claim


def test_receipt_base_line_measures_the_real_distance(monkeypatch, tmp_path):
    """(x-d401 / x-3ae1) AC6-HP: base= never reads as a bare ref. A stale
    local origin/main answers rev-list 0 for a branch dozens of commits
    behind, so the receipt fetches and prints the measured distance."""
    wt = tmp_path / "wt"
    wt.mkdir()
    _wire_happy(monkeypatch, wt, manifest_exists=False)

    def fake_git_out(cwd, *args):
        if args == ("rev-list", "--count", "HEAD..origin/main"):
            return "10"
        return "/canonical/repo"

    monkeypatch.setattr(target_cli, "_git_out", fake_git_out)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0, result.stdout
    assert "base=origin/main behind=10" in result.stdout


def test_receipt_base_line_marks_an_unmeasured_distance_when_fetch_fails(
    monkeypatch, tmp_path
):
    """AC6-ERR: offline, the distance is explicitly unmeasured, never a bare
    ref that reads as up-to-date."""
    wt = tmp_path / "wt"
    wt.mkdir()
    _wire_happy(monkeypatch, wt, manifest_exists=False)

    def fake_run(args, **kwargs):
        argv = args if isinstance(args, list) else list(args)
        if argv[3:5] == ["fetch", "--quiet"]:
            return subprocess.CompletedProcess(args, 1, stdout="", stderr="offline")
        if "ensure" in argv:
            return subprocess.CompletedProcess(args, 0, stdout=str(wt), stderr="")
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0, result.stdout
    assert "behind=unmeasured:fetch-failed" in result.stdout


def test_already_claimed_receipt_names_the_live_holder(monkeypatch, tmp_path):
    """(x-d401 / x-3ae1) The claim line says WHO holds the node, read from the
    live claim lockfile, rather than only that someone does."""
    wt = tmp_path / "wt"
    wt.mkdir()
    _wire_happy(monkeypatch, wt, manifest_exists=True)
    monkeypatch.setattr(
        target_cli,
        "_classify_node_claim",
        lambda n: (
            "ours",
            {"state": "live", "holder": "target-session:abc123", "pid": 4242},
        ),
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda n: {"id": n})
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0, result.stdout
    assert "node=already-claimed" in result.stdout
    assert "holder=target-session:abc123" in result.stdout
    # The idempotent path must stay pure-local: no fetch, and the base
    # field SAYS it took no measurement rather than paying for one.
    assert "behind=unmeasured:idempotent-path-does-no-network" in result.stdout


def test_ensure_failure_is_loud_and_skips_init(monkeypatch, tmp_path):
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *a: "/canonical/repo")
    init_calls = []

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 1, stdout="", stderr="boom")
        if "init" in args:
            init_calls.append(True)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 1
    assert init_calls == []  # never proceed past a failed ensure


# --------------------------- tier projection (x-d7a7) --------------------- #
def _wire_start(monkeypatch, wt: Path):
    """Stub the four seams `start` shells so only model threading is exercised.

    Returns the list `start` builds as the `fno do target init` argv (captured).
    """
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: False)
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(target_cli, "_git_out", lambda cwd, *a: "/canonical/repo")
    monkeypatch.setattr("fno.worktree._run_setup_worktree_hook", lambda r, w: (0, ""))
    init_args: list[str] = []

    def fake_run(args, **kwargs):
        if "ensure" in args:
            return subprocess.CompletedProcess(args, 0, stdout=str(wt), stderr="")
        if "init" in args:
            init_args.extend(args)
            return subprocess.CompletedProcess(args, 0)
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)
    return init_args


def test_start_bare_tiered_node_threads_resolved_model(monkeypatch, tmp_path):
    """AC1-HP: bare start on a tiered node carries the resolved model + source."""
    wt = tmp_path / "wt"
    wt.mkdir()
    init_args = _wire_start(monkeypatch, wt)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None, **_kw: ("claude-sonnet-5", "task-pin"),
    )
    result = runner.invoke(target_app, ["start", "x-d7a7"])
    assert result.exit_code == 0, result.stdout
    assert init_args[init_args.index("--model") + 1] == "claude-sonnet-5"
    assert "model=claude-sonnet-5 (task-pin)" in result.stdout


def test_start_explicit_model_wins_over_tier(monkeypatch, tmp_path):
    """AC1-EDGE: an explicit -m wins and the node is never loaded to read a tier."""
    wt = tmp_path / "wt"
    wt.mkdir()
    init_args = _wire_start(monkeypatch, wt)

    # `start` now reads the node ONCE for the containment redirect (x-e957),
    # which has to happen before `worktree ensure` or a contained node leaves an
    # orphan worktree behind. So this can no longer ban every node read; it
    # counts them, which still catches a tier lookup sneaking back in as a
    # SECOND read. The invariant itself - explicit -m never loads the node -
    # belongs to _resolve_node_model and is pinned directly below.
    seen = []
    real_find = target_cli._find_node

    def _counted(nid):
        seen.append(nid)
        return real_find(nid)

    monkeypatch.setattr(target_cli, "_find_node", _counted)
    result = runner.invoke(target_app, ["start", "x-d7a7", "-m", "glm-4.7"])
    assert result.exit_code == 0, result.stdout
    assert init_args[init_args.index("--model") + 1] == "glm-4.7"
    assert "model=glm-4.7 (explicit)" in result.stdout
    assert len(seen) <= 1, f"node read more than once with explicit -m: {seen}"


def test_resolve_node_model_never_loads_the_node_when_explicit(monkeypatch):
    """AC1-EDGE, pinned where the invariant lives rather than at a caller.

    Asserted on the helper because any unrelated node read in `start` would trip
    a caller-level ban - which is exactly what happened when the containment
    redirect landed, and a test that fails for an unrelated reason is one people
    edit rather than read.
    """
    def _boom(nid):
        raise AssertionError("node loaded despite explicit -m")

    monkeypatch.setattr(target_cli, "_find_node", _boom)
    model, source = target_cli._resolve_node_model("x-d7a7", explicit="glm-4.7")
    assert model == "glm-4.7"
    assert source == "explicit"


def test_start_untiered_node_forwards_no_model(monkeypatch, tmp_path):
    """Invariant: a node with no pin/tier -> no --model, receipt byte-identical."""
    wt = tmp_path / "wt"
    wt.mkdir()
    init_args = _wire_start(monkeypatch, wt)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None, **_kw: (None, "provider-default"),
    )
    result = runner.invoke(target_app, ["start", "x-d7a7"])
    assert result.exit_code == 0, result.stdout
    assert "--model" not in init_args
    assert "model=" not in result.stdout
    assert result.stdout.rstrip().endswith("node=claimed")


def test_resolve_node_model_degrades_on_error(monkeypatch):
    """AC1-ERR: any load/resolve error -> (None, provider-default), never raises."""

    def _raise(_p):
        raise RuntimeError("snapshot unreadable")

    monkeypatch.setattr("fno.graph.load.load_graph", _raise)
    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    assert target_cli._resolve_node_model("x-d7a7") == (None, "provider-default")


def test_resolve_node_model_error_preserves_explicit(monkeypatch):
    """A resolve error with an explicit -m degrades to that value, not the default."""

    def _raise(**_kw):
        raise RuntimeError("router boom")

    monkeypatch.setattr("fno.route_resolve.resolve_dispatch_model", _raise)
    assert target_cli._resolve_node_model("x-d7a7", explicit="glm-4.7") == (
        "glm-4.7",
        "explicit",
    )


def test_resolve_node_model_uses_route_resolve(monkeypatch):
    """The helper reads the node's model pin and defers to route_resolve;
    difficulty stays out of the call (the capacity grid owns it, x-baef)."""
    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda p: [{"id": "x-d7a7", "model": "glm-5.2", "difficulty": "high"}],
    )
    seen = {}

    def fake_resolve(**kw):
        seen.update(kw)
        return "glm-5.2", "task-pin", ["task-pin"]

    monkeypatch.setattr(
        "fno.route_resolve.resolve_dispatch_model", fake_resolve
    )
    assert target_cli._resolve_node_model("x-d7a7") == ("glm-5.2", "task-pin")
    assert seen.get("task_model") == "glm-5.2"
    assert seen.get("task_difficulty") is None


def test_resolve_node_model_scopes_by_provider(monkeypatch):
    """The seam scopes resolution by the provider it is handed (x-da6e)."""
    monkeypatch.setattr("fno.paths.graph_json", lambda: "ignored")
    monkeypatch.setattr(
        "fno.graph.load.load_graph",
        lambda p: [{"id": "x-d7a7", "model": "claude-sonnet-5"}],
    )
    seen = {}

    def fake_resolve(*, provider, **_kw):
        seen["provider"] = provider
        return "claude-sonnet-5", "task-pin", ["task-pin"]

    monkeypatch.setattr("fno.route_resolve.resolve_dispatch_model", fake_resolve)
    target_cli._resolve_node_model("x-d7a7", provider="claude")
    assert seen == {"provider": "claude"}


def test_resolve_model_command_prints_model(monkeypatch):
    """`fno do target resolve-model` prints the resolved model for bash dispatchers."""
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None, **_kw: ("claude-sonnet-5", "task-pin"),
    )
    result = runner.invoke(target_app, ["resolve-model", "x-d7a7"])
    assert result.exit_code == 0
    assert result.stdout.strip() == "claude-sonnet-5"


def test_resolve_model_command_empty_when_no_model(monkeypatch):
    """No pin/tier -> prints nothing (caller uses the provider default)."""
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None, **_kw: (None, "provider-default"),
    )
    result = runner.invoke(target_app, ["resolve-model", "x-d7a7"])
    assert result.exit_code == 0
    assert result.stdout.strip() == ""


def test_resolve_model_provider_filter_drops_cross_harness(monkeypatch):
    """--harness claude drops a tier that resolved to a codex model (bg is claude-only)."""
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None, **_kw: ("gpt-5.4", "task-pin"),
    )
    # gpt-5.4 maps to the codex harness in the real REACHABILITY table.
    result = runner.invoke(
        target_app, ["resolve-model", "x-d7a7", "--harness", "claude"]
    )
    assert result.exit_code == 0
    assert result.stdout.strip() == ""  # dropped -> caller uses the provider default


def test_resolve_model_provider_filter_keeps_same_harness(monkeypatch):
    """--harness claude keeps a claude-reachable tier model."""
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda nid, explicit=None, provider=None, **_kw: ("claude-sonnet-5", "task-pin"),
    )
    result = runner.invoke(
        target_app, ["resolve-model", "x-d7a7", "--harness", "claude"]
    )
    assert result.exit_code == 0
    assert result.stdout.strip() == "claude-sonnet-5"


def test_model_reachable_by_conservative_on_unknown(monkeypatch):
    """An unknown model is treated as reachable (guard only drops CONFIRMED mismatches)."""
    assert target_cli._model_reachable_by("gpt-5.4", "claude") is False
    assert target_cli._model_reachable_by("claude-sonnet-5", "claude") is True
    assert target_cli._model_reachable_by("some-unmapped-model", "claude") is True


# ================= ownership guard: refuse a foreign live session (x-84fc) =====
# _foreign_live_holder unit tests -------------------------------------------- #
def _wire_claim(monkeypatch, status, *, own_pid=None):
    monkeypatch.setattr("fno.claims.core.claim_status", lambda key, root=None: status)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: None)
    monkeypatch.setattr(
        "fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: own_pid
    )


def test_foreign_live_holder_free_returns_none(monkeypatch):
    # AC2-ERR: a free claim -> proceed.
    _wire_claim(monkeypatch, {"key": "node:N", "state": "free"})
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_dead_returns_none(monkeypatch):
    # AC2-ERR: stale/dead is not live/suspect -> proceed unchanged.
    _wire_claim(
        monkeypatch,
        {"key": "node:N", "state": "stale", "holder": "target-session:A"},
    )
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_different_live_returns_info(monkeypatch):
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:A", "pid": 999, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=123)  # own pid != holder pid
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_suspect_cross_host_returns_info(monkeypatch):
    # A suspect holder (live-on-another-host) folds into refuse.
    status = {
        "key": "node:N", "state": "suspect",
        "holder": "target-session:A", "pid": 999, "host": "other",
    }
    _wire_claim(monkeypatch, status, own_pid=None)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_lapsed_ttl_still_live(monkeypatch):
    # AC1-EDGE: classify() returns "live" from the durable pid even with TTL
    # lapsed -> the guard still surfaces the holder (park, not "idle").
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:A", "pid": 999, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=None)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_ours_by_tsid(monkeypatch):
    # AC1-ERR: same-session identity by TARGET_SESSION_ID -> not foreign.
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:X", "pid": 1, "host": "h",
    }
    _wire_claim(monkeypatch, status)
    monkeypatch.setenv("TARGET_SESSION_ID", "X")
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_ours_by_codex_thread(monkeypatch):
    # Codex parity: a codex session's claim owner is its CODEX_THREAD_ID (no
    # TARGET_SESSION_ID), so a same-thread re-run must NOT be seen as foreign.
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:thread-abc", "pid": 1, "host": "h",
    }
    _wire_claim(monkeypatch, status)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-abc")
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_different_codex_thread_is_foreign(monkeypatch):
    # A DIFFERENT codex thread's live claim is still foreign -> refuse.
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:thread-OTHER", "pid": 999, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=None)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-abc")
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_ours_by_pid_host(monkeypatch):
    # Bare interactive re-run: durable pid + machine match -> not foreign.
    # Ownership keys on the stable machine id, not gethostname().
    status = {
        "key": "node:N", "state": "live", "holder": "target-session:Z",
        "pid": 555, "host": "whatever-the-name-is-now", "machine_id": "mine",
    }
    _wire_claim(monkeypatch, status, own_pid=555)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.setattr("fno.claims.hostid.machine_id", lambda: "mine")
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_ours_by_pid_host_pre_change_claim(monkeypatch):
    # A claim written before machine_id existed still resolves as ours through
    # the hostname fallback, so an upgrade does not orphan a running session.
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:Z", "pid": 555, "host": "myhost",
    }
    _wire_claim(monkeypatch, status, own_pid=555)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.setattr("fno.claims.hostid.hostname", lambda: "myhost")
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_probe_error_degrades_none(monkeypatch):
    # AC1-FR: claim_status raising -> None (never blocks a legit start).
    def boom(key, root=None):
        raise RuntimeError("corrupt claim file")

    monkeypatch.setattr("fno.claims.core.claim_status", boom)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: None)
    assert target_cli._foreign_live_holder("N") is None


def test_foreign_live_holder_uncapturable_pid_parks(monkeypatch):
    # AC2-FR: own pid None + no TSID + foreign live -> return info (park).
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:A", "pid": 999, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=None)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_gethostname_raises_parks(monkeypatch):
    # Contract: never raises. socket.gethostname() can OSError in a sandbox ->
    # own identity is uncapturable -> a foreign live claim parks (AC2-FR).
    # hostid swallows the OSError and reports "" (never this machine), so the
    # park still happens; the contract is the outcome, not the exception path.
    status = {
        "key": "node:N", "state": "live",
        "holder": "target-session:A", "pid": 555, "host": "h",
    }
    _wire_claim(monkeypatch, status, own_pid=555)
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)

    def boom():
        raise OSError("no hostname in sandbox")

    monkeypatch.setattr("fno.claims.hostid.socket.gethostname", boom)
    monkeypatch.setattr("fno.claims.hostid.machine_id", lambda: "")
    assert target_cli._foreign_live_holder("N") == status


def test_foreign_live_holder_freetext_reads_free(monkeypatch):
    # AC2-EDGE: a free-text arg keys node:<text>, reads free, never false-refuses.
    seen = {}

    def status(key, root=None):
        seen["key"] = key
        return {"key": key, "state": "free"}

    monkeypatch.setattr("fno.claims.core.claim_status", status)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: None)
    assert target_cli._foreign_live_holder("fix the login bug") is None
    assert seen["key"] == "node:fix the login bug"


# manifest-present exit (Site B) --------------------------------------------- #
def test_manifest_present_foreign_holder_refuses(monkeypatch, tmp_path):
    # AC1-HP: foreign live holder at the manifest-present exit -> park, exit 1.
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=True)
    monkeypatch.setattr(
        target_cli,
        "_classify_node_claim",
        lambda n: ("foreign_live", {"holder": "target-session:A", "pid": 4321, "host": "boxA", "state": "live"}),
    )
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 1
    assert "target-session:A" in result.output
    assert "pid=4321" in result.output
    assert "boxA" in result.output
    assert "node=already-claimed" not in result.output
    assert init_calls == []  # never proceeds into a shared worktree


def test_manifest_present_own_rerun_proceeds(monkeypatch, tmp_path):
    # AC1-ERR: the claim is ours (same-session re-run) -> idempotent
    # already-claimed. (A dead predecessor now re-acquires instead - see
    # test_successor_dead_predecessor_reacquires.)
    wt = tmp_path / "wt"
    wt.mkdir()
    _wire_happy(monkeypatch, wt, manifest_exists=True)
    monkeypatch.setattr(target_cli, "_classify_node_claim", lambda n: ("ours", {"state": "live"}))
    monkeypatch.setattr(target_cli, "_find_node", lambda n: {"id": n})
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0
    assert "node=already-claimed" in result.output


# already-isolated exit (Site A) --------------------------------------------- #
def test_already_isolated_foreign_holder_refuses(monkeypatch):
    # AC2-HP: cwd is a foreign live session's worktree -> park, exit 1.
    monkeypatch.setattr(target_cli, "_is_linked_worktree", lambda cwd: True)
    monkeypatch.setattr(target_cli, "_resolve_node_id", lambda n: n)
    monkeypatch.setattr(
        target_cli, "_foreign_live_holder",
        lambda nid: {"holder": "target-session:A", "pid": 4321, "host": "boxA"},
    )
    spawned = []
    monkeypatch.setattr(
        target_cli.subprocess, "run", lambda *a, **k: spawned.append(a) or None
    )
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 1
    assert "target-session:A" in result.output
    assert "already isolated" not in result.output
    assert spawned == []  # never created/entered anything


# shared park-message printer (Site 1.4) ------------------------------------- #
def test_park_message_names_holder_pid_host_worktree(capsys):
    info = {"holder": "target-session:A", "pid": 4321, "host": "boxA"}
    target_cli._print_foreign_holder_park("x-84fc", info, Path("/wt/x-84fc"))
    err = capsys.readouterr().err
    assert "x-84fc" in err  # node id
    assert "target-session:A" in err  # holder
    assert "pid=4321" in err  # pid
    assert "boxA" in err  # host
    assert "/wt/x-84fc" in err  # worktree path


# successor re-acquisition (x-a7ab 1.3) -------------------------------------- #
def test_classify_node_claim_free(monkeypatch):
    _wire_claim(monkeypatch, {"key": "node:N", "state": "free"})
    assert target_cli._classify_node_claim("N") == ("free", {"key": "node:N", "state": "free"})


def test_classify_node_claim_unreadable_is_free(monkeypatch):
    def _raise(*a, **k):
        raise RuntimeError("unreadable")

    monkeypatch.setattr("fno.claims.core.claim_status", _raise)
    monkeypatch.setattr("fno.claims.io.claims_root_for", lambda key: None)
    verdict, info = target_cli._classify_node_claim("N")
    assert verdict == "free" and info is None


def test_classify_node_claim_dead_predecessor(monkeypatch):
    _wire_claim(
        monkeypatch,
        {"key": "node:N", "state": "stale", "holder": "target-session:A", "pid": 1, "host": "h"},
        own_pid=2,
    )
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    verdict, info = target_cli._classify_node_claim("N")
    assert verdict == "dead_predecessor"
    assert info["holder"] == "target-session:A"


def test_classify_node_claim_ours(monkeypatch):
    _wire_claim(
        monkeypatch,
        {"key": "node:N", "state": "live", "holder": "target-session:X", "pid": 1, "host": "h"},
    )
    monkeypatch.setenv("TARGET_SESSION_ID", "X")
    assert target_cli._classify_node_claim("N")[0] == "ours"


def test_classify_node_claim_foreign_live(monkeypatch):
    _wire_claim(
        monkeypatch,
        {"key": "node:N", "state": "live", "holder": "target-session:A", "pid": 9, "host": "h"},
        own_pid=2,
    )
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    assert target_cli._classify_node_claim("N")[0] == "foreign_live"


def test_successor_dead_predecessor_reacquires(monkeypatch, tmp_path):
    # AC3-ERR: a dead predecessor's claim is re-acquired under this session.
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=True)
    monkeypatch.setattr(
        target_cli,
        "_classify_node_claim",
        lambda n: ("dead_predecessor", {"state": "stale", "holder": "target-session:DEAD", "pid": 111, "host": "h"}),
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda n: {"id": n})
    acq = []

    def fake_acquire(*a, **k):
        acq.append({"args": a, "kw": k})
        return None

    monkeypatch.setattr("fno.claims.core.acquire_claim", fake_acquire)
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0, result.stdout
    assert "node=reacquired" in result.stdout
    assert "DEAD" in result.stdout  # prior holder named (loud takeover, never silent)
    assert init_calls == []  # no double-init (manifest is write-once)
    assert len(acq) == 1 and acq[0]["args"][0] == "node:x-d91b"


def test_successor_ours_is_idempotent_no_reacquire(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    init_calls = _wire_happy(monkeypatch, wt, manifest_exists=True)
    monkeypatch.setattr(
        target_cli, "_classify_node_claim", lambda n: ("ours", {"state": "live"})
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda n: {"id": n})
    acq = []
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: acq.append(1))
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0, result.stdout
    assert "node=already-claimed" in result.stdout
    assert acq == [] and init_calls == []  # ours -> no re-acquire, no init


def test_successor_foreign_live_refuses(monkeypatch, tmp_path):
    # AC3-ERR: a live foreign holder -> refuse, naming the holder.
    wt = tmp_path / "wt"
    wt.mkdir()
    _wire_happy(monkeypatch, wt, manifest_exists=True)
    monkeypatch.setattr(
        target_cli,
        "_classify_node_claim",
        lambda n: ("foreign_live", {"state": "live", "holder": "target-session:OTHER", "pid": 999, "host": "h"}),
    )
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 1
    assert "OTHER" in result.output  # refusal names the live holder
    assert "held by a live session" in result.output


def test_successor_ghost_node_refuses(monkeypatch, tmp_path):
    # A node the manifest references but that is no longer in the graph is never
    # re-acquired (no claim for a ghost).
    wt = tmp_path / "wt"
    wt.mkdir()
    _wire_happy(monkeypatch, wt, manifest_exists=True)
    (wt / ".fno" / "target-state.md").write_text("session_id: x\ngraph_node_id: x-ghost\n")
    monkeypatch.setattr(
        target_cli, "_classify_node_claim", lambda n: ("dead_predecessor", {"state": "stale"})
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda n: None)
    acq = []
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: acq.append(1))
    result = runner.invoke(target_app, ["start", "x-ghost"])
    assert result.exit_code == 1
    assert "not in the backlog graph" in result.output
    assert acq == []


def test_free_text_rerun_not_treated_as_ghost(monkeypatch, tmp_path):
    # F7: a free-text/plan-only session (no graph_node_id) rerun is NOT a ghost -
    # it re-acquires and proceeds even though _find_node would return None.
    wt = tmp_path / "wt"
    wt.mkdir()
    _wire_happy(monkeypatch, wt, manifest_exists=True)  # manifest: session_id only
    monkeypatch.setattr(
        target_cli, "_classify_node_claim", lambda n: ("dead_predecessor", {"state": "stale"})
    )
    monkeypatch.setattr(target_cli, "_find_node", lambda n: None)  # would look ghostly
    acq = []
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: acq.append(a[0]))
    result = runner.invoke(target_app, ["start", "some feature text"])
    assert result.exit_code == 0, result.output
    assert "node=reacquired" in result.output
    assert "not in the backlog graph" not in result.output
    assert acq


def test_reacquire_fails_closed_on_corrupt_claim(monkeypatch):
    # F6: a corrupt prior claim (ClaimCorrupted) exits cleanly instead of
    # tracebacking (the write can fail even when the read said "free").
    from fno.claims.core import ClaimCorrupted

    monkeypatch.setattr(target_cli, "_successor_claim_holder", lambda: "target-session:ME")
    monkeypatch.setattr("fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: 1)

    def raise_corrupt(*a, **k):
        raise ClaimCorrupted("corrupt prior claim")

    monkeypatch.setattr("fno.claims.core.acquire_claim", raise_corrupt)
    with pytest.raises(typer.Exit) as exc:
        target_cli._reacquire_node_claim("N", Path("/wt"), {"state": "stale"})
    assert exc.value.exit_code == 1


def test_successor_free_claim_reacquires(monkeypatch, tmp_path):
    # A stale-free claim (manifest exists, claim gone) also re-acquires.
    wt = tmp_path / "wt"
    wt.mkdir()
    _wire_happy(monkeypatch, wt, manifest_exists=True)
    monkeypatch.setattr(target_cli, "_classify_node_claim", lambda n: ("free", None))
    monkeypatch.setattr(target_cli, "_find_node", lambda n: {"id": n})
    acq = []
    monkeypatch.setattr("fno.claims.core.acquire_claim", lambda *a, **k: acq.append(a[0]))
    result = runner.invoke(target_app, ["start", "x-d91b"])
    assert result.exit_code == 0, result.stdout
    assert "node=reacquired" in result.stdout
    assert "no prior claim" in result.stdout
    assert acq == ["node:x-d91b"]


def test_reacquire_parks_on_live_race(monkeypatch):
    # A live-other racing the re-acquire -> park loudly + non-zero (backstop;
    # the classifier already filtered foreign-live, but acquire_claim is the
    # last word and must fail closed).
    from fno.claims.core import ClaimHeldByOther

    monkeypatch.setattr(target_cli, "_successor_claim_holder", lambda: "target-session:ME")
    monkeypatch.setattr("fno.claims.session_pid.resolve_session_pid", lambda from_pid=None: 1)

    def raise_other(*a, **k):
        raise ClaimHeldByOther("target-session:RIVAL", 999, "h", "node:N")

    monkeypatch.setattr("fno.claims.core.acquire_claim", raise_other)
    with pytest.raises(typer.Exit) as exc:
        target_cli._reacquire_node_claim("N", Path("/wt"), {"state": "stale"})
    assert exc.value.exit_code == 1


def test_successor_claim_holder_prefers_tsid(monkeypatch):
    monkeypatch.setenv("TARGET_SESSION_ID", "abc-123")
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda *a, **k: SimpleNamespace(
            harness="claude", session_id="abc-123", disposition="single"
        ),
    )
    assert target_cli._successor_claim_holder() == "target-session:abc-123"


def test_successor_claim_holder_codex_parity(monkeypatch):
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-z")
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda *a, **k: SimpleNamespace(
            harness="codex", session_id="thread-z", disposition="single"
        ),
    )
    assert target_cli._successor_claim_holder() == "target-session:thread-z"


def test_successor_claim_holder_resolves_each_acquire(monkeypatch):
    identities = iter(
        [
            SimpleNamespace(session_id="session-a"),
            SimpleNamespace(session_id="session-b"),
        ]
    )
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda *a, **k: next(identities),
    )
    assert target_cli._successor_claim_holder() == "target-session:session-a"
    assert target_cli._successor_claim_holder() == "target-session:session-b"


def test_successor_claim_holder_refuses_without_proven_identity(monkeypatch):
    monkeypatch.delenv("TARGET_SESSION_ID", raising=False)
    monkeypatch.delenv("CODEX_THREAD_ID", raising=False)
    monkeypatch.setattr(
        "fno.claims.self_identity.resolve_self_identity",
        lambda *a, **k: SimpleNamespace(
            harness="claude", session_id=None, disposition="single", markers_present=()
        ),
    )
    assert target_cli._successor_claim_holder() is None


def test_worktree_occupancy_dirty_recent_transcript_refuses(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    monkeypatch.setattr(
        "fno.worktree_reapable.reapable",
        lambda path: SimpleNamespace(
            reapable=False, reason="modified-tracked", detail="src/app.py"
        ),
    )
    monkeypatch.setattr(
        "fno.agents.registry.load_registry",
        lambda: [
            SimpleNamespace(
                cwd=str(wt), harness_session_id="worker-session", harness="codex"
            )
        ],
    )
    monkeypatch.setattr(
        "fno.agents.watchdog.tail_facts",
        lambda sid, cwd, **kwargs: (
            SimpleNamespace(last_event_epoch=time.time() - 5)
            if kwargs.get("agent") == "codex"
            else None
        ),
    )
    verdict, info = target_cli._classify_worktree_occupancy(wt)
    assert verdict == "occupied_worktree"
    assert info["session_id"] == "worker-session"


def test_worktree_occupancy_probe_failure_is_unknown(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    monkeypatch.setattr(
        "fno.worktree_reapable.reapable",
        lambda path: SimpleNamespace(reapable=False, reason="probe-failed", detail="git"),
    )
    verdict, info = target_cli._classify_worktree_occupancy(wt)
    assert verdict == "unknown"
    assert info["reason"] == "probe-failed"


def test_worktree_occupancy_clean_is_available(monkeypatch, tmp_path):
    wt = tmp_path / "wt"
    wt.mkdir()
    monkeypatch.setattr(
        "fno.worktree_reapable.reapable",
        lambda path: SimpleNamespace(reapable=True, reason="clean", detail=""),
    )
    assert target_cli._classify_worktree_occupancy(wt) == ("available", None)


def test_no_merge_reaches_init_argv_on_codex_native_path(monkeypatch, tmp_path):
    """--no-merge must reach init on EVERY path start can exit through.

    The codex-native branch builds its own `target init` argv. It carried
    --beastmode but not --no-merge, so `fno do target start --no-merge <node>`
    inside an app-owned worktree exited 0 and wrote auto_merge_approved: true
    whenever config enabled it - a silent grant against an explicit refusal.
    """
    canonical = tmp_path / "canonical"
    canonical.mkdir()
    native = tmp_path / "native"
    native.mkdir()
    seen: list[list[str]] = []

    monkeypatch.setattr(
        target_cli, "_prepare_codex_native_branch", lambda cwd, node: "origin/main"
    )
    monkeypatch.setattr(
        "fno.worktree._run_setup_worktree_hook", lambda repo, wt: (0, "")
    )
    monkeypatch.setattr(target_cli, "_resolve_fno_cmd", lambda: ["fno"])
    monkeypatch.setattr(
        target_cli,
        "_resolve_node_model",
        lambda node, explicit=None, provider=None, **_kw: (None, "provider-default"),
    )

    def fake_run(args, **kwargs):
        seen.append(list(args))
        (native / ".fno").mkdir(exist_ok=True)
        (native / ".fno" / "target-state.md").write_text("graph_node_id: null\n")
        return subprocess.CompletedProcess(args, 0, stdout="", stderr="")

    monkeypatch.setattr(target_cli.subprocess, "run", fake_run)

    target_cli._start_codex_native(
        canonical=canonical,
        cwd=native,
        node="x-e938",
        plan_path=None,
        size=None,
        model=None,
        harness=None,
        beastmode=False,
        no_merge=True,
    )

    init_argv = [a for a in seen if "init" in a]
    assert init_argv, f"no target init invocation captured: {seen}"
    assert any("--no-merge" in a for a in init_argv), (
        f"--no-merge never reached target init: {init_argv}"
    )


def _clear_remote_cache():
    """Both memos, always. `_TIMED_OUT_REMOTES` is process-global like its
    sibling, so clearing only one leaves a prior test's timeout short-
    circuiting the next one's fetch."""
    target_cli._REFRESHED_REMOTES.clear()
    target_cli._TIMED_OUT_REMOTES.clear()


def test_refresh_remote_marks_a_timeout_apart_from_a_fetch_error(monkeypatch):
    """A timeout is not a verdict (x-d401).

    The 60s bound is this branch's addition; before it the fetch simply ran to
    completion. A caller must be able to tell "the network was slow" from
    "the remote said no", or a slow-but-working fetch reads as a failure.
    """
    _clear_remote_cache()

    def _timeout(*_a, **_k):
        raise subprocess.TimeoutExpired(cmd="git fetch", timeout=60)

    monkeypatch.setattr(subprocess, "run", _timeout)
    ok, err = target_cli._refresh_remote(Path("/repo"), "origin")
    assert ok is False
    assert err == target_cli._TIMED_OUT

    _clear_remote_cache()

    def _refused(*_a, **_k):
        return SimpleNamespace(returncode=128, stderr="fatal: Authentication failed")

    monkeypatch.setattr(subprocess, "run", _refused)
    ok, err = target_cli._refresh_remote(Path("/repo"), "origin")
    assert ok is False
    assert err != target_cli._TIMED_OUT
    assert "Authentication failed" in err


def test_remote_base_ref_survives_a_slow_fetch_but_still_refuses_a_real_error(monkeypatch):
    """A timed-out refresh must NOT refuse a cold start that used to work.

    `origin/main` carried no timeout before this branch, so hard-exiting on
    one would break a start on a slow network. A genuine fetch error still
    exits, exactly as it did.
    """
    _clear_remote_cache()
    monkeypatch.setattr(
        target_cli, "_refresh_remote", lambda *_a, **_k: (False, target_cli._TIMED_OUT)
    )
    monkeypatch.setattr(
        target_cli, "_git_out", lambda _cwd, *args: "abc123" if "rev-parse" in args else ""
    )
    assert target_cli._remote_base_ref(Path("/repo"), fetch=True) == "origin/main"

    monkeypatch.setattr(
        target_cli, "_refresh_remote", lambda *_a, **_k: (False, "fatal: could not read from remote")
    )
    with pytest.raises(typer.Exit) as excinfo:
        target_cli._remote_base_ref(Path("/repo"), fetch=True)
    assert excinfo.value.exit_code == 1


def test_a_timed_out_remote_is_not_refetched_in_the_same_process(monkeypatch):
    """One slow link, one 60s bound (x-d401).

    A start resolves the base ref and then measures the distance: two fetches
    of the same remote. Re-paying the bound to re-learn the network is slow
    stalled a cold start for two minutes and changed no answer.
    """
    _clear_remote_cache()
    target_cli._TIMED_OUT_REMOTES.clear()
    calls = []

    def _timeout(*args, **_k):
        calls.append(args)
        raise subprocess.TimeoutExpired(cmd="git fetch", timeout=60)

    monkeypatch.setattr(subprocess, "run", _timeout)
    assert target_cli._refresh_remote(Path("/repo"), "origin") == (False, target_cli._TIMED_OUT)
    assert len(calls) == 1
    # The narrower per-branch fetch the measurement makes next.
    assert target_cli._refresh_remote(Path("/repo"), "origin", "main") == (
        False,
        target_cli._TIMED_OUT,
    )
    assert len(calls) == 1, "the second fetch must not pay the bound again"
    target_cli._TIMED_OUT_REMOTES.clear()


def test_truthful_base_names_a_timeout_apart_from_a_fetch_failure(monkeypatch):
    """A slow network and a refused remote are different facts (x-d401)."""
    _clear_remote_cache()
    monkeypatch.setattr(
        target_cli, "_refresh_remote", lambda *_a, **_k: (False, target_cli._TIMED_OUT)
    )
    assert "behind=unmeasured:fetch-timed-out" in target_cli._truthful_base(
        Path("/repo"), "origin/main"
    )

    monkeypatch.setattr(
        target_cli, "_refresh_remote", lambda *_a, **_k: (False, "fatal: repository not found")
    )
    assert "behind=unmeasured:fetch-failed" in target_cli._truthful_base(
        Path("/repo"), "origin/main"
    )

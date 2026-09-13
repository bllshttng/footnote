"""The codex sandbox probe asserts a positive marker per tool, and an unrun probe proves nothing."""
from __future__ import annotations

import os
import pwd
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

from fno.agents import sandbox_probe
from fno.agents.sandbox_probe import probe_codex_sandbox

HEAD = "a" * 40
NO_NETWORK = "error connecting to api.github.com"
EPERM = "fatal: prepare: cannot lock ref 'refs/fno-probe/x': Operation not permitted"


def _done(argv, returncode=0, stdout="", stderr=""):
    return subprocess.CompletedProcess(argv, returncode, stdout, stderr)


class FakeRun:
    """Answers each probe command by what it runs and where, and records every call.

    ``gh_inside``/``gh_outside`` and ``lock_inside``/``lock_outside`` say
    whether that tool works in that place. ``transactions`` keeps every ref
    transaction fed on stdin.
    """

    def __init__(self, *, control=None, gh_inside=True, gh_outside=True,
                 lock_inside=True, lock_outside=True, repo=True):
        self.calls: list[tuple[bool, list[str]]] = []
        self.transactions: list[str] = []
        self.control = control
        self.gh = {True: gh_inside, False: gh_outside}
        self.lock = {True: lock_inside, False: lock_outside}
        self.repo = repo

    def __call__(self, argv, input=None, **kwargs):
        sandboxed = list(argv[:2]) == ["codex", "sandbox"]
        cmd = list(argv[argv.index("--") + 1:]) if sandboxed else list(argv)
        self.calls.append((sandboxed, cmd))
        if cmd[0] == "/bin/echo":
            return self.control(argv) if self.control else _done(argv, stdout=cmd[1] + "\n")
        if cmd[0] == "gh":
            if self.gh[sandboxed]:
                return _done(argv, stdout="5000\n")
            return _done(argv, 1, stderr=f"{NO_NETWORK}\ncheck your internet connection or https://githubstatus.com\n")
        if cmd[:3] == ["git", "rev-parse", "HEAD"]:
            return _done(argv, stdout=HEAD + "\n") if self.repo else _done(argv, 128, stderr="fatal: not a git repository\n")
        if cmd == ["git", "update-ref", "--stdin"]:
            self.transactions.append(input)
            if self.lock[sandboxed]:
                return _done(argv, stdout="start: ok\nprepare: ok\nabort: ok\n")
            return _done(argv, 128, stdout="start: ok\n", stderr=EPERM + "\n")
        return _done(argv)

    def ran(self, *prefix: str, sandboxed=None) -> bool:
        return any(
            cmd[: len(prefix)] == list(prefix) and (sandboxed is None or where == sandboxed)
            for where, cmd in self.calls
        )

    def every_transaction_aborts(self) -> bool:
        return bool(self.transactions) and all(
            txn.splitlines()[-1] == "abort" and "commit" not in txn.splitlines()
            for txn in self.transactions
        )


@pytest.fixture(autouse=True)
def _no_real_grant(monkeypatch):
    import fno.agents.harnesses.codex as codex

    monkeypatch.setattr(codex, "git_writable_config_args", lambda cwd: [])


def test_every_marker_present_reads_reachable(tmp_path):
    run = FakeRun()
    assert probe_codex_sandbox(tmp_path, run=run) == sandbox_probe.SandboxProbe("reachable")
    assert run.every_transaction_aborts()
    assert "create refs/fno-probe/" in run.transactions[0] and HEAD in run.transactions[0]
    assert not run.ran("gh", sandboxed=False)


def test_gh_that_answers_outside_but_not_inside_is_blocked(tmp_path):
    probe = probe_codex_sandbox(tmp_path, run=FakeRun(gh_inside=False))
    assert probe.verdict == "blocked"
    assert probe.blocked == [("gh", NO_NETWORK)]


def test_gh_that_fails_outside_too_says_nothing_about_the_sandbox(tmp_path):
    probe = probe_codex_sandbox(tmp_path, run=FakeRun(gh_inside=False, gh_outside=False))
    assert probe.verdict == "unknown"
    assert probe.blocked == []
    assert "gh fails outside the sandbox too" in probe.note


def test_gh_exit_zero_without_a_limit_is_not_a_pass(tmp_path):
    run = FakeRun()
    original = run.__call__

    def empty_inside(argv, **kwargs):
        if list(argv[:2]) == ["codex", "sandbox"] and "gh" in argv:
            run.calls.append((True, ["gh"]))
            return _done(argv, 0, stdout="")
        return original(argv, **kwargs)

    probe = probe_codex_sandbox(tmp_path, run=empty_inside)
    assert [tool for tool, _ in probe.blocked] == ["gh"]


def test_a_long_first_line_keeps_both_its_ends():
    line = (
        "fatal: prepare: cannot lock ref 'refs/fno-probe/x': Unable to create '"
        + "/deep" * 40
        + "/x.lock': Operation not permitted"
    )
    kept = sandbox_probe._why(_done([], 128, stderr=f"{line}\nsecond line\n"))
    assert len(kept) <= 160
    assert kept.startswith("fatal: prepare: cannot lock ref")
    assert kept.endswith("Operation not permitted")


def test_a_ref_lock_only_the_sandbox_refuses_is_blocked(tmp_path):
    run = FakeRun(lock_inside=False)
    probe = probe_codex_sandbox(tmp_path, run=run)
    assert probe.verdict == "blocked"
    assert probe.blocked == [("git", EPERM)]
    assert run.every_transaction_aborts()


def test_a_ref_lock_that_fails_everywhere_is_unknown(tmp_path):
    probe = probe_codex_sandbox(tmp_path, run=FakeRun(lock_inside=False, lock_outside=False))
    assert probe.verdict == "unknown"
    assert "git fails outside the sandbox too" in probe.note


def test_a_control_that_never_ran_is_unknown_and_probes_nothing(tmp_path):
    def missing(argv):
        raise FileNotFoundError("codex")

    run = FakeRun(control=missing)
    probe = probe_codex_sandbox(tmp_path, run=run)
    assert probe.verdict == "unknown"
    assert not run.ran("gh")


def test_a_control_that_echoes_the_wrong_text_is_unknown(tmp_path):
    run = FakeRun(control=lambda argv: _done(argv, 2, stderr="error: unrecognized subcommand 'sandbox'\n"))
    probe = probe_codex_sandbox(tmp_path, run=run)
    assert probe.verdict == "unknown"
    assert "unrecognized subcommand" in probe.note


def test_outside_a_repo_the_git_row_is_skipped(tmp_path):
    run = FakeRun(repo=False)
    assert probe_codex_sandbox(tmp_path, run=run).verdict == "reachable"
    assert not run.ran("git", "update-ref")


@pytest.mark.skipif(
    shutil.which("codex") is None or shutil.which("gh") is None or sys.platform != "darwin",
    reason="needs the codex and gh binaries and macOS seatbelt",
)
@pytest.mark.parametrize("network,blocked", [("false", True), ("true", False)])
def test_live_codex_sandbox_tracks_the_network_setting(tmp_path, monkeypatch, network, blocked):
    home = tmp_path / "codex-home"
    home.mkdir()
    (home / "config.toml").write_text(
        f'sandbox_mode = "workspace-write"\n[sandbox_workspace_write]\nnetwork_access = {network}\n'
    )
    monkeypatch.setenv("CODEX_HOME", str(home))
    # The suite sandboxes HOME, which hides gh's login. The passwd home is the
    # real one, so gh can authenticate and a pass means the network answered.
    real_gh = Path(pwd.getpwuid(os.getuid()).pw_dir) / ".config" / "gh"
    if real_gh.is_dir():
        monkeypatch.setenv("GH_CONFIG_DIR", str(real_gh))
    repo = tmp_path / "repo"
    repo.mkdir()
    for argv in (
        ["git", "init", "-q"],
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"],
    ):
        subprocess.run(argv, cwd=repo, check=True)
    import fno.agents.harnesses.codex as codex

    common = str((repo / ".git").resolve())
    monkeypatch.setattr(
        codex,
        "git_writable_config_args",
        lambda cwd: ["-c", f'sandbox_workspace_write.writable_roots=["{common}"]'],
    )
    probe = probe_codex_sandbox(repo)
    assert probe.verdict != "unknown", probe.note
    assert ("gh" in [tool for tool, _ in probe.blocked]) is blocked, probe
    assert "git" not in [tool for tool, _ in probe.blocked], probe
    residue = subprocess.run(
        ["git", "for-each-ref", "refs/fno-probe"], cwd=repo, capture_output=True, text=True, check=True
    )
    assert residue.stdout == ""


@pytest.mark.skipif(
    shutil.which("codex") is None or sys.platform != "darwin",
    reason="needs the codex binary and macOS seatbelt",
)
def test_live_ref_lock_without_the_grant_is_blocked(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    repo.mkdir()
    for argv in (
        ["git", "init", "-q"],
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"],
    ):
        subprocess.run(argv, cwd=repo, check=True)
    probe = probe_codex_sandbox(repo)
    assert probe.verdict == "blocked", probe
    assert [tool for tool, _ in probe.blocked if tool == "git"] == ["git"], probe
    # git words the denial by what it could not make: the lock file when the
    # ref's directory exists, the directory when it does not.
    assert "cannot lock ref" in dict(probe.blocked)["git"]

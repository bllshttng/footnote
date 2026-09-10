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
EPERM = "fatal: cannot lock ref: Operation not permitted"


def _done(argv, returncode=0, stdout="", stderr=""):
    return subprocess.CompletedProcess(argv, returncode, stdout, stderr)


class FakeRun:
    """Answers each probe command by what it runs and where, and records every call.

    ``gh_inside``/``gh_outside`` and ``ref_inside``/``ref_outside`` say whether
    that tool works in that place.
    """

    def __init__(self, *, control=None, gh_inside=True, gh_outside=True,
                 ref_inside=True, ref_outside=True, repo=True):
        self.calls: list[tuple[bool, list[str]]] = []
        self.control = control
        self.gh = {True: gh_inside, False: gh_outside}
        self.ref = {True: ref_inside, False: ref_outside}
        self.repo = repo
        self.ref_written = False

    def __call__(self, argv, **kwargs):
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
        if cmd[:2] == ["git", "update-ref"] and "-d" in cmd:
            self.ref_written = False
            return _done(argv)
        if cmd[:2] == ["git", "update-ref"]:
            if self.ref[sandboxed]:
                self.ref_written = True
                return _done(argv)
            return _done(argv, 128, stderr=EPERM + "\n")
        if cmd[:2] == ["git", "rev-parse"]:
            return _done(argv, stdout=HEAD + "\n") if self.ref_written else _done(argv, 1)
        return _done(argv)

    def ran(self, *prefix: str, sandboxed=None) -> bool:
        return any(
            cmd[: len(prefix)] == list(prefix) and (sandboxed is None or where == sandboxed)
            for where, cmd in self.calls
        )


@pytest.fixture(autouse=True)
def _no_real_grant(monkeypatch):
    import fno.agents.harnesses.codex as codex

    monkeypatch.setattr(codex, "git_writable_config_args", lambda cwd: [])


def test_every_marker_present_reads_reachable(tmp_path):
    run = FakeRun()
    assert probe_codex_sandbox(tmp_path, run=run) == sandbox_probe.SandboxProbe("reachable")
    assert run.ran("git", "update-ref", "-d")
    assert not run.ran("gh", sandboxed=False)


def test_gh_that_answers_outside_but_not_inside_is_blocked(tmp_path):
    run = FakeRun(gh_inside=False)
    probe = probe_codex_sandbox(tmp_path, run=run)
    assert probe.verdict == "blocked"
    assert probe.blocked == [("gh", NO_NETWORK)]
    assert run.ran("git", "update-ref", "-d")


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


def test_a_ref_write_only_the_sandbox_refuses_is_blocked(tmp_path):
    run = FakeRun(ref_inside=False)
    probe = probe_codex_sandbox(tmp_path, run=run)
    assert probe.verdict == "blocked"
    assert probe.blocked == [("git", EPERM)]
    assert run.ran("git", "update-ref", "-d")


def test_a_ref_write_that_fails_everywhere_is_unknown(tmp_path):
    probe = probe_codex_sandbox(tmp_path, run=FakeRun(ref_inside=False, ref_outside=False))
    assert probe.verdict == "unknown"
    assert "git ref write fails outside the sandbox too" in probe.note


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

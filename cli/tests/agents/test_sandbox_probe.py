"""The codex sandbox probe bridge: the verdict is the Rust verb's answer, an
unavailable owner reads unknown, and a guessed reachable is impossible."""
from __future__ import annotations

import os
import pwd
import shutil
import sys
from pathlib import Path

import pytest

from fno.agents import sandbox_probe
from fno.agents.sandbox_probe import probe_codex_sandbox


def _stub_verb(monkeypatch, answer=None, raises=None):
    """Pin the Rust verb seam; record each request payload."""
    calls: list[dict] = []

    def call(verb, payload, _err):
        calls.append({"verb": verb, **payload})
        if raises is not None:
            raise raises
        return answer if answer is not None else {}

    import fno.rust_binary as rust

    monkeypatch.setattr(rust, "verb_call", call)
    return calls


def test_bridge_maps_the_envelopes_verdict_blocked_and_note(tmp_path, monkeypatch):
    calls = _stub_verb(
        monkeypatch,
        answer={"verdict": "blocked", "note": "no network grant",
                "blocked": [["gh", "error connecting"], ["git", "cannot lock"]]},
    )
    probe = probe_codex_sandbox(tmp_path)
    assert probe.verdict == "blocked"
    assert probe.blocked == [("gh", "error connecting"), ("git", "cannot lock")]
    assert probe.note == "no network grant"
    assert calls == [{"verb": "sandbox-probe", "cwd": str(tmp_path), "mode": None}]


def test_bridge_forwards_the_requested_posture_mode(tmp_path, monkeypatch):
    calls = _stub_verb(monkeypatch, answer={"verdict": "reachable"})
    probe_codex_sandbox(tmp_path, mode="danger-full-access")
    assert calls[0]["mode"] == "danger-full-access"


def test_an_absent_verdict_reads_unknown_never_reachable(tmp_path, monkeypatch):
    _stub_verb(monkeypatch, answer={"note": "half an answer"})
    probe = probe_codex_sandbox(tmp_path)
    assert probe.verdict == "unknown"


def test_an_unavailable_owner_is_unknown_with_the_reason(tmp_path, monkeypatch):
    from fno.rust_binary import VerbUnavailable

    _stub_verb(monkeypatch, raises=VerbUnavailable("no binary"))
    probe = probe_codex_sandbox(tmp_path)
    assert probe.verdict == "unknown"
    assert "no binary" in probe.note


def test_malformed_blocked_rows_are_dropped_not_fatal(tmp_path, monkeypatch):
    _stub_verb(
        monkeypatch,
        answer={"verdict": "blocked", "blocked": [["gh", "ok"], "junk", [1]]},
    )
    probe = probe_codex_sandbox(tmp_path)
    assert probe.blocked == [("gh", "ok")]


@pytest.mark.smoke
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
    import subprocess

    for argv in (
        ["git", "init", "-q"],
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"],
    ):
        subprocess.run(argv, cwd=repo, check=True)
    probe = probe_codex_sandbox(repo)
    assert probe.verdict != "unknown", probe.note
    assert ("gh" in [tool for tool, _ in probe.blocked]) is blocked, probe
    assert "git" not in [tool for tool, _ in probe.blocked], probe


@pytest.mark.smoke
@pytest.mark.skipif(
    shutil.which("codex") is None or sys.platform != "darwin",
    reason="needs the codex binary and macOS seatbelt",
)
def test_live_ref_lock_without_the_grant_is_blocked(tmp_path, monkeypatch):
    repo = tmp_path / "repo"
    repo.mkdir()
    import subprocess

    for argv in (
        ["git", "init", "-q"],
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"],
    ):
        subprocess.run(argv, cwd=repo, check=True)
    probe = probe_codex_sandbox(repo)
    assert probe.verdict == "blocked", probe

"""Regression for the autouse provider-exec guard (ab-c1bf3552).

The ``_block_live_provider_exec`` fixture in ``conftest.py`` must stop a test
that reaches the Python dispatch path without isolating PATH from exec'ing a
*real* claude/codex/gemini binary and leaking a live session (e.g. an immortal
``claude --bg``). The safe pattern - a fake binary on a tmp-isolated PATH - is
already exercised by the rest of the suite (those execs are allowed); here we
prove the complementary half: an un-isolated provider exec is blocked.
"""
from __future__ import annotations

import subprocess

import pytest

from fno.agents.harnesses import claude as _claude
from fno.agents.harnesses import codex as _codex


@pytest.mark.parametrize(
    "module,attr",
    [
        (_claude, "_subprocess_run"),
        (_codex, "_subprocess_popen"),
    ],
)
def test_guard_blocks_unisolated_provider_exec(module, attr, tmp_path, monkeypatch):
    """With no fake on PATH, a bare provider name resolves to the ambient real
    binary (or nothing) - never a tmp-isolated fake - so the guard must raise."""
    # PATH = an empty tmp dir: no fake claude/codex/gemini is reachable.
    monkeypatch.setenv("PATH", str(tmp_path))
    seam = getattr(module, attr)  # the autouse-patched guard wrapper
    name = "claude" if module is _claude else "codex"
    with pytest.raises(AssertionError, match="live provider exec blocked"):
        seam([name, "--bg", "--name", "leaktest", "hi"])


def test_guard_allows_fake_under_tmp(tmp_path, monkeypatch):
    """A fake provider binary under the pytest tmp tree resolves inside the temp
    root, so the guard passes the call through (no AssertionError). It runs the
    fake and returns its result - proving the discriminator is the binary's
    location, not the mere fact of a subprocess."""
    fake = tmp_path / "claude"
    fake.write_text("#!/bin/sh\nexit 0\n")
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path))
    result = _claude._subprocess_run(
        ["claude", "--version"], capture_output=True, text=True
    )
    assert result.returncode == 0


def test_guard_blocks_live_launchctl_mutation(tmp_path, monkeypatch):
    """A mutating launchctl verb resolved outside the tmp tree would change the
    operator's real launchd domain, so the guard must raise before any process
    starts (x-63aa: two unstubbed pr-watch install tests re-registered
    sh.fno.pr-watcher from a pytest tempdir and killed every launchd arm)."""
    monkeypatch.setenv("PATH", str(tmp_path))
    with pytest.raises(AssertionError, match="live launchctl mutation blocked"):
        subprocess.run(["launchctl", "bootstrap", "gui/501", "/tmp/x.plist"])


def test_guard_allows_fake_launchctl_and_read_verbs(tmp_path, monkeypatch):
    """A fake launchctl on a tmp-isolated PATH may run any verb, including a
    mutating one - the discriminator is binary location and verb, not the mere
    presence of launchctl."""
    fake = tmp_path / "launchctl"
    fake.write_text("#!/bin/sh\nexit 0\n")
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path))
    assert subprocess.run(["launchctl", "bootout", "gui/501/x"]).returncode == 0
    assert subprocess.run(["launchctl", "list"]).returncode == 0

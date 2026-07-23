"""Regression for the autouse provider-exec guard (ab-c1bf3552).

The ``_block_live_provider_exec`` fixture in ``conftest.py`` must stop a test
that reaches the Python dispatch path without isolating PATH from exec'ing a
*real* claude/codex/gemini binary and leaking a live session (e.g. an immortal
``claude --bg``). The safe pattern - a fake binary on a tmp-isolated PATH - is
already exercised by the rest of the suite (those execs are allowed); here we
prove the complementary half: an un-isolated provider exec is blocked.
"""
from __future__ import annotations

import pytest

from fno.agents.providers import claude as _claude
from fno.agents.providers import codex as _codex
from fno.agents.providers import gemini as _gemini


@pytest.mark.parametrize(
    "module,attr",
    [
        (_claude, "_subprocess_run"),
        (_codex, "_subprocess_popen"),
        (_gemini, "_subprocess_popen"),
    ],
)
def test_guard_blocks_unisolated_provider_exec(module, attr, tmp_path, monkeypatch):
    """With no fake on PATH, a bare provider name resolves to the ambient real
    binary (or nothing) - never a tmp-isolated fake - so the guard must raise."""
    # PATH = an empty tmp dir: no fake claude/codex/gemini is reachable.
    monkeypatch.setenv("PATH", str(tmp_path))
    seam = getattr(module, attr)  # the autouse-patched guard wrapper
    name = "claude" if module is _claude else ("codex" if module is _codex else "gemini")
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

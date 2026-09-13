"""Proof that the provider-exec guard covers tests OUTSIDE cli/tests/agents/.

The old guard lived in ``cli/tests/agents/conftest.py`` and a conftest guards
only its own directory, so 36 test files at or below the ``cli/tests/`` root
reached a spawn seam unguarded (x-ec81). This file therefore sits at the
``cli/tests/`` root ON PURPOSE: its location is the evidence for root
coverage. Moving it under ``agents/`` would prove nothing.
"""
from __future__ import annotations

import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest


def _write_fake_provider(bin_dir: Path, name: str = "claude") -> Path:
    fake = bin_dir / name
    fake.write_text("#!/bin/sh\necho fake-provider\n")
    fake.chmod(0o755)
    return fake


def test_guard_blocks_real_provider_outside_basetemp(monkeypatch):
    """A fake claude that resolves OUTSIDE the pytest tmp tree is a real
    install as far as the discriminator is concerned, so subprocess.run must
    raise before any process starts."""
    bin_dir = Path(tempfile.mkdtemp(prefix="fno-guard-proof-"))
    try:
        _write_fake_provider(bin_dir)
        monkeypatch.setenv("PATH", str(bin_dir))
        with pytest.raises(AssertionError, match="live provider exec blocked"):
            subprocess.run(["claude", "--version"], capture_output=True, text=True)
    finally:
        shutil.rmtree(bin_dir, ignore_errors=True)


def test_guard_allows_fake_provider_inside_basetemp(tmp_path, monkeypatch):
    """The same fake under tmp_path (inside the basetemp) runs: the
    discriminator is the binary's location, not the fact of a subprocess."""
    _write_fake_provider(tmp_path)
    monkeypatch.setenv("PATH", str(tmp_path))
    result = subprocess.run(["claude", "--version"], capture_output=True, text=True)
    assert result.returncode == 0
    assert "fake-provider" in result.stdout


def test_guard_allows_non_provider_outside_basetemp(monkeypatch):
    """Positive control for the two tests above: a binary outside the provider
    set runs from anywhere, so a green guard run here is not a guard that
    blocks everything."""
    bin_dir = Path(tempfile.mkdtemp(prefix="fno-guard-proof-"))
    try:
        bystander = bin_dir / "not-a-provider"
        bystander.write_text("#!/bin/sh\necho bystander\n")
        bystander.chmod(0o755)
        monkeypatch.setenv("PATH", str(bin_dir))
        result = subprocess.run(
            ["not-a-provider"], capture_output=True, text=True
        )
        assert result.returncode == 0
        assert "bystander" in result.stdout
    finally:
        shutil.rmtree(bin_dir, ignore_errors=True)

"""Tests for the /do orchestrator's explicit-harness input (AC15).

AC15-HP: a Codex session with ``CODEX_PLUGIN_ROOT`` unset, running normal wave
execution with an explicit Codex harness argument, must take the harness from
the explicit argument - never from the env sniff. The env sniff stays as a
SURFACED fallback (``source == "env-fallback"``) so an absent signal cannot
silently read as a deliberate choice and redirect the waves to Claude.
"""
from __future__ import annotations

import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
ORCH_DIR = REPO_ROOT / "skills" / "do"
if str(ORCH_DIR) not in sys.path:
    sys.path.insert(0, str(ORCH_DIR))

import orchestrator  # noqa: E402  (skills/do is not a package; added to sys.path)


def test_explicit_harness_wins_with_env_absent():
    # AC15: explicit codex, no env signal -> codex via explicit, not claude fallback.
    harness, source = orchestrator.resolve_invoking_harness("codex", env={})
    assert harness == "codex"
    assert source == "explicit"


def test_no_explicit_and_no_env_falls_back_to_claude_and_says_so():
    harness, source = orchestrator.resolve_invoking_harness(None, env={})
    assert harness == "claude"
    assert source == "env-fallback"


def test_env_sniff_runs_only_when_no_explicit_value():
    # CODEX_PLUGIN_ROOT is honored, but only on the fallback path.
    harness, source = orchestrator.resolve_invoking_harness(
        None, env={"CODEX_PLUGIN_ROOT": "/x"}
    )
    assert harness == "codex"
    assert source == "env-fallback"


def test_explicit_wins_over_a_conflicting_env_signal():
    # A gemini env signal must not override an explicit codex argument.
    harness, source = orchestrator.resolve_invoking_harness(
        "codex", env={"GEMINI_PROJECT_DIR": "/x"}
    )
    assert harness == "codex"
    assert source == "explicit"


def test_unknown_explicit_harness_is_rejected():
    # An invalid explicit value fails closed rather than silently falling through.
    with pytest.raises(ValueError):
        orchestrator.resolve_invoking_harness("loop", env={})


def test_resolve_wave_execution_mode_surfaces_harness_source():
    # The wave decision carries the source so a sniffed harness reads as a fallback.
    wave = orchestrator.Wave(number=1, mode="sequential", tasks=["3.1"], reason="t")
    decision = orchestrator.resolve_wave_execution_mode(
        wave, plan_path="irrelevant", provider="codex"
    )
    assert decision["provider"] == "codex"
    assert decision["harness_source"] == "explicit"


def test_resolve_wave_execution_mode_marks_env_fallback():
    wave = orchestrator.Wave(number=1, mode="sequential", tasks=["3.1"], reason="t")
    decision = orchestrator.resolve_wave_execution_mode(
        wave, plan_path="irrelevant", provider=None
    )
    assert decision["provider"] == orchestrator.detect_provider()
    assert decision["harness_source"] == "env-fallback"

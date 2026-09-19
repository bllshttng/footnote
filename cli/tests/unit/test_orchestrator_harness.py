"""Tests for the /execute orchestrator's harness resolution (AC7).

AC7-HP: an explicit ``opencode`` resolves; no unknown-harness error.
AC7-ERR: an environment carrying only ``CODEX_THREAD_ID`` or only
``OPENCODE_SESSION_ID`` resolves to codex/opencode with source ``env-marker``.
AC7-EDGE: an environment with no harness marker answers claude with source
``env-default``, distinguishable from a positively identified marker.
"""
from __future__ import annotations

import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
ORCH_DIR = REPO_ROOT / "skills" / "execute"
if str(ORCH_DIR) not in sys.path:
    sys.path.insert(0, str(ORCH_DIR))

import orchestrator  # noqa: E402  (skills/execute is not a package; added to sys.path)


def test_explicit_harness_wins_with_env_absent():
    harness, source = orchestrator.resolve_invoking_harness("codex", env={})
    assert harness == "codex"
    assert source == "explicit"


def test_explicit_opencode_resolves_against_the_canonical_roster():
    # AC7-HP: opencode is a shipped harness the private roster used to refuse.
    harness, source = orchestrator.resolve_invoking_harness("opencode", env={})
    assert harness == "opencode"
    assert source == "explicit"


def test_no_explicit_and_no_marker_answers_claude_as_env_default():
    # AC7-EDGE: the default is distinguishable from a positive identification.
    harness, source = orchestrator.resolve_invoking_harness(None, env={})
    assert harness == "claude"
    assert source == "env-default"


def test_codex_thread_id_marker_resolves_to_codex():
    # AC7-ERR: the marker the old two-variable sniff never read.
    harness, source = orchestrator.resolve_invoking_harness(
        None, env={"CODEX_THREAD_ID": "thread-123"}
    )
    assert harness == "codex"
    assert source == "env-marker"


def test_opencode_session_id_marker_resolves_to_opencode():
    # AC7-ERR: the marker the old two-variable sniff never read.
    harness, source = orchestrator.resolve_invoking_harness(
        None, env={"OPENCODE_SESSION_ID": "ses_abc"}
    )
    assert harness == "opencode"
    assert source == "env-marker"


def test_conflicting_family_markers_degrade_to_env_default():
    # Mixed families refuse to guess; the answer is the named default.
    harness, source = orchestrator.resolve_invoking_harness(
        None, env={"CODEX_THREAD_ID": "t1", "CLAUDE_CODE_SESSION_ID": "c1"}
    )
    assert harness == "claude"
    assert source == "env-default"


def test_explicit_wins_over_a_conflicting_env_signal():
    # A gemini env marker must not override an explicit codex argument.
    harness, source = orchestrator.resolve_invoking_harness(
        "codex", env={"GEMINI_SESSION_ID": "g1"}
    )
    assert harness == "codex"
    assert source == "explicit"


def test_unknown_explicit_harness_is_rejected():
    # An invalid explicit value fails closed rather than silently falling through.
    with pytest.raises(ValueError):
        orchestrator.resolve_invoking_harness("loop", env={})


def test_resolve_wave_execution_mode_surfaces_harness_source():
    wave = orchestrator.Wave(number=1, mode="sequential", tasks=["3.1"], reason="t")
    decision = orchestrator.resolve_wave_execution_mode(
        wave, plan_path="irrelevant", provider="codex"
    )
    assert decision["provider"] == "codex"
    assert decision["harness_source"] == "explicit"


def test_resolve_wave_execution_mode_marks_env_default():
    wave = orchestrator.Wave(number=1, mode="sequential", tasks=["3.1"], reason="t")
    decision = orchestrator.resolve_wave_execution_mode(
        wave, plan_path="irrelevant", provider=None
    )
    assert decision["provider"] == "claude"
    assert decision["harness_source"] == "env-default"

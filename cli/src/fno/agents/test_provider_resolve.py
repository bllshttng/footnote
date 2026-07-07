"""Unit tests for the shared dispatch provider/model resolver.

Covers the precedence chain, harness inference, and the empty-flag rejections
behind AC1-ERR / AC2-ERR / AC1-EDGE.
"""

from __future__ import annotations

import pytest

from fno.agents.provider_resolve import (
    DispatchFlagError,
    infer_invoking_harness,
    reject_empty_model,
    resolve_dispatch_provider,
)

_ALL_MARKERS = ("CLAUDE_CODE_SESSION_ID", "CODEX_SESSION_ID", "GEMINI_SESSION_ID")


@pytest.fixture(autouse=True)
def _clear_harness_env(monkeypatch):
    """Every test starts with no ambient harness marker set."""
    for m in _ALL_MARKERS:
        monkeypatch.delenv(m, raising=False)


# --- explicit provider (top of precedence) --------------------------------


@pytest.mark.parametrize("prov", ["claude", "codex", "gemini"])
def test_explicit_valid_provider(prov):
    assert resolve_dispatch_provider(prov) == (prov, "explicit")


def test_explicit_beats_harness_marker():
    # AC2-EDGE-adjacent: an explicit flag outranks the inferred harness.
    env = {"CODEX_SESSION_ID": "abc"}
    assert resolve_dispatch_provider("claude", env=env) == ("claude", "explicit")


def test_explicit_unknown_provider_passed_through():
    # The resolver does NOT validate the provider set (substrate-aware downstream
    # check owns AC1-ERR); it returns any non-empty explicit value verbatim.
    assert resolve_dispatch_provider("opencode") == ("opencode", "explicit")


def test_explicit_empty_provider_rejected():
    with pytest.raises(DispatchFlagError):
        resolve_dispatch_provider("   ")


# --- harness inference (middle) -------------------------------------------


@pytest.mark.parametrize(
    "marker,expected",
    [
        ("CLAUDE_CODE_SESSION_ID", "claude"),
        ("CODEX_SESSION_ID", "codex"),
        ("GEMINI_SESSION_ID", "gemini"),
    ],
)
def test_harness_inferred_from_marker(marker, expected):
    assert resolve_dispatch_provider(None, env={marker: "sid"}) == (
        expected,
        "harness-inferred",
    )


def test_multiple_markers_are_ambiguous():
    # Inference never guesses: two markers -> None (caller falls to builtin).
    env = {"CODEX_SESSION_ID": "b", "GEMINI_SESSION_ID": "c"}
    assert infer_invoking_harness(env) is None
    assert resolve_dispatch_provider(None, env=env) == ("claude", "builtin-default")


def test_whitespace_marker_is_absent():
    assert infer_invoking_harness({"CLAUDE_CODE_SESSION_ID": "  "}) is None
    # A whitespace-only marker does not count toward the ambiguity tally either.
    env = {"CLAUDE_CODE_SESSION_ID": "sid", "CODEX_SESSION_ID": "  "}
    assert infer_invoking_harness(env) == "claude"


# --- builtin default (bottom) ---------------------------------------------


def test_no_marker_falls_to_builtin_claude():
    # AC1-EDGE: bare shell, no markers, no config default -> claude/builtin.
    assert resolve_dispatch_provider(None, env={}) == ("claude", "builtin-default")


# --- model flag validation -------------------------------------------------


def test_model_none_passes_through():
    assert reject_empty_model(None) is None


@pytest.mark.parametrize("bad", ["", "   ", "\t"])
def test_empty_model_rejected(bad):
    # AC2-ERR: empty/whitespace model is a usage error.
    with pytest.raises(DispatchFlagError):
        reject_empty_model(bad)


@pytest.mark.parametrize("name", ["glm-4.7", "claude-sonnet-5", "zai,glm-5.2", "gpt-5:codex"])
def test_model_exact_passthrough(name):
    # Invariant: dots/colons/commas/dashes survive verbatim (no fuzzy resolve).
    assert reject_empty_model(name) == name

"""Direct pins for the observation walker's marker identity (``_identity``).

The walker feeds observation rows; a mismatched (harness, session_id) pair
here is a foreign id laundered into the record, the same defect class the
identity resolvers refuse.
"""

from __future__ import annotations

import pytest

from fno.context_observation import _identity
from fno.harness_identity import HARNESS_SESSION_MARKERS

_IDENTITY_ENV = [marker for marker, _ in HARNESS_SESSION_MARKERS] + [
    "CLAUDE_SESSION_ID",
    "FNO_PLATFORM",
    "CLAUDE_PLUGIN_ROOT",
    "GEMINI_PROJECT_DIR",
    "CODEX_PLUGIN_ROOT",
]


@pytest.fixture(autouse=True)
def _clean_identity_env(monkeypatch):
    for name in _IDENTITY_ENV:
        monkeypatch.delenv(name, raising=False)


def test_conflicted_family_blocks_a_later_family_refill(monkeypatch):
    # Measured regression: a codex family that disagrees clears its id, and
    # the next family's marker refilled the slot, recording a gemini id
    # under a codex harness.
    monkeypatch.setenv("CODEX_THREAD_ID", "aaaa-1111")
    monkeypatch.setenv("CODEX_SESSION_ID", "bbbb-2222")
    monkeypatch.setenv("GEMINI_SESSION_ID", "gggg-3333")

    session_id, harness, _entry, _digest = _identity(b"{}", None)

    assert harness == "codex"
    assert session_id == ""


def test_single_clean_family_still_resolves(monkeypatch):
    monkeypatch.setenv("CODEX_THREAD_ID", "aaaa-1111")

    session_id, harness, _entry, _digest = _identity(b"{}", None)

    assert (session_id, harness) == ("aaaa-1111", "codex")


def test_conflicted_pick_clears_the_slot_and_stays_empty(monkeypatch):
    # Table order puts codex first, so the codex family holds the slot; when
    # it later disagrees the slot clears and STAYS empty - the already-seen
    # clean claude sibling must not refill it.
    monkeypatch.setenv("CODEX_THREAD_ID", "t-1")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "claude-1")
    monkeypatch.setenv("CODEX_SESSION_ID", "t-2")

    session_id, harness, _entry, _digest = _identity(b"{}", None)

    assert (session_id, harness) == ("", "codex")

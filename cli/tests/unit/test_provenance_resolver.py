"""Unit tests for fno.provenance.resolver (Task 2.3).

Tests the resolve_transcript() function with an injectable projects_root so
tests never touch ~/.claude.

Acceptance criteria:
  AC-HP : claude pointer + matching on-disk .jsonl -> resolved=True, real path
          Both exact-id match and 8-hex-prefix glob match are covered.
  AC-EDGE: foreign harness (codex/gemini/etc.) -> resolved=False, no raise.
           Claude pointer whose file does NOT exist -> resolved=False, no raise.
"""
from __future__ import annotations

from pathlib import Path

import pytest


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_projects_root(tmp_path: Path, cwd: str, session_id: str) -> Path:
    """Create a fake ~/.claude/projects/<slug>/<session_id>.jsonl and return
    the projects_root so the resolver can find it."""
    slug = cwd.replace("/", "-").replace(".", "-")
    proj_dir = tmp_path / slug
    proj_dir.mkdir(parents=True, exist_ok=True)
    (proj_dir / f"{session_id}.jsonl").write_text('{"type":"summary"}\n')
    return tmp_path


# ---------------------------------------------------------------------------
# AC-HP: exact session_id match
# ---------------------------------------------------------------------------

def test_ac_hp_exact_id_match(tmp_path):
    """AC-HP: exact session_id + matching .jsonl -> resolved=True, correct path."""
    from fno.provenance.resolver import resolve_transcript

    cwd = "/Users/bb16/code/me/abilities"
    session_id = "4ec8a08b-9fe7-4550-8e40-00c7fd4e600a"
    projects_root = _make_projects_root(tmp_path, cwd, session_id)

    result = resolve_transcript("claude", session_id, cwd, projects_root=projects_root)

    assert result.resolved is True
    assert result.transcript_path is not None
    assert result.transcript_path.endswith(f"{session_id}.jsonl")
    assert result.harness == "claude"
    assert result.session_id == session_id
    assert result.cwd == cwd


def test_ac_hp_slug_leading_slash_to_dash(tmp_path):
    """AC-HP: leading '/' in cwd becomes leading '-' in the slug (confirmed example)."""
    from fno.provenance.resolver import resolve_transcript

    cwd = "/Users/bb16/code/me/abilities"
    session_id = "4ec8a08b-9fe7-4550-8e40-00c7fd4e600a"

    # Build the tree manually to confirm slug calculation
    slug = "-Users-bb16-code-me-abilities"
    proj_dir = tmp_path / slug
    proj_dir.mkdir(parents=True, exist_ok=True)
    (proj_dir / f"{session_id}.jsonl").write_text("{}\n")

    result = resolve_transcript("claude", session_id, cwd, projects_root=tmp_path)
    assert result.resolved is True


# ---------------------------------------------------------------------------
# AC-HP: 8-hex prefix glob match
# ---------------------------------------------------------------------------

def test_ac_hp_prefix_glob_single_match(tmp_path):
    """AC-HP: 8-hex prefix + single matching file -> resolved=True."""
    from fno.provenance.resolver import resolve_transcript

    cwd = "/Users/bb16/code/footnote"
    full_id = "abcdef12-1234-5678-9abc-def012345678"
    prefix = "abcdef12"  # 8-char prefix

    projects_root = _make_projects_root(tmp_path, cwd, full_id)

    result = resolve_transcript("claude", prefix, cwd, projects_root=projects_root)

    assert result.resolved is True
    assert result.transcript_path is not None
    assert full_id in result.transcript_path


def test_ac_hp_prefix_glob_multiple_matches_returns_first_sorted(tmp_path):
    """AC-HP: prefix matches multiple files -> resolved=True, first sorted, ambiguous note."""
    from fno.provenance.resolver import resolve_transcript

    cwd = "/Users/bb16/code/footnote"
    prefix = "abcdef12"
    cwd_slug = cwd.replace("/", "-").replace(".", "-")
    proj_dir = tmp_path / cwd_slug
    proj_dir.mkdir(parents=True, exist_ok=True)

    id_a = f"{prefix}-aaaa-1111-2222-333333333333"
    id_b = f"{prefix}-bbbb-1111-2222-444444444444"
    (proj_dir / f"{id_a}.jsonl").write_text("{}\n")
    (proj_dir / f"{id_b}.jsonl").write_text("{}\n")

    result = resolve_transcript("claude", prefix, cwd, projects_root=tmp_path)

    assert result.resolved is True
    # First sorted match
    assert id_a in result.transcript_path
    # Ambiguity is noted
    assert result.ambiguous is True


# ---------------------------------------------------------------------------
# AC-EDGE: file not found
# ---------------------------------------------------------------------------

def test_ac_edge_claude_file_not_found(tmp_path):
    """AC-EDGE: claude pointer with no matching file -> resolved=False, no raise."""
    from fno.provenance.resolver import resolve_transcript

    result = resolve_transcript(
        "claude",
        "99999999-0000-0000-0000-000000000000",
        "/Users/bb16/code/missing",
        projects_root=tmp_path,  # empty - no files
    )

    assert result.resolved is False
    assert result.transcript_path is None
    # Never raises; harness/session/cwd echoed back
    assert result.harness == "claude"


# ---------------------------------------------------------------------------
# AC-EDGE: foreign harnesses
# ---------------------------------------------------------------------------

@pytest.mark.parametrize("harness", ["codex", "gemini", "antigravity", "weird", "openai"])
def test_ac_edge_foreign_harness_not_supported(tmp_path, harness):
    """AC-EDGE: any non-claude harness -> resolved=False, no raise, reason set."""
    from fno.provenance.resolver import resolve_transcript

    result = resolve_transcript(
        harness,
        "some-session-id",
        "/some/cwd",
        projects_root=tmp_path,
    )

    assert result.resolved is False
    assert result.transcript_path is None
    assert result.reason == "harness-not-supported"
    # Never raises


# ---------------------------------------------------------------------------
# AC-EDGE: missing/empty inputs
# ---------------------------------------------------------------------------

def test_ac_edge_empty_session_id(tmp_path):
    """AC-EDGE: empty session_id -> resolved=False, no raise."""
    from fno.provenance.resolver import resolve_transcript

    result = resolve_transcript("claude", "", "/some/cwd", projects_root=tmp_path)
    assert result.resolved is False


def test_ac_edge_none_session_id(tmp_path):
    """AC-EDGE: None session_id -> resolved=False, no raise."""
    from fno.provenance.resolver import resolve_transcript

    result = resolve_transcript("claude", None, "/some/cwd", projects_root=tmp_path)
    assert result.resolved is False


def test_ac_edge_none_cwd(tmp_path):
    """AC-EDGE: None cwd -> resolved=False, no raise."""
    from fno.provenance.resolver import resolve_transcript

    result = resolve_transcript("claude", "some-session", None, projects_root=tmp_path)
    assert result.resolved is False


# ---------------------------------------------------------------------------
# Return type is JSON-serializable
# ---------------------------------------------------------------------------

def test_result_is_json_serializable(tmp_path):
    """Result dataclass fields are JSON-serializable (for --json output in CLI)."""
    import json
    import dataclasses
    from fno.provenance.resolver import resolve_transcript

    result = resolve_transcript("codex", "sid", "/cwd", projects_root=tmp_path)
    # Must not raise
    serialized = json.dumps(dataclasses.asdict(result))
    parsed = json.loads(serialized)
    assert parsed["resolved"] is False

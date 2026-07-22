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

    cwd = "/Users/bb16/code/me/fno"
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

    cwd = "/Users/bb16/code/me/fno"
    session_id = "4ec8a08b-9fe7-4550-8e40-00c7fd4e600a"

    # Build the tree manually to confirm slug calculation
    slug = "-Users-bb16-code-me-fno"
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
# AC1-HP (x-a472): transcript resolution spans project dirs, newest wins
# ---------------------------------------------------------------------------

def test_worktree_transcript_wins_over_stale_canonical_stub(tmp_path):
    """AC1-HP: after EnterWorktree the live transcript lives in a worktree-keyed
    project dir while a stale stub sits in the canonical dir. The resolver must
    return the WORKTREE transcript (newest mtime), not the canonical stub -- the
    original blind-after-EnterWorktree bug."""
    import os

    from fno.provenance.resolver import resolve_transcript

    canonical = "/Users/bb16/code/footnote/footnote"
    worktree = "/Users/bb16/code/footnote/footnote/.claude/worktrees/x-a472"
    session_id = "4ec8a08b-9fe7-4550-8e40-00c7fd4e600a"

    # Session was dispatched with the canonical cwd (what the roster/registry
    # records), so that is the cwd the caller passes.
    canon_dir = tmp_path / canonical.replace("/", "-").replace(".", "-")
    wt_dir = tmp_path / worktree.replace("/", "-").replace(".", "-")
    canon_dir.mkdir(parents=True)
    wt_dir.mkdir(parents=True)
    stub = canon_dir / f"{session_id}.jsonl"
    live = wt_dir / f"{session_id}.jsonl"
    stub.write_text("")  # empty canonical stub, written at dispatch
    live.write_text('{"type":"assistant"}\n')
    # Make the worktree transcript strictly newer.
    os.utime(stub, (1000, 1000))
    os.utime(live, (2000, 2000))

    result = resolve_transcript("claude", session_id, canonical, projects_root=tmp_path)

    assert result.resolved is True
    assert result.transcript_path == str(live)
    assert result.ambiguous is False


def test_transcript_resolves_when_only_worktree_dir_has_it(tmp_path):
    """AC1-HP: the canonical dir has no stub at all (session only ever wrote in
    the worktree). The cwd-slug dir misses; the store-wide search still finds it."""
    from fno.provenance.resolver import resolve_transcript

    canonical = "/Users/bb16/code/footnote/footnote"
    worktree = "/Users/bb16/code/footnote/footnote/.claude/worktrees/x-a472"
    session_id = "abcdef12-1234-5678-9abc-def012345678"

    wt_dir = tmp_path / worktree.replace("/", "-").replace(".", "-")
    wt_dir.mkdir(parents=True)
    (wt_dir / f"{session_id}.jsonl").write_text('{"type":"assistant"}\n')

    result = resolve_transcript("claude", session_id, canonical, projects_root=tmp_path)

    assert result.resolved is True
    assert result.transcript_path.endswith(f"{session_id}.jsonl")


def _conv(text: str) -> str:
    import json
    return json.dumps({"type": "assistant", "message": {"role": "assistant",
                       "content": [{"type": "text", "text": text}]}})


def _meta_stub() -> str:
    import json
    # header-only records CC writes; NO user/assistant turn -> a metadata stub.
    return json.dumps({"type": "last-prompt", "sessionId": "x"})


def test_newer_metadata_stub_loses_to_older_real_transcript(tmp_path):
    """AC1-HP (codex P1): a NEWER worktree metadata-only stub must NOT win over
    an OLDER canonical transcript that actually holds the conversation. mtime
    alone would pick the empty stub and re-open the blind-peek failure."""
    import os

    from fno.provenance.resolver import resolve_transcript

    canonical = "/Users/bb16/code/footnote/footnote"
    worktree = "/Users/bb16/code/footnote/footnote/.claude/worktrees/x-a472"
    sid = "4ec8a08b-9fe7-4550-8e40-00c7fd4e600a"

    canon_dir = tmp_path / canonical.replace("/", "-").replace(".", "-")
    wt_dir = tmp_path / worktree.replace("/", "-").replace(".", "-")
    canon_dir.mkdir(parents=True)
    wt_dir.mkdir(parents=True)
    real = canon_dir / f"{sid}.jsonl"
    stub = wt_dir / f"{sid}.jsonl"
    real.write_text(_conv("actual conversation here") + "\n")
    stub.write_text(_meta_stub() + "\n")
    os.utime(real, (1000, 1000))  # older
    os.utime(stub, (2000, 2000))  # newer, but content-free

    result = resolve_transcript("claude", sid, worktree, projects_root=tmp_path)

    assert result.resolved is True
    assert result.transcript_path == str(real)  # content wins over newer mtime


def test_full_uuid_ignores_orphaned_and_syncconflict_siblings(tmp_path):
    """AC1-EDGE (codex P2): sibling artifacts sharing the uuid prefix
    (`<uuid>.orphaned-*`, `<uuid>.sync-conflict-*`) must not make a full-uuid
    resolution ambiguous or get chosen over the real `<uuid>.jsonl`."""
    from fno.provenance.resolver import resolve_transcript

    cwd = "/Users/bb16/code/footnote"
    sid = "abcdef12-1234-5678-9abc-def012345678"
    d = tmp_path / cwd.replace("/", "-").replace(".", "-")
    d.mkdir(parents=True)
    real = d / f"{sid}.jsonl"
    real.write_text(_conv("real") + "\n")
    (d / f"{sid}.orphaned-2026.jsonl").write_text(_conv("orphan") + "\n")
    (d / f"{sid}.sync-conflict-20260101.jsonl").write_text(_conv("dup") + "\n")

    result = resolve_transcript("claude", sid, cwd, projects_root=tmp_path)

    assert result.resolved is True
    assert result.transcript_path == str(real)
    assert result.ambiguous is False


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

@pytest.mark.parametrize("harness", ["gemini", "antigravity", "weird", "openai"])
def test_ac_edge_foreign_harness_not_supported(tmp_path, harness):
    """AC-EDGE: an unsupported harness -> resolved=False, no raise, reason set.

    codex/opencode are supported now (AC4-HP); agy/gemini/etc. stay unsupported.
    """
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


def test_glob_metachar_session_id_does_not_overmatch(tmp_path):
    """gemini review: a session_id with a glob metachar is escaped, not expanded.

    An unescaped '*' would match every transcript in the dir; with glob.escape
    the id is a literal prefix, so a real-but-unrelated transcript is not returned.
    """
    from fno.provenance.resolver import resolve_transcript

    proj = tmp_path / "-cwd"
    proj.mkdir()
    (proj / "real-session.jsonl").write_text("{}", encoding="utf-8")

    result = resolve_transcript("claude", "*", "/cwd", projects_root=tmp_path)
    assert result.resolved is False
    assert result.transcript_path is None


# ---------------------------------------------------------------------------
# Return type is JSON-serializable
# ---------------------------------------------------------------------------

def test_result_is_json_serializable(tmp_path):
    """Result dataclass fields are JSON-serializable (for --json output in CLI)."""
    import json
    import dataclasses
    from fno.provenance.resolver import resolve_transcript

    # Inject an empty codex root so this never scans the real ~/.codex.
    result = resolve_transcript(
        "codex", "sid", "/cwd", projects_root=tmp_path, codex_sessions_dir=tmp_path
    )
    # Must not raise
    serialized = json.dumps(dataclasses.asdict(result))
    parsed = json.loads(serialized)
    assert parsed["resolved"] is False
    assert parsed["kind"] == "jsonl"  # default field is JSON-serializable


# ---------------------------------------------------------------------------
# AC4-HP: codex + opencode arms resolve against injected stores
# ---------------------------------------------------------------------------

def _write_codex_rollout(sessions_dir: Path, session_id: str, cwd: str) -> Path:
    """Fake ~/.codex/sessions/YYYY/MM/DD/rollout-...<uuid>.jsonl with a
    session_meta line 1 (the real codex 0.1x shape)."""
    import json as _json

    day = sessions_dir / "2026" / "07" / "21"
    day.mkdir(parents=True, exist_ok=True)
    # Rollout filename embeds the session id (codex convention -> fast path).
    path = day / f"rollout-2026-07-21T00-00-00-{session_id}.jsonl"
    path.write_text(
        _json.dumps({"type": "session_meta", "payload": {"id": session_id, "cwd": cwd}})
        + "\n",
        encoding="utf-8",
    )
    return path


def test_ac4_codex_resolves_by_filename(tmp_path):
    """AC4-HP: codex rollout embedding the session uuid resolves, kind=jsonl."""
    from fno.provenance.resolver import resolve_transcript

    sid = "019f837c-3461-7911-811f-3290b8b34934"
    cwd = "/Users/bb16/code/footnote"
    rollout = _write_codex_rollout(tmp_path, sid, cwd)

    result = resolve_transcript("codex", sid, cwd, codex_sessions_dir=tmp_path)

    assert result.resolved is True
    assert result.transcript_path == str(rollout)
    assert result.kind == "jsonl"


def test_ac4_codex_resolves_by_session_meta_fallback(tmp_path):
    """AC4-HP: a rollout named by a turn id still resolves via session_meta."""
    import json as _json
    from fno.provenance.resolver import resolve_transcript

    sid = "019f837c-3461-7911-811f-3290b8b34934"
    cwd = "/Users/bb16/code/footnote"
    day = tmp_path / "2026" / "07" / "21"
    day.mkdir(parents=True, exist_ok=True)
    # Filename carries a TURN id, not the session id -> forces the meta fallback.
    path = day / "rollout-2026-07-21T00-00-00-aaaaaaaa-turn-0000-0000-000000000000.jsonl"
    path.write_text(
        _json.dumps({"type": "session_meta", "payload": {"id": sid, "cwd": cwd}}) + "\n",
        encoding="utf-8",
    )

    result = resolve_transcript("codex", sid, cwd, codex_sessions_dir=tmp_path)

    assert result.resolved is True
    assert result.transcript_path == str(path)


def test_ac4_codex_not_found(tmp_path):
    """AC4-HP: no matching rollout -> resolved=False, no raise."""
    from fno.provenance.resolver import resolve_transcript

    result = resolve_transcript(
        "codex", "no-such-sid", "/cwd", codex_sessions_dir=tmp_path
    )
    assert result.resolved is False
    assert result.reason == "not-found"


def _make_opencode_db(tmp_path: Path, session_id: str) -> Path:
    """Minimal opencode.db with the real session/message/part shape."""
    import sqlite3

    db = tmp_path / "opencode.db"
    con = sqlite3.connect(db)
    con.executescript(
        """
        CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT,
                              time_updated INTEGER);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT,
                              time_created INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT,
                           session_id TEXT, time_created INTEGER, data TEXT);
        """
    )
    con.execute(
        "INSERT INTO session VALUES (?,?,?)", (session_id, "/Users/bb16", 1784093335827)
    )
    con.commit()
    con.close()
    return db


def test_ac4_opencode_resolves(tmp_path):
    """AC4-HP: an opencode session id present in the store resolves, kind=opencode-db."""
    from fno.provenance.resolver import resolve_transcript

    db = _make_opencode_db(tmp_path, "ses_test")
    result = resolve_transcript("opencode", "ses_test", None, opencode_db_path=db)

    assert result.resolved is True
    assert result.transcript_path == str(db)
    assert result.kind == "opencode-db"


def test_ac4_opencode_id_not_case_folded(tmp_path):
    """Domain pitfall: ses_ ids are mixed-case and must never be folded."""
    from fno.provenance.resolver import resolve_transcript

    db = _make_opencode_db(tmp_path, "ses_MixedCase")
    miss = resolve_transcript("opencode", "ses_mixedcase", None, opencode_db_path=db)
    assert miss.resolved is False
    hit = resolve_transcript("opencode", "ses_MixedCase", None, opencode_db_path=db)
    assert hit.resolved is True


def test_ac4_opencode_missing_db(tmp_path):
    """AC4-HP: absent store -> resolved=False, no raise."""
    from fno.provenance.resolver import resolve_transcript

    result = resolve_transcript(
        "opencode", "ses_x", None, opencode_db_path=tmp_path / "nope.db"
    )
    assert result.resolved is False
    assert result.reason == "not-found"

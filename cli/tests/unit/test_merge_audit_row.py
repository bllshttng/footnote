"""The merge's own audit row: the record that says a merge consulted the gate.

`fno do pr merge` builds a `session_satisfied{source:pr_merge}` event, and it
had produced zero rows in a 21605-event journal - not zero for one PR, zero for
every merge ever taken. Three silent early returns guarded the emit, and the
first of them fired every time: the session manifest moved to the space while
this path still looked for it beside the checkout.

Every assertion here is a POSITIVE marker (a row, a field value), never the
absence of a diagnostic.
"""

from pathlib import Path

import pytest

from fno.pr import _merge


def _rows(path: Path) -> list[dict]:
    from tests._event_rows import event_rows

    return event_rows(path)


@pytest.fixture()
def journal(tmp_path: Path, monkeypatch) -> Path:
    """The journal the RESOLVER answers, which is what an audit reads.

    The row used to be written to `<checkout>/.fno/events.jsonl` instead. That
    file is a plain file the post-merge worktree reap deletes, so a row landing
    there is as unauditable as no row at all - the defect this emit exists to
    close. Pinning the resolver here is what proves the destination.
    """
    path = tmp_path / "space" / "events.jsonl"
    path.parent.mkdir(parents=True)
    monkeypatch.setattr("fno.paths.project_events_json", lambda *a, **kw: path)
    return path


def test_emit_writes_a_row_when_the_manifest_is_absent(tmp_path: Path, journal: Path) -> None:
    """The absent-manifest case: the one that ate every merge row."""
    _merge._emit_session_satisfied(
        "https://github.com/o/r/pull/1", str(tmp_path / ".fno" / "target-state.md")
    )

    rows = _rows(journal)
    assert len(rows) == 1, rows
    data = rows[0]["data"]
    assert rows[0]["type"] == "session_satisfied"
    assert data["source"] == "pr_merge"
    assert data["reason"] == "pr_merged"
    # Degraded, and NAMED as degraded. The auto-complete matcher requires both
    # of these to equal the live session's values, so a sentinel row can never
    # be adopted as a completion signal.
    assert data["session_id"] == _merge._MERGE_ROW_UNKNOWN
    assert data["gate_state_hash"] == _merge._MERGE_ROW_UNKNOWN


def test_emit_carries_the_real_session_when_the_manifest_is_readable(
    tmp_path: Path, journal: Path
) -> None:
    manifest = tmp_path / "target-state.md"
    manifest.write_text("session_id: sess-42\nharness: claude\n", encoding="utf-8")

    _merge._emit_session_satisfied("", str(manifest))

    data = _rows(journal)[0]["data"]
    assert data["session_id"] == "sess-42"
    assert data["gate_state_hash"] != _merge._MERGE_ROW_UNKNOWN
    assert len(data["gate_state_hash"]) == 32


def test_manifest_file_prefers_the_space_slice(tmp_path: Path, monkeypatch) -> None:
    """`_manifest_file` resolves the space manifest init actually writes."""
    root = tmp_path / "repo"
    (root / ".fno").mkdir(parents=True)
    space = tmp_path / "space" / "target-state.md"
    space.parent.mkdir(parents=True)
    space.write_text("session_id: sess-space\n", encoding="utf-8")

    monkeypatch.setattr(_merge, "_repo_state_dir", lambda cwd: str(root / ".fno"))
    monkeypatch.setattr(
        "fno.paths.target_state_path_or_legacy", lambda project_root=None: space
    )

    assert _merge._manifest_file(str(root)) == str(space)
    assert _merge._read_state_field(_merge._manifest_file(str(root)), "session_id") == "sess-space"


def test_manifest_file_degrades_to_the_checkout_path(tmp_path: Path, monkeypatch) -> None:
    root = tmp_path / "repo"
    (root / ".fno").mkdir(parents=True)
    monkeypatch.setattr(_merge, "_repo_state_dir", lambda cwd: str(root / ".fno"))

    def _boom(project_root=None):
        raise RuntimeError("spaces root unreadable")

    monkeypatch.setattr("fno.paths.target_state_path_or_legacy", _boom)

    assert _merge._manifest_file(str(root)) == str(root / ".fno" / "target-state.md")


def test_an_absent_manifest_records_the_ambient_session(tmp_path: Path, journal: Path, monkeypatch) -> None:
    """A canonical checkout has no manifest; the merging process names itself."""
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-king")

    _merge._emit_session_satisfied(
        "https://github.com/o/r/pull/1", str(tmp_path / ".fno" / "target-state.md")
    )

    data = _rows(journal)[0]["data"]
    assert data["session_id"] == "sess-king"
    # The hash needs a manifest to hash, so it still degrades to the sentinel.
    assert data["gate_state_hash"] == _merge._MERGE_ROW_UNKNOWN


def test_the_manifest_session_still_wins_over_ambient(
    tmp_path: Path, journal: Path, monkeypatch
) -> None:
    manifest = tmp_path / "target-state.md"
    manifest.write_text("session_id: sess-42\nharness: claude\n", encoding="utf-8")
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-king")

    _merge._emit_session_satisfied("", str(manifest))

    assert _rows(journal)[0]["data"]["session_id"] == "sess-42"


def test_mixed_family_markers_degrade_to_unknown(
    tmp_path: Path, journal: Path, monkeypatch
) -> None:
    monkeypatch.setenv("CLAUDE_CODE_SESSION_ID", "sess-king")
    monkeypatch.setenv("CODEX_THREAD_ID", "thread-other")

    _merge._emit_session_satisfied(
        "https://github.com/o/r/pull/1", str(tmp_path / ".fno" / "target-state.md")
    )

    data = _rows(journal)[0]["data"]
    assert data["session_id"] == _merge._MERGE_ROW_UNKNOWN


def test_an_absent_manifest_prints_one_honest_line(
    tmp_path: Path, journal: Path, capsys
) -> None:
    """One absent file is one cause, so one line - not a missing-key line
    for a file that was never there plus an unreadable line for the same
    file."""
    _merge._emit_session_satisfied("", str(tmp_path / ".fno" / "target-state.md"))

    err = capsys.readouterr().err
    pr_lines = [ln for ln in err.splitlines() if ln.startswith("pr-merge:")]
    assert len(pr_lines) == 1, err
    assert "no target manifest at" in pr_lines[0]
    assert "no session_id on" not in pr_lines[0]
    assert "unreadable" not in pr_lines[0]


def test_a_readable_manifest_without_a_session_prints_one_line(
    tmp_path: Path, journal: Path, capsys
) -> None:
    manifest = tmp_path / "target-state.md"
    manifest.write_text("harness: claude\n", encoding="utf-8")

    _merge._emit_session_satisfied("", str(manifest))

    err = capsys.readouterr().err
    pr_lines = [ln for ln in err.splitlines() if ln.startswith("pr-merge:")]
    assert len(pr_lines) == 1, err
    assert pr_lines[0].startswith(f"pr-merge: no session_id on {manifest}")

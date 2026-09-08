"""The merge's own audit row: the record that says a merge consulted the gate.

`fno do pr merge` builds a `session_satisfied{source:pr_merge}` event, and it
had produced zero rows in a 21605-event journal - not zero for one PR, zero for
every merge ever taken. Three silent early returns guarded the emit, and the
first of them fired every time: the session manifest moved to the space while
this path still looked for it beside the checkout.

Every assertion here is a POSITIVE marker (a row, a field value), never the
absence of a diagnostic.
"""

import json
from pathlib import Path

import pytest

from fno.pr import _merge


def _rows(path: Path) -> list[dict]:
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


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

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

from fno.pr import _merge


def _rows(state_dir: Path) -> list[dict]:
    path = state_dir / "events.jsonl"
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def test_emit_writes_a_row_when_the_manifest_is_absent(tmp_path: Path) -> None:
    """The absent-manifest case: the one that ate every merge row."""
    state_dir = tmp_path / ".fno"
    state_dir.mkdir()

    _merge._emit_session_satisfied(
        "https://github.com/o/r/pull/1", str(state_dir), str(state_dir / "target-state.md")
    )

    rows = _rows(state_dir)
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


def test_emit_carries_the_real_session_when_the_manifest_is_readable(tmp_path: Path) -> None:
    state_dir = tmp_path / ".fno"
    state_dir.mkdir()
    manifest = tmp_path / "target-state.md"
    manifest.write_text("session_id: sess-42\nharness: claude\n", encoding="utf-8")

    _merge._emit_session_satisfied("", str(state_dir), str(manifest))

    data = _rows(state_dir)[0]["data"]
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

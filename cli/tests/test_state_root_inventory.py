"""Gate: the state root may hold nothing the inventory doc does not name (x-a469)."""
from __future__ import annotations

import fnmatch
from pathlib import Path

from fno.graph._state_root_inventory import top_level_patterns, undocumented
from fno.paths_testing import use_tmpdir

REPO_ROOT = Path(__file__).resolve().parents[2]
DOC = REPO_ROOT / "docs" / "state-root-inventory.md"


def test_positive_control_names_the_undocumented_file(tmp_path):
    # A bare zero from the gate proves nothing; prove it can name a violation.
    (tmp_path / "ledger.json").touch()
    marker = "totally-undocumented-marker-file"
    (tmp_path / marker).touch()
    assert undocumented(tmp_path, DOC) == [marker]


def test_state_root_mirroring_the_doc_is_fully_documented(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths
    from fno.graph.store import commit_rows_via_store

    root = Path(paths.state_dir())
    for pattern in top_level_patterns(DOC):
        path = root / pattern
        if pattern in {"backups", "db", "install", "heal"}:
            # Folders the doc documents; materialize them as directories.
            path.mkdir(exist_ok=True)
        elif not path.exists():
            path.touch()
    # A real writer's output must also read as documented, not just the
    # materialized mirror: the mutation emits the db/ trio under the new
    # anchor, its render, and the backups/ rotation beside it.
    commit_rows_via_store(paths.graph_json(), lambda entries: entries)
    assert undocumented(root, DOC) == []


def test_root_fence_names_are_all_rowed_in_the_doc():
    """Every fence name the write door admits is matched by a doc row.

    The fence (paths._ROOT_STATE_FILE_ROWS) is the runtime half of the
    shrink-only rule: root_state_file() refuses any leaf outside it. If a
    row dies in the doc and the fence entry survives, the door would keep
    admitting a write the inventory no longer names - the exact drift this
    parity test refuses. (The reverse direction is the doc gate's job: a
    new row cannot enter at all, and a stray fence entry shows up here.)
    """
    import re

    from fno import paths

    patterns = top_level_patterns(DOC)
    strays = sorted(
        name
        for name in paths._ROOT_STATE_FILE_ROWS
        if not any(fnmatch.fnmatchcase(name, pat) for pat in patterns)
    )
    assert strays == [], (
        f"fence names without an inventory row: {strays}; delete the entry in "
        "the same PR that moves the writer into a subfolder"
    )
    # The door stays importable where callers (and the CI gate) read it.
    assert callable(paths.root_state_file)
    source = (REPO_ROOT / "cli" / "src" / "fno" / "paths.py").read_text(
        encoding="utf-8"
    )
    assert "_ROOT_STATE_FILE_ROWS" in source

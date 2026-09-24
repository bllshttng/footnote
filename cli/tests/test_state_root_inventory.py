"""Gate: the state root may hold nothing the inventory doc does not name (x-a469)."""
from __future__ import annotations
from tests.fixtures.graph_seed import seed_graph

from pathlib import Path

from fno.graph._state_root_inventory import top_level_patterns, undocumented
from fno.paths_testing import use_tmpdir

REPO_ROOT = Path(__file__).resolve().parents[2]
DOC = REPO_ROOT / "docs" / "state-root-inventory.md"


def test_positive_control_names_the_undocumented_file(tmp_path):
    # A bare zero from the gate proves nothing; prove it can name a violation.
    (tmp_path / "graph.json").touch()
    marker = "totally-undocumented-marker-file"
    (tmp_path / marker).touch()
    assert undocumented(tmp_path, DOC) == [marker]


def test_state_root_mirroring_the_doc_is_fully_documented(tmp_path, monkeypatch):
    use_tmpdir(monkeypatch, tmp_path)
    from fno import paths
    from fno.graph.store import commit_rows_via_store

    root = Path(paths.state_dir())
    seed_graph(root / "graph.json", '{"entries": []}\n')
    for pattern in top_level_patterns(DOC):
        path = root / pattern
        if pattern == "backups":
            path.mkdir(exist_ok=True)
        elif pattern in {"graph.db", "graph.db-wal", "graph.db-shm"}:
            continue  # SQLite creates and owns these files.
        elif not path.exists():
            path.touch()
    # A real writer's output must also read as documented, not just the
    # materialized mirror: the mutation emits the graph, its render, and the
    # backups/ rotation beside it.
    commit_rows_via_store(root / "graph.json", lambda entries: entries)
    assert undocumented(root, DOC) == []

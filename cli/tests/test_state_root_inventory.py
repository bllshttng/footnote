"""Gate: the state root may hold nothing the inventory doc does not name (x-a469)."""
from __future__ import annotations

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
    from fno.graph.store import locked_mutate_graph

    root = Path(paths.state_dir())
    for pattern in top_level_patterns(DOC):
        (root / pattern).touch()
    # A real writer's output must also read as documented, not just the
    # materialized mirror: the mutation emits the graph, its render, and the
    # backups/ rotation beside it.
    (root / "graph.json").write_text('{"entries": []}\n', encoding="utf-8")
    locked_mutate_graph(root / "graph.json", lambda entries: entries)
    assert undocumented(root, DOC) == []

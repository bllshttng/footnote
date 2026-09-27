from __future__ import annotations

from pathlib import Path

import pytest


def test_wire_rows_surfaces_store_unavailable_for_an_absent_anchor(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import fno.graph.api as graph_api
    from fno.graph.store import StoreUnavailable

    path = tmp_path / "graph.json"

    def unavailable(*args, **kwargs):
        raise StoreUnavailable("spawn_failed", "no keeper")

    monkeypatch.setattr(graph_api, "_api", unavailable)
    with pytest.raises(StoreUnavailable, match="no keeper"):
        graph_api.wire_rows(path=path)

"""Hermetic isolation for the tests living under ``cli/src/fno/``.

This is a SEPARATE pytest root from ``cli/tests/``, which is the trap: a scrub
applied over there protects nothing here. A targeted ``pytest cli/src/fno/...``
reaches target_cli.py and the carveout/done/log readers with the live session's
markers still set, and resolves the identity of the session running the tests.

Both trees now call the same ``fno.hermetic.neutralise``, so they cannot drift
on what counts as ambient - which is the failure that produced three specimens
through three different channels on 2026-08-11.

Applied at module load rather than as a fixture, because a fixture is too late
for anything that reads a marker at import time.

Tests that intentionally exercise the global-fallback path (e.g.
``test_global_active_combo_falls_back_when_no_project_override``) opt out
per-test with ``monkeypatch.delenv("FNO_GLOBAL_SETTINGS_PATH", raising=False)``
before redirecting ``HOME``; that still works, since neutralise sets the pin in
``os.environ`` exactly as the retired autouse fixture did.
"""
from __future__ import annotations

import os
import tempfile
from pathlib import Path

import pytest

from fno.hermetic import neutralise

_SANDBOX = tempfile.mkdtemp(prefix="fno-src-test-sandbox-")
_hermetic_env = neutralise(os.environ, Path(_SANDBOX))
os.environ.clear()
os.environ.update(_hermetic_env)


@pytest.fixture(autouse=True)
def _reset_project_resolve_cache():
    """Clear the project-name resolver's cache before and after every test.

    Mirrors the same fixture in ``cli/tests/conftest.py``: this is a SEPARATE
    pytest root (see module docstring), so a scrub over there protects
    nothing here. ``fno.projects.resolve`` caches ``~/.fno/config.toml`` in a
    module-level dict on first use and never invalidates it; a test in this
    tree (``fno/projects/test_resolve.py``) that points ``SETTINGS_PATH`` at
    a tmp fixture and clears the cache only before reading leaves that
    fixture's project map live for whatever test runs next in the same
    xdist worker.
    """
    from fno.projects import resolve as proj_resolve

    proj_resolve._clear_cache()
    yield
    proj_resolve._clear_cache()


@pytest.fixture(autouse=True, scope="session")
def _config_search_ceiling(tmp_path_factory: pytest.TempPathFactory):
    """Widen the config ceiling to include the pytest basetemp.

    Both trees need this and for the same reason, so both carry it: tests here
    write project-local and global settings files under ``tmp_path``, and the
    ceiling ``neutralise`` sets covers only the sandbox. Adding it to one tree
    and not the other is how three tests in this file went red while the
    cli/tests tree stayed green - the same one-of-N-paths shape this whole
    change exists to remove.
    """
    basetemp = str(tmp_path_factory.getbasetemp())
    previous = os.environ.get("FNO_CONFIG_SEARCH_ROOT", "")
    os.environ["FNO_CONFIG_SEARCH_ROOT"] = os.pathsep.join(
        [basetemp, previous] if previous else [basetemp]
    )
    yield
    os.environ["FNO_CONFIG_SEARCH_ROOT"] = previous


def pytest_sessionfinish(session, exitstatus) -> None:  # noqa: ANN001
    import shutil

    shutil.rmtree(_SANDBOX, ignore_errors=True)
    # Sweep AFTER the rmtree: the only ordering that catches the keepers
    # that were serving the just-deleted sandbox graphs.
    from fno.graph.store import sweep_orphaned_keepers

    sweep_orphaned_keepers(timeout=15.0)


@pytest.fixture(autouse=True)
def _in_memory_hold_verdict(tmp_path, monkeypatch):
    """The hold verdict answers from the graph on disk (one fno-agents
    receipt). These tests build in-memory graphs; persist each call's rows
    so the real reader sees them. A test that stubs the verdict itself
    overrides this."""

    import json as _json
    import sqlite3 as _sqlite3

    def seed(path, rows):
        path.parent.mkdir(parents=True, exist_ok=True)
        store = path.with_suffix(".db")
        connection = _sqlite3.connect(store)
        connection.execute(
            "CREATE TABLE IF NOT EXISTS entries ("
            "id TEXT PRIMARY KEY, ordinal INTEGER NOT NULL, row TEXT NOT NULL)"
        )
        for ordinal, entry in enumerate(rows):
            connection.execute(
                "INSERT INTO entries(id, ordinal, row) VALUES (?, ?, ?)",
                (entry["id"], ordinal, _json.dumps(entry)),
            )
        connection.commit()
        connection.close()
        from fno.graph.store import read_graph_strict

        read_graph_strict(path)

    from fno.graph import ladder

    real = ladder.dispatch_hold_verdict

    def patched(entry, by_id):
        rows = list(by_id.values())
        if isinstance(entry, dict) and entry not in rows:
            rows = rows + [entry]
        complete = {}
        for e in rows:
            if not isinstance(e, dict) or not e.get("id"):
                continue
            row = {"type": "feature", "priority": "p2", "status": "ready", **e}
            row.setdefault("title", str(row.get("id")))
            row.setdefault("slug", str(row.get("id")))
            complete[str(row["id"])] = row
        graph = tmp_path / "verdict-graph.json"
        for stale in (graph, graph.with_suffix(".db")):
            stale.unlink(missing_ok=True)
        seed(graph, list(complete.values()))
        monkeypatch.setattr("fno.paths.graph_json", lambda: graph)
        if not isinstance(entry, dict) or not entry.get("id"):
            # A row the graph cannot carry: read as unheld, as before.
            return None
        return real(entry, by_id)

    monkeypatch.setattr(ladder, "dispatch_hold_verdict", patched)

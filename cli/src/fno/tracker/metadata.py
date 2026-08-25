"""Footnote-minted node metadata: the guarded default-backend read.

Not every graph field made it into the seam, on purpose. ``size``, ``type``,
``project``, ``dep``, the model pins, and the persistent ``slug`` are written
only by footnote's own creation/mutation verbs - the same verbs that refuse on
an external backend (a node an external tracker owns was never sized, typed,
or pinned by footnote). The five-field read contract deliberately does not
carry them, and the sidecar deliberately does not mirror them
(``docs/architecture/external-tracker.md`` lists them as tracker-side). So a
reader of those fields has exactly one legitimate source: the default graph
store, and only when it is the selected backend.

Readers therefore route through :func:`read_entries` here instead of calling
``read_graph`` directly. On the default backend the behavior is byte-identical
to the pre-migration read. On an external selection the helper raises, and the
caller degrades along whatever missing-data path it already had - it must
never fall back to opening ``graph.json`` behind an external backend, because
stale default-store rows leaking into an external run is exactly the two
stores problem the seam exists to close. This module is the one sanctioned
graph-read owner for that reader class; the consumer census
(``scripts/ci/check-tracker-consumers.sh``) allowlists it by name.
"""
from __future__ import annotations

from pathlib import Path


class ExternalMetadataUnavailable(RuntimeError):
    """Footnote-minted metadata is unreachable under an external backend.

    Raised by :func:`read_entries`. Callers catch this and take their existing
    missing-data path (advisory None, empty scan, fallback shape); swallowing
    it into a graph.json fallback is the one forbidden move.
    """

    def __init__(self, reader: str) -> None:
        super().__init__(
            f"{reader}: footnote-minted node metadata (size/type/project/"
            "dep/model pins/persistent slug) lives only in the default graph "
            "store and is unavailable under an external tracker backend"
        )


def _graph_store_path() -> Path:
    """Resolve through ``paths.graph_json()`` at call time (config override
    and test redirects honored); fail open to the default location the same
    way ``fno.graph._constants`` does on a broken settings file."""
    try:
        from fno import paths

        return paths.graph_json()
    except Exception:  # noqa: BLE001 - mirror _constants' fail-open default
        from fno.graph._constants import _state_dir

        return _state_dir() / "graph.json"


def read_entries(reader: str, *, strict: bool = False) -> list[dict]:
    """Raw default-backend entries, for the guarded metadata reader class.

    ``reader`` names the calling module (diagnostics read better than a bare
    traceback). Raises :class:`ExternalMetadataUnavailable` before any store
    read when an external backend is selected.
    """
    from . import active_backend_name

    if active_backend_name() != "graph":
        raise ExternalMetadataUnavailable(reader)
    from fno.graph.store import read_graph, read_graph_strict

    path = _graph_store_path()
    return read_graph_strict(path) if strict else read_graph(path)

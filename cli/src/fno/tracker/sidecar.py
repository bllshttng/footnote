"""Footnote-owned state for a work item, keyed by the opaque tracker id.

The sidecar stores ONLY fields the tracker cannot express. Zero overlap with
the five-field read interface means zero sync, which is the partition rule the
whole bring-your-own-id design rests on. ``scripts/ci/check-tracker-partition.sh``
fails if a field name below ever appears on ``TrackerNode``.

What does NOT live here, deliberately:
  * the claim pointer (locked_by / session_id / locked_by_harness*). That is
    live coordination state, already owned by the claims subsystem
    (``fno.claims.io``), which keys on the opaque id and never opens the graph.
    Mirroring it here would make the sidecar a second writer for claim state,
    which is the two-sources-of-truth bug this design exists to avoid.
  * any field an external tracker can express (title / state / priority /
    parent / blocked_by / size / domain / details). Those stay in the tracker.

``cwd`` is the field most likely to be missed and the most damaging to drop:
no tracker models a local checkout, yet it is the authority for multi-repo
dispatch.
"""
from __future__ import annotations

import os
import tempfile
from pathlib import Path
from typing import Optional

from pydantic import BaseModel, ConfigDict, Field

from fno.paths import sidecar_path


class Sidecar(BaseModel):
    """Footnote-owned fields for one work item. Keyed externally by ``id``.

    ``id`` is carried for traceability but is NOT authoritative here: the
    filename (the url-encoded id) is the lookup key, matching how the claims
    dir keys its lockfiles. Every field has a default so a sidecar for a
    brand-new item is just ``Sidecar(id=...)``.
    """

    # extra="forbid" closes the runtime hole the static partition gate cannot
    # see: a Sidecar constructed or loaded with a tracker-owned field (title,
    # state, priority, ...) must fail loudly here, not persist a forbidden second
    # copy while check-tracker-partition.sh stays green on declared names alone.
    model_config = ConfigDict(extra="forbid")

    id: str
    # The repository this work happens in. No tracker concept; multi-repo
    # dispatch depends on it entirely.
    cwd: Optional[str] = None
    # The design document. Unstampable on a tracker issue.
    plan_path: Optional[str] = None
    # Ship evidence footnote writes; the tracker gets a link, not a mirror.
    pr_number: Optional[int] = None
    pr_url: Optional[str] = None
    additional_prs: list[dict] = Field(default_factory=list)
    # Telemetry no tracker models.
    cost_usd: Optional[float] = None
    cost_sessions: list[dict] = Field(default_factory=list)
    # When this id was first claimed (footnote-owned timestamp; the live holder
    # is in the claims dir, not here).
    claimed_at: Optional[str] = None
    # Batch-lane membership (an open batch ships this node via the batch PR,
    # so selection must drop it; cleared on abandon). Selection fact the
    # tracker cannot express.
    batch: Optional[str] = None
    # Delivery-unit containment (x-e957): this node's work ships inside
    # another node's PR, so it is not separately dispatchable. Selection
    # fact the tracker cannot express.
    contained_in: Optional[str] = None
    # Lifecycle provenance (append-only phase records).
    sessions: list[dict] = Field(default_factory=list)
    # Agent provenance.
    source_harness: Optional[str] = None
    source_cwd: Optional[str] = None
    source_node_id: Optional[str] = None
    source_plan_path: Optional[str] = None
    spawned_by_session: Optional[str] = None
    spawned_by_harness: Optional[str] = None
    spawned_by_cwd: Optional[str] = None


def _external_mode() -> bool:
    """True when the selected tracker backend is not the default graph one.

    Resolved through :func:`fno.tracker.active_backend_name` (lazily, to keep
    this module importable before the package finishes initializing) so the
    sidecar store and the tracker can never disagree about which backend is
    live. Graph mode projects sidecar fields in place inside the graph entry;
    every other backend uses the per-id JSON file.
    """
    from . import active_backend_name

    return active_backend_name() != "graph"


# Every Sidecar field except the join key projects 1:1 onto a graph entry field
# of the same name (the graph entry is "a sidecar plus a tracker merged into
# one record"). Derived from the model so a new field cannot be added without
# automatically joining the projection.
_GRAPH_PROJECTED_FIELDS = tuple(
    name for name in Sidecar.model_fields if name != "id"
)


def _load_from_graph(id: str) -> Sidecar:
    from fno.graph.store import read_graph
    from fno.paths import graph_json

    for entry in read_graph(graph_json()):
        if entry.get("id") == id:
            return Sidecar(
                id=id,
                **{
                    name: entry[name]
                    for name in _GRAPH_PROJECTED_FIELDS
                    if name in entry
                },
            )
    # A missing row has no sidecar anywhere: the graph is the store, so there
    # is no per-id file to fall back to (mirrors the no-file branch below).
    return Sidecar(id=id)


def _save_to_graph(sidecar: Sidecar) -> Path:
    from fno.graph.store import locked_mutate_graph
    from fno.paths import graph_json

    from .types import NodeNotFound

    # Only fields explicitly set on this instance are written back, so a
    # partial Sidecar cannot null out entry values it never carried.
    payload = sidecar.model_dump(exclude_unset=True, exclude={"id"})
    path = graph_json()

    def _apply(entries: list[dict]) -> list[dict]:
        for entry in entries:
            if entry.get("id") == sidecar.id:
                entry.update(payload)
                return entries
        raise NodeNotFound(sidecar.id)

    locked_mutate_graph(path, _apply)
    return path


def load(id: str) -> Sidecar:
    """Read the sidecar for ``id``. Returns an empty Sidecar if none exists yet.

    Selects the logical sidecar store for the active backend: graph mode
    projects the footnote-owned fields out of the item's graph entry; an
    external backend reads the per-id JSON file. One physical owner per
    backend, never both.
    """
    if _external_mode():
        path = sidecar_path(id)
        if not path.exists():
            return Sidecar(id=id)
        return Sidecar.model_validate_json(path.read_text(encoding="utf-8"))
    return _load_from_graph(id)


def save(sidecar: Sidecar) -> Path:
    """Persist ``sidecar`` to the active backend's sidecar store. Returns the
    physical path written (the graph file in graph mode, the per-id JSON
    otherwise).

    External mode is temp-file + ``os.replace`` so a concurrent reader on
    another item never sees a half-written file; one file per item means
    writers on different ids never contend. Graph mode routes through
    ``locked_mutate_graph`` so the projection stays atomic with the rest of
    the entry and recompute_statuses/canonicalization run as usual.
    """
    if _external_mode():
        return _save_to_file(sidecar)
    return _save_to_graph(sidecar)


def _save_to_file(sidecar: Sidecar) -> Path:
    path = sidecar_path(sidecar.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = sidecar.model_dump_json(indent=2, exclude_unset=True) + "\n"
    fd, tmp_name = tempfile.mkstemp(prefix=".sidecar-tmp-", dir=str(path.parent))
    raw = payload.encode("utf-8")
    try:
        # os.write may short-write; loop until every byte lands (mirrors
        # fno.claims.io.atomic_create_exclusive).
        view = memoryview(raw)
        while view:
            view = view[os.write(fd, view):]
    except BaseException:
        os.close(fd)
        try:
            os.unlink(tmp_name)
        except OSError:
            pass
        raise
    os.close(fd)
    os.replace(tmp_name, path)
    return path

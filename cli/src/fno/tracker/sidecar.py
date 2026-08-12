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


def load(id: str) -> Sidecar:
    """Read the sidecar for ``id``. Returns an empty Sidecar if none exists yet."""
    path = sidecar_path(id)
    if not path.exists():
        return Sidecar(id=id)
    return Sidecar.model_validate_json(path.read_text(encoding="utf-8"))


def save(sidecar: Sidecar) -> Path:
    """Atomically write ``sidecar`` to its per-id path. Returns the path.

    Temp-file + ``os.replace`` so a concurrent reader on another item never sees
    a half-written file. One file per item means writers on different ids never
    contend.
    """
    path = sidecar_path(sidecar.id)
    path.parent.mkdir(parents=True, exist_ok=True)
    payload = sidecar.model_dump_json(indent=2, exclude_unset=True) + "\n"
    fd, tmp_name = tempfile.mkstemp(prefix=".sidecar-tmp-", dir=str(path.parent))
    try:
        os.write(fd, payload.encode("utf-8"))
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

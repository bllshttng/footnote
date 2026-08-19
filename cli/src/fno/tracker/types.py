"""The work-item interface footnote consumes, independent of where items live.

This is the seam between footnote and whatever stores work items. An external
tracker (GitHub Issues, Linear, Jira) supplies five read fields and one write
operation; footnote keeps everything else in a sidecar it owns
(:mod:`fno.tracker.sidecar`). The partition between the two is load-bearing:
zero overlap means zero sync, which is what keeps a two-sources-of-truth bug
out of the design. ``scripts/ci/check-tracker-partition.sh`` fails if a sidecar
field name ever appears on the read interface below.
"""
from __future__ import annotations

from enum import Enum
from typing import Optional, Protocol, runtime_checkable

from pydantic import BaseModel, Field


class TrackerState(str, Enum):
    """The only state vocabulary an external backend must supply.

    footnote derives its own rung (ready / in_progress / blocked / done / ...)
    from the plan and the PR exactly as it does today, so a backend does not
    need to learn that vocabulary. It only needs to say whether the item is
    still open or has been closed.
    """

    open = "open"
    closed = "closed"


class TrackerNode(BaseModel):
    """The five-field projection of a work item that footnote reads.

    Nothing else crosses the adapter boundary on the read side. ``title`` is
    display-only; ``parent`` drives epic rollup and board ordering;
    ``blocked_by`` drives merge-triggered dispatch (``backlog advance``).
    """

    id: str
    title: Optional[str] = None
    state: TrackerState = TrackerState.open
    parent: Optional[str] = None
    blocked_by: list[str] = Field(default_factory=list)


class TrackerCandidate(TrackerNode):
    """Selection-only projection of an open item, returned by ``list_open``.

    Ordinary reads stay five-field (Locked Decision 3); ``fno backlog next``
    additionally needs the stable ordering inputs footnote's board ranking
    consumes, so ``list_open`` widens each open item with exactly ``priority``
    (flat priority), ``rank`` (curated board rank), and ``created_at`` (the
    tiebreaker). Nothing else crosses here: these are the only sort inputs
    ``_intake.make_selection_sort_key`` reads off the candidate itself, so the
    graph backend's winner order is preserved without recreating the full
    graph entry. The partition gate inspects this projection like any other:
    none of these three names may appear on :class:`fno.tracker.sidecar.Sidecar`.
    """

    priority: str = "p2"
    rank: Optional[float] = None
    created_at: Optional[str] = None


class TrackerError(Exception):
    """Base for failures talking to a work-item backend."""


class NodeNotFound(TrackerError):
    """The backend has no item with this id."""


@runtime_checkable
class NodeTracker(Protocol):
    """Read five fields, write one operation. That is the whole contract.

    Implementations: the default graph.json backend
    (:class:`fno.tracker.graph_backend.GraphTracker`), and future external
    backends. The id is opaque and globally unique within one backend; it is
    the same string used as a claim key, so it must not contain ``:`` (the
    claim-key partition character) -- backends validate that at their
    boundary rather than here.
    """

    name: str

    def read(self, id: str) -> TrackerNode:
        """Return the five-field projection for ``id``.

        Raises :class:`NodeNotFound` when the backend has no such item.
        """
        ...

    def list_open(self) -> list[TrackerCandidate]:
        """Return every open item, as ``read`` would project each, plus the
        selection-only ordering inputs (:class:`TrackerCandidate`).

        ``fno backlog next`` enumerates these and applies footnote's own
        ranking to them (the board-as-work-order contract ``advance``
        depends on), so a backend must NOT pre-rank: it returns the open
        set in any order and footnote orders it. Enumeration must stay
        bounded to the open set: a backend with historical rows must not
        materialize them merely to choose the next candidate.
        """
        ...

    def close(self, id: str) -> None:
        """Mark ``id`` closed in the backend.

        This is the single write-back footnote makes to a tracker, issued at
        node closure. A PR link is a comment or a native issue-PR link, not a
        field write, so it does not appear on this interface.
        """
        ...

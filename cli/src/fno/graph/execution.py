"""Derived, disposable execution-graph contract (wave 4 of the context-graph epic).

A read-only typed topology COMPILED on demand from canonical state (plan,
backlog ``blocked_by``, file ownership, live claims + liveness, verifiers,
required evidence). It is neither a new hand-edited graph nor an orchestration
runtime: it is derived, inspected, and thrown away. Code owns schema, routing,
counts, barriers, and state transitions here; agents stay bounded judgment
nodes.

The types mirror the typed-evidence-artifact house style of
``fno.resume.receipt``: frozen dataclasses, a module ``EXEC_GRAPH_VERSION``,
``to_dict()`` for stable JSON, and a strict ``parse_*``/``_from_dict`` that
fails CLOSED on any schema defect (``MalformedGraphError``). Two invariants the
compiler upholds:

* Every node justifies its existence with one of ``NODE_JUSTIFICATIONS``
  (specialized context/tools, true parallelism, auditable branching, resource
  exclusion, or a distinct verifier). A lone node with only the default
  justification collapses to the ordinary serial target loop.
* Every edge is one of ``EDGE_KINDS`` and carries a concrete crossing
  ``invariant`` string; an edge without one is malformed, never silently kept.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Iterable, Optional, Sequence

EXEC_GRAPH_VERSION = 1

# A node earns a slot in the graph only by one of these. "specialized" is the
# weakest and, when it is the *only* justification of a lone node, triggers the
# collapse to serial (see compile_graph).
NODE_JUSTIFICATIONS: frozenset[str] = frozenset(
    {
        "specialized",       # specialized context or tools
        "parallelism",       # true parallelism with a sibling
        "branching",         # auditable branching
        "resource",          # resource exclusion (owns a contended file)
        "verifier",          # a distinct verifier
    }
)

# The only legal edge kinds. Each REQUIRES a concrete crossing invariant.
EDGE_KINDS: frozenset[str] = frozenset({"data", "ordering", "resource", "verification"})

_ACCESS: frozenset[str] = frozenset({"read", "write"})
_LIVENESS: frozenset[str] = frozenset({"", "live", "dead", "unknown"})


class MalformedGraphError(ValueError):
    """An execution-graph artifact failed schema validation.

    A malformed DECLARED graph is rejected explicitly rather than treated as
    empty: silently degrading a corrupt topology would let a caller synthesize
    on no evidence, the exact failure this contract prevents.
    """


def _req_str(d: Any, key: str, origin: str) -> str:
    v = d.get(key) if isinstance(d, dict) else None
    if not isinstance(v, str) or not v.strip():
        raise MalformedGraphError(f"{origin}: missing/empty string field {key!r}")
    return v


def _opt_str(d: Any, key: str) -> str:
    v = d.get(key) if isinstance(d, dict) else None
    return v if isinstance(v, str) else ""


def _req_int(d: Any, key: str, origin: str) -> int:
    v = d.get(key) if isinstance(d, dict) else None
    # bool is an int subclass; reject it so True/False never pose as 1/0.
    if isinstance(v, bool) or not isinstance(v, int):
        raise MalformedGraphError(f"{origin}: field {key!r} must be an integer")
    return v


def _str_tuple(v: Any, key: str, origin: str) -> tuple[str, ...]:
    if v is None:
        return ()
    if not isinstance(v, list) or not all(isinstance(x, str) for x in v):
        raise MalformedGraphError(f"{origin}: field {key!r} must be a list of strings")
    return tuple(v)


def _nonneg_int(v: Any, name: str) -> int:
    if isinstance(v, bool) or not isinstance(v, int) or v < 0:
        raise MalformedGraphError(f"{name} must be a non-negative int, got {v!r}")
    return v


@dataclass(frozen=True)
class Budget:
    """Iteration / spend / retry bounds for a node. 0 = unbounded."""

    max_iterations: int = 0
    max_usd: float = 0.0
    max_attempts: int = 1  # retry-and-stop bound; >= 1

    def __post_init__(self) -> None:
        _nonneg_int(self.max_iterations, "budget.max_iterations")
        if isinstance(self.max_usd, bool) or not isinstance(self.max_usd, (int, float)) or self.max_usd < 0:
            raise MalformedGraphError(f"budget.max_usd must be a non-negative number, got {self.max_usd!r}")
        if isinstance(self.max_attempts, bool) or not isinstance(self.max_attempts, int) or self.max_attempts < 1:
            raise MalformedGraphError(f"budget.max_attempts must be an int >= 1, got {self.max_attempts!r}")

    def to_dict(self) -> dict[str, Any]:
        return {
            "max_iterations": self.max_iterations,
            "max_usd": self.max_usd,
            "max_attempts": self.max_attempts,
        }

    @staticmethod
    def from_dict(d: Any, origin: str = "<budget>") -> "Budget":
        if d is None:
            return Budget()
        if not isinstance(d, dict):
            raise MalformedGraphError(f"{origin}: budget must be an object")
        return Budget(
            max_iterations=d.get("max_iterations", 0),
            max_usd=d.get("max_usd", 0.0),
            max_attempts=d.get("max_attempts", 1),
        )


@dataclass(frozen=True)
class ExecNode:
    """One node contract: identity, role, access, objective, I/O, policy, bounds.

    Ownership (repo/worktree/owns_files/claim_key), evidence requirements,
    candidate SHA, reviewer policy, and expected output are explicit so the
    topology is inspectable without re-deriving it from prose.
    """

    node_id: str
    role: str
    justification: str
    objective: str
    repo: str
    access: str = "write"
    worktree: str = ""
    owns_files: tuple[str, ...] = ()
    claim_key: str = ""
    liveness: str = ""
    inputs: tuple[str, ...] = ()
    output_schema: str = "task_result"
    tool_policy: tuple[str, ...] = ()
    builder: str = ""
    model_route: str = ""
    artifact_authority: bool = False
    budget: Budget = field(default_factory=Budget)
    verifier: str = ""
    requires_evidence: tuple[str, ...] = ()
    candidate_sha: str = ""
    reviewer_policy: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        for name, val in (("node_id", self.node_id), ("role", self.role),
                          ("objective", self.objective), ("repo", self.repo)):
            if not isinstance(val, str) or not val.strip():
                raise MalformedGraphError(f"node.{name} must be a non-empty string")
        if self.justification not in NODE_JUSTIFICATIONS:
            raise MalformedGraphError(
                f"node.justification must be one of {sorted(NODE_JUSTIFICATIONS)}, "
                f"got {self.justification!r}"
            )
        if self.access not in _ACCESS:
            raise MalformedGraphError(f"node.access must be one of {sorted(_ACCESS)}, got {self.access!r}")
        if self.liveness not in _LIVENESS:
            raise MalformedGraphError(f"node.liveness must be one of {sorted(_LIVENESS)}, got {self.liveness!r}")

    def to_dict(self) -> dict[str, Any]:
        return {
            "node_id": self.node_id,
            "role": self.role,
            "justification": self.justification,
            "objective": self.objective,
            "repo": self.repo,
            "access": self.access,
            "worktree": self.worktree,
            "owns_files": list(self.owns_files),
            "claim_key": self.claim_key,
            "liveness": self.liveness,
            "inputs": list(self.inputs),
            "output_schema": self.output_schema,
            "tool_policy": list(self.tool_policy),
            "builder": self.builder,
            "model_route": self.model_route,
            "artifact_authority": self.artifact_authority,
            "budget": self.budget.to_dict(),
            "verifier": self.verifier,
            "requires_evidence": list(self.requires_evidence),
            "candidate_sha": self.candidate_sha,
            "reviewer_policy": list(self.reviewer_policy),
        }

    @staticmethod
    def from_dict(d: Any, origin: str = "<node>") -> "ExecNode":
        if not isinstance(d, dict):
            raise MalformedGraphError(f"{origin}: node must be an object")
        if not isinstance(d.get("artifact_authority", False), bool):
            raise MalformedGraphError(f"{origin}: node.artifact_authority must be a bool")
        return ExecNode(
            node_id=_req_str(d, "node_id", origin),
            role=_req_str(d, "role", origin),
            justification=_req_str(d, "justification", origin),
            objective=_req_str(d, "objective", origin),
            repo=_req_str(d, "repo", origin),
            access=d.get("access", "write"),
            worktree=_opt_str(d, "worktree"),
            owns_files=_str_tuple(d.get("owns_files"), "owns_files", origin),
            claim_key=_opt_str(d, "claim_key"),
            liveness=d.get("liveness", ""),
            inputs=_str_tuple(d.get("inputs"), "inputs", origin),
            output_schema=_opt_str(d, "output_schema") or "task_result",
            tool_policy=_str_tuple(d.get("tool_policy"), "tool_policy", origin),
            builder=_opt_str(d, "builder"),
            model_route=_opt_str(d, "model_route"),
            artifact_authority=bool(d.get("artifact_authority", False)),
            budget=Budget.from_dict(d.get("budget"), f"{origin}.budget"),
            verifier=_opt_str(d, "verifier"),
            requires_evidence=_str_tuple(d.get("requires_evidence"), "requires_evidence", origin),
            candidate_sha=_opt_str(d, "candidate_sha"),
            reviewer_policy=_str_tuple(d.get("reviewer_policy"), "reviewer_policy", origin),
        )


@dataclass(frozen=True)
class ExecEdge:
    """A directed edge; one of EDGE_KINDS carrying a concrete crossing invariant."""

    src: str
    dst: str
    kind: str
    invariant: str

    def __post_init__(self) -> None:
        if not (isinstance(self.src, str) and self.src.strip()):
            raise MalformedGraphError("edge.src must be non-empty")
        if not (isinstance(self.dst, str) and self.dst.strip()):
            raise MalformedGraphError("edge.dst must be non-empty")
        if self.src == self.dst:
            raise MalformedGraphError(f"edge is a self-loop: {self.src}")
        if self.kind not in EDGE_KINDS:
            raise MalformedGraphError(f"edge.kind must be one of {sorted(EDGE_KINDS)}, got {self.kind!r}")
        if not (isinstance(self.invariant, str) and self.invariant.strip()):
            raise MalformedGraphError(f"edge {self.src}->{self.dst} ({self.kind}) needs a crossing invariant")

    def to_dict(self) -> dict[str, Any]:
        return {"src": self.src, "dst": self.dst, "kind": self.kind, "invariant": self.invariant}

    @staticmethod
    def from_dict(d: Any, origin: str = "<edge>") -> "ExecEdge":
        if not isinstance(d, dict):
            raise MalformedGraphError(f"{origin}: edge must be an object")
        return ExecEdge(
            src=_req_str(d, "src", origin),
            dst=_req_str(d, "dst", origin),
            kind=_req_str(d, "kind", origin),
            invariant=_req_str(d, "invariant", origin),
        )


@dataclass(frozen=True)
class Barrier:
    """A fan-in point: every id in ``expected`` must resolve before ``at`` proceeds."""

    at: str
    expected: tuple[str, ...]

    def __post_init__(self) -> None:
        if not (isinstance(self.at, str) and self.at.strip()):
            raise MalformedGraphError("barrier.at must be non-empty")
        if not self.expected:
            raise MalformedGraphError(f"barrier at {self.at} has no expected inputs")

    def to_dict(self) -> dict[str, Any]:
        return {"at": self.at, "expected": list(self.expected)}

    @staticmethod
    def from_dict(d: Any, origin: str = "<barrier>") -> "Barrier":
        if not isinstance(d, dict):
            raise MalformedGraphError(f"{origin}: barrier must be an object")
        return Barrier(
            at=_req_str(d, "at", origin),
            expected=_str_tuple(d.get("expected"), "expected", origin),
        )


@dataclass(frozen=True)
class ExecGraph:
    """The compiled topology. ``collapsed`` marks a one-node serial fallback."""

    version: int
    root: str
    nodes: tuple[ExecNode, ...]
    edges: tuple[ExecEdge, ...] = ()
    barriers: tuple[Barrier, ...] = ()
    collapsed: bool = False

    def to_dict(self) -> dict[str, Any]:
        # Sorted for stable snapshots: nodes by id, edges by (src,dst,kind),
        # barriers by anchor. A stable serialization is what makes the golden
        # JSON fixtures deterministic across runs and machines.
        return {
            "version": self.version,
            "root": self.root,
            "collapsed": self.collapsed,
            "nodes": [n.to_dict() for n in sorted(self.nodes, key=lambda n: n.node_id)],
            "edges": [
                e.to_dict()
                for e in sorted(self.edges, key=lambda e: (e.src, e.dst, e.kind))
            ],
            "barriers": [
                b.to_dict() for b in sorted(self.barriers, key=lambda b: b.at)
            ],
        }

    def to_json(self) -> str:
        return json.dumps(self.to_dict(), indent=2, sort_keys=True)

    @staticmethod
    def from_dict(d: Any, origin: str = "<graph>") -> "ExecGraph":
        if not isinstance(d, dict):
            raise MalformedGraphError(f"{origin}: graph must be an object")
        version = _req_int(d, "version", origin)
        if version != EXEC_GRAPH_VERSION:
            raise MalformedGraphError(
                f"{origin}: graph version {version} != supported {EXEC_GRAPH_VERSION}"
            )
        collapsed = d.get("collapsed", False)
        if not isinstance(collapsed, bool):
            raise MalformedGraphError(f"{origin}: collapsed must be a bool")
        nodes = tuple(ExecNode.from_dict(n, f"{origin}.node") for n in (d.get("nodes") or []))
        edges = tuple(ExecEdge.from_dict(e, f"{origin}.edge") for e in (d.get("edges") or []))
        barriers = tuple(Barrier.from_dict(b, f"{origin}.barrier") for b in (d.get("barriers") or []))
        node_ids = {n.node_id for n in nodes}
        for e in edges:
            if e.src not in node_ids or e.dst not in node_ids:
                raise MalformedGraphError(f"{origin}: edge {e.src}->{e.dst} references an unknown node")
        return ExecGraph(
            version=version,
            root=_req_str(d, "root", origin),
            nodes=nodes,
            edges=edges,
            barriers=barriers,
            collapsed=collapsed,
        )


# ── compilation from canonical state ────────────────────────────────────────
#
# compile_graph is PURE: it takes already-gathered canonical state as plain
# dicts and never touches disk. The CLI layer (graph/cli.py) enriches backlog
# entries with live claims/liveness/ownership and hands them here, mirroring
# how resume.receipt keeps build_receipt pure and does I/O separately.


def _entry_node(entry: dict, *, justification: str) -> ExecNode:
    """Build an ExecNode from a canonical backlog entry dict.

    Recognized enrichment keys the caller may attach to the entry:
      owns_files, claim_holder, liveness, verifier, requires_evidence,
      reviewer_policy, candidate_sha, worktree, builder, model_route, role.
    """
    node_id = str(entry.get("id") or "").strip()
    return ExecNode(
        node_id=node_id,
        role=str(entry.get("role") or "builder"),
        justification=justification,
        objective=str(entry.get("title") or entry.get("objective") or node_id),
        repo=str(entry.get("project") or entry.get("repo") or "unknown"),
        access=str(entry.get("access") or "write"),
        worktree=str(entry.get("worktree") or ""),
        owns_files=tuple(entry.get("owns_files") or ()),
        claim_key=str(entry.get("claim_holder") or ""),
        liveness=str(entry.get("liveness") or ""),
        inputs=tuple(entry.get("blocked_by") or ()),
        tool_policy=tuple(entry.get("tool_policy") or ()),
        builder=str(entry.get("builder") or ""),
        model_route=str(entry.get("model") or entry.get("model_route") or ""),
        budget=Budget.from_dict(entry.get("budget")),
        verifier=str(entry.get("verifier") or ""),
        requires_evidence=tuple(entry.get("requires_evidence") or ()),
        candidate_sha=str(entry.get("candidate_sha") or ""),
        reviewer_policy=tuple(entry.get("reviewer_policy") or ()),
    )


def _serial_fallback(root: str, entries: Sequence[dict]) -> ExecGraph:
    """A collapsed one-node graph == the ordinary serial target loop.

    Used both for a genuinely unjustified single node and as the fail-closed
    landing when compilation raises: a caller that cannot trust the topology
    still gets a valid, minimal graph rather than an exception.
    """
    match = next((e for e in entries if str(e.get("id") or "") == root), None)
    entry = match or {"id": root, "title": root, "project": "unknown"}
    node = _entry_node(entry, justification="specialized")
    return ExecGraph(version=EXEC_GRAPH_VERSION, root=root, nodes=(node,), collapsed=True)


def compile_graph(root: str, entries: Sequence[dict]) -> ExecGraph:
    """Compile the minimum disposable graph from canonical state.

    ``entries`` is the pre-selected relevant subgraph (the root plus every
    node it depends on / that depends on it, already enriched with ownership,
    claims, liveness, verifier, and evidence keys). Relationships become edges:

      * ``ordering``     from each ``blocked_by`` dependency present in the set.
      * ``resource``     between any two nodes that own a common file.
      * ``verification`` from a node to its declared verifier node.
      * ``data``         from an evidence producer to a node that requires it.

    A ``Barrier`` is emitted at any node with 2+ ordering predecessors (a real
    fan-in). A lone node whose only justification is ``specialized`` collapses
    to the serial loop. Any failure falls back to the serial loop, never raises.
    """
    if not root or not str(root).strip():
        raise MalformedGraphError("compile_graph: root must be non-empty")
    try:
        by_id: dict[str, dict] = {}
        for e in entries:
            nid = str(e.get("id") or "").strip()
            if nid:
                by_id[nid] = e
        if root not in by_id:
            # Root not in the provided set: nothing to compile from, serial.
            return _serial_fallback(root, entries)

        ids = set(by_id)
        justifications: dict[str, str] = {}
        edges: list[ExecEdge] = []

        # ordering edges from blocked_by (dependency -> dependent)
        preds: dict[str, list[str]] = {nid: [] for nid in ids}
        for nid, e in by_id.items():
            for dep in (e.get("blocked_by") or []):
                if dep in ids:
                    edges.append(ExecEdge(dep, nid, "ordering",
                                          f"{dep} must reach a terminal state before {nid} starts"))
                    preds[nid].append(dep)

        # resource edges: any two nodes owning a common file must serialize.
        files_by: dict[str, set[str]] = {
            nid: set(by_id[nid].get("owns_files") or []) for nid in ids
        }
        sorted_ids = sorted(ids)
        for i, a in enumerate(sorted_ids):
            for b in sorted_ids[i + 1:]:
                shared = files_by[a] & files_by[b]
                if shared:
                    f = sorted(shared)[0]
                    edges.append(ExecEdge(a, b, "resource",
                                          f"shared file {f}: writes serialized to avoid collision"))
                    justifications[a] = "resource"
                    justifications[b] = "resource"

        # verification edges: node -> its verifier node (when the verifier is a node).
        for nid, e in by_id.items():
            v = str(e.get("verifier") or "").strip()
            if v and v in ids and v != nid:
                edges.append(ExecEdge(nid, v, "verification",
                                      f"{v} must attest {nid}'s output against its contract"))
                justifications[v] = "verifier"

        # data edges: evidence producer -> consumer that requires it.
        produces: dict[str, set[str]] = {
            nid: set(by_id[nid].get("produces_evidence") or []) for nid in ids
        }
        for nid, e in by_id.items():
            for ev in (e.get("requires_evidence") or []):
                for src in sorted_ids:
                    if src != nid and ev in produces[src]:
                        edges.append(ExecEdge(src, nid, "data",
                                              f"evidence {ev} flows from {src} to {nid}"))

        # justification: fan-in parents branch, parallel siblings parallelize.
        succ_count: dict[str, int] = {nid: 0 for nid in ids}
        for nid, plist in preds.items():
            if len(plist) >= 2:
                justifications.setdefault(nid, "branching")
            for p in plist:
                succ_count[p] += 1
        for nid, n in succ_count.items():
            if n >= 2:  # one node feeds multiple dependents -> real parallelism downstream
                for nb in ids:
                    if nid in preds[nb]:
                        justifications.setdefault(nb, "parallelism")
        for nid in ids:
            justifications.setdefault(nid, "specialized")

        nodes = tuple(_entry_node(by_id[nid], justification=justifications[nid]) for nid in ids)

        # Collapse: a lone, unjustified (specialized-only) node IS the serial loop.
        if len(nodes) == 1 and nodes[0].justification == "specialized" and not edges:
            return ExecGraph(version=EXEC_GRAPH_VERSION, root=root, nodes=nodes, collapsed=True)

        barriers = tuple(
            Barrier(at=nid, expected=tuple(sorted(preds[nid])))
            for nid in sorted(ids)
            if len(preds[nid]) >= 2
        )
        return ExecGraph(
            version=EXEC_GRAPH_VERSION, root=root, nodes=nodes,
            edges=tuple(edges), barriers=barriers, collapsed=False,
        )
    except MalformedGraphError:
        # A schema defect in the derived topology is not fatal to the run:
        # degrade to the serial loop rather than block. (Plan: compilation
        # failure falls back to serial operation.)
        return _serial_fallback(root, entries)

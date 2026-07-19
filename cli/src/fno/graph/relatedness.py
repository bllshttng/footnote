"""Node-to-node relatedness for the backlog graph (deterministic v1).

Computes a lightweight relatedness map from signals already in ``graph.json``
- shared domain, shared epic (roadmap_id/parent), and token overlap over
title+slug+details - and persists it to a sidecar the offer path (x-9ed6) and
``/triage`` read. Pure logic here; CLI wiring lives in ``graph/cli.py``.

The map is a regenerable artifact (like codemap): last-writer-wins, atomic
write, never part of graph.json. ``build_map`` only READS entries.
"""
from __future__ import annotations

import json
import os
import re
import tempfile
from pathlib import Path
from typing import Any, Optional

Entry = dict[str, Any]

# Small stopword set - drop the words that co-occur in most backlog titles and
# would otherwise inflate every Jaccard score toward noise.
_STOPWORDS = frozenset({
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with",
    "is", "are", "be", "at", "by", "as", "it", "its", "this", "that", "from",
    "add", "fix", "update", "make", "use", "via", "not", "no", "so",
})

_TOKEN_RE = re.compile(r"[a-z0-9]+")

# Below this combined score a pair is dropped as unrelated.
_MIN_SCORE = 0.15
_DOMAIN_BONUS = 0.10
_EPIC_BONUS = 0.25


class NoMapError(Exception):
    """The relatedness sidecar does not exist / could not be read.

    Distinct from "node has no related edges" (a valid empty list) so callers
    (x-9ed6's offer path) can fall back correctly.
    """


def _keep(t: str) -> bool:
    # Drop stopwords, sub-3-char fragments, and pure-digit tokens: date parts
    # ("04", "19", "2026") and ids in details are high-frequency noise that
    # would rank nodes by shared dates instead of shared meaning.
    return len(t) >= 3 and not t.isdigit() and t not in _STOPWORDS


def _tokens(e: Entry) -> frozenset[str]:
    text = " ".join(
        v for f in ("title", "slug", "details") if isinstance((v := e.get(f)), str)
    ).lower()
    return frozenset(t for t in _TOKEN_RE.findall(text) if _keep(t))


def _epic_key(e: Entry) -> Optional[str]:
    # An epic is a roadmap group or an explicit parent; either shared is a
    # strong relatedness signal.
    for f in ("roadmap_id", "parent"):
        v = e.get(f)
        if isinstance(v, str) and v.strip():
            return f"{f}:{v}"
    return None


def _score(a: Entry, b: Entry, ta: frozenset[str], tb: frozenset[str]) -> tuple[float, str]:
    """Combined relatedness score for a pair + a one-line reason. 0 => drop."""
    reasons: list[str] = []
    combined = 0.0

    if ta and tb:
        inter = ta & tb
        if inter:
            jac = len(inter) / len(ta | tb)
            combined += jac
            shown = sorted(inter)[:3]
            reasons.append(f"{len(inter)} shared terms ({', '.join(shown)})")

    da, db = a.get("domain"), b.get("domain")
    if isinstance(da, str) and da and da == db:
        combined += _DOMAIN_BONUS
        reasons.append(f"shared domain '{da}'")

    ea, eb = _epic_key(a), _epic_key(b)
    if ea is not None and ea == eb:
        combined += _EPIC_BONUS
        reasons.append(f"same epic ({ea})")

    if combined < _MIN_SCORE:
        return 0.0, ""
    return round(combined, 4), "; ".join(reasons)


# An epic in one of these states is no longer a rollup target.
_RETIRED_EPIC_STATUSES = frozenset({"done", "superseded", "deferred"})


def epic_candidates(
    entry: Entry, entries: list[Entry], k: int = 3
) -> list[tuple[str, float, str]]:
    """Score ``entry`` against the live epics only, best-first, top-K.

    The rollup counterpart to ``build_map``: same ``_score``, narrowed to
    candidate parents so intake, ``maintain``, and ``/think`` cannot drift into
    a second similarity implementation. Ties break on id so a run is
    reproducible. Pairs below ``_MIN_SCORE`` are absent (``_score`` drops them).
    """
    ta = _tokens(entry)
    nid = entry.get("id")
    scored: list[tuple[str, float, str]] = []
    for e in entries:
        if not isinstance(e, dict) or e.get("type") != "epic":
            continue
        eid = e.get("id")
        if not isinstance(eid, str) or eid == nid:
            continue
        if e.get("_status") in _RETIRED_EPIC_STATUSES:
            continue
        score, reason = _score(entry, e, ta, _tokens(e))
        if score > 0.0:
            scored.append((eid, score, reason))
    scored.sort(key=lambda r: (-r[1], r[0]))
    return scored[:k]


def build_map(entries: list[Entry], k: int = 5) -> dict[str, list[dict[str, Any]]]:
    """Return ``{node_id: [{id, score, reason}, ...]}`` best-first, top-K.

    Read-only over ``entries``. Zero-signal pairs are absent. Rows without a
    string ``id`` are skipped (malformed, not fatal). Empty graph -> ``{}``.
    """
    nodes = [(nid, e, _tokens(e)) for e in entries if isinstance((nid := e.get("id")), str)]

    # ponytail: O(n^2) pair scan, fine for a nightly batch over ~2300 nodes.
    # Upgrade path if it ever drags: an inverted token index to skip zero-overlap
    # pairs before scoring.
    result: dict[str, list[dict[str, Any]]] = {nid: [] for nid, _, _ in nodes}
    for i in range(len(nodes)):
        nid_a, a, ta = nodes[i]
        for j in range(i + 1, len(nodes)):
            nid_b, b, tb = nodes[j]
            score, reason = _score(a, b, ta, tb)
            if score <= 0.0:
                continue
            result[nid_a].append({"id": nid_b, "score": score, "reason": reason})
            result[nid_b].append({"id": nid_a, "score": score, "reason": reason})

    for nid in result:
        result[nid].sort(key=lambda r: r["score"], reverse=True)
        del result[nid][k:]
    return result


def write_map(path: Path, mapping: dict[str, list[dict[str, Any]]]) -> None:
    """Atomically write the map (temp + os.replace) so a reader never sees a
    partial file. Raises on write failure - never swallowed."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=str(path.parent), suffix=".tmp")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(json.dumps(mapping, indent=2) + "\n")
        os.replace(tmp, str(path))
    except Exception:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def get_related(path: Path, node_id: str, k: Optional[int] = None) -> list[dict[str, Any]]:
    """Return the top related nodes for ``node_id`` (best-first, capped at k).

    Raises ``NoMapError`` when the sidecar is missing/unreadable. A present map
    with no edges for ``node_id`` returns ``[]`` - the two cases are distinct so
    callers fall back correctly (AC3).
    """
    if not path.exists():
        raise NoMapError(f"no relatedness map at {path}")
    try:
        # A corrupt/unreadable map RAISES (distinct from "no edges") so callers
        # fall back correctly - do not degrade to an empty map here. ValueError
        # covers json.JSONDecodeError and UnicodeDecodeError.
        mapping = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise NoMapError(f"unreadable relatedness map at {path}: {exc}") from exc
    edges = mapping.get(node_id, [])
    if not isinstance(edges, list):
        edges = []
    return edges[:k] if k is not None else edges

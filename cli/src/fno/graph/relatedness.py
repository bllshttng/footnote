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
import subprocess
import tarfile
import tempfile
from io import BytesIO
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
_FULL_SHA_RE = re.compile(r"[0-9a-f]{40}", re.IGNORECASE)

# Below this combined score a pair is dropped as unrelated.
_MIN_SCORE = 0.15
_DOMAIN_BONUS = 0.10
_EPIC_BONUS = 0.25

# Filing-time dedup floor: above this an existing node is surfaced as a likely
# duplicate when a new node is born. Evidence-based (plan x-6ac7 Overview):
# real specimen duplicates score 0.568 and 0.447, the epic-sibling noise pair
# sits at 0.234, and the floor is 0/1558 false positives on the live graph.
_DEDUP_MIN_SCORE = 0.30


def _resolve_main_sha(repo: Path) -> str | None:
    try:
        result = subprocess.run(
            ["git", "ls-remote", "--exit-code", "origin", "refs/heads/main"],
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    fields = result.stdout.strip().split()
    if (
        result.returncode != 0
        or len(fields) != 2
        or fields[1] != "refs/heads/main"
        or _FULL_SHA_RE.fullmatch(fields[0]) is None
    ):
        return None
    return fields[0].lower()


def _commit_on_main(repo: Path, commit: str, main_sha: str) -> bool:
    try:
        result = subprocess.run(
            ["git", "merge-base", "--is-ancestor", commit, main_sha],
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0


def _run_main_probe(repo: Path, main_sha: str, command: list[str]) -> dict[str, Any] | None:
    if (
        not command
        or len(command) > 128
        or not all(isinstance(arg, str) and arg and len(arg) <= 4096 for arg in command)
    ):
        return None
    runner = Path(command[0])
    if runner.is_absolute() or ".." in runner.parts:
        return None
    try:
        archive = subprocess.run(
            ["git", "archive", "--format=tar", main_sha],
            cwd=repo,
            capture_output=True,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if archive.returncode != 0:
        return None
    try:
        with tempfile.TemporaryDirectory(prefix="fno-closure-") as temp_dir:
            with tarfile.open(fileobj=BytesIO(archive.stdout), mode="r:") as snapshot:
                snapshot.extractall(temp_dir, filter="data")
            snapshot_root = Path(temp_dir).resolve()
            executable = (snapshot_root / runner).resolve()
            if (
                snapshot_root not in executable.parents
                or not executable.is_file()
                or not os.access(executable, os.X_OK)
            ):
                return None
            observed = subprocess.run(
                [str(executable), *command[1:]],
                cwd=temp_dir,
                capture_output=True,
                text=True,
                timeout=60,
            )
    except (OSError, subprocess.TimeoutExpired, tarfile.TarError):
        return None
    return {
        "status": "passed" if observed.returncode == 0 else "failed",
        "exit_code": observed.returncode,
    }


def _commit_names_behavior(repo: Path, commit: str, behavior: str) -> bool:
    try:
        result = subprocess.run(
            ["git", "show", "-s", "--format=%B", commit],
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return result.returncode == 0 and behavior.casefold() in result.stdout.casefold()


def classify_closure(
    *,
    behavior: str,
    repo: Path | None = None,
    probe_command: list[str] | None = None,
    merged_commit: str | None = None,
) -> dict[str, Any]:
    """Classify pre-design closure without treating carriers as evidence.

    A current-main observation takes precedence over history. Positive and
    negative probe results both bind the named behavior, command, and full HEAD;
    every incomplete or unresolved specimen remains ``unknown``.
    """
    behavior = behavior.strip()
    unknown = {
        "state": "unknown",
        "behavior": behavior or None,
        "proof": None,
    }
    if not behavior:
        return unknown
    if repo is None:
        return unknown
    main_sha = _resolve_main_sha(repo)
    if main_sha is None:
        return unknown

    if probe_command is not None:
        observation = _run_main_probe(repo, main_sha, probe_command)
        if observation is None:
            return unknown
        status = observation["status"]
        return {
            "state": "already_shipped" if status == "passed" else "live",
            "behavior": behavior,
            "proof": {
                "kind": "current_main_probe",
                "command": list(probe_command),
                "head": main_sha,
                "result": status,
                "exit_code": observation["exit_code"],
                "main_head": main_sha,
            },
        }

    if merged_commit is not None:
        if (
            not isinstance(merged_commit, str)
            or _FULL_SHA_RE.fullmatch(merged_commit) is None
            or not _commit_on_main(repo, merged_commit, main_sha)
            or not _commit_names_behavior(repo, merged_commit, behavior)
        ):
            return unknown
        return {
            "state": "already_shipped",
            "behavior": behavior,
            "proof": {
                "kind": "merged_commit",
                "commit": merged_commit.lower(),
                "observed": behavior,
                "on_current_main": True,
                "main_head": main_sha,
            },
        }

    return unknown


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


def _score(
    a: Entry,
    b: Entry,
    ta: frozenset[str],
    tb: frozenset[str],
    *,
    include_epic: bool = True,
) -> tuple[float, str]:
    """Combined relatedness score for a pair + a one-line reason. 0 => drop.

    ``include_epic`` drops the epic-parent bonus: dedup scoring calls with it
    False, because two children of one epic are related, not duplicates, and the
    +0.25 bonus would push an epic-sibling pair past the dedup threshold.
    """
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

    if include_epic:
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
        if e.get("status") in _RETIRED_EPIC_STATUSES:
            continue
        score, reason = _score(entry, e, ta, _tokens(e))
        if score > 0.0:
            scored.append((eid, score, reason))
    scored.sort(key=lambda r: (-r[1], r[0]))
    return scored[:k]


def similar_nodes(
    entry: Entry, entries: list[Entry], k: int = 3, *, floor: float | None = None
) -> list[tuple[str, float, str]]:
    """Score ``entry`` against every live node for filing-time dedup, top-K.

    The dedup twin of ``epic_candidates``: same ``_score`` substrate, narrowed
    to duplicate detection at node birth. Three differences from rollup
    scoring: every non-superseded node is a candidate (not just epics - a
    shipped ``done`` node is the answer to a duplicate filing), the epic bonus
    is excluded (siblings under one epic are related, not duplicates), and the
    floor is the dedup threshold ``_DEDUP_MIN_SCORE`` instead of ``_MIN_SCORE``.
    Ties break on id for reproducibility. The just-born node's own id is
    excluded so a filing never warns about itself.

    ``floor`` narrows that third difference for readers who want the ranked
    list (blueprint's consolidation gate) rather than a dedup verdict: the real
    lock family behind that gate scores 0.26-0.27 and pure noise scores the
    same, so no threshold separates them - recall is the scorer's job,
    judgment is the full-context reader's. The default stays
    ``_DEDUP_MIN_SCORE`` so intake's tuned 0.30 behavior is unchanged. One
    scorer, one parameter (the ``include_epic`` precedent), never a second
    implementation.
    """
    threshold = _DEDUP_MIN_SCORE if floor is None else floor
    ta = _tokens(entry)
    nid = entry.get("id")
    by_id = {
        e.get("id"): e
        for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    # Exclude the node's own lineage: a child legitimately resembles its parent
    # (rollup parented it there for exactly that reason), so flagging an
    # ancestor as a duplicate would cry wolf on most filings and recommend
    # superseding the node's own epic (codex P2).
    lineage: set[str] = set()
    cur = entry.get("parent")
    while isinstance(cur, str) and cur in by_id and cur not in lineage:
        lineage.add(cur)
        cur = by_id[cur].get("parent")
    scored: list[tuple[str, float, str]] = []
    for e in entries:
        if not isinstance(e, dict):
            continue
        eid = e.get("id")
        if not isinstance(eid, str) or eid == nid or eid in lineage:
            continue
        if e.get("status") == "superseded":
            continue
        score, reason = _score(entry, e, ta, _tokens(e), include_epic=False)
        if score >= threshold:
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

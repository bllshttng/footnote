"""Backlog PR-merge drift detection for ``fno backlog reconcile``.

Pure logic split from I/O, mirroring ``fno.megatron.reconcile``. The
GitHub query is the only I/O dependency; tests stub ``query_pr_merge_state``
via the ``query`` parameter on :func:`scan_merge_drift`.

The completion ritual (stamp plan -> mark node ``done`` -> capture follow-ups)
runs automatically only when work flows through ``/target``'s ship gate or
``scripts/lib/pr-merge.sh``. A PR merged any other way (manual GitHub merge,
bare ``gh pr merge``) leaves the node open. This module detects that drift so
the CLI can close it mechanically, no memory required.
"""
from __future__ import annotations

import hashlib
import json
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from functools import lru_cache
from pathlib import Path
from typing import Any, Callable, Literal, Optional

# `gh pr view` default timeout; covers network hangs and stuck auth prompts.
GH_QUERY_TIMEOUT_S = 30.0


@lru_cache(maxsize=1)
def _gh_executable() -> Optional[str]:
    """Resolve the gh binary path once per process.

    query_pr_merge_state runs per-PR inside scan_merge_drift's loop, so a
    bare shutil.which("gh") would re-scan PATH on every node. Cached here.
    """
    return shutil.which("gh")

# Extract `owner/repo` from a GitHub PR/issue URL. A trailing `.git` is never
# present on web URLs but stripped defensively. Stops before `/pull/<n>`.
_PR_URL_RE = re.compile(r"github\.com/([^/\s]+)/([^/\s]+?)(?:\.git)?/(?:pull|issues)/")


def repo_slug_from_url(url: Optional[str]) -> Optional[str]:
    """Return ``owner/repo`` parsed from a GitHub PR URL, or None.

    Used to scope ``gh pr view`` to the node's actual repository via
    ``--repo``. Without this, gh resolves the PR number against whatever repo
    the process happens to run in, so a same-number PR in a different repo
    could be mistaken for the node's PR.
    """
    if not url:
        return None
    m = _PR_URL_RE.search(url)
    return f"{m.group(1)}/{m.group(2)}" if m else None

# GitHub PR states we resolve from gh, plus the "UNKNOWN" sentinel used on a
# drift record whose query failed. Kept here as the single home for the
# vocabulary so both types below reference it rather than bare strings.
PrStateLiteral = Literal["OPEN", "CLOSED", "MERGED", "UNKNOWN"]


class ReconcileError(Exception):
    """Raised on gh failure or other I/O errors during a PR-state query."""


@dataclass
class PrMergeState:
    number: int
    # PrStateLiteral rather than the 3-value subset: query_pr_merge_state
    # falls back to "UNKNOWN" when gh omits the state field.
    state: PrStateLiteral
    url: Optional[str]
    merged_at: Optional[str]


@dataclass
class MergeDriftRecord:
    """One open node carrying a PR whose GitHub state we resolved.

    ``closeable`` records hold a MERGED PR and are safe to close. Records with
    a non-None ``error`` could not be resolved (gh failure) and are surfaced
    but never closed.
    """

    node_id: str
    plan_path: Optional[str]
    pr_number: int
    pr_url: Optional[str]
    pr_state: PrStateLiteral
    merged_at: Optional[str]
    error: Optional[str] = None
    # The owning target session + its working directory, carried from the graph
    # node so the CLI can emit a session_satisfied event for that session after
    # closing the node (Group 1 / ab-f7f8bc53). Both optional: a node may have
    # been intaken without a session (session_id null) or without a cwd.
    session_id: Optional[str] = None
    cwd: Optional[str] = None

    @property
    def closeable(self) -> bool:
        return self.error is None and self.pr_state == "MERGED"


def node_is_open(node: dict) -> bool:
    """A node is open when it is neither done nor superseded.

    Keyed off the underlying fields rather than the derived ``_status`` so the
    predicate holds even on an entries list that has not been through
    ``recompute_statuses`` (e.g. a raw test fixture).
    """
    return not node.get("completed_at") and not node.get("superseded_by")


def node_pr_refs(node: dict) -> list[tuple[int, Optional[str]]]:
    """Return ``(pr_number, pr_url)`` pairs for a node, primary first.

    Combines the primary ``pr_number``/``pr_url`` with any ``additional_prs``
    entries. De-duplicates by number (primary wins) so a PR listed in both
    places is queried once.
    """
    refs: list[tuple[int, Optional[str]]] = []
    seen: set[int] = set()

    primary = node.get("pr_number")
    if isinstance(primary, int):
        refs.append((primary, node.get("pr_url")))
        seen.add(primary)

    for extra in node.get("additional_prs") or []:
        if not isinstance(extra, dict):
            continue
        num = extra.get("number")
        if not isinstance(num, int) or num in seen:
            continue
        refs.append((num, extra.get("url")))
        seen.add(num)

    return refs


def query_pr_merge_state(
    pr_number: int,
    *,
    repo: Optional[str] = None,
    cwd: Optional[str] = None,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    timeout_s: float = GH_QUERY_TIMEOUT_S,
) -> PrMergeState:
    """Shell out to ``gh pr view <n> --json ...`` and parse the result.

    ``repo`` (``owner/repo``) scopes the lookup explicitly via ``--repo`` and
    is the authoritative way to avoid resolving a PR number against the wrong
    repository. ``cwd`` is a weaker fallback (gh infers owner/repo from the
    working directory's origin remote). Raises :class:`ReconcileError` on any
    gh failure (missing binary, auth, network, parse error, timeout).
    """
    if _gh_executable() is None:
        raise ReconcileError("gh CLI not found on PATH")

    cmd = ["gh", "pr", "view", str(pr_number)]
    if repo:
        cmd += ["--repo", repo]
    cmd += ["--json", "number,state,url,mergedAt"]
    try:
        result = runner(
            cmd,
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout_s,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired as exc:
        raise ReconcileError(
            f"gh pr view #{pr_number} timed out after {timeout_s}s"
        ) from exc
    except OSError as exc:
        raise ReconcileError(f"gh subprocess failed to launch: {exc}") from exc

    if result.returncode != 0:
        raise ReconcileError(
            f"gh pr view #{pr_number} failed (rc={result.returncode}): "
            f"{(result.stderr or '').strip()}"
        )

    try:
        row = json.loads(result.stdout or "{}")
    except json.JSONDecodeError as exc:
        raise ReconcileError(f"gh stdout was not JSON: {exc}") from exc

    return PrMergeState(
        number=row.get("number", pr_number),
        state=row.get("state", "UNKNOWN"),
        url=row.get("url"),
        merged_at=row.get("mergedAt"),
    )


# W4 causal links: best-effort revert detection. A GitHub revert PR titles
# itself `Revert "..."` and auto-writes `Reverts owner/repo#N` in the body;
# a hand-written one usually keeps the git subject. Misses are a documented
# limitation with the manual `fno backlog update --reverted` fallback.
_REVERT_TITLE_RE = re.compile(r"^\s*Revert\b")
_REVERT_BODY_PR_RE = re.compile(r"\breverts\s+(?:([\w.-]+/[\w.-]+))?#(\d+)", re.IGNORECASE)


def fetch_recent_merged_prs(
    *,
    repo: Optional[str] = None,
    cwd: Optional[str] = None,
    limit: int = 30,
    runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
    timeout_s: float = GH_QUERY_TIMEOUT_S,
) -> list[dict]:
    """Recently merged PRs (number/title/body) for revert detection.

    Returns ``[]`` when gh is absent (reconcile auto-fires on SessionStart;
    a gh-less machine must stay quiet). Raises :class:`ReconcileError` on a
    real gh failure so the caller can degrade with one warning.
    """
    if _gh_executable() is None:
        return []
    cmd = ["gh", "pr", "list", "--state", "merged", "--limit", str(limit)]
    if repo:
        cmd += ["--repo", repo]
    cmd += ["--json", "number,title,body,url"]
    try:
        result = runner(
            cmd, capture_output=True, text=True, check=False, timeout=timeout_s, cwd=cwd
        )
    except (subprocess.TimeoutExpired, OSError) as exc:
        raise ReconcileError(f"gh pr list failed: {exc}") from exc
    if result.returncode != 0:
        raise ReconcileError(
            f"gh pr list failed (rc={result.returncode}): {(result.stderr or '').strip()}"
        )
    try:
        rows = json.loads(result.stdout or "[]")
    except json.JSONDecodeError as exc:
        raise ReconcileError(f"gh stdout was not JSON: {exc}") from exc
    return rows if isinstance(rows, list) else []


def detect_reverted_nodes(
    merged_prs: list[dict], entries: list[dict]
) -> list[tuple[str, int]]:
    """(node_id, revert_pr_number) pairs to stamp ``reverted: true``.

    A merged PR whose title starts with ``Revert`` and whose body references
    a PR number carried by a not-yet-reverted graph node names that node's
    ship as reverted. Pure (no I/O) so tests need no gh.

    Matching is REPO-SCOPED: the graph is global across projects, so bare PR
    numbers collide. A candidate node must carry a ``pr_url`` in the SAME
    repo as the revert PR's own ``url``, a body qualifier
    (``Reverts other/repo#N``) must match that repo, and an ambiguous match
    (two same-repo nodes on one number) stamps nothing - the same
    ambiguity-resolves-to-nothing rule as the W1 backfill.
    """
    # pr_number -> [(entry, that ref's repo slug)]; refs without a parseable
    # pr_url are indexed with slug None and never match (conservative).
    by_pr: dict[int, list[tuple[dict, Optional[str]]]] = {}
    for e in entries:
        for num, url in node_pr_refs(e):
            by_pr.setdefault(num, []).append((e, repo_slug_from_url(url)))

    out: list[tuple[str, int]] = []
    seen: set[str] = set()
    for pr in merged_prs:
        number = pr.get("number")
        if not isinstance(number, int) or number <= 0:
            continue
        if not _REVERT_TITLE_RE.match(str(pr.get("title") or "")):
            continue
        revert_slug = repo_slug_from_url(pr.get("url"))
        if revert_slug is None:
            # No repo context for the revert PR itself: refuse to match a
            # bare number against the multi-project graph.
            continue
        for m in _REVERT_BODY_PR_RE.finditer(str(pr.get("body") or "")):
            qualifier, target = m.group(1), int(m.group(2))
            if qualifier and qualifier.lower() != revert_slug.lower():
                continue  # explicit cross-repo reference: not this repo's PR
            matches = [
                e
                for e, slug in by_pr.get(target, [])
                if slug is not None
                and slug.lower() == revert_slug.lower()
                and not e.get("reverted")
            ]
            hit_ids = {e.get("id") for e in matches}
            if len(hit_ids) != 1:
                continue  # zero or ambiguous: stamp nothing
            nid = hit_ids.pop()
            if isinstance(nid, str) and nid not in seen:
                seen.add(nid)
                out.append((nid, number))
    return out


def scan_merge_drift(
    entries: list[dict],
    *,
    query: Optional[Callable[..., PrMergeState]] = None,
    node_id: Optional[str] = None,
) -> list[MergeDriftRecord]:
    """Find open nodes whose PR has merged outside the ship gate.

    Returns one record per open node that resolves to a MERGED PR, plus
    records flagged with ``error`` for nodes whose PR state could not be
    resolved. Nodes whose PRs are all still OPEN (or closed-unmerged) yield no
    record - they are not drift. ``node_id`` restricts the scan to a single
    node. Tests inject a ``query`` stub to avoid shelling out to gh.
    """
    if query is None:
        query = query_pr_merge_state

    records: list[MergeDriftRecord] = []

    for node in entries:
        nid = node.get("id")
        if not isinstance(nid, str):
            continue
        if node_id is not None and nid != node_id:
            continue
        if not node_is_open(node):
            continue

        refs = node_pr_refs(node)
        if not refs:
            continue

        cwd = node.get("cwd")
        merged: Optional[PrMergeState] = None
        first_error: Optional[str] = None

        for number, url in refs:
            # Prefer an explicit repo parsed from the PR URL so we never
            # resolve a PR number against the wrong repository. Fall back to
            # the node's cwd only when no URL is available. With neither, we
            # cannot safely identify the repo: record a failure rather than
            # risk closing a node off a same-numbered PR elsewhere.
            repo = repo_slug_from_url(url)
            if repo is None and not cwd:
                if first_error is None:
                    first_error = (
                        f"PR #{number}: no repo context (pr_url unparseable and "
                        f"cwd unset); refusing to query to avoid a wrong-repo match"
                    )
                continue
            try:
                state = query(number, repo=repo, cwd=cwd)
            except ReconcileError as exc:
                if first_error is None:
                    first_error = str(exc)
                continue
            if state.state == "MERGED":
                merged = state
                break

        if merged is not None:
            records.append(
                MergeDriftRecord(
                    node_id=nid,
                    plan_path=node.get("plan_path"),
                    pr_number=merged.number,
                    pr_url=merged.url,
                    pr_state="MERGED",
                    merged_at=merged.merged_at,
                    session_id=node.get("session_id"),
                    cwd=cwd,
                )
            )
        elif first_error is not None:
            # Could not resolve any PR for this open node. Surface it so the
            # caller can report a query failure rather than silently dropping.
            number, url = refs[0]
            records.append(
                MergeDriftRecord(
                    node_id=nid,
                    plan_path=node.get("plan_path"),
                    pr_number=number,
                    pr_url=url,
                    pr_state="UNKNOWN",
                    merged_at=None,
                    error=first_error,
                    session_id=node.get("session_id"),
                    cwd=cwd,
                )
            )

    return records


def write_retro_sentinel(record: MergeDriftRecord, *, sentinel_dir: Path) -> Path:
    """Drop a per-node retro sentinel naming a node closed by reconcile.

    The sentinel hands the judgment half (follow-up capture via inbox/triage)
    to a later session's LLM/human pass. Reconcile must NOT auto-create inbox
    lines or backlog nodes - that stays explicit. Overwrites an existing
    sentinel for the same node (idempotent).

    Only ever called with a closeable (resolved-merged) record; the assertion
    catches misuse (e.g. handing it an error record) in tests and dev.
    """
    assert record.closeable, f"refusing to write sentinel for non-closeable {record.node_id}"
    sentinel_dir.mkdir(parents=True, exist_ok=True)
    path = sentinel_dir / f"{record.node_id}.json"
    payload = {
        "node_id": record.node_id,
        "pr_number": record.pr_number,
        "pr_url": record.pr_url,
        "merged_at": record.merged_at,
        "plan_path": record.plan_path,
        "closed_by": "backlog-reconcile",
        "closed_at": datetime.now(timezone.utc).isoformat(),
    }
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return path


def _owning_state_path(record: MergeDriftRecord) -> Optional[Path]:
    """Resolve the owning target-state.md from a record's ``cwd``.

    Returns None when the record carries no usable cwd (nothing to satisfy).
    The ``isinstance`` guard matters because ``cwd`` is copied raw from the graph
    node (``node.get("cwd")``) with no type check; a corrupted/hand-edited graph
    could carry a non-string cwd, and ``Path(non_str)`` raises TypeError. A raise
    here would abort the reconcile loop and strand later records, violating the
    best-effort contract - so a non-string cwd is treated as "no cwd".
    """
    if not isinstance(record.cwd, str) or not record.cwd:
        return None
    return Path(record.cwd) / ".fno" / "target-state.md"


def _read_state_frontmatter(state_path: Path) -> dict[str, Any]:
    """Parse the YAML frontmatter of a target-state.md into a dict.

    Local parse (mirrors ``worker.reconcile._read_state``) so this module does
    not couple to a private events helper. Returns {} on any parse problem.
    """
    import yaml  # local import: keeps the module's import cost off the hot scan path

    try:
        text = state_path.read_text(encoding="utf-8")
    except OSError:
        return {}
    if not text.startswith("---"):
        return {}
    rest = text[3:].lstrip("\n")
    end = rest.find("\n---")
    if end == -1:
        return {}
    try:
        return yaml.safe_load(rest[:end]) or {}
    except yaml.YAMLError:
        return {}


def emit_session_satisfied_for_record(
    record: MergeDriftRecord,
    *,
    reason: str = "reconcile_detected_merge",
) -> Optional[Path]:
    """Emit a ``session_satisfied{source:"pr_merge"}`` event for the target
    session that owns a merged-and-now-closed node (Group 1 / ab-f7f8bc53).

    Today only an in-gate merge through ``scripts/lib/pr-merge.sh`` emits this
    signal, so an out-of-band merge (web button, bare ``gh pr merge``) leaves the
    owning session hot and the stop hook hard re-blocks it. After reconcile
    closes the drifted node, this hands the same auto-complete signal to the
    owning session.

    The event binds to that session via ``session_id`` + ``gate_state_hash`` (the
    md5 of the owning target-state.md at emit time), matching the stop hook's
    staleness check (``check_session_satisfied``). The defensive stop-hook probe
    (Task 1.2) is the backstop for when this emit is stale or never lands.

    Best-effort and non-fatal: returns the events.jsonl path on a successful
    emit, or None when there is nothing to satisfy (no cwd, no live state file,
    already-COMPLETE session, missing session_id) or any failure. A failure here
    must never abort the reconcile close.
    """
    state_path = _owning_state_path(record)
    if state_path is None or not state_path.exists():
        return None

    fields = _read_state_frontmatter(state_path)
    # Only satisfy a session that is still live. A COMPLETE/BLOCKED/ABORTED
    # session needs no nudge; emitting would be noise.
    if str(fields.get("status") or "").strip() != "IN_PROGRESS":
        return None

    # Cross-check the resolved state file actually owns THIS node's PR before
    # nudging it. A cwd can be recycled: node A (merged, being reconciled here)
    # recorded a worktree that a live session is now using for an unrelated node
    # B. Without this guard we would emit a pr_merge session_satisfied bound to
    # B's session. It is not a false-complete (B's stop hook re-derives the
    # verified-merge ground truth against B's own pr_number and re-enforces its
    # gates), but it is a spurious cross-session signal. Skip unless the state
    # file's pr_number matches the merged PR we are reconciling.
    state_pr = fields.get("pr_number")
    try:
        if state_pr is None or int(state_pr) != int(record.pr_number):
            return None
    except (TypeError, ValueError):
        return None

    # The state file is the source of truth the stop hook compares against, so
    # read session_id from it (not record.session_id, which may be stale).
    session_id = str(fields.get("session_id") or "").strip()
    if not session_id or session_id == "null":
        return None

    try:
        gate_state_hash = hashlib.md5(state_path.read_bytes()).hexdigest()
    except OSError:
        return None

    events_path = state_path.parent / "events.jsonl"
    try:
        from fno import events

        event = events.session_satisfied(
            trigger="pr_merge",
            reason=reason,
            session_id=session_id,
            gate_state_hash=gate_state_hash,
            evidence_url=record.pr_url,
            source="backlog",
        )
        events.append_event(event, events_path)
    except Exception as exc:  # best-effort: never abort the close on emit failure
        print(
            f"reconcile: session_satisfied emit failed for {record.node_id} "
            f"(session={session_id}): {type(exc).__name__}: {exc}; "
            f"merge close unaffected, defensive stop-hook probe is the backstop",
            file=sys.stderr,
        )
        return None
    return events_path


def emit_human_touch_for_record(record: MergeDriftRecord) -> Optional[Path]:
    """Emit ``human_touch{source:merge}`` for a node reconcile just closed (W4).

    An out-of-band merge (web merge button, bare ``gh pr merge``) is a human
    steering action no loop performed. Reconcile closes a node exactly once (a
    closed node is no longer scanned), so this fires once per node with no
    separate idempotence ledger (AC4-EDGE). Best-effort: a failure prints a
    diagnostic and never aborts the close.
    """
    try:
        from fno.events import _build, append_event

        event = _build(
            "human_touch",
            "backlog",
            {
                "graph_node_id": record.node_id,
                "source": "merge",
                "resolution": "ok",
            },
        )
        if isinstance(record.cwd, str) and record.cwd:
            events_path = Path(record.cwd) / ".fno" / "events.jsonl"
            append_event(event, events_path=events_path)
            return events_path
        append_event(event)
        return None
    except Exception as exc:  # best-effort: never abort the close on emit failure
        print(
            f"reconcile: human_touch emit failed for {record.node_id}: "
            f"{type(exc).__name__}: {exc}; merge close unaffected",
            file=sys.stderr,
        )
        return None

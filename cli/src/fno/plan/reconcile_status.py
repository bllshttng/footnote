"""fno plan reconcile-status - normalize drifted plan frontmatter status in place.

Plans stay FLAT in the plans dir; an Obsidian Base filters by frontmatter
``status``. Drifted or blank statuses lie to that Base, so this one-shot-then-
idempotent sweep rewrites them to the canonical vocabulary (x-ff83 W2):

    axis:      design ready in_progress shipped
    terminals: done archived   (off-axis, written directly)

Three tiers. Tier 1 is a pure synonym rewrite (no history needed). Tier 2 (blank
/ ``implemented`` / any unknown token) needs a true-state signal: a linked node
that is closed -> ``done``, else ``archived`` (an honest "off the board", never
a false ``done``). Tier 3 recomputes a CANONICAL-but-stale status from the
linked node's derived ``_status`` (the x-76ea class: plan ``design`` while its
node is ``done``), forward-only and graph-required. Dry-run by default;
``--apply`` writes.

Only DRIFT tokens are in scope, so a canonical status is never touched: the
sweep corrects, never downgrades, and is safe to re-run - after a human
re-activates an ``archived`` plan to (say) ``design``, the next run skips it.
The status scalar is rewritten as a single-line double-quoted value and the
body is left byte-for-byte intact (the graduate/_stamp wrapped-scalar parser
chokes on multi-line status; keep it single-line).
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from datetime import datetime, timezone
from functools import lru_cache
from pathlib import Path
from typing import Callable, Optional

from fno.plan._doc import load_plan
from fno.plan._stamp import _atomic_write
from fno.plan._status import KNOWN_STATUSES, project_plan_status

# Tier 1: pure synonym rewrite. Roughly half the drift is node-lifecycle
# vocabulary (idea/superseded/planned) leaking into plan frontmatter; the sweep
# touches plan `status:` only and never writes graph.json.
_TIER1: dict[str, str] = {
    "designed": "design",  # typo
    "draft": "design",
    "planned": "design",
    "pending": "design",
    "idea": "design",
    "ready-for-blueprint": "design",
    "design-locked": "ready",
    "reviewing": "shipped",  # pruned axis states (x-f34f) fold into shipped
    "shipping": "shipped",
    "superseded": "archived",
    "superseded-by-implementation": "archived",
}

# Frontmatter block: leading ---\n ... \n--- . Non-greedy so the FIRST block
# wins even if the body contains a --- rule.
_FRONT_RE = re.compile(r"\A(---\n)(?P<fm>.*?)(\n---)(?P<rest>.*)\Z", re.DOTALL)
_STATUS_LINE_RE = re.compile(r"(?m)^(?P<indent>[ \t]*)status[ \t]*:.*$")
_DONE_AT_LINE_RE = re.compile(r"(?m)^[ \t]*done_at[ \t]*:.*$")


def ensure_done_at(text: str, ts: str) -> str:
    """Append a `done_at: "<ts>"` line to the frontmatter if absent (first-write
    only). A sweep that promotes a plan to `done` must stamp the completion
    timestamp, else `done == done` no-ops leave `done_at` permanently missing.
    Byte-preserving: the body and existing keys are untouched.
    """
    m = _FRONT_RE.match(text)
    if not m or _DONE_AT_LINE_RE.search(m.group("fm")):
        return text
    fm = m.group("fm")
    line = f'done_at: "{ts}"'
    new_fm = f"{fm}\n{line}" if fm else line
    return m.group(1) + new_fm + m.group(3) + m.group("rest")


def _norm(raw: object) -> str:
    """Normalize a raw frontmatter status to a bare lowercase token."""
    return str(raw if raw is not None else "").strip().strip("'\"").lower()


def target_status(raw: object, signal: Callable[[], bool]) -> Optional[str]:
    """Canonical status a drifted `raw` should become, or None to leave it alone.

    ``signal`` is a thunk (evaluated lazily, only for the signal-gated tier) that
    returns True when the plan's linked node reads as closed/merged.
    """
    s = _norm(raw)
    if s in KNOWN_STATUSES:
        return None  # already canonical - the sweep corrects drift only
    if s in _TIER1:
        return _TIER1[s]
    # Tier 2: blank, implemented/REVISED, or any unrecognized token.
    return "done" if signal() else "archived"


def rewrite_status(text: str, new_status: str) -> Optional[str]:
    """Return *text* with the frontmatter `status:` scalar set to *new_status*.

    Rewrites (or, if absent, inserts) exactly the status line, double-quoted and
    single-line; the body is byte-for-byte unchanged. Returns None when *text*
    has no parseable frontmatter block (caller skips the file).
    """
    m = _FRONT_RE.match(text)
    if not m:
        return None
    fm = m.group("fm")
    line = f'status: "{new_status}"'
    if _STATUS_LINE_RE.search(fm):
        new_fm = _STATUS_LINE_RE.sub(lambda mm: f"{mm.group('indent')}{line}", fm, count=1)
    else:
        # No status key present (the "(no status)" drift): add one, first line.
        new_fm = f"{line}\n{fm}" if fm else line
    return m.group(1) + new_fm + m.group(3) + m.group("rest")


@dataclass
class SweepResult:
    normalized: int = 0  # rewritten to a non-archived canonical status
    archived: int = 0  # rewritten to `archived`
    skipped: int = 0  # already canonical, no frontmatter, or unparseable
    changes: list[tuple[str, str, str]] = field(default_factory=list)  # (path, old, new)
    warnings: list[str] = field(default_factory=list)

    def summary(self) -> str:
        return f"{self.normalized} normalized, {self.archived} archived, {self.skipped} skipped"


@lru_cache(maxsize=1)
def _done_node_ids() -> frozenset:
    """Ids of every closed (`_status == done`) node. Read once per process.

    ponytail: cached for the life of a one-shot sweep so the graph is parsed
    once, not once per plan file (gemini PR#149). A merged-PR probe would add gh
    calls; node-closed is the one cheap true-state signal.
    """
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json

        return frozenset(
            e.get("id") for e in read_graph(graph_json()) if e.get("_status") == "done"
        )
    except Exception:  # noqa: BLE001 - no graph => no signal => archived (honest)
        return frozenset()


def _plan_link_id(frontmatter: dict) -> Optional[str]:
    """The node id a plan links to: ``node``, then ``claims``, then
    ``graph_node_id`` (the legacy fallbacks stay for one release, until the
    US7 migration collapses the synonym keys).
    """
    return (
        frontmatter.get("node")
        or frontmatter.get("claims")
        or frontmatter.get("graph_node_id")
    )


def _default_signal(frontmatter: dict) -> bool:
    """True when the plan's linked node reads as closed."""
    node_id = _plan_link_id(frontmatter)
    return bool(node_id) and node_id in _done_node_ids()


@lru_cache(maxsize=1)
def _node_status_map() -> dict:
    """Map node id -> derived ``_status``. Empty when the graph is unreadable,
    which disables Tier 3 (it must never rewrite on absent evidence).
    """
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json

        return {
            e.get("id"): e.get("_status")
            for e in read_graph(graph_json())
            if e.get("id")
        }
    except Exception:  # noqa: BLE001 - no graph => no Tier 3
        return {}


def _tier3_target(
    frontmatter: dict, current: str, status_map: dict, warnings: list[str], name: str
) -> Optional[str]:
    """Canonical-but-stale -> the node's forward projection, or None to leave it.

    Fixes the x-76ea class (plan ``design`` while its node is ``done``). Requires
    a readable graph: an empty ``status_map`` disables Tier 3 (never rewrite on
    absent evidence). An unlinked plan is skipped; a link that resolves to no
    node in a readable graph is treated as unlinked and warned.
    """
    if not status_map:  # graph unreadable -> Tier 3 off (AC2-ERR)
        return None
    link = _plan_link_id(frontmatter)
    if not link:  # unlinked canonical plan -> Tier 3 skips it (AC2-EDGE)
        return None
    node_status = status_map.get(link)
    if node_status is None:
        warnings.append(f"tier3 skip (link {link} not in graph): {name}")
        return None
    return project_plan_status(current, node_status)


def sweep(
    plans_dir: Path,
    *,
    apply: bool = False,
    signal_for: Callable[[dict], bool] = _default_signal,
    status_map: Optional[dict] = None,
) -> SweepResult:
    """Scan every ``*.md`` in *plans_dir*, classify + (if apply) rewrite drift.

    Tier 1 (synonym) and Tier 2 (unknown token -> node signal) correct DRIFT
    tokens; Tier 3 recomputes a CANONICAL-but-stale status from the linked
    node's derived ``_status`` (forward-only, graph-required).
    """
    res = SweepResult()
    if not plans_dir.is_dir():
        res.warnings.append(f"plans dir not found: {plans_dir}")
        return res

    if status_map is None:
        status_map = _node_status_map()

    for path in sorted(plans_dir.glob("*.md")):
        try:
            text = path.read_text(encoding="utf-8")
            doc = load_plan(path)
        except Exception as exc:  # noqa: BLE001 - malformed => skip, body untouched
            res.skipped += 1
            res.warnings.append(f"skip (unparseable): {path.name}: {exc}")
            continue

        raw = doc.frontmatter.get("status")
        s = _norm(raw)
        if s in KNOWN_STATUSES:
            # Tier 3: a canonical status may still be stale vs its node.
            new = _tier3_target(doc.frontmatter, s, status_map, res.warnings, path.name)
        else:
            # Tiers 1-2: drift-token rewrite (synonym / signal-gated).
            new = target_status(raw, lambda: signal_for(doc.frontmatter))
        if new is None:
            res.skipped += 1
            continue

        rewritten = rewrite_status(text, new)
        if rewritten is None:
            res.skipped += 1
            res.warnings.append(f"skip (no frontmatter): {path.name}")
            continue

        # A promotion to `done` must carry a first-write done_at, else later
        # sweeps/projections see done == done and never backfill it.
        if new == "done":
            ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
            rewritten = ensure_done_at(rewritten, ts)

        res.changes.append((str(path), _norm(raw) or "(none)", new))
        if new == "archived":
            res.archived += 1
        else:
            res.normalized += 1
        if apply:
            # Atomic (tmp + os.replace): an interrupted sweep never leaves a
            # half-written plan, so a re-run stays idempotent and recoverable.
            _atomic_write(path, rewritten)

    return res

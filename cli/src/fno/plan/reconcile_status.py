"""fno plan reconcile-status - normalize drifted plan frontmatter status in place.

Plans stay FLAT in the plans dir; an Obsidian Base filters by frontmatter
``status``. Drifted or blank statuses lie to that Base, so this one-shot-then-
idempotent sweep rewrites them to the canonical vocabulary (x-ff83 W2):

    axis:      design ready in_progress reviewing shipping shipped
    terminals: done archived   (off-axis, written directly)

Two tiers. Tier 1 is a pure synonym rewrite (no history needed). Tier 2 (blank /
``implemented`` / any unknown token) needs a true-state signal: a linked node
that is closed -> ``done``, else ``archived`` (an honest "off the board", never
a false ``done``). Dry-run by default; ``--apply`` writes.

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
from pathlib import Path
from typing import Callable, Optional

from fno.plan._doc import load_plan
from fno.plan._status import KNOWN_STATUSES

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
    "superseded": "archived",
    "superseded-by-implementation": "archived",
}

# Frontmatter block: leading ---\n ... \n--- . Non-greedy so the FIRST block
# wins even if the body contains a --- rule.
_FRONT_RE = re.compile(r"\A(---\n)(?P<fm>.*?)(\n---)(?P<rest>.*)\Z", re.DOTALL)
_STATUS_LINE_RE = re.compile(r"(?m)^(?P<indent>[ \t]*)status[ \t]*:.*$")


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


def _default_signal(frontmatter: dict) -> bool:
    """True when the plan's linked node reads as closed (`_status == done`).

    ponytail: node-closed is the one cheap true-state signal; a merged-PR probe
    would add gh calls. Upgrade to a PR check if blank-status plans with an open
    node but a merged PR start mis-archiving.
    """
    node_id = frontmatter.get("node")
    if not node_id:
        return False
    try:
        from fno.graph.store import read_graph
        from fno.paths import graph_json

        for e in read_graph(graph_json()):
            if e.get("id") == node_id:
                return e.get("_status") == "done"
    except Exception:  # noqa: BLE001 - no graph => no signal => archived (honest)
        return False
    return False


def sweep(
    plans_dir: Path,
    *,
    apply: bool = False,
    signal_for: Callable[[dict], bool] = _default_signal,
) -> SweepResult:
    """Scan every ``*.md`` in *plans_dir*, classify + (if apply) rewrite drift."""
    res = SweepResult()
    if not plans_dir.is_dir():
        res.warnings.append(f"plans dir not found: {plans_dir}")
        return res

    for path in sorted(plans_dir.glob("*.md")):
        try:
            text = path.read_text(encoding="utf-8")
            doc = load_plan(path)
        except Exception as exc:  # noqa: BLE001 - malformed => skip, body untouched
            res.skipped += 1
            res.warnings.append(f"skip (unparseable): {path.name}: {exc}")
            continue

        raw = doc.frontmatter.get("status")
        new = target_status(raw, lambda: signal_for(doc.frontmatter))
        if new is None:
            res.skipped += 1
            continue

        rewritten = rewrite_status(text, new)
        if rewritten is None:
            res.skipped += 1
            res.warnings.append(f"skip (no frontmatter): {path.name}")
            continue

        res.changes.append((str(path), _norm(raw) or "(none)", new))
        if new == "archived":
            res.archived += 1
        else:
            res.normalized += 1
        if apply:
            path.write_text(rewritten, encoding="utf-8")

    return res

"""PR-independent sweep of the carve-out ledger.

The PR-scoped harvest (:mod:`fno.retro.routine`) only ever sees carve-outs a
trigger points it at: a retro-pending sentinel, a ``.triage-pending`` fast-path,
or an explicit ``--pr-number``. Each of those scopes the read to the PR's owning
session(s), and an unresolvable owner degrades to read-only. So a carve-out
written with no session (``session_id: null``), or one whose PR merged without
ever dropping a sentinel, is never harvested by any path - it accumulates in
``.fno/carveouts.jsonl`` while ``fno retro run`` truthfully reports "no
retro-pending sentinels to triage". That gap is what this module closes: one
sweep over the whole ledger, keyed off nothing but the ledger itself.

Three dispositions, and the split between them is the whole feature. Filing one
node per carve-out is WORSE than not harvesting: a ledger whose items are
already tracked turns into a pile of duplicates that no verb can delete.

``resolve``
    An EXACT match proves the carve-out is already tracked: its cv-id appears
    verbatim in a node (every node this sweep files cites its cv-id, so a
    re-run is a no-op), or a retro-triage trailer carries the same content hash
    from an earlier PR-scoped harvest. Consume the ledger row, file nothing.

``review``
    A FUZZY subject match. Never auto-consumed and never filed: a wrong fuzzy
    match would either lose the work (consume) or duplicate it (file), so the
    only safe move is to name the suspected node and let a human decide. A
    ``deferred`` item parked here keeps blocking its node's close, which is the
    correct outcome - the gate exists to force exactly that look.

``file``
    No match at all. Mint a node (queued in interactive mode, so a human still
    acks it via ``fno backlog pick``), then consume the row.

Nothing is consumed without a node id attached to it. Clearing the ledger to
turn a gate green, with the work tracked nowhere, is the failure the close gate
exists to prevent.

Applying is MANUAL on purpose. There is no ``fno backlog delete``, so every
node filed here is permanent, and an irreversible operation must not run
unattended - which rules out the background SessionStart hook that fires
``fno retro run``. The cost is real and is not softened anywhere: the close
gate keeps refusing until a human runs ``--apply``.

``deferred`` and ``oos-bug`` are both swept, and the sweep does NOT flatten
them: ``deferred`` is declared scope that did not ship (it blocks a close via
condition D in :mod:`fno.graph._reconcile`), ``oos-bug`` is discovery and never
blocks. ``backfill`` is skipped entirely - it belongs to ``/fno:pr merged``'s
backfill slot, the same carve-out the PR-scoped harvest steps around.
"""
from __future__ import annotations

import re
from dataclasses import dataclass, field
from difflib import SequenceMatcher
from pathlib import Path
from typing import Callable, Optional

from fno.carveout.core import BACKFILL_KIND
from fno.retro.classify import classify_item, derive_title
from fno.retro.dedup import assign_hashes, content_hash
from fno.retro.land import MODE_INTERACTIVE, land_candidates
from fno.retro.types import KIND_CARVEOUT, Candidate, RawItem

DISPOSITION_FILE = "file"
DISPOSITION_RESOLVE = "resolve"
DISPOSITION_REVIEW = "review"

# Fuzzy-match floor. Deliberately high: a false positive here parks real work
# for a human instead of filing it, and a false negative only costs one triage
# row. Short strings are excluded outright (below) because SequenceMatcher is
# noisy on them - "unrelated" scores high against half the graph.
TITLE_MATCH_THRESHOLD = 0.85
TITLE_MATCH_MIN_LEN = 20

_PUNCT_RE = re.compile(r"[^a-z0-9 ]+")
_WS_RE = re.compile(r"\s+")


@dataclass
class SweepItem:
    """One carve-out and what the sweep decided about it."""

    carveout: dict
    disposition: str
    candidate: Optional[Candidate] = None
    # For resolve/review: the node already tracking this work. For file: the
    # node minted by an --apply run (None on a dry run or a failed mint).
    node_id: Optional[str] = None
    match_reason: Optional[str] = None
    error: Optional[str] = None

    @property
    def carveout_id(self) -> str:
        return str(self.carveout.get("id") or "?")

    @property
    def kind(self) -> str:
        return str(self.carveout.get("kind") or "?")


@dataclass
class SweepReport:
    items: list[SweepItem] = field(default_factory=list)
    applied: bool = False
    consumed: int = 0
    warnings: list[str] = field(default_factory=list)
    # The ledger exists but could not be read. NOT the same as an empty ledger:
    # read_carveouts raises rather than returning [] precisely so a failed read
    # is never a silent success, and the report must carry that distinction to
    # the exit code or the CLI re-introduces the masquerade.
    read_failed: bool = False

    def by_disposition(self, disposition: str) -> list[SweepItem]:
        return [i for i in self.items if i.disposition == disposition]

    @property
    def failed(self) -> bool:
        return self.read_failed or any(i.error for i in self.items)


def _normalize_subject(text: str) -> str:
    """Lowercase, strip punctuation, collapse whitespace - for fuzzy compare."""
    return _WS_RE.sub(" ", _PUNCT_RE.sub(" ", (text or "").lower())).strip()


def _node_text(node: dict) -> str:
    return f"{node.get('title') or ''}\n{node.get('details') or ''}"


def find_tracking_node(
    rec: dict, nodes: list[dict]
) -> "tuple[Optional[str], Optional[str], bool]":
    """Find a node that already tracks this carve-out.

    Returns ``(node_id, reason, exact)``. ``exact`` is True only for a
    provable match (the cv-id quoted verbatim, or an identical retro-triage
    content hash); a fuzzy subject match returns ``exact=False`` so the caller
    parks it for review rather than consuming the ledger row on a guess.

    Done nodes count: a carve-out whose work already shipped is tracked, and
    re-filing it would be the duplicate this sweep exists to avoid.
    """
    cv_id = str(rec.get("id") or "")
    if cv_id:
        for node in nodes:
            if cv_id in _node_text(node):
                return str(node.get("id") or ""), f"cv-id cited in {node.get('id')}", True

    # An earlier PR-scoped harvest writes `finding_hash=<h>` into the node's
    # details. Match on the hash alone, ignoring the trailer's source_pr: this
    # sweep has no PR, and the same carve-out filed under PR #N is still filed.
    digest = content_hash(str(rec.get("description") or ""))
    if digest:
        needle = f"finding_hash={digest}"
        for node in nodes:
            if needle in str(node.get("details") or ""):
                return (
                    str(node.get("id") or ""),
                    f"retro-triage content hash matches {node.get('id')}",
                    True,
                )

    # Compare on the SAME string the node would be titled with. Matching on
    # `need` instead left the sweep blind to every node it had filed itself
    # whenever need was set, since the filed title comes from the description.
    subject = _normalize_subject(derive_title(_raw_item(rec)))
    if len(subject) >= TITLE_MATCH_MIN_LEN:
        best_id, best_ratio = None, 0.0
        for node in nodes:
            title = _normalize_subject(str(node.get("title") or ""))
            if len(title) < TITLE_MATCH_MIN_LEN:
                continue
            ratio = SequenceMatcher(None, subject, title).ratio()
            if ratio > best_ratio:
                best_id, best_ratio = str(node.get("id") or ""), ratio
        if best_id and best_ratio >= TITLE_MATCH_THRESHOLD:
            return best_id, f"subject {best_ratio:.2f} similar to {best_id}", False

    return None, None, False


def _raw_item(rec: dict) -> RawItem:
    """One ledger row as a harvest item. ``source_pr`` is None by construction -
    the sweep is PR-independent, and a carve-out's cite is its own cv-id."""
    return RawItem(
        kind=KIND_CARVEOUT,
        text=str(rec.get("description") or ""),
        source_pr=None,
        source_id=str(rec.get("id") or ""),
        priority=rec.get("priority"),
        title_hint=rec.get("need"),
        subkind=rec.get("kind"),
    )


def plan_sweep(carveouts: list[dict], nodes: list[dict]) -> list[SweepItem]:
    """Decide a disposition for every carve-out. Pure: reads nothing, writes
    nothing, so the dry run and the applied run cannot diverge.

    Within-batch duplicates are caught here rather than at land time, for that
    same reason: the dry run has to show the collapse it is going to perform.
    """
    items: list[SweepItem] = []
    seen_hashes: dict[str, str] = {}  # content hash -> the cv-id that claimed it
    for rec in carveouts:
        if rec.get("kind") == BACKFILL_KIND:
            continue
        cv_id = str(rec.get("id") or "")
        if not cv_id:
            # read_carveouts does not require an id, and a row without one has
            # no cite, so it can never be filed. Say that once rather than
            # failing the whole sweep on every future run.
            items.append(
                SweepItem(
                    rec,
                    DISPOSITION_REVIEW,
                    match_reason="row carries no id; cannot be cited or consumed",
                )
            )
            continue
        node_id, reason, exact = find_tracking_node(rec, nodes)
        if node_id and exact:
            items.append(
                SweepItem(rec, DISPOSITION_RESOLVE, node_id=node_id, match_reason=reason)
            )
            continue
        if node_id:
            items.append(
                SweepItem(rec, DISPOSITION_REVIEW, node_id=node_id, match_reason=reason)
            )
            continue
        candidate = classify_item(_raw_item(rec))
        # assign_hashes fills content_hash, which land writes into the node's
        # dedup trailer. Skipping it left every filed node with an empty
        # `finding_hash=`, matching no trailer pattern, so neither this sweep
        # nor the PR-scoped harvest could ever dedup against what it filed.
        (candidate,) = assign_hashes([candidate])
        twin = seen_hashes.get(candidate.content_hash)
        if twin:
            # The same text twice in one batch (one blocker carved out from two
            # sessions). Filing both mints two permanent nodes for one piece of
            # work. Park rather than resolve: the twin's node does not exist
            # yet, and nothing is consumed without a node.
            items.append(
                SweepItem(
                    rec,
                    DISPOSITION_REVIEW,
                    candidate=candidate,
                    match_reason=f"same text as {twin} earlier in this sweep",
                )
            )
            continue
        seen_hashes[candidate.content_hash] = cv_id
        items.append(SweepItem(rec, DISPOSITION_FILE, candidate=candidate))
    return items


def sweep_carveouts(
    *,
    repo_root: Path,
    carveout_root: Path,
    nodes: list[dict],
    apply: bool = False,
    mode: str = MODE_INTERACTIVE,
    project: Optional[str] = None,
    cwd: Optional[str] = None,
    kind: Optional[str] = None,
    read_fn: Optional[Callable] = None,
    create_fn: Optional[Callable] = None,
    inbox_fn: Optional[Callable] = None,
    consume_fn: Optional[Callable] = None,
) -> SweepReport:
    """Plan (and optionally apply) a sweep of the whole carve-out ledger.

    With ``apply=False`` nothing is written: the report says exactly what a
    later ``--apply`` would file and what it would consume. ``kind`` narrows
    the sweep to one carve-out kind without changing any disposition.
    """
    from fno.carveout.core import consume_carveouts, read_carveouts

    report = SweepReport(applied=apply)
    reader = read_fn or read_carveouts
    try:
        rows = reader(carveout_root, kind=kind)
    except Exception as exc:  # a present-but-unreadable ledger is not "empty"
        report.warnings.append(f"cannot read carve-out ledger: {exc}")
        report.read_failed = True
        return report

    report.items = plan_sweep(rows, nodes)
    if not apply:
        return report

    consume = consume_fn or consume_carveouts
    to_consume: list[str] = []
    for item in report.items:
        if item.disposition == DISPOSITION_REVIEW:
            continue
        if item.disposition == DISPOSITION_RESOLVE:
            to_consume.append(item.carveout_id)
            continue
        # file: mint the node FIRST; the ledger row is consumed only once the
        # work has somewhere to live.
        results = land_candidates(
            [item.candidate],
            mode=mode,
            repo_root=repo_root,
            project=project,
            cwd=cwd,
            create_fn=create_fn,
            inbox_fn=inbox_fn,
        )
        if not results:
            item.error = "candidate did not land (no result)"
            continue
        result = results[0]
        if result.outcome == "failed":
            item.error = result.error or "land failed"
            continue
        item.node_id = result.node_id
        # An inbox line carries no node id but IS a durable record of the work,
        # so the row is still consumed.
        to_consume.append(item.carveout_id)

    if to_consume:
        removed = consume(carveout_root, to_consume)
        report.consumed = removed
        if removed < len(to_consume):
            report.warnings.append(
                f"consumed only {removed}/{len(to_consume)} carve-out row(s); "
                "the remainder stay in the ledger and re-sweep next run"
            )
    return report


def render_sweep(report: SweepReport) -> list[str]:
    """Human-readable sweep report, one line per carve-out plus a summary."""
    lines: list[str] = []
    verb = "applied" if report.applied else "dry run"
    counts = {
        d: len(report.by_disposition(d))
        for d in (DISPOSITION_FILE, DISPOSITION_RESOLVE, DISPOSITION_REVIEW)
    }
    lines.append(
        f"carve-out sweep ({verb}): {len(report.items)} unharvested "
        f"-> {counts[DISPOSITION_FILE]} file, "
        f"{counts[DISPOSITION_RESOLVE]} already tracked, "
        f"{counts[DISPOSITION_REVIEW]} need a human look"
    )
    for disposition, label in (
        (DISPOSITION_FILE, "FILE"),
        (DISPOSITION_RESOLVE, "TRACKED"),
        (DISPOSITION_REVIEW, "REVIEW"),
    ):
        rows = report.by_disposition(disposition)
        if not rows:
            continue
        lines.append("")
        lines.append(f"{label} ({len(rows)}):")
        for item in rows:
            head = f"  {item.carveout_id} [{item.kind}]"
            if disposition == DISPOSITION_FILE:
                title = item.candidate.title if item.candidate else "(unclassified)"
                priority = item.candidate.priority if item.candidate else "?"
                tier = item.candidate.tier if item.candidate else "?"
                landed = f" -> {item.node_id}" if item.node_id else ""
                lines.append(f"{head} {priority} {tier}: {title}{landed}")
            else:
                lines.append(f"{head} {item.match_reason}")
            if item.error:
                lines.append(f"    FAILED: {item.error}")
    if report.applied:
        lines.append("")
        lines.append(f"consumed {report.consumed} ledger row(s)")
    return lines

"""Read the ten queues that decide whether a king still has work.

Three properties are load-bearing and each has a test that fails loudly when it
breaks.

**Every row carries the shell command that produced it.** Board emptiness has to
be reproducible by a third party who runs those commands by hand. A king that
asserted it was finished would be the receipts-can-lie shape the pitfalls corpus
already records; a king that hands you the commands is checkable.

**A queue only a human can shrink is reported and never counted.** Counting one
would hold the loop open until a person answered, which is idle-forever with a
report attached. ``operator_question``, ``carveout_pending``,
``capture_pending``, and ``unreachable_worker`` therefore report their real
counts with ``actionable: false`` and a note naming why.

**Staffed is an activity reading, never a status word.** The roster's status
vocabulary collapses four real states into three words: a worker consuming
tokens, a worker parked at its prompt, and a worker whose model refused all
render the same. A board that read that word would drop every stalled node out
of its queue and terminate NoWork while claims are held and nothing moves, which
is worse than no loop at all. So the discriminator here is membership in
:data:`ACTIVE_STATES` plus a fresh activity age. The set is IMPORTED, not
copied, so the vocabulary fix that adds the missing fourth word reaches this
board without an edit.

One more asymmetry worth stating. An unreadable queue is not an empty one. No
rows has two explanations, a clean board and a broken reader, and only a
positive read can tell them apart. An unreadable queue is therefore counted as
work in its own right (a blind king may not exit) and the process exits
non-zero.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass, field
from typing import Any, Optional

from fno import paths
from fno.agents.reachability import _ACTIVE_STATES as ACTIVE_STATES
from fno.agents.session_truth import STALLED_AFTER_S
from fno.king.lane import LaneItem, LaneRead, open_items, parked_items, read_lane
from fno.pr import _reviews
from fno.pr._status import _classify, _latest_per_name, without_coverage_statuses

#: Priorities a king treats as its own work. Lower bands are the operator's to
#: rank up; a king that dispatched p2 would spend the fleet on the wrong thing.
KING_PRIORITIES = frozenset({"p0", "p1"})

#: Per-queue row cap. The count stays honest; only the rendered rows are cut,
#: and the cut is reported. A silent cap reads as full coverage.
DEFAULT_MAX_ROWS = 25  # render bound only; see _queue

#: Claim states that mean the lock outlived its holder. `fno agents claim list
#: --include-stale` returns both, and a corrupted lockfile is as unreapable by
#: its owner as an expired one, so both belong in the queue the king clears
#: with `fno agents claim reap`.
_DEAD_CLAIM_STATES = frozenset({"stale", "corrupted"})

#: The literal commands a reader can re-run. These strings ARE the checkability
#: property, so they live beside the readers that run them.
SRC_UNDISPATCHED = "fno backlog undispatched --json"
SRC_READY = "fno backlog ready --json -A"
SRC_CLAIMS = "fno agents claim list -J --include-stale --prefix node:"
SRC_PRS = (
    "gh pr list --state open --json number,title,mergeable,statusCheckRollup,headRefName,url"
)
SRC_QUESTIONS = "fno inbox outstanding --json"
SRC_NEEDS = "fno agents needs --json"


@dataclass(frozen=True)
class SourceRead:
    """One underlying verb's answer, or the reason there is no answer.

    ``error`` set is terminal for every queue fed by this source. A reader that
    degraded to an empty payload here would launder a broken verb into a clean
    board, which is the one failure this whole module is arranged against.
    """

    payload: Any = None
    error: str = ""

    @property
    def ok(self) -> bool:
        return not self.error

    def rows(self) -> list[dict]:
        return list(self.payload or [])


@dataclass(frozen=True)
class BoardInputs:
    """Everything the board reads, already fetched.

    Split from the fetching so the queue logic is testable without a live
    machine, and so one source feeding three queues is fetched once.

    ``holder_activity`` maps a claim holder to its activity reading
    (``{"state": ..., "age_s": ...}``). Resolved only for the holders of nodes
    the king cares about, which is a handful, rather than for the whole roster.
    """

    ready: SourceRead
    claims: SourceRead
    #: The backlog rows for nodes holding a LIVE claim. A separate source from
    #: `ready` on purpose: `ready` excludes exactly these, so a wedged worker is
    #: invisible from that side.
    claimed_nodes: SourceRead
    holder_activity: dict[str, dict]
    prs: SourceRead
    #: The WHOLE ``fno inbox outstanding --json`` payload. One read feeds three
    #: queues (questions, carveouts, captures); keeping a quarter of the
    #: payload here is how 661 of 665 awaiting-a-human items went invisible.
    outstanding: SourceRead
    needs: SourceRead
    #: The operator's own ranked lane. A 66-byte file read done in process at
    #: fetch time, never through `_run_json` - there is no verb behind it.
    lane: SourceRead
    warnings: list[str] = field(default_factory=list)
    #: Independent planned-unclaimed inventory. Production collection always
    #: supplies it; None keeps pure test fixtures source-compatible.
    undispatched: SourceRead | None = None


def _as_dict(value: Any) -> dict:
    """The nested-shape half of the degrade-not-crash promise.

    The top-level payload is already type-checked, but the board shells out
    through a PATH-resolved ``fno``, so a stale deployed CLI can answer with an
    older stream shape (a list where a dict belongs, string counts). A
    structural surprise in ONE stream must degrade that stream, never raise
    out of ``build_board`` - an exception here kills all ten queues instead
    of the designed one-unreadable-queue exit.
    """
    return value if isinstance(value, dict) else {}


def _as_int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _holder_is_active(activity: Optional[dict]) -> bool:
    """True only on positive evidence that the holder is doing something.

    An absent reading is not a staffed lane. A holder we cannot resolve gets the
    same answer as one we resolved and found quiet, because in both cases there
    is no evidence anyone is working the node, and the king's action (one wake)
    is harmless either way.
    """
    if not activity:
        return False
    if activity.get("state") not in ACTIVE_STATES:
        return False
    age = activity.get("age_s")
    if age is None:
        return False
    return age <= STALLED_AFTER_S


def compile_scope_ids(scope: str, entries: list[dict], *, resolve=None) -> set[str]:
    """Compile a canonical crown scope into the graph node ids it contains."""
    from fno.agents.crown import _canonical_project, resolve_crown, split_scope

    resolver = resolve or resolve_crown
    level, canonical = resolver(split_scope(scope))
    if level == 2:
        from fno.graph._intake import descendants_of

        by_id = {row.get("id"): row for row in entries if isinstance(row, dict)}
        root = by_id.get(canonical)
        if not root or root.get("type") != "epic":
            raise ValueError(f"crown scope {canonical!r} is not an epic in the graph")
        return {canonical, *descendants_of(entries, canonical)}

    projects = set(split_scope(canonical))
    return {
        str(row["id"])
        for row in entries
        if isinstance(row, dict)
        and row.get("id")
        and (_canonical_project(str(row.get("project") or "")) or row.get("project"))
        in projects
    }


def _queue(
    name: str,
    source: str,
    read: SourceRead,
    rows: list[dict],
    *,
    actionable: bool,
    note: str = "",
    count: "int | None" = None,
    verb: str = "",
) -> dict:
    if not read.ok:
        return {
            "name": name,
            "source": source,
            "status": "unreadable",
            "error": read.error,
            "count": None,
            "rows": [],
            "actionable": actionable,
            "note": note,
            "verb": verb,
        }
    # Every row, not the rendered slice. The consumer of this payload is the
    # loop, which derives each row's identity to tell progress from a stall. A
    # payload capped at the render limit made the loop blind past row 25: rows
    # cleared beyond the cut left no identity behind, so a king draining a long
    # queue read as making no progress and burned to NoProgress.
    #
    # The row cap now lives ONLY in `cli._render`, where a human is the
    # consumer and eliding is correct. A cap the data applied silently was
    # worse than a visible render bound: nothing downstream could see it had
    # happened, so the loop read a short list as the whole truth.
    return {
        "name": name,
        "source": source,
        "status": "ok",
        "error": "",
        # Summary queues (per-kind, per-project rows) pass the STREAM total:
        # a by-kind pair reporting 2 for a 14-row ledger is the misleading
        # count this board exists to prevent.
        "count": len(rows) if count is None else count,
        "rows": rows,
        "actionable": actionable,
        "note": note,
        "verb": verb,
    }


def build_board(
    inputs: BoardInputs,
    *,
    autonomous_merge: bool = False,
    crown_scope: Optional[str] = None,
    scope_ids: Optional[set[str]] = None,
) -> dict:
    """Turn fetched sources into the board payload. Pure; does no I/O."""
    out_of_scope: list[dict] = []
    undispatched_read = inputs.undispatched or inputs.ready

    def in_scope(queue: str, node_id: object, row: dict) -> bool:
        if scope_ids is None or not node_id:
            return True
        node_id = str(node_id)
        if node_id in scope_ids:
            return True
        out_of_scope.append(
            {
                "queue": queue,
                "id": node_id,
                **({"title": row.get("title")} if row.get("title") else {}),
            }
        )
        return False

    claim_by_node: dict[str, dict] = {}
    for row in inputs.claims.rows():
        key = str(row.get("key") or "")
        if key.startswith("node:"):
            claim_by_node[key[len("node:") :]] = row

    undispatched: list[dict] = []
    for node in undispatched_read.rows():
        if node.get("priority") not in KING_PRIORITIES:
            continue
        if not node.get("plan_path"):
            continue
        # `fno backlog ready` already drops every node holding a LIVE claim, so
        # anything still here is unstaffed. A STALE claim survives that filter,
        # and it belongs to the `stale_claim` queue: counting it here too would
        # let the king spawn a second worker over a lock nobody reaped. Reaping
        # first makes the node undispatched on the next read, which converges
        # without the duplicate.
        claim = claim_by_node.get(str(node.get("id")))
        if claim is not None and claim.get("state") in _DEAD_CLAIM_STATES:
            continue
        if not in_scope("undispatched", node.get("id"), node):
            continue
        undispatched.append(
            {
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
            }
        )

    # The complementary half of `undispatched`: its source requires a
    # finalized plan, so a planless node is absent by construction. `ready` is
    # the selection authority for cold-dispatchable ideas and already drops
    # deferred, blocked, live-claimed, batched, containerized and PR-bearing
    # rows. Its output has no status, so those guards stay upstream. A stale
    # claim still belongs to `stale_claim`, not this queue.
    unplanned: list[dict] = []
    for node in inputs.ready.rows():
        if node.get("priority") not in KING_PRIORITIES:
            continue
        if node.get("plan_path"):
            continue
        claim = claim_by_node.get(str(node.get("id")))
        if claim is not None and claim.get("state") in _DEAD_CLAIM_STATES:
            continue
        if not in_scope("unplanned", node.get("id"), node):
            continue
        unplanned.append(
            {
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
            }
        )

    # `stalled_holder` CANNOT be sourced from the ready list, and that mistake
    # is what made this queue structurally unreachable in the first cut.
    # `fno backlog ready` filters through `live_claimed_node_ids`, so a node
    # with a live holder is exactly the node that has already been removed.
    # Measured 2026-08-18: 12 live-or-suspect node claims on one machine, zero
    # of them in the ready payload. Reading a wedged worker therefore has to
    # start from the CLAIM and look the node up, which is the opposite
    # direction from every other queue here.
    stalled: list[dict] = []
    from fno.graph.statuses import TERMINAL_RUNGS

    for node in inputs.claimed_nodes.rows():
        if node.get("priority") not in KING_PRIORITIES:
            continue
        # A terminal node's claim is a leak the closure release or the
        # node-aware reaper owns, not a wedged worker. Reporting it here sent
        # the king at done work (x-94f8's stalled_holder queue).
        if node.get("status") in TERMINAL_RUNGS:
            continue
        claim = claim_by_node.get(str(node.get("id")))
        if claim is None or claim.get("state") in _DEAD_CLAIM_STATES:
            continue
        holder = str(claim.get("holder") or "")
        if _holder_is_active(inputs.holder_activity.get(holder)):
            continue
        if not in_scope("stalled_holder", node.get("id"), node):
            continue
        stalled.append(
            {
                "id": node.get("id"),
                "priority": node.get("priority"),
                "title": node.get("title"),
                "holder": holder,
                "claim_state": claim.get("state"),
            }
        )

    # A corrupted lockfile is as much a lock nobody will reap as a stale one,
    # and it carries no holder, so leaving it out points the king at a wake it
    # cannot perform instead of the reap it can.
    stale_claims: list[dict] = []
    for row in inputs.claims.rows():
        if row.get("state") not in _DEAD_CLAIM_STATES:
            continue
        key = str(row.get("key") or "")
        node_id = key[len("node:") :] if key.startswith("node:") else ""
        if not in_scope("stale_claim", node_id, row):
            continue
        stale_claims.append(
            {"key": row.get("key"), "holder": row.get("holder"), "state": row.get("state")}
        )

    lane_items = [LaneItem(**r) for r in inputs.lane.rows()] if inputs.lane.ok else []
    lane_open = open_items(LaneRead(items=lane_items)) if inputs.lane.ok else []
    lane_parked = parked_items(LaneRead(items=lane_items)) if inputs.lane.ok else []
    lane_rows = [{"text": i.text, "line": i.line} for i in lane_open]
    lane_note = (
        "the operator's own ranking. File each with `fno backlog idea \"<text>\"` "
        "and stamp `-> <id>` onto its line, or park it with `-> parked: <reason>`."
    )
    if lane_parked:
        lane_note += f" {len(lane_parked)} parked, reasons are in the file."

    pr_rows = [
        {"number": r.get("number"), "title": r.get("title")} for r in inputs.prs.rows()
    ]

    # One outstanding read, three streams. A non-dict payload (a verb that
    # changed shape) degrades to empty streams rather than a crash, mirroring
    # every other reader here.
    outstanding_payload = (
        inputs.outstanding.payload if inputs.outstanding.ok else {}
    )
    outstanding_payload = (
        outstanding_payload if isinstance(outstanding_payload, dict) else {}
    )
    question_rows = [
        {"id": r.get("id"), "question": r.get("question"), "ts": r.get("ts")}
        for r in (outstanding_payload.get("questions") or [])
    ]

    carveout_stream = outstanding_payload.get("carveouts")
    carveout_stream = carveout_stream if isinstance(carveout_stream, dict) else {}
    carveout_rows = [
        {"kind": kind, "n": _as_int(n)}
        for kind, n in sorted(_as_dict(carveout_stream.get("by_kind")).items())
    ]
    roots = outstanding_payload.get("roots")
    carveout_root = _as_dict(_as_dict(roots).get("carveouts")).get("root")

    capture_stream = outstanding_payload.get("captures")
    capture_stream = capture_stream if isinstance(capture_stream, dict) else {}
    capture_by_project = _as_dict(capture_stream.get("by_project"))
    # Per-project COUNTS, never the rows themselves: a hundreds-row stream
    # cannot be listed, and the cut is stated as a row so the loop and the
    # human render both see it.
    capture_rows = [
        {"project": project, "n": _as_int(n)}
        for project, n in sorted(
            capture_by_project.items(), key=lambda kv: (-_as_int(kv[1]), kv[0])
        )[:_CAPTURE_PROJECT_CAP]
    ]
    elided = len(capture_by_project) - len(capture_rows)
    if elided > 0:
        capture_rows.append({"elided_projects": elided})

    # `fno agents needs` emits operator questions in the same list. They are the
    # queue above, read from its own verb; counting them here would report one
    # human queue twice.
    needs_rows: list[dict] = []
    for row in inputs.needs.rows():
        if row.get("kind") == "operator_question":
            continue
        if not in_scope("unreachable_worker", row.get("node"), row):
            continue
        needs_rows.append(
            {"kind": row.get("kind"), "name": row.get("name"), "node": row.get("node")}
        )

    queues = [
        _queue(
            "operator_lane",
            f"cat {paths.operator_lane()}",
            inputs.lane,
            lane_rows,
            actionable=scope_ids is None,
            note=lane_note
            + (
                ""
                if scope_ids is None
                else " report-only under a crown: lane lines are the operator's "
                "global priorities and carry no node id, so a scoped king cannot "
                "attribute them to its subtree"
            ),
        ),
        _queue(
            "undispatched",
            f"{SRC_UNDISPATCHED} + {SRC_CLAIMS}",
            SourceRead(error=undispatched_read.error or inputs.claims.error),
            undispatched,
            actionable=True,
            note="one worker per node; these already carry a plan",
            verb="/fno:target",
        ),
        _queue(
            "unplanned",
            f"{SRC_READY} + {SRC_CLAIMS}",
            SourceRead(error=inputs.ready.error or inputs.claims.error),
            unplanned,
            actionable=True,
            note="batch: up to 3 blueprints per session; merge same-shape nodes into one waved plan",
            verb="/fno:blueprint",
        ),
        _queue(
            "stalled_holder",
            f"{SRC_CLAIMS} + fno backlog get <id> + fno agents peek <holder>",
            SourceRead(error=inputs.claims.error or inputs.claimed_nodes.error),
            stalled,
            actionable=True,
        ),
        _queue(
            "mergeable_pr",
            SRC_PRS,
            inputs.prs,
            pr_rows,
            actionable=autonomous_merge,
            note=(
                ""
                if autonomous_merge
                else "report-only: merging is outward and hard to reverse, so it "
                "waits on config.king.autonomous_merge"
            ),
        ),
        _queue(
            "stale_claim",
            SRC_CLAIMS,
            inputs.claims,
            stale_claims,
            actionable=True,
        ),
        _queue(
            "operator_question",
            SRC_QUESTIONS,
            inputs.outstanding,
            question_rows,
            actionable=False,
            note="report-only: a human answers these, so counting them would "
            "hold the loop open forever",
        ),
        _queue(
            "carveout_pending",
            SRC_QUESTIONS,
            inputs.outstanding,
            carveout_rows,
            actionable=False,
            note=(
                "report-only: the sweep is a human verb"
                + (f"; root {carveout_root}" if carveout_root else "")
            ),
            count=_as_int(carveout_stream.get("total")),
        ),
        _queue(
            "capture_pending",
            SRC_QUESTIONS,
            inputs.outstanding,
            capture_rows,
            actionable=False,
            note="report-only: per-project counts only; the rows cannot be listed",
            count=_as_int(capture_stream.get("total")),
        ),
        _queue(
            "unreachable_worker",
            SRC_NEEDS,
            inputs.needs,
            needs_rows,
            actionable=False,
            note="report-only: the refusal event a king would act on does not "
            "exist yet",
        ),
    ]
    if scope_ids is not None:
        queues.append(
            _queue(
                "out_of_scope",
                f"king manifest scope {crown_scope}",
                SourceRead(payload=out_of_scope),
                out_of_scope,
                actionable=False,
                note=f"report-only: outside crown scope {crown_scope}",
            )
        )

    actionable = 0
    unreadable = 0
    for q in queues:
        if q["status"] == "unreadable":
            unreadable += 1
            # A blind ACTIONABLE queue is work: the king may not exit while it
            # cannot see a queue it could have shrunk. A blind report-only queue
            # is loud (the exit code below) and still uncounted, because the
            # rule that a human-gated queue never gates NoWork does not get
            # weaker when the verb behind it breaks. `fno inbox outstanding`
            # timing out is a measured failure on this machine, and counting it would
            # wedge the loop on exactly the queue that must never wedge it.
            if q["actionable"]:
                actionable += 1
        elif q["actionable"]:
            actionable += q["count"]

    return {
        "actionable": actionable,
        "unreadable": unreadable,
        "queues": queues,
        "warnings": list(inputs.warnings),
        "exit_code": 1 if unreadable else 0,
    }


# ---------------------------------------------------------------------------
# Fetching
# ---------------------------------------------------------------------------


def _run_json(cmd: list[str], *, timeout: float) -> SourceRead:
    """Run a verb and parse its JSON. Every failure arrives as an error string.

    A non-zero exit, a timeout, a missing binary and unparseable output are all
    the same thing to the board: this queue could not be read. The timeout is
    REQUIRED and always the caller's derived slice of the whole-board budget -
    there is no default, because an independent per-source default is the
    inversion this module exists to prevent.
    """
    try:
        proc = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except FileNotFoundError:
        return SourceRead(error=f"{cmd[0]}: not found")
    except subprocess.TimeoutExpired:
        return SourceRead(error=f"{' '.join(cmd)}: timed out after {timeout:.1f}s")
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()[:500]
        return SourceRead(error=f"exit {proc.returncode}: {detail}")
    try:
        return SourceRead(payload=json.loads(proc.stdout or "null"))
    except json.JSONDecodeError as exc:
        return SourceRead(error=f"unparseable output: {exc}")


def _read_prs(timeout: float, max_pr_reads: int) -> tuple[SourceRead, list[str]]:
    """Open PRs that are green, mergeable and unmerged.

    Costs exactly ONE call: the rollup arrives inside the listing, so there is
    no per-PR status read to cap. The bound therefore sits on the CALL, via
    ``--limit``, which is the only place it buys anything.

    Two silent truncations lived here. ``gh pr list`` fetches 30 by default, so
    an unbounded listing hid every PR past the thirtieth behind no message at
    all. The cap then sliced rows BEFORE filtering, discarding PRs already
    fetched and paid for. Either one drops an eligible PR from the count, and a
    board that undercounts reaches ``NoWork`` while real work is open - the
    same silent-truncation class as the row cap this module already fixed.
    """
    listing = _run_json(
        [
            "gh",
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            str(max_pr_reads),
            "--json",
            "number,title,mergeable,statusCheckRollup,headRefName,url",
        ],
        timeout=timeout,
    )
    if not listing.ok:
        return listing, []
    rows = listing.rows()
    warnings: list[str] = []
    # A listing that comes back exactly AT its limit is indistinguishable from
    # one that had more waiting, so it is reported rather than assumed whole.
    # Assert the bound was reached; never infer completeness from its absence.
    if len(rows) >= max_pr_reads:
        warnings.append(
            f"mergeable_pr: the open-PR listing hit its {max_pr_reads}-PR limit, "
            f"so more open PRs can exist; raise max_pr_reads to read further"
        )
    try:
        from fno.graph._reconcile import classify_open_pr_bindings
        from fno.graph.store import read_graph_strict

        bindings = classify_open_pr_bindings(rows, read_graph_strict(paths.graph_json()))
    except Exception as exc:  # noqa: BLE001 - mergeability remains readable
        warnings.append(f"pr_node_binding_unreadable: {exc}")
    else:
        for binding in bindings:
            if binding.verdict == "missing":
                warnings.append(
                    f"pr_node_binding_missing: #{binding.pr_number} "
                    f"{binding.head_ref} -> {binding.node_id}"
                )
    # Every fetched row is judged. They cost the same one call whether read or
    # discarded, so dropping any of them buys nothing and loses real work.
    ready: list[dict] = []
    for pr in rows:
        if pr.get("mergeable") != "MERGEABLE":
            continue
        # Dedup to the latest run per check name/context BEFORE classifying -
        # same fix as `_status._latest_per_name`, and load-bearing for the same
        # reason: a force/amend push leaves a superseded FAILURE or CANCELLED
        # beside the fresh SUCCESS in the rollup, and reading every entry's
        # conclusion into one flat set poisons that set with the stale result.
        # A body gate that re-ran green then still read red here, twice, the
        # night this was measured.
        # Only the DIAGNOSTIC context is noise here. The required coverage
        # context is a verdict this board must honor: its FAILURE means
        # uncovered (the PR is not ready work no matter how green CI is), its
        # PENDING means not-yet-known. Dropping both would admit an uncovered
        # PR onto the ready list; keeping both would drop a covered one over a
        # pending instrument stamp.
        raw = _latest_per_name(pr.get("statusCheckRollup") or [])
        deduped = without_coverage_statuses(
            raw, contexts=frozenset({_reviews.COVERAGE_UNAVAILABLE_STATUS_CONTEXT})
        )
        if raw and not deduped:
            # Diagnostic-only rollup: CI has not reported. An empty class set
            # is not green - the pre-filter rows kept this PR out and so does
            # this.
            continue
        classes = {_classify(c) for c in deduped}
        if "fail" in classes:
            continue
        if "pending" in classes:
            continue
        ready.append({"number": pr.get("number"), "title": pr.get("title")})
    return SourceRead(payload=ready), warnings


#: Per-project rows rendered for the capture stream. The count stays whole;
#: the cut is stated as an elided_projects row so nothing reads as full
#: coverage.
_CAPTURE_PROJECT_CAP = 8


def _read_outstanding() -> SourceRead:
    """The whole four-stream payload; one in-process read feeds three queues.

    `fno.outstanding.core.collect` is exactly what `fno inbox outstanding`
    serializes, so the board calls it and carries the same dict shape.
    """
    from fno.outstanding.cli import _storage_root
    from fno.outstanding.core import OutstandingError, collect

    try:
        payload = collect(_storage_root(), lane=read_lane()).as_dict()
    except OutstandingError as exc:
        return SourceRead(error=f"fno inbox outstanding: {exc}")
    except Exception as exc:  # noqa: BLE001 - one unreadable queue, not a dead board
        return SourceRead(error=f"fno inbox outstanding: {exc}")
    return SourceRead(payload=payload if isinstance(payload, dict) else {})


def _read_lane() -> SourceRead:
    """A 66-byte file read done in process; there is no verb behind it."""
    lane = read_lane()
    if lane.error:
        return SourceRead(error=lane.error)
    rows = [
        {"text": i.text, "node": i.node, "parked": i.parked, "done": i.done, "line": i.line}
        for i in lane.items
    ]
    return SourceRead(payload=rows)


def _resolve_holder_activity(holders: set[str]) -> dict[str, dict]:
    """Read the activity axis for exactly the holders the king cares about.

    Deliberately not the whole roster: ``fno agents list`` pays a harness
    shellout and a transcript read per row and takes over a minute on a busy
    machine, which would distort the stop hook it feeds. A holder resolution is
    one transcript read, and there are only ever a handful of held king nodes.
    """
    from fno.agents.session_truth import resolve_session_truth

    out: dict[str, dict] = {}
    for holder in holders:
        # `target-session:<harness session id>` is the shape fno do target init
        # writes; the id after the colon is what resolves.
        token = holder.split(":", 1)[1] if ":" in holder else holder
        try:
            truth = resolve_session_truth(token)
        except Exception:  # noqa: BLE001 - an unresolved holder is a stalled one
            continue
        out[holder] = {
            "state": truth.get("state"),
            "age_s": truth.get("last_activity_age_s"),
        }
    return out


#: Live node claims resolved per board read. Each costs one `fno backlog get`,
#: and the read feeds a stop hook, so the count is capped and the cut is
#: reported rather than swallowed.
MAX_CLAIMED_NODE_READS = 20


def _read_claimed_nodes(claims: SourceRead) -> tuple[SourceRead, set[str], list[str]]:
    """Look up the backlog row behind each LIVE node claim.

    This is the read that makes `stalled_holder` reachable at all. Every other
    queue starts from a list of nodes; this one starts from a list of LOCKS,
    because `fno backlog ready` has already removed exactly the nodes a wedged
    worker is holding.

    In process, and this is where the fan-out died: the subprocess shape was
    one fresh `fno` interpreter PER live claim (measured: eight live claims
    made thirteen board spawns), each paying ~3.5s of startup to ask this
    running package a question it can answer with one graph read. The graph
    is read ONCE and each claim resolves against it - the same exact-only
    `resolve_node` tiers `fno backlog get` applies.
    """
    if not claims.ok:
        return SourceRead(error=claims.error), set(), []

    held: list[tuple[str, str]] = []
    for row in claims.rows():
        key = str(row.get("key") or "")
        if not key.startswith("node:"):
            continue
        if row.get("state") in _DEAD_CLAIM_STATES:
            continue
        holder = str(row.get("holder") or "")
        if holder:
            held.append((key[len("node:") :], holder))

    warnings: list[str] = []
    if len(held) > MAX_CLAIMED_NODE_READS:
        warnings.append(
            f"stalled_holder: capped at {MAX_CLAIMED_NODE_READS} of {len(held)} live claims"
        )
        held = held[:MAX_CLAIMED_NODE_READS]

    nodes: list[dict] = []
    holders: set[str] = set()
    from fno.graph.fuzzy import resolve_node
    from fno.graph.statuses import TERMINAL_RUNGS
    from fno.graph.store import read_graph_strict

    try:
        entries = read_graph_strict(paths.graph_json())
    except Exception as exc:  # noqa: BLE001 - an unreadable graph is an unreadable queue
        return SourceRead(error=f"backlog get: {exc}"), set(), warnings

    for node_id, holder in held:
        try:
            match = resolve_node(node_id, entries)
        except Exception as exc:  # noqa: BLE001 - one unreadable node is not an unreadable queue
            warnings.append(f"stalled_holder: {node_id} unreadable: {exc}")
            continue
        node = (
            match.candidates[0]
            if match.kind == "exact" and match.candidates
            else None
        )
        if not isinstance(node, dict):
            # One unreadable node is not an unreadable queue: the other claims
            # still answer. It is reported so a silent gap never reads as a
            # clean lane.
            warnings.append(f"stalled_holder: {node_id} unreadable: not found")
            continue
        # A terminal node's claim is a leak the closure release or the
        # node-aware reaper owns. Drop it at the source so it never reaches a
        # queue AND its holder never costs an activity transcript read.
        if node.get("status") in TERMINAL_RUNGS:
            continue
        nodes.append(node)
        if node.get("priority") in KING_PRIORITIES:
            holders.add(holder)
    return SourceRead(payload=nodes), holders, warnings


def collect_inputs(*, budget_ms: int, max_pr_reads: int = 20) -> BoardInputs:
    """Fetch every source inside ONE whole-board budget. Never raises; every
    failure lands in a SourceRead - including the budget itself running out,
    which lands as a SourceRead naming the source, never as a killed process.

    ``budget_ms`` is the TOTAL the caller enforces (the Rust stop gate passes
    its own bound in via ``--budget-ms``) or the hand-run constant. Every
    per-source slice is derived HERE from what remains, minus the
    serialization reserve: there is deliberately no second, independent
    per-source timeout to invert. As the budget runs out the board stops
    starting reads, marks each unstarted source, and emits the payload it
    has, so the caller parses an answer instead of timing out.
    """
    deadline = time.monotonic() + budget_ms / 1000.0
    state: dict[str, Optional[str]] = {"last": None}

    def slice_s() -> Optional[float]:
        """The seconds this next source may spend, or None once spent."""
        left = deadline - time.monotonic() - SERIALIZE_RESERVE_MS / 1000.0
        return left if left > 0 else None

    def spent_error() -> str:
        last = state["last"]
        if last is None:
            return "not-read: board budget exhausted before any source"
        return f"not-read: board budget exhausted after {last}"

    def start(name: str) -> Optional[float]:
        """Claim the budget for `name`; returns its slice or None once spent."""
        s = slice_s()
        if s is not None:
            state["last"] = name
        return s

    def safe(read: "Callable[[], SourceRead]") -> SourceRead:
        """A read that raises lands as a SourceRead, exactly as a non-zero
        exit does. This is the collector-side half of the contract; each
        in-process reader also catches its own, so one bad source can never
        take the board down from either layer."""
        try:
            return read()
        except Exception as exc:  # noqa: BLE001 - one unreadable queue, not a dead board
            return SourceRead(error=f"{type(exc).__name__}: {exc}")

    # Fixed reads. `undispatched`, `claims`, `outstanding` and the per-claim
    # `backlog get` run IN PROCESS (they are functions the verbs already wrap;
    # a fresh interpreter of this same package per read cost ~3.5s of startup
    # against ~0.25s of query). `ready` and `needs` stay subprocesses and say
    # why at their call sites.
    s = start("backlog undispatched")
    undispatched = (
        safe(_read_undispatched) if s is not None else SourceRead(error=spent_error())
    )

    s = start("agents claim list")
    claims = safe(_read_claims) if s is not None else SourceRead(error=spent_error())

    s = start(SRC_PRS)
    if s is None:
        prs, warnings = SourceRead(error=spent_error()), []
    else:
        try:
            prs, warnings = _read_prs(s, max_pr_reads)
        except Exception as exc:  # noqa: BLE001 - one unreadable queue, not a dead board
            prs, warnings = SourceRead(error=f"{type(exc).__name__}: {exc}"), []

    s = start("stalled_holder lookups")
    if s is None:
        claimed_nodes, holders, claimed_warnings = SourceRead(error=spent_error()), set(), []
    else:
        try:
            claimed_nodes, holders, claimed_warnings = _read_claimed_nodes(claims)
        except Exception as exc:  # noqa: BLE001 - one unreadable queue, not a dead board
            claimed_nodes = SourceRead(error=f"{type(exc).__name__}: {exc}")
            holders, claimed_warnings = set(), []
    warnings.extend(claimed_warnings)

    s = start(SRC_READY)
    # `backlog ready` stays a subprocess: its selection logic lives inline in
    # the typer command with no function behind it, and the plan for this
    # change forbids rewriting the verb to make it importable - a second
    # implementation of its filter chain here would drift from `next`'s.
    ready = (
        _run_json([*_fno(), "backlog", "ready", "--json", "-A"], timeout=s)
        if s is not None
        else SourceRead(error=spent_error())
    )

    s = start(SRC_QUESTIONS)
    outstanding = (
        safe(_read_outstanding) if s is not None else SourceRead(error=spent_error())
    )

    s = start(SRC_NEEDS)
    # `agents needs` is owned by the Rust runtime binary, not this package:
    # there is no Python function behind the verb to call in process, so the
    # read crosses a real language boundary exactly like the `gh` one.
    needs = (
        _run_json([*_fno(), "agents", "needs", "--json"], timeout=s)
        if s is not None
        else SourceRead(error=spent_error())
    )

    return BoardInputs(
        ready=ready,
        claims=claims,
        claimed_nodes=claimed_nodes,
        holder_activity=_resolve_holder_activity(holders),
        prs=prs,
        outstanding=outstanding,
        needs=needs,
        lane=_read_lane(),
        warnings=warnings,
        undispatched=undispatched,
    )


def _read_undispatched() -> SourceRead:
    """Unwrap and validate the observer receipt before board construction.

    In process: `read_planned_unclaimed` is exactly the function
    `fno backlog undispatched` wraps.
    """
    from fno.backlog.undispatched import read_planned_unclaimed

    try:
        receipt = read_planned_unclaimed(graph_path=paths.graph_json())
    except Exception as exc:  # noqa: BLE001 - one unreadable queue, not a dead board
        return SourceRead(error=f"undispatched: {exc}")
    if not isinstance(receipt, dict) or receipt.get("status") != "ok":
        return SourceRead(error="undispatched: unreadable observer receipt")
    rows = receipt.get("rows")
    if not isinstance(rows, list) or not all(isinstance(row, dict) for row in rows):
        return SourceRead(error="undispatched: receipt rows are unreadable")
    return SourceRead(payload=rows)


def _read_claims() -> SourceRead:
    """Live + dead node claims, in process.

    The function `fno agents claim list` wraps: both roots read and merged
    exactly as the command merges them, so a claim visible to the verb is
    visible here and never only to one of the two.
    """
    from fno.claims.cli import _merge_claims_across_roots, _node_aware_root
    from fno.claims.io import dedup_claims_roots, global_claims_root

    try:
        roots = dedup_claims_roots(
            [global_claims_root(), _node_aware_root("node:")]
        )
        rows, _row_roots, _totals = _merge_claims_across_roots(
            roots, prefix="node:", include_stale=True
        )
    except Exception as exc:  # noqa: BLE001 - one unreadable queue, not a dead board
        return SourceRead(error=f"agents claim list: {exc}")
    return SourceRead(payload=rows)


def _fno() -> "list[str]":
    """The argv prefix for a self-shellout, resolved without a PATH dependency.

    A bare ``["fno", ...]`` fails on a cargo-only install, where only the Rust
    mux is on PATH. All six queues would then read "fno: not found", the board
    would exit 1 with every queue unreadable, and the king would block on a
    blind board until the dry-fire ceiling. The shared resolver is the
    established convention for exactly this case.
    """
    from fno import _subprocess_util

    return _subprocess_util.fno_py_cmd()


def autonomous_merge_enabled() -> bool:
    """Resolve ``config.king.autonomous_merge``, fail-safe to off.

    An unreadable config resolves an outward, hard-to-reverse action to off,
    which is the invariant every gate resolver in this codebase applies to
    itself.
    """
    try:
        from fno.config import load_settings

        return bool(load_settings().king.autonomous_merge)
    except Exception:  # noqa: BLE001
        return False


#: Whole-board budget when a human runs `fno inbox board` by hand and passes
#: no `--budget-ms`. This is the budget for the ENTIRE board, never a
#: per-source one: every per-source slice is derived from it (or from the
#: caller's `--budget-ms`) at collection time. An independent per-source
#: default is exactly what must not exist here - the old 60s-per-read default
#: was twice the Rust caller's 30s whole-board kill, so no inner timeout could
#: ever fire and thirteen sequential interpreter startups blew the ceiling
#: before a single query ran.
HAND_RUN_BUDGET_MS = 30_000

#: Held back from the sources so the board can still serialize and print its
#: payload before the caller's outer timer fires.
SERIALIZE_RESERVE_MS = 1_000


def _hand_run_budget_ms() -> int:
    """Read the hand-run whole-board budget, degrading loudly on a bad value.

    The env override keeps its historical unit (seconds) and its historical
    name; what it bounds changed with this module: it now budgets the whole
    board, not each read. A bare `int(...)` used to raise on a non-numeric
    override, and the Rust caller saw that as "king board output unparseable" -
    a misconfigured env var wearing the costume of a broken board. Name the
    real cause and carry on with the default, because a typo in an env var
    must not read as an unreadable board.
    """
    raw = os.environ.get("FNO_KING_BOARD_TIMEOUT")
    if raw is None:
        return HAND_RUN_BUDGET_MS
    try:
        value = int(raw)
    except ValueError:
        print(
            f"king: FNO_KING_BOARD_TIMEOUT={raw!r} is not an integer; "
            f"using {HAND_RUN_BUDGET_MS}ms",
            file=sys.stderr,
        )
        return HAND_RUN_BUDGET_MS
    if value <= 0:
        print(
            f"king: FNO_KING_BOARD_TIMEOUT={value} must be positive; "
            f"using {HAND_RUN_BUDGET_MS}ms",
            file=sys.stderr,
        )
        return HAND_RUN_BUDGET_MS
    return value * 1000


def read_board(*, scope: Optional[str] = None, budget_ms: Optional[int] = None) -> dict:
    if budget_ms is None:
        budget_ms = _hand_run_budget_ms()
    scope_ids = None
    if scope is not None:
        try:
            from fno.tracker.metadata import read_entries

            scope_ids = compile_scope_ids(scope, read_entries("king.board.scope"))
        except Exception as exc:  # noqa: BLE001 - blind scope is actionable work
            return {
                "actionable": 1,
                "unreadable": 1,
                "queues": [
                    _queue(
                        "scope",
                        f"king manifest scope {scope}",
                        SourceRead(error=str(exc)),
                        [],
                        actionable=True,
                    )
                ],
                "warnings": [],
                "exit_code": 1,
            }
    return build_board(
        collect_inputs(budget_ms=budget_ms),
        autonomous_merge=autonomous_merge_enabled(),
        crown_scope=scope,
        scope_ids=scope_ids,
    )

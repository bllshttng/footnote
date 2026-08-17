"""Read the six queues that decide whether a king still has work.

Three properties are load-bearing and each has a test that fails loudly when it
breaks.

**Every row carries the shell command that produced it.** Board emptiness has to
be reproducible by a third party who runs those commands by hand. A king that
asserted it was finished would be the receipts-can-lie shape the pitfalls corpus
already records; a king that hands you the commands is checkable.

**A queue only a human can shrink is reported and never counted.** Counting one
would hold the loop open until a person answered, which is idle-forever with a
report attached. ``operator_question`` and ``unreachable_worker`` therefore
report their real counts with ``actionable: false`` and a note naming why.

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
from dataclasses import dataclass, field
from typing import Any, Optional

from fno.agents.reachability import _ACTIVE_STATES as ACTIVE_STATES
from fno.agents.session_truth import STALLED_AFTER_S

#: Priorities a king treats as its own work. Lower bands are the operator's to
#: rank up; a king that dispatched p2 would spend the fleet on the wrong thing.
KING_PRIORITIES = frozenset({"p0", "p1"})

#: Per-queue row cap. The count stays honest; only the rendered rows are cut,
#: and the cut is reported. A silent cap reads as full coverage.
DEFAULT_MAX_ROWS = 25

#: The literal commands a reader can re-run. These strings ARE the checkability
#: property, so they live beside the readers that run them.
SRC_READY = "fno backlog ready --json"
SRC_CLAIMS = "fno claim list -J --include-stale --prefix node:"
SRC_PRS = (
    "gh pr list --state open --json number,title,mergeable,statusCheckRollup"
)
SRC_QUESTIONS = "fno outstanding --json"
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
    holder_activity: dict[str, dict]
    prs: SourceRead
    questions: SourceRead
    needs: SourceRead
    warnings: list[str] = field(default_factory=list)


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


def _queue(
    name: str,
    source: str,
    read: SourceRead,
    rows: list[dict],
    *,
    actionable: bool,
    note: str = "",
    max_rows: int = DEFAULT_MAX_ROWS,
) -> dict:
    if not read.ok:
        return {
            "name": name,
            "source": source,
            "status": "unreadable",
            "error": read.error,
            "count": None,
            "rows": [],
            "truncated": 0,
            "actionable": actionable,
            "note": note,
        }
    return {
        "name": name,
        "source": source,
        "status": "ok",
        "error": "",
        "count": len(rows),
        "rows": rows[:max_rows],
        "truncated": max(0, len(rows) - max_rows),
        "actionable": actionable,
        "note": note,
    }


def build_board(
    inputs: BoardInputs,
    *,
    autonomous_merge: bool = False,
    max_rows: int = DEFAULT_MAX_ROWS,
) -> dict:
    """Turn fetched sources into the board payload. Pure; does no I/O."""
    claim_by_node: dict[str, dict] = {}
    for row in inputs.claims.rows():
        key = str(row.get("key") or "")
        if key.startswith("node:"):
            claim_by_node[key[len("node:") :]] = row

    undispatched: list[dict] = []
    stalled: list[dict] = []
    for node in inputs.ready.rows():
        if node.get("priority") not in KING_PRIORITIES:
            continue
        if not node.get("plan_path"):
            continue
        node_id = node.get("id")
        claim = claim_by_node.get(str(node_id))
        row = {
            "id": node_id,
            "priority": node.get("priority"),
            "title": node.get("title"),
        }
        if claim is None:
            undispatched.append(row)
            continue
        # A stale claim is the `stale_claim` queue's job. Counting the node as
        # undispatched too would let the king spawn a second worker over a lock
        # nobody reaped yet; reaping first makes it undispatched on the next
        # read, which converges without the duplicate.
        if claim.get("state") == "stale":
            continue
        holder = str(claim.get("holder") or "")
        if not _holder_is_active(inputs.holder_activity.get(holder)):
            stalled.append({**row, "holder": holder, "claim_state": claim.get("state")})

    stale_claims = [
        {"key": r.get("key"), "holder": r.get("holder")}
        for r in inputs.claims.rows()
        if r.get("state") == "stale"
    ]

    pr_rows = [
        {"number": r.get("number"), "title": r.get("title")} for r in inputs.prs.rows()
    ]

    question_rows = [
        {"id": r.get("id"), "question": r.get("question"), "ts": r.get("ts")}
        for r in inputs.questions.rows()
    ]

    # `fno agents needs` emits operator questions in the same list. They are the
    # queue above, read from its own verb; counting them here would report one
    # human queue twice.
    needs_rows = [
        {"kind": r.get("kind"), "name": r.get("name"), "node": r.get("node")}
        for r in inputs.needs.rows()
        if r.get("kind") != "operator_question"
    ]

    queues = [
        _queue(
            "undispatched",
            f"{SRC_READY} + {SRC_CLAIMS}",
            SourceRead(error=inputs.ready.error or inputs.claims.error),
            undispatched,
            actionable=True,
            max_rows=max_rows,
        ),
        _queue(
            "stalled_holder",
            f"{SRC_READY} + {SRC_CLAIMS} + fno agents peek <holder>",
            SourceRead(error=inputs.ready.error or inputs.claims.error),
            stalled,
            actionable=True,
            max_rows=max_rows,
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
            max_rows=max_rows,
        ),
        _queue(
            "stale_claim",
            SRC_CLAIMS,
            inputs.claims,
            stale_claims,
            actionable=True,
            max_rows=max_rows,
        ),
        _queue(
            "operator_question",
            SRC_QUESTIONS,
            inputs.questions,
            question_rows,
            actionable=False,
            note="report-only: a human answers these, so counting them would "
            "hold the loop open forever",
            max_rows=max_rows,
        ),
        _queue(
            "unreachable_worker",
            SRC_NEEDS,
            inputs.needs,
            needs_rows,
            actionable=False,
            note="report-only: the refusal event a king would act on does not "
            "exist yet",
            max_rows=max_rows,
        ),
    ]

    actionable = 0
    unreadable = 0
    for q in queues:
        if q["status"] == "unreadable":
            unreadable += 1
            # A blind ACTIONABLE queue is work: the king may not exit while it
            # cannot see a queue it could have shrunk. A blind report-only queue
            # is loud (the exit code below) and still uncounted, because the
            # rule that a human-gated queue never gates NoWork does not get
            # weaker when the verb behind it breaks. `fno outstanding` timing
            # out is a measured failure on this machine, and counting it would
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


def _run_json(cmd: list[str], *, timeout: int = 60) -> SourceRead:
    """Run a verb and parse its JSON. Every failure arrives as an error string.

    A non-zero exit, a timeout, a missing binary and unparseable output are all
    the same thing to the board: this queue could not be read.
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
        return SourceRead(error=f"{' '.join(cmd)}: timed out after {timeout}s")
    if proc.returncode != 0:
        detail = (proc.stderr or proc.stdout or "").strip()[:500]
        return SourceRead(error=f"exit {proc.returncode}: {detail}")
    try:
        return SourceRead(payload=json.loads(proc.stdout or "null"))
    except json.JSONDecodeError as exc:
        return SourceRead(error=f"unparseable output: {exc}")


def _read_prs(timeout: int, max_pr_reads: int) -> tuple[SourceRead, list[str]]:
    """Open PRs that are green, mergeable and unmerged.

    Costs one listing plus one status read per open PR, so the per-PR reads are
    capped and the truncation is reported rather than swallowed.
    """
    listing = _run_json(
        [
            "gh",
            "pr",
            "list",
            "--state",
            "open",
            "--json",
            "number,title,mergeable,statusCheckRollup",
        ],
        timeout=timeout,
    )
    if not listing.ok:
        return listing, []
    rows = listing.rows()
    warnings: list[str] = []
    if len(rows) > max_pr_reads:
        warnings.append(
            f"mergeable_pr: capped at {max_pr_reads} of {len(rows)} open PRs"
        )
        rows = rows[:max_pr_reads]
    ready: list[dict] = []
    for pr in rows:
        if pr.get("mergeable") != "MERGEABLE":
            continue
        rollup = pr.get("statusCheckRollup") or []
        states = {
            (c.get("conclusion") or c.get("state") or "").upper() for c in rollup
        }
        if states & {"FAILURE", "CANCELLED", "TIMED_OUT", "ERROR"}:
            continue
        if "PENDING" in states or "IN_PROGRESS" in states or "QUEUED" in states:
            continue
        ready.append({"number": pr.get("number"), "title": pr.get("title")})
    return SourceRead(payload=ready), warnings


def _read_questions(timeout: int) -> SourceRead:
    read = _run_json(["fno", "outstanding", "--json"], timeout=timeout)
    if not read.ok:
        return read
    payload = read.payload if isinstance(read.payload, dict) else {}
    return SourceRead(payload=payload.get("questions") or [])


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
        # `target-session:<harness session id>` is the shape fno target init
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


def collect_inputs(*, timeout: int = 60, max_pr_reads: int = 20) -> BoardInputs:
    """Fetch every source. Never raises; every failure lands in a SourceRead."""
    ready = _run_json(["fno", "backlog", "ready", "--json"], timeout=timeout)
    claims = _run_json(
        ["fno", "claim", "list", "-J", "--include-stale", "--prefix", "node:"],
        timeout=timeout,
    )
    prs, warnings = _read_prs(timeout, max_pr_reads)

    holders: set[str] = set()
    if ready.ok and claims.ok:
        wanted = {
            str(n.get("id"))
            for n in ready.rows()
            if n.get("priority") in KING_PRIORITIES and n.get("plan_path")
        }
        for row in claims.rows():
            key = str(row.get("key") or "")
            if not key.startswith("node:") or row.get("state") == "stale":
                continue
            if key[len("node:") :] in wanted and row.get("holder"):
                holders.add(str(row["holder"]))

    return BoardInputs(
        ready=ready,
        claims=claims,
        holder_activity=_resolve_holder_activity(holders),
        prs=prs,
        questions=_read_questions(timeout),
        needs=_run_json(["fno", "agents", "needs", "--json"], timeout=timeout),
        warnings=warnings,
    )


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


def read_board(*, max_rows: int = DEFAULT_MAX_ROWS) -> dict:
    timeout = int(os.environ.get("FNO_KING_BOARD_TIMEOUT", "60"))
    return build_board(
        collect_inputs(timeout=timeout),
        autonomous_merge=autonomous_merge_enabled(),
        max_rows=max_rows,
    )

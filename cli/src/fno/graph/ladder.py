"""The readiness authority: what rung is a node's linked plan on, and what
may each caller do about it?

Readiness used to be answered in seven places across three languages over four
vocabularies, with two of the answers using opposite failure policies and no
file referencing another. This module is the single Python answer; the shell
callers reach it through ``fno do plan rung`` rather than re-parsing ``^status:``.

Three policies, one read:

- :func:`plan_rung` - the classification. Never raises.
- :func:`is_dispatchable` - may a fresh-context worker be launched against this
  plan? Fails CLOSED on the same rung.
- :func:`is_cold_dispatchable` - may the autonomous drain hand a plan-less idea
  to target so target can author its plan?

Autonomous selection reads ``plan_rung`` directly through its shared guards;
there is no second selection predicate whose name can imply coverage it lacks.
Dispatch fails closed because building against an unreadable plan is worse than
parking.

Derived per read rather than persisted into ``status``. A plan doc is external
mutable state that ``/blueprint`` rewrites WITHOUT touching the graph, and
``read_graph`` does not recompute ``status`` (only ``locked_mutate_graph``
does), so a persisted ``design`` would never re-arm once the blueprint landed -
the node would starve invisibly forever. Same shape as
``statuses.live_claimed_node_ids``, which overlays the claim lockfile for the
same reason.
"""
from __future__ import annotations

import os
import sys
from dataclasses import dataclass
from datetime import date
from enum import Enum
from typing import Optional

#: One-shot latch so a missing PyYAML warns once per process, not once per plan.
_WARNED_NO_YAML = False


class DispatchHoldState(Enum):
    ABSENT = "absent"
    HELD = "held"
    INVALID = "invalid"


@dataclass(frozen=True)
class DispatchHold:
    state: DispatchHoldState
    reason: str = ""
    release_when: str = ""
    review_on: str = ""
    set_by: str = ""
    detail: str = ""


@dataclass(frozen=True)
class DispatchHoldVerdict:
    owner_id: str
    hold: DispatchHold

    @property
    def guard_reason(self) -> str:
        prefix = (
            "dispatch-hold"
            if self.hold.state is DispatchHoldState.HELD
            else "dispatch-hold-invalid"
        )
        return f"{prefix}:{self.owner_id}"


def resolve_plan_probe(entry: dict) -> Optional[str]:
    """Filesystem path for a node's plan doc, or None when it has no usable one.

    Resolves the way the node itself would: strip a ``#anchor`` fragment,
    expand ``~``, and resolve a repo-relative path against the NODE's own
    ``cwd`` rather than the calling process's. The daemon selects across
    projects, so probing a foreign node's relative path against the current
    process cwd would silently find nothing - on the live graph that is the
    majority of linked plans, not an edge case.
    """
    if not isinstance(entry, dict):
        return None
    plan_path = entry.get("plan_path")
    if not isinstance(plan_path, str) or not plan_path:
        return None
    probe = os.path.expanduser(plan_path.split("#", 1)[0])
    if not probe:
        return None
    if not os.path.isabs(probe):
        cwd = entry.get("cwd")
        if not (isinstance(cwd, str) and cwd):
            # No anchor to resolve against. Returning the relative path would
            # silently resolve it against the CALLING process's cwd, where a
            # coincidentally-matching local doc could design-gate an unrelated
            # node. Refuse to guess and let the caller fail open instead.
            return None
        probe = os.path.join(cwd, probe)
    return probe


class Rung(Enum):
    """Where a node's plan sits, or why we cannot tell.

    ``NONE`` and ``IDEA`` stay distinct even though both derive to graph status
    ``idea``: ``NONE`` (nothing on disk) is COLD-DISPATCHABLE - ``/target``
    authors the plan - while ``IDEA`` (a linked-but-undesigned decompose
    scaffold) needs warm inline-fill, so it stays gated behind
    ``--include-ideas``. See :func:`is_cold_dispatchable`.
    """

    NONE = "none"  # no usable plan_path - nothing on disk
    IDEA = "idea"  # doc exists, declares no design yet (or says so outright)
    DESIGN = "design"
    READY = "ready"
    IN_PROGRESS = "in_progress"
    IN_REVIEW = "in_review"
    DONE = "done"
    SUPERSEDED = "superseded"
    UNREADABLE = "unreadable"  # cannot classify: see the two policies below


# Plan-frontmatter vocabulary -> rung, after `canonical_status` has resolved any
# retired spelling (`stub`->idea, `shipped`->in_review, `archived`->superseded).
# Total over that vocabulary by construction: a word missing here is a word the
# binary does not know, which routes to UNREADABLE.
_STATUS_TO_RUNG: dict[str, Rung] = {
    "idea": Rung.IDEA,
    "design": Rung.DESIGN,
    "ready": Rung.READY,
    "in_progress": Rung.IN_PROGRESS,
    "in_review": Rung.IN_REVIEW,
    "done": Rung.DONE,
    "superseded": Rung.SUPERSEDED,
}

# Rungs a fresh-context worker may be launched against. Exactly the set
# `handoff.sh` accepted before it delegated here, so the verb is a drop-in for
# its `grep | sed | tr` block rather than a new policy. Rung.NONE is NOT here:
# a plan-less node has no plan to launch "against". It is instead COLD-dispatched
# (the worker authors the plan via think -> blueprint) through the separate
# :func:`is_cold_dispatchable` gate, which adds the required ``status == "idea"``
# conjunct so a blocked/done plan-less node is never admitted (x-e24a). Keeping
# NONE out of this set preserves is_dispatchable's "has a launchable plan"
# contract; the cold path is its own predicate, not an overload of this one.
_DISPATCHABLE: frozenset[Rung] = frozenset(
    {Rung.READY, Rung.IN_PROGRESS, Rung.IN_REVIEW}
)

# Rungs the daemon must NOT pick up on its own. Undesigned work needs a design
# pass first. UNREADABLE remains outside this rung set because the attributable
# hold reader supplies its own stronger fail-closed verdict for unreadable plans.
#
# Public because ``selection_guards`` needs the SET, not the bool: it reports a
# rung-specific reason and would otherwise pay a second filesystem read to
# recover which rung it was. Sharing the set keeps one definition of the policy
# across both shapes.
UNSELECTABLE_RUNGS: frozenset[Rung] = frozenset({Rung.IDEA, Rung.DESIGN})


def _read_frontmatter(probe: str) -> tuple[Optional[dict], bool]:
    """``(mapping_or_None, readable)`` for the plan at *probe*."""
    try:
        text = open(probe, encoding="utf-8").read()
    except (OSError, UnicodeDecodeError, ValueError):
        return (None, False)

    if not text.strip():
        return (None, False)

    lines = text.splitlines()
    if lines[0].strip() != "---":
        return ({}, True)

    end = next((i for i in range(1, len(lines)) if lines[i].strip() == "---"), None)
    if end is None:
        return (None, False)

    try:
        import yaml
    except ImportError:
        global _WARNED_NO_YAML
        if not _WARNED_NO_YAML:
            _WARNED_NO_YAML = True
            print(
                "ladder: PyYAML missing - every plan reads UNREADABLE "
                "(undispatchable). Install it to restore rung classification.",
                file=sys.stderr,
            )
        return (None, False)

    try:
        fm = yaml.safe_load("\n".join(lines[1:end]))
    except Exception:  # noqa: BLE001 - malformed YAML: genuinely cannot tell
        return (None, False)

    if fm is None:
        return ({}, True)
    if not isinstance(fm, dict):
        return (None, False)
    return (fm, True)


def _read_status_scalar(probe: str) -> tuple[Optional[str], bool]:
    """``(status_or_None, readable)`` for the plan at *probe*.

    Deliberately NOT built on ``_read_plan_frontmatter``: that function returns
    ``{}`` for missing, unreadable, malformed AND status-less alike (its
    docstring calls the total collapse a feature, and for its own callers it
    is). A rung resolver cannot inherit it, because "cannot read this file" and
    "this file declares no status" must route to opposite failure policies.

    ``readable`` is False only when we genuinely could not parse the document.
    A readable doc with no frontmatter, or frontmatter with no ``status``,
    returns ``(None, True)`` - an answer, not a failure.
    """
    fm, readable = _read_frontmatter(probe)
    if not readable or fm is None:
        return (None, False)
    if "status" not in fm:
        return (None, True)
    return (str(fm["status"] if fm["status"] is not None else ""), True)


def dispatch_hold(entry: object) -> DispatchHold:
    """Read one plan's hold declaration, failing closed on an unreadable plan."""
    if not isinstance(entry, dict):
        return DispatchHold(DispatchHoldState.ABSENT)
    plan_path = entry.get("plan_path")
    if not isinstance(plan_path, str) or not plan_path:
        return DispatchHold(DispatchHoldState.ABSENT)
    probe = resolve_plan_probe(entry)
    if not probe:
        return DispatchHold(DispatchHoldState.ABSENT)
    # Cross-project and temporarily unmounted plan roots are an established
    # no-signal case: there is no declaration to validate in this process. An
    # existing file that cannot be parsed is different - its hold state was
    # reached but is unreadable, and that fails closed below.
    if not os.path.exists(probe):
        return DispatchHold(DispatchHoldState.ABSENT)
    fm, readable = _read_frontmatter(probe)
    if not readable or fm is None:
        return DispatchHold(
            DispatchHoldState.INVALID,
            detail=f"plan frontmatter is unreadable: {probe}",
        )
    if "dispatch_hold" not in fm:
        return DispatchHold(DispatchHoldState.ABSENT)
    try:
        from fno.plan.schema import DispatchHoldBlock

        parsed = DispatchHoldBlock.model_validate(fm["dispatch_hold"])
    except Exception as exc:  # noqa: BLE001 - invalid means refuse, never raise
        return DispatchHold(
            DispatchHoldState.INVALID,
            detail=f"dispatch_hold is invalid: {exc}",
        )
    review_on = parsed.review_on.isoformat()
    detail = ""
    if parsed.review_on < date.today():
        detail = f"review date {review_on} has passed; hold remains active"
    return DispatchHold(
        DispatchHoldState.HELD,
        reason=parsed.reason,
        release_when=parsed.release_when,
        review_on=review_on,
        set_by=parsed.set_by,
        detail=detail,
    )


def dispatch_hold_verdict(
    entry: object,
    entries_by_id: dict,
) -> Optional[DispatchHoldVerdict]:
    """Find a hold on a node, its parents, or its contained delivery owner."""
    if not isinstance(entry, dict):
        return None
    queue = [entry]
    seen: set[str] = set()
    steps = 0
    while queue and steps < 64:
        current = queue.pop(0)
        node_id = str(current.get("id") or "unknown")
        if node_id in seen:
            continue
        seen.add(node_id)
        hold = dispatch_hold(current)
        if hold.state is not DispatchHoldState.ABSENT:
            return DispatchHoldVerdict(node_id, hold)
        for relation in ("contained_in", "parent"):
            related = current.get(relation)
            if isinstance(related, str) and related and related not in seen:
                ancestor = entries_by_id.get(related)
                if isinstance(ancestor, dict):
                    queue.append(ancestor)
        steps += 1
    return None


def plan_rung(entry: object) -> Rung:
    """The rung a node's linked plan sits on. Never raises.

    Takes the whole entry, not a bare path, because resolving the path needs
    the node's ``cwd`` (see :func:`resolve_plan_probe`).

    An unrecognized-but-readable status is ``UNREADABLE``, not its own rung:
    an unknown word is exactly the case ``handoff.sh`` parked on before this
    verb existed, and folding it here keeps that behavior. The cost is that a
    doc written by a NEWER binary (a vocabulary this one has not learned) is
    indistinguishable from a corrupt one. Both park, which is the safe answer
    in either case.

    Keys on frontmatter rather than a ``## Execution Strategy`` heading because
    `/blueprint quick` deliberately omits that heading (blueprint SKILL.md),
    so the heading would misread every quick-plan as unfinished.
    """
    if not isinstance(entry, dict):
        return Rung.NONE
    plan_path = entry.get("plan_path")
    if not (isinstance(plan_path, str) and plan_path):
        return Rung.NONE

    probe = resolve_plan_probe(entry)
    if not probe:
        # A plan_path IS declared, we just cannot resolve it to a file - a
        # repo-relative path on a node with no `cwd` to anchor it. That is
        # "cannot tell", not "nothing on disk": `resolve_plan_probe` refuses to
        # guess precisely so the caller can fail open here. Collapsing it into
        # NONE would be the same two-failure-modes-one-value mistake this
        # resolver exists to undo, one function upstream.
        return Rung.UNREADABLE

    raw, readable = _read_status_scalar(probe)
    if not readable:
        return Rung.UNREADABLE
    if raw is None:
        # Readable, but declares no status. NOT ``UNREADABLE`` - we read it
        # fine, and AC4-ERR exists to keep those two apart - and deliberately
        # ``READY``, which is what every surface derived for it before this
        # module existed.
        #
        # Treating silence as pre-design is tempting and wrong. The defect this
        # module fixes is ``status: stub``, a WORD in no vocabulary that
        # therefore read as ``ready``; an ABSENT status is a different thing.
        # Most plan docs in a mature vault predate the status vocabulary
        # entirely, and `fno backlog intake <plan.md>` on one of them must still
        # produce a workable node - demoting them all to `idea` would empty the
        # board to fix a bug they never had.
        return Rung.READY

    from fno.plan._status import canonical_status

    return _STATUS_TO_RUNG.get(canonical_status(raw), Rung.UNREADABLE)


def is_dispatchable(entry: object) -> bool:
    """May a fresh-context worker be launched against this plan? FAILS CLOSED.

    True only for :data:`_DISPATCHABLE` (the plan-bearing rungs). A plan-less
    idea (``Rung.NONE``) has no plan to launch against, so this returns False for
    it - but it IS cold-dispatchable; see :func:`is_cold_dispatchable`, the
    separate gate the autonomous drain uses. Everything else parks, including an
    unreadable plan.
    """
    return plan_rung(entry) in _DISPATCHABLE


def is_cold_dispatchable(entry: object) -> bool:
    """A plan-less idea node the autonomous drain may dispatch without a plan.

    The SOLE admission path for ``Rung.NONE`` (x-e24a): ``is_dispatchable`` stays
    False for plan-less nodes (no plan to launch against), so this predicate -
    not ``_DISPATCHABLE`` - is what the four drain selectors OR into their status
    gate. Requires ``status == "idea"`` (so blocked / in_progress / done
    plan-less nodes are excluded) AND ``plan_rung`` is ``NONE`` (so a linked
    decompose stub, ``Rung.IDEA``, stays excluded). ``/target`` authors the plan
    (think -> blueprint -> do), so a plan-less idea is dispatchable as-is.

    The status conjunct is load-bearing: ``plan_rung`` is ``NONE`` for any node
    without a plan_path, including one whose graph status is ``blocked``, so the
    rung alone would over-admit.
    """
    return (
        isinstance(entry, dict)
        and entry.get("status") == "idea"
        and plan_rung(entry) is Rung.NONE
    )


def is_design_stage(entry: object) -> bool:
    """True only when the node's linked plan says ``status: design``.

    Kept as a name so its existing callers do not churn; the classification
    now comes from :func:`plan_rung`.
    """
    return plan_rung(entry) is Rung.DESIGN

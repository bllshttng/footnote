"""The readiness authority: what rung is a node's linked plan on, and what
may each caller do about it?

Readiness used to be answered in seven places across three languages over four
vocabularies, with two of the answers using opposite failure policies and no
file referencing another. This module is the single Python answer; the shell
callers reach it through ``fno plan rung`` rather than re-parsing ``^status:``.

Three names, one read:

- :func:`plan_rung` - the classification. Never raises.
- :func:`is_selectable` - may the daemon pick this node up on its own? Fails
  OPEN on :attr:`Rung.UNREADABLE`.
- :func:`is_dispatchable` - may a fresh-context worker be launched against this
  plan? Fails CLOSED on the same rung.

**The two policies disagree on purpose, permanently.** Selection must fail open
because plans live in a symlinked vault: demoting on a read failure would
quarantine the entire backlog the moment it unmounts. Dispatch must fail closed
because building against an unreadable plan is worse than parking. Both are
correct; the defect this module fixes was that they were scattered and
undocumented relative to each other, not that they differ. A test asserts they
disagree on ``UNREADABLE`` so a future "simplification" fails loudly.

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
from enum import Enum
from typing import Optional

#: One-shot latch so a missing PyYAML warns once per process, not once per plan.
_WARNED_NO_YAML = False


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

# Rungs a fresh-context worker may be launched against. The ready-family rungs
# are exactly the set `handoff.sh` accepted before it delegated here, so the
# verb is a drop-in for its `grep | sed | tr` block. NONE is the autonomous
# cold-dispatch addition (x-e24a): a plan-less node has no plan to launch
# against, but `/target` authors one (think -> blueprint), so the drain treats
# it as dispatchable. :func:`is_cold_dispatchable` is the live gate that pairs
# the rung with the ``status == "idea"`` requirement (a blocked/done plan-less
# node is not admitted even though its rung is NONE).
_DISPATCHABLE: frozenset[Rung] = frozenset(
    {Rung.NONE, Rung.READY, Rung.IN_PROGRESS, Rung.IN_REVIEW}
)

# Rungs the daemon must NOT pick up on its own. Undesigned work needs a design
# pass first; every other rung (including UNREADABLE) stays in the pool.
#
# Public because ``selection_guards`` needs the SET, not the bool: it reports a
# rung-specific reason and would otherwise pay a second filesystem read to
# recover which rung it was. Sharing the set keeps one definition of the policy
# across both shapes.
UNSELECTABLE_RUNGS: frozenset[Rung] = frozenset({Rung.IDEA, Rung.DESIGN})


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
    try:
        text = open(probe, encoding="utf-8").read()
    except (OSError, UnicodeDecodeError, ValueError):
        return (None, False)

    if not text.strip():
        # An empty file is a truncated write or a stray `touch`, not a plan that
        # predates the status vocabulary. Silence means READY below, which would
        # make a 0-byte file dispatchable; "cannot classify" is the honest read.
        return (None, False)

    lines = text.splitlines()
    if lines[0].strip() != "---":
        return (None, True)  # no frontmatter block: declares nothing, readably

    end = next((i for i in range(1, len(lines)) if lines[i].strip() == "---"), None)
    if end is None:
        return (None, False)  # opened a frontmatter block and never closed it

    try:
        import yaml
    except ImportError:
        # A tooling gap, not a plan defect - and it parks EVERY plan at once,
        # so say so rather than letting the operator read "malformed
        # frontmatter" nine times about nine healthy docs. Warned once per
        # process; the verdict is still UNREADABLE (fail closed for dispatch).
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
        return (None, True)  # empty frontmatter block
    if not isinstance(fm, dict):
        return (None, False)  # a scalar or list where a mapping belongs
    if "status" not in fm:
        return (None, True)
    return (str(fm["status"] if fm["status"] is not None else ""), True)


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


def is_selectable(entry: object) -> bool:
    """May the daemon pick this node up on its own? FAILS OPEN.

    False only for the two undesigned rungs. An unreadable plan stays
    selectable on purpose: see the module docstring's fail-open argument.
    """
    return plan_rung(entry) not in UNSELECTABLE_RUNGS


def is_dispatchable(entry: object) -> bool:
    """May a fresh-context worker be launched against this plan? FAILS CLOSED.

    True only for :data:`_DISPATCHABLE`: the ready-family rungs, plus ``NONE``
    (cold-dispatchable - ``/target`` authors the plan). Everything else parks,
    including an unreadable plan - the deliberate disagreement with
    :func:`is_selectable`, which fails open on it.
    """
    return plan_rung(entry) in _DISPATCHABLE


def is_cold_dispatchable(entry: object) -> bool:
    """A plan-less idea node the autonomous drain may dispatch without a plan.

    The live gate for x-e24a: ``status == "idea"`` (so blocked / in_progress /
    done plan-less nodes are excluded) AND ``plan_rung`` is ``NONE`` (so a linked
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

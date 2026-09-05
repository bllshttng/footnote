"""The adoption guards, shared by `fno backlog decompose` and `fno backlog adopt`.

`refuse_dead_owner` and `adopt_into` moved out of the decompose loop so both
callers run one implementation (principle 9). The refusals keep decompose's
message text; `context` is the caller's noun phrase where those messages say
`group 'slug'`, so the verb reads `adopt` and decompose keeps its wording.
The withheld-containment warning is returned, not raised: each caller renders
its own sentence from it.
"""

from dataclasses import dataclass

from fno.graph._decompose import DecomposeError
from fno.graph._intake import _would_create_cycle
from fno.graph.statuses import _LEGACY_DEFER_PREFIX, node_is_done


def refuse_dead_owner(owner: dict, *, context: str) -> None:
    """Refuse a deferred or superseded owner: a dead delivery unit cannot own containment."""
    completed = owner.get("completed_at")
    legacy_defer = isinstance(completed, str) and completed.startswith(_LEGACY_DEFER_PREFIX)
    if not (bool(completed) and not legacy_defer) and (
        owner.get("deferred_at") or owner.get("superseded_by") or legacy_defer
    ):
        superseder = owner.get("superseded_by")
        if superseder:
            how = f"was superseded by {superseder}"
            containment = (
                "its death already released the nodes it contained and nothing re-runs that release"
            )
            remedy = (
                f"Run `fno backlog unsupersede {owner['id']}` to revive it "
                "(clears superseded_by; `undefer` does not), or point the "
                "adopt list at the superseding node, or give the group a "
                "new slug so it mints a live delivery unit"
            )
        else:
            how = "is deferred"
            containment = (
                "its contained nodes remain folded under it (defer keeps "
                "containment; nothing re-runs a release)"
            )
            remedy = (
                f"Run `fno backlog undefer {owner['id']}` first (its "
                "children resume contained, not released), or drop the "
                "adopt list from this group"
            )
        raise DecomposeError(
            f"{context} resolves to {owner['id']}, which {how}; {containment}, "
            "so stamping containment here would leave every adoptee "
            f"undispatchable with no verb to free it. {remedy}",
            exit_code=2,
        )


@dataclass
class AdoptOutcome:
    """One target's adoption result.

    `adopted` is whether the parent pointer moved this call; `warning` is the
    short reason containment was withheld (a done node with a PR or cost),
    for the caller to render.
    """

    adopted: bool
    warning: str | None


def adopt_into(
    entries: list[dict],
    owner: dict,
    target: dict,
    *,
    live_worker,
    context: str,
) -> AdoptOutcome:
    """Fold `target` into `owner`: guards, then stamp `contained_in` + `parent`.

    Guard order is load-bearing: live worker, cycle, one-level children,
    mid-flight delivery unit (refuse), then the withheld-containment case for
    a done node with its own PR or cost. Callers hold the graph lock.
    """
    holder = live_worker(target["id"])
    if holder:
        raise DecomposeError(
            f"{context} adopts {target['id']}, which is being built right now "
            f"by {holder}; adopting it would leave that session holding a "
            "claim on a node that no longer dispatches, and it would still "
            "open its own PR. Wait for it to land, or stop it first",
            exit_code=2,
        )
    if _would_create_cycle(entries, target["id"], owner["id"]):
        raise DecomposeError(
            f"adopting {target['id']} into {context} would create a cycle",
            exit_code=2,
        )
    kids = [
        e.get("id")
        for e in entries
        if isinstance(e, dict) and e.get("parent") == target["id"]
    ]
    if kids:
        raise DecomposeError(
            f"{context} adopts {target['id']}, which has {len(kids)} child(ren) "
            f"({', '.join(str(k) for k in kids[:3])}{'...' if len(kids) > 3 else ''}); "
            "containment is one level, so they would stay dispatchable and open "
            "their own PRs while their parent closed. Adopt the children "
            "individually, or re-parent them out first",
            exit_code=2,
        )
    own_pr = target.get("pr_number")
    own_cost = target.get("cost_usd")
    if (own_pr or own_cost is not None) and not node_is_done(target):
        what = f"has an open PR (#{own_pr})" if own_pr else "has accrued cost"
        raise DecomposeError(
            f"{context} adopts {target['id']}, which {what} and has not "
            "landed; it is its own delivery unit mid-flight. Adopting it "
            "would hang open work under the group, and the epic would close "
            "over it when the group merges. Let it land first, or drop it "
            "from the adopt list",
            exit_code=2,
        )
    warning = None
    if own_pr or own_cost is not None:
        warning = f"carries PR #{own_pr}" if own_pr else "carries cost"
    else:
        target["contained_in"] = owner["id"]
    if target.get("parent") == owner["id"]:
        return AdoptOutcome(adopted=False, warning=warning)  # already adopted
    target["parent"] = owner["id"]
    return AdoptOutcome(adopted=True, warning=warning)

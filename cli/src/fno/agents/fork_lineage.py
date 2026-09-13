"""What a resume fork inherits from the row it forked FROM.

A wake or relaunch fork mints a NEW row for a session claude resumed off an
old transcript (claude mints a fresh uuid no flag carries back). Whatever the
caller left unstated, the lineage row states: stamping ambient defaults
instead re-bills and re-routes the woken incarnation (a zai row waking as
anthropic, a model pin falling back) and drops the node binding that says
which backlog node the worker serves. Caller-explicit flags always outrank
the carry.
"""
from __future__ import annotations

from typing import Any, Optional, Sequence


def lineage_row_for(entries: Sequence[Any], resume_session_id: Optional[str]) -> Any:
    """The registry row this resume forks FROM, matched by uuid alone - the
    route/account lookups need a recorded route file; the identity axes do not."""
    if not resume_session_id:
        return None
    return next(
        (
            e
            for e in entries
            if getattr(e, "harness_session_id", None) == resume_session_id
        ),
        None,
    )


def inherited_model(src: Any, model: Optional[str], route_model: Optional[str]) -> Optional[str]:
    """The lineage model, when the caller named no model of its own."""
    if model or route_model:
        return None
    return getattr(src, "model", None)


def predecessor_ids(resume_session_id: Optional[str], revive: bool) -> list[str]:
    """A fork to a new name retires the resumed uuid, so the row records it as
    a predecessor: mail addressed to the old id still lands on the surviving
    row. A revive keeps the resumed uuid as its primary, so nothing to record."""
    return [resume_session_id] if resume_session_id and not revive else []


def axis_overrides(
    src: Any,
    *,
    model: Optional[str],
    route_model: Optional[str],
    verified_model: Optional[str],
    route_provider: Optional[str],
    lane_provider: Optional[str],
    effort: Optional[str],
    route_provider_id: Optional[str],
    model_name: Optional[str],
    resume_session_id: Optional[str],
    revive: bool,
) -> dict[str, Any]:
    """Mint kwargs carrying every lineage axis the caller left unstated.

    Each chain reads explicit > lineage > lane default; ``getattr`` on a None
    src resolves to the default, so a fresh spawn (no lineage) stamps exactly
    what it stamped before this module existed.
    """
    return dict(
        # Lineage beats the lane default but loses to an explicit route - a
        # woken worker bills on the provider it was running.
        provider=route_provider or getattr(src, "provider", None) or lane_provider,
        model=verified_model or model or route_model or inherited_model(src, model, route_model),
        model_basis=(
            "verified"
            if verified_model
            else (
                "requested"
                if (model or route_model)
                else getattr(src, "model_basis", None)
            )
        ),
        effort=effort or getattr(src, "effort", None),
        # The REQUEST verbatim beside the effect, so a silent substitution
        # stays diffable.
        requested_model=model
        or route_model
        or getattr(src, "requested_model", None)
        or inherited_model(src, model, route_model),
        requested_provider=route_provider
        or getattr(src, "requested_provider", None)
        or getattr(src, "provider", None)
        or lane_provider,
        requested_effort=effort or getattr(src, "requested_effort", None),
        predecessor_session_ids=predecessor_ids(resume_session_id, revive),
        route_provider_id=route_provider_id or getattr(src, "route_provider_id", None),
        model_name=model_name or getattr(src, "model_name", None),
    )

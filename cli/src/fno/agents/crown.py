"""The crown vocabulary: what a scope is, what rung it implies, and what a
grantor may hand down.

Everything about orchestrator authority that is not storage lives here.
``registry`` owns the three fields on the row (``crown_level`` /
``crown_scope`` / ``crown_grantor``) and nothing about their meaning; this
module owns the meaning and touches no file.

THE LADDER IS THREE RUNGS, AND EACH IS A FACT ABOUT THE SCOPE:

    0   several projects   scope names 2+ config projects (a portfolio)
    1   one project        scope names one config project
    2   one epic           scope is a backlog node with type == "epic"

There is deliberately no rung for an implementer. A node that is not an epic is
work, not a territory, and nobody reigns for a day over a single task - so a
crown aimed at one is REFUSED rather than stamped at some bottom rung. That
refusal is the whole reason no caller passes a level: ``derive_crown_level``
reads the rung off the scope, and a scope that names no territory has no rung to
read, which is exactly the case that should fail.

The practical payoff is that the counter-intuitive part of the old surface is
gone. Callers used to hand-type an altitude on a ladder whose direction reads
backwards (0 is the TOP), and typing the wrong one silently minted authority at
the wrong altitude. Now the only input is the territory, which the operator
already knows by name.
"""
from __future__ import annotations

from typing import Any, Optional

#: The bottom rung. Kept as a bound rather than a magic number so the stored
#: value stays far inside the Rust row's ``crown_level: Option<u32>``.
MAX_CROWN_LEVEL = 2

#: Separator for a level-0 scope. The registry field is a single string - the
#: Rust side only custodies and displays it (``crown_scope.as_deref()`` in
#: client.rs is its one read) - so a portfolio scope is stored as its member
#: project names joined by this, rather than costing a schema version bump and
#: ~90 mechanical edits across two crates for a field the daemon never reasons
#: about.
SCOPE_SEPARATOR = ","


class CrownScopeError(ValueError):
    """A scope that names no territory this ladder recognizes."""


def canonical_scope(scopes: list[str]) -> str:
    """The stored form: one name, or sorted unique members joined.

    Canonical on purpose. The one-live-crown guard compares scopes by equality,
    so ``{web,etl}`` and ``{etl,web}`` must reduce to the SAME string or a second
    crown over one portfolio slips through spelled in a different order.
    """
    return SCOPE_SEPARATOR.join(sorted({s.strip() for s in scopes if s.strip()}))


def split_scope(scope: Optional[str]) -> list[str]:
    """The members of a stored scope; a single-name scope yields one element."""
    if not scope:
        return []
    return [s for s in (part.strip() for part in scope.split(SCOPE_SEPARATOR)) if s]


def _graph_entry(node_id: str) -> Optional[dict]:
    """The backlog entry for ``node_id``, or None. Path resolved dynamically:
    ``read_graph``'s default arg is frozen at import time and would ignore a
    redirected state dir (tests, overrides)."""
    from fno import paths
    from fno.graph.store import read_graph

    return next(
        (
            e
            for e in read_graph(paths.graph_json())
            if isinstance(e, dict) and e.get("id") == node_id
        ),
        None,
    )


def _is_project(name: str) -> bool:
    """Does ``name`` resolve to a configured project? Projects are declared in
    config (``work.workspaces.<ws>.projects[].name``) and need no backlog node -
    which is why the top two rungs cannot be derived from the graph alone."""
    try:
        from fno.projects.resolve import resolve_project_name

        resolve_project_name(name)
        return True
    except Exception:
        return False


def derive_crown_level(scopes: list[str]) -> int:
    """The rung ``scopes`` implies. Raises :class:`CrownScopeError` when they
    name no territory - the refusal that keeps implementers uncrowned.

    Mixed scopes are refused rather than coerced: a portfolio is projects, so
    naming a project and an epic together is a mistake about what is being ruled,
    not a level-0 crown over both.
    """
    members = split_scope(canonical_scope(scopes))
    if not members:
        raise CrownScopeError("a crown needs a scope: name an epic or a project")

    if len(members) > 1:
        unknown = [m for m in members if not _is_project(m)]
        if unknown:
            raise CrownScopeError(
                f"a multi-scope crown rules PROJECTS, but {', '.join(unknown)} "
                f"{'is not a configured project' if len(unknown) == 1 else 'are not configured projects'}. "
                "Name projects from your config, or pass a single epic instead."
            )
        return 0

    name = members[0]
    if _is_project(name):
        return 1

    entry = _graph_entry(name)
    if entry is None:
        raise CrownScopeError(
            f"{name!r} is neither a configured project nor a backlog node; "
            "nothing to reign over (check for a typo)"
        )
    if entry.get("type") != "epic":
        raise CrownScopeError(
            f"{name!r} is a {entry.get('type') or 'node'}, not an epic. "
            "Implementers get no crowns - a single node is work, not a territory. "
            "Crown the epic above it, or its project."
        )
    return 2


def scope_contains(outer: Optional[str], inner: Optional[str]) -> bool:
    """Does a crown over ``outer`` strictly contain one over ``inner``?

    Real containment, not the honor system it replaces. The old rule could only
    check that two scopes DIFFERED, because scopes were opaque ids and
    project>epic>node containment was not derivable. Under this ladder it is: a
    project is in a portfolio by name, and an epic carries the project it belongs
    to, so a grantor can no longer hand down authority it does not hold.
    """
    outer_members = set(split_scope(outer))
    inner_members = split_scope(inner)
    if not outer_members or not inner_members:
        return False
    if set(inner_members) == outer_members:
        return False  # a peer crown, not a subordinate one

    if len(inner_members) > 1:
        return set(inner_members) < outer_members

    name = inner_members[0]
    if name in outer_members:
        return True
    entry = _graph_entry(name)
    return bool(entry and entry.get("project") in outer_members)


def crown_validation_error(level: Any, scope: Any) -> Optional[str]:
    """Why ``(level, scope)`` is not a stampable crown, or None when it is.

    The last gate before the shared store, and it lives away from the CLI on
    purpose: ``--crown`` reaches a parser, but ``dispatch_spawn`` and
    ``dispatch_spawn_pane`` take ``(level, scope)`` straight from in-process
    callers. A value that skipped this lands in the registry, and a negative or
    boolean level cannot deserialize into the Rust row's ``Option<u32>``, so one
    bad write breaks reads for every reader, not just the caller.

    Both-None is the uncrowned spawn and passes. Anything else must name BOTH
    halves: a level with no scope stamps a crown that rules nothing and that the
    one-live-crown guard, which keys on scope, can never see or supersede.
    """
    if level is None and scope is None:
        return None
    if level is None or scope is None:
        return f"a crown needs both level and scope; got level={level!r} scope={scope!r}"
    # bool before int: `True` is an int subclass and would serialize as JSON
    # `true`, failing the same u32 decode a negative does.
    if isinstance(level, bool) or not isinstance(level, int):
        return f"crown level must be an int 0..{MAX_CROWN_LEVEL}; got {level!r}"
    if not 0 <= level <= MAX_CROWN_LEVEL:
        return (
            f"crown level must be 0..{MAX_CROWN_LEVEL} "
            f"(0 several projects, 1 one project, 2 one epic); got {level}"
        )
    if not isinstance(scope, str) or not scope.strip():
        return f"crown scope must be a nonblank id; got {scope!r}"
    members = split_scope(scope)
    if canonical_scope(members) != scope:
        return (
            "crown scope must be canonical (sorted, deduped, no blank members); "
            f"got {scope!r}, want {canonical_scope(members)!r}"
        )
    if len(members) > 1 and level != 0:
        return f"a scope naming {len(members)} projects is level 0, not {level}"
    return None

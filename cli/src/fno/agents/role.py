"""The role vocabulary: what a scope is, what rung it implies, and what a
grantor may hand down. ``registry`` owns the three row fields; this module owns
their meaning. Its one file write is the role journal: the events a grant,
a vacate, or a spawn-time handoff leaves in ``~/.fno/events.jsonl``.

THE LADDER IS THREE RUNGS, EACH A FACT ABOUT THE SCOPE:

    0   several projects   scope names 2+ config projects (a portfolio)
    1   one project        scope names one config project
    2   a set of epics     every scope names a backlog node with type == "epic"

No rung for an implementer: a non-epic node is work, not territory, so a role
aimed at one is REFUSED - which is why no caller passes a level;
``derive_role_level`` reads the rung off the scope, and a scope naming no
territory has none to read. Callers hand-type no altitude (the old surface let a
backwards ladder - 0 is the TOP - silently mint wrong authority).

CANONICALIZATION CONTRACT: the one-live-role guard compares stored scopes by
exact equality, so ``resolve_role`` guarantees every stamp is canonical
(aliases resolved, members sorted and deduped). The only gap is a pre-redesign
``--promote level=,scope=`` row storing a raw alias spelling - that path is
deleted and was unreleased; re-role such a row rather than merging spellings.
"""
from __future__ import annotations

import os
from dataclasses import asdict, replace
from typing import Any, Optional

#: The bottom rung. Kept as a bound rather than a magic number so the stored
#: value stays far inside the Rust row's ``role_level: Option<u32>``.
MAX_ROLE_LEVEL = 2

#: Separator for a level-0 scope. The registry field is a single string - the
#: Rust side only custodies and displays it (``role_scope.as_deref()`` in
#: client.rs is its one read) - so a portfolio scope is stored as its member
#: project names joined by this, rather than costing a schema version bump and
#: ~90 mechanical edits across two crates for a field the daemon never reasons
#: about.
SCOPE_SEPARATOR = ","


class RoleScopeError(ValueError):
    """A scope that names no territory this ladder recognizes."""


#: Sentinel for "the caller carries an agent identity but the registry could not
#: be read to resolve it to a row." Distinct from ``None`` (an attended human
#: with no identity at all), so an unreadable registry fails CLOSED in
#: :func:`grant_error` instead of authorizing like a human. An authorization
#: check that cannot read the registry does not know the caller is human, so it
#: must refuse rather than assume the most privileged answer.
REGISTRY_UNREADABLE: Any = object()

#: Sentinel for "the caller carries an agent identity but no registry row
#: matches it": distinct from ``None`` (attended human) and
#: :data:`REGISTRY_UNREADABLE` (read failed). The registry WAS read, the caller
#: claims to be an agent, but is not joined - it holds no verified authority,
#: so :func:`grant_error` refuses with the heal rather than authorize. Covers
#: the clean-miss path ``_find_by_session`` answers with ``None``, not raising.
AGENT_UNREGISTERED: Any = object()


def calling_agent_row():
    """The calling session's registry row; :func:`grant_error` treats each
    outcome differently:

    - ``None`` - an attended human. A human may grant any scope.
    - an ``AgentEntry`` - a joined agent; :func:`grant_error` checks its role.
    - :data:`REGISTRY_UNREADABLE` - the caller HAS an agent identity but the
      registry could not be read. Flattening this to ``None`` would let a read
      failure promote any worker to human authority - fail-open on "you cannot
      hand down authority you do not hold" - so it surfaces as a sentinel.
    - :data:`AGENT_UNREGISTERED` - identity present, registry read, but no row
      matches (a just-spawned worker, a session without ``/fno-me``). The clean
      miss flows out as ``None`` without this sentinel, the same fail-open one
      branch over; surfaces so :func:`grant_error` refuses with the heal.

    Resolved the same way ``fno whoami`` does, so "who am I" has one answer.
    """
    from fno.agents.registry import load_registry
    from fno.agents.whoami import _find_by_session
    from fno.agents.self_stamp import resolve_self_identity

    ident = resolve_self_identity()
    if ident.disposition in {
        "ambiguous",
        "invalid",
        "contradiction",
        "name_only",
    } or (ident.disposition == "canonical" and not ident.session_id):
        return REGISTRY_UNREADABLE
    if not ident.session_id or not ident.harness:
        if (os.environ.get("FNO_AGENT_SELF") or "").strip():
            return REGISTRY_UNREADABLE
        return None
    try:
        row = _find_by_session(load_registry(), ident.session_id, ident.harness)
    except Exception:
        # Surface the failure, not flatten it: None means "attended human" and
        # would authorize any grant. The caller refuses on the sentinel instead.
        return REGISTRY_UNREADABLE
    if row is None:
        # Identity present, registry read, no match: an agent the registry does
        # not know yet. _find_by_session returns None on a clean miss WITHOUT
        # raising, so this never reached the except above - returning None here
        # would authorize like a human, the same fail-open one branch over.
        return AGENT_UNREGISTERED
    return row


def role_reading(row) -> Optional[dict[str, Any]]:
    """The one rendering of a role, or ``None`` when ``row`` holds none.

    Reads ``row.role_label`` (``registry.AgentEntry.role_label``) rather than
    re-deriving the "L{level} {scope}" string, so ``fno whoami``, ``fno agents
    whoami`` and ``fno agents top`` cannot drift into three different renderings
    of the same fact. Safe on the :func:`calling_agent_row` sentinels and on
    ``None``: neither carries ``role_label``, so ``getattr`` answers ``None``
    rather than raising.
    """
    label = getattr(row, "role_label", None)
    if label is None:
        return None
    grantor = getattr(row, "role_grantor", None) or "human"
    level = getattr(row, "role_level", None)
    scope = getattr(row, "role_scope", None)
    return {
        "level": level,
        "scope": scope,
        "grantor": grantor,
        "label": label,
        "text": f"{label} (by {grantor})",
    }


def current_role() -> Optional[dict[str, Any]]:
    """This session's role reading, or ``None`` - never raises.

    ``fno whoami`` must render byte-for-byte unchanged for an attended human
    shell (AC8-EDGE), so any failure resolving the caller's identity or
    registry row degrades to "no role" rather than surfacing.
    """
    try:
        return role_reading(calling_agent_row())
    except Exception:
        return None


def canonical_scope(scopes: list[str]) -> str:
    """The stored form: one name, or sorted unique members joined.

    Canonical on purpose. The one-live-role guard compares scopes by equality,
    so ``{web,etl}`` and ``{etl,web}`` must reduce to the SAME string or a second
    role over one portfolio slips through spelled in a different order.
    """
    return SCOPE_SEPARATOR.join(sorted({s.strip() for s in scopes if s.strip()}))


def split_scope(scope: Optional[str]) -> list[str]:
    """The members of a stored scope; a single-name scope yields one element.

    Guards on type, not just truthiness: a corrupted registry row can carry a
    non-string ``role_scope`` (e.g. a stray int from a hand-edit), and
    ``5.split(...)`` would raise past every caller here, including
    ``fno agents team``, which promises to exit 0 on a read.
    """
    if not scope or not isinstance(scope, str):
        return []
    return [s for s in (part.strip() for part in scope.split(SCOPE_SEPARATOR)) if s]


def _graph_entry(node_id: str) -> Optional[dict]:
    """The node record for ``node_id``, or None. Reads footnote-minted
    metadata (type/project) through the guarded default-backend reader: the
    role ladder's epic rung classifies nodes footnote's own verbs typed, so
    an external selection degrades to None - the same path as an unknown id
    (configured projects, the top rungs, still resolve from settings)."""
    from fno.tracker.metadata import ExternalMetadataUnavailable, read_entries

    try:
        entries = read_entries("agents.role")
    except ExternalMetadataUnavailable:
        return None
    return next(
        (
            e
            for e in entries
            if isinstance(e, dict) and e.get("id") == node_id
        ),
        None,
    )


def _graph_index() -> Optional[dict[str, dict]]:
    """Return the readable role graph as an id index, or ``None``.

    ONE parse serves the availability probe and every per-row node lookup its
    callers make. ``read_entries`` raises ``ExternalMetadataUnavailable`` under
    an external tracker backend, so ``None`` is a distinct answer from an empty
    index: without it a containment or agreement check would silently degrade to
    "not contained" / "not in the graph" on a machine it never read.
    """
    from fno.tracker.metadata import read_entries

    try:
        entries = read_entries("agents.role")
    except Exception:
        return None
    by_id: dict[str, dict] = {}
    for entry in entries:
        node_id = entry.get("id") if isinstance(entry, dict) else None
        if isinstance(node_id, str) and node_id:
            by_id[node_id] = entry
    return by_id


def _canonical_project(name: str) -> Optional[str]:
    """``name`` resolved to its CANONICAL project name, or None if it is not a
    project. Projects are declared in config
    (``work.workspaces.<ws>.projects[].name``) and need no backlog node - which is
    why the top two rungs cannot be derived from the graph alone.

    Returning the canonical name rather than a bool is load-bearing: the resolver
    accepts a project's ``short_name`` alias too, so a caller can spell one
    territory two ways. Storing the raw spelling would let `alpha` and its alias
    `a` sit as two live roles over one project (the guard compares scopes by
    equality), and naming both at once would read as a two-project portfolio
    instead of one project named twice.
    """
    try:
        from fno.projects.resolve import resolve_project_name

        return resolve_project_name(name)
    except Exception:
        return None


def _epic_or_refuse(raw: str, *, graph_entry=None) -> str:
    """``raw`` as a backlog epic id, or the refusal that names why not.

    One message set for the single-epic and epic-set paths alike: a member of
    a set that is a typo or a non-epic node gets the same named remedy it
    would get alone. ``graph_entry`` lets a set-resolving caller pass one
    ``_graph_index`` read so members do not pay a full graph parse apiece.
    """
    entry = (graph_entry or _graph_entry)(raw)
    if entry is None:
        raise RoleScopeError(
            f"{raw!r} is neither a configured project nor a backlog node; "
            "nothing to term over (check for a typo)"
        )
    if entry.get("type") != "epic":
        raise RoleScopeError(
            f"{raw!r} is a {entry.get('type') or 'node'}, not an epic. "
            "Implementers get no roles - a single node is work, not a territory. "
            f"If {raw} IS meant to be an epic: fno backlog update {raw} --type epic. "
            "Otherwise role the epic above it, or its project."
        )
    return raw


def _member_rung(raw: str, canon: Optional[str], *, graph_entry=None) -> str:
    """One scope member spelled with its rung, for a mixed-rung refusal."""
    if canon:
        return f"{canon} (a project)"
    entry = (graph_entry or _graph_entry)(raw)
    if entry is not None and entry.get("type") == "epic":
        return f"{raw} (an epic)"
    if entry is not None:
        return f"{raw} (a {entry.get('type') or 'node'}, not an epic)"
    return f"{raw} (not a configured project or a known node)"


def resolve_role(scopes: list[str], *, graph_entry=None) -> "tuple[int, str]":
    """``scopes`` -> the (rung, stored scope) they imply, both derived together.

    ONE call rather than a derive-then-encode pair, because the two answers must
    agree and a caller holding them separately can mismatch them: the rung is
    counted over CANONICAL project names, so a scope encoded from the raw
    spelling would be a different territory than the one that was counted.

    Raises :class:`RoleScopeError` when the scopes name no territory - the
    refusal that keeps implementers unpromoted. Mixed scopes are refused rather
    than coerced: a portfolio is projects and rung 2 is a set of epics, so
    naming a project and an epic together is a mistake about what is being
    ruled, not a role over both.
    """
    members = split_scope(canonical_scope(scopes))
    if not members:
        raise RoleScopeError("a role needs a scope: name an epic or a project")

    # Resolve aliases FIRST, then dedupe: `-k alpha -k a` is one project spelled
    # twice, not a two-project portfolio.
    resolved = [(m, _canonical_project(m)) for m in members]
    projects = [canon for _, canon in resolved if canon]
    non_projects = [raw for raw, canon in resolved if not canon]

    if len(members) > 1:
        if not non_projects:
            scope = canonical_scope(projects)
            # One project named twice collapses to one project, not a portfolio.
            return (0 if len(split_scope(scope)) > 1 else 1), scope
        # ONE graph parse serves every per-member refusal below; a graph this
        # rung could not read answers None, and the per-call fallback keeps
        # the single-read behavior for that machine.
        by_id = None if graph_entry else _graph_index()
        entry_of = graph_entry or (_graph_entry if by_id is None else by_id.get)
        if not projects:
            # Rung 2 rules a SET of epics, stored with the same separator: a
            # lead over two epics at once is one role, not a failed portfolio.
            return 2, canonical_scope(
                [_epic_or_refuse(raw, graph_entry=entry_of) for raw in non_projects]
            )
        raise RoleScopeError(
            "a multi-scope role rules PROJECTS or EPICS, never both at once: "
            f"{', '.join(_member_rung(raw, canon, graph_entry=entry_of) for raw, canon in resolved)}. "
            "Name projects only (a portfolio) or epics only (a set)."
        )

    raw = members[0]
    if projects:
        return 1, projects[0]
    return 2, _epic_or_refuse(raw, graph_entry=graph_entry)


def derive_role_level(scopes: list[str]) -> int:
    """The rung alone. Thin wrapper over :func:`resolve_role` for callers that
    only need the altitude."""
    return resolve_role(scopes)[0]


def scope_contains(
    outer: Optional[str],
    inner: Optional[str],
    *,
    graph_entry=None,
) -> bool:
    """Does a role over ``outer`` strictly contain one over ``inner``?

    Real containment, not the honor system it replaces. The old rule could only
    check that two scopes DIFFERED, because scopes were opaque ids and
    project>epic>node containment was not derivable. Under this ladder it is: a
    project is in a portfolio by name, and an epic carries the project it belongs
    to, so a grantor can no longer hand down authority it does not hold.

    ``graph_entry`` overrides the per-call graph read (an ``id -> entry``
    callable). Omitted, the FIRST call resolves ``_graph_index`` once and
    hands it down the set-membership recursion: a five-member grant costs
    one graph parse, not five.
    """
    if graph_entry is None:
        by_id = _graph_index()
        if by_id is not None:
            graph_entry = by_id.get
    outer_members = _canonical_members(outer)
    inner_members = _canonical_members(inner)
    if not outer_members or not inner_members:
        return False
    if inner_members == outer_members:
        return False  # a peer role, not a subordinate one

    if len(inner_members) > 1:
        if inner_members < outer_members:
            return True  # a portfolio inside a wider portfolio
        # The other multi-member inner is a rung-2 epic set (mixed scopes are
        # refused at resolve time). Epic ids never equal project names, so the
        # subset test above cannot place it: a set is contained exactly when
        # EVERY member is contained, member by member.
        return all(
            scope_contains(outer, m, graph_entry=graph_entry)
            for m in split_scope(inner)
        )

    name = next(iter(inner_members))
    if name in outer_members:
        return True
    entry = (graph_entry or _graph_entry)(name)
    if not entry:
        return False
    # Canonicalize the entry's project before the comparison: graph intake stores
    # the project field RAW (the short_name alias a node was filed under), while
    # outer_members is canonicalized - a raw 'a' would never match a canonical
    # 'alpha', so a lead over 'alpha' would be falsely refused an epic filed as
    # 'a'. Same alias-normalization the _canon helper applies to the scopes.
    raw_proj = entry.get("project")
    if not raw_proj:
        return False
    proj = _canonical_project(raw_proj) or raw_proj
    return proj in outer_members


def grant_error(
    requested_scope: str,
    caller_row,
    *,
    allow_terminal_recovery: bool = False,
    allow_succession: bool = False,
) -> Optional[str]:
    """Why this caller may not grant a role over ``requested_scope``, or None.

    The rule the docs have always stated and nothing was enforcing after the
    promotion verb was deleted: you cannot hand down authority you do not hold.
    ``scope_contains`` existed but had no caller, so any spawned worker could
    mint portfolio-level authority for its child.

    Two grantor classes, matching what the verb used to accept, plus three
    failure modes that must fail closed:

    - **an attended human** (``caller_row`` is None - a shell with no agent
      identity in its environment) may grant any scope; there is nobody above a
      human to check against.
    - **an agent** must hold a live role that STRICTLY contains the request.
      Unpromoted means it holds nothing to hand down. An equal scope is a
      transfer, not a grant, and is refused unless the caller carries explicit
      succession intent.
    - **an unreadable registry** (``caller_row`` is
      :data:`REGISTRY_UNREADABLE`) is refused outright. The check cannot verify
      the caller holds anything, so it must not fall back to the most privileged
      answer (human); a registry read failure does not make the caller human.
    - **an unregistered agent** (``caller_row`` is :data:`AGENT_UNREGISTERED`)
      has an identity but no registry row. The registry was read and the caller
      is not in it, so it is an agent holding no verified authority, not a human;
      refused with the heal (run ``/fno-me`` to join, or wait for the row) rather
      than authorized.
    - **a terminal grantor** (its STORED status is exited/orphaned/failed/
      permanent_dead) cannot grant a role: authority it can no longer exercise
      is not authority to hand down.

    ``allow_terminal_recovery`` admits ONLY a request for the SAME territory the
    terminal row already holds - handing the whole thing to a successor. A strict
    subset is still refused there, because narrowing from a dead grantor leaves
    the remainder ruled by nobody live. ``allow_succession`` is the separate
    live-holder exemption for an intentional same-scope transfer.
    """
    if caller_row is None:
        return None
    if caller_row is REGISTRY_UNREADABLE:
        # A shell that once hosted a worker keeps its FNO_AGENT_SELF. The
        # registry is not the problem (2026-09-27: role refusals blamed a
        # healthy registry), so name the stray variable and its heal instead.
        stray = (os.environ.get("FNO_AGENT_SELF") or "").strip()
        if stray:
            from fno.agents.self_stamp import resolve_self_identity

            if not resolve_self_identity().session_id:
                return (
                    f"cannot verify the grantor's authority: this shell carries "
                    f"FNO_AGENT_SELF={stray} with no harness session, so it reads "
                    "as an agent fno cannot resolve, not an attended human. A "
                    "shell that once hosted a worker keeps that variable. If you "
                    "are the human at this shell, rerun as: "
                    "env -u FNO_AGENT_SELF fno agents org promote <args>."
                )
        return (
            "cannot verify the grantor's authority: the agent registry could not "
            "be read, so this session is treated as an agent whose authority is "
            "unknown, not as an attended human. A role may not be granted when "
            "the grantor cannot be checked; retry, or spawn from an attended shell."
        )
    if caller_row is AGENT_UNREGISTERED:
        return (
            "cannot verify the grantor's authority: this session carries an agent "
            "identity but has no registry row (it spawned before its row landed, "
            "or has not run /fno-me), so it is an agent with no verified authority, "
            "not an attended human. Run /fno-me to join or wait for the row, then "
            "retry; or spawn from an attended shell."
        )
    from fno.agents.registry import TERMINAL_STATUSES

    status = getattr(caller_row, "status", None)
    if status in TERMINAL_STATUSES:
        if allow_terminal_recovery and _same_territory(
            getattr(caller_row, "role_scope", None), requested_scope
        ):
            # A stale shell may still carry the identity of a terminal holder.
            # Spawn is the recovery path for that abandoned scope; it must not
            # require succession from a session that can no longer spawn.
            return None
        return (
            f"cannot verify the grantor's authority: its STORED status is "
            f"{status!r}, which is terminal, so it cannot grant a role. "
            "Re-register the grantor in its live session or use an attended shell."
        )
    holder = getattr(caller_row, "role_scope", None)
    if not holder:
        return (
            "a role is handed DOWN, and this session holds none: an unpromoted "
            "agent cannot grant one. Ask a lead whose scope contains "
            f"{requested_scope!r}, or spawn from an attended shell."
        )
    # SUCCESSION, not a grant. An equal scope is legal only when the spawn caller
    # explicitly names the transfer; otherwise the caller could strip itself by
    # accident while believing --promote was additive.
    if _same_territory(holder, requested_scope):
        if allow_succession:
            return None
        return (
            f"this session already holds its own role over {requested_scope!r}; "
            "a same-scope spawn is a transfer, not a grant. Re-run with "
            "`--hand-off`, or choose a different scope so this session keeps its role."
        )
    if not scope_contains(holder, requested_scope):
        return (
            f"this session's role over {holder!r} neither contains nor equals "
            f"{requested_scope!r}, so it cannot grant it. A grant must be a "
            "strict subset of what the grantor holds; an equal scope is allowed "
            "only as succession, which hands YOUR scope to a successor."
        )
    return None


def _canonical_members(scope: Optional[str]) -> set:
    """Alias-normalized member set of a stored scope; empty for None/blank.

    Shared by containment (``scope_contains``) and equality
    (``_same_territory``) on purpose: the strand scan relies on both
    answering from ONE normalization, so a copy that drifts would let a
    scope read as "same territory" to one and "strictly contained" to the
    other.
    """
    return {(_canonical_project(m) or m) for m in split_scope(scope)}


def _territory_key(scope: Optional[str]) -> frozenset[str]:
    """The hashable normalized territory used by all scope equality checks."""
    return frozenset(_canonical_members(scope))


def _same_territory(a: Optional[str], b: Optional[str]) -> bool:
    """Do two stored scopes name the same territory, aliases normalized?"""
    left, right = _territory_key(a), _territory_key(b)
    return bool(left) and left == right


def _territories_overlap(a: Optional[str], b: Optional[str]) -> bool:
    """Do two stored scopes share ANY territory, aliases normalized?

    The one-live-role scans key on this, not on equality: a stored set
    (a portfolio, or a rung-2 epic set) already rules each of its members,
    so a second role over any member alone is a double rule even though no
    two stored strings are equal.
    """
    left, right = _territory_key(a), _territory_key(b)
    return bool(left) and bool(right) and bool(left & right)


def _derived_level(scope: Optional[str]) -> Optional[int]:
    """The rung the SCOPE sits on, read off its members, never a stored number:
    all projects is 1/0 by count, none is an epic set (2), a mix is ``None`` -
    the undecidable case the rivalry guard fails closed on.
    """
    members = _canonical_members(scope)
    if not members:
        return None
    project_members = {m for m in members if _canonical_project(m)}
    if project_members and len(project_members) == len(members):
        return 1 if len(members) == 1 else 0
    if not project_members:
        return 2
    return None


def _role_rivals(
    a_scope: Optional[str],
    a_level: Optional[int],
    b_scope: Optional[str],
    b_level: Optional[int],
) -> bool:
    """Do two live roles double-rule territory, ladder-aware?

    The ladder's team is legitimate (a portfolio's team IS project leads),
    so rivalry is rung-scoped: same rung double-rules on any shared member,
    different rungs only on the same territory outright. Rungs derive from
    the members - a row stamped ``level=0`` over ``e-1,e-2`` is exactly how a
    bypass used to switch this guard off. Stored levels tie-break only when
    derivation cannot classify either side; otherwise overlap surfaces.
    """
    a_rung = _derived_level(a_scope)
    b_rung = _derived_level(b_scope)
    if a_rung is None and b_rung is None:
        if a_level is not None and b_level is not None and a_level != b_level:
            return _same_territory(a_scope, b_scope)
    elif a_rung is not None and b_rung is not None and a_rung != b_rung:
        return _same_territory(a_scope, b_scope)
    return _territories_overlap(a_scope, b_scope)


def role_scope_matches(held: Optional[str], requested: Optional[str]) -> bool:
    """Will the row-keyed lead readers accept a role over ``held`` for
    ``requested``? Territory equality, aliases normalized.

    NOT a containment check: grant answers "may this role grant that scope"
    with the ladder, while the readers answer a string question
    (``leads/{role_scope}.md`` built verbatim; ``done`` refuses on
    ``own != scope``), so a project role does NOT satisfy an epic manifest.
    A first cut reused ``scope_contains`` and silenced exactly the state the
    warning exists to make loud. Reuse the rule that matches the QUESTION.
    A blank scope on either side answers False.
    """
    if not held or not requested:
        return False
    return _same_territory(held, requested)


def role_answers_to(held: Optional[str], requested: Optional[str]) -> bool:
    """Does a live role over ``held`` answer when ``requested`` is addressed?

    Territory equality, plus one widening: a rung-2 epic set answers for any
    subset of its members. Not ``scope_contains``: a project or portfolio role
    never answers for a narrower scope, because its team may hold that role.
    """
    if role_scope_matches(held, requested):
        return True
    asked = _canonical_members(requested)
    return bool(asked) and _derived_level(held) == 2 and asked <= _canonical_members(held)


def resolve_to_lead(scope: str, *, registry_path=None) -> list[str]:
    """Every live row holding role ``scope`` right now, by name, sorted.

    Read at send time, never off a handle a peer learned while that handle was
    promoted; a pointer written at departure goes stale the second time the role
    moves. Empty is vacant, one is the holder, more is the split role
    ``fno agents team`` already reports. A rung-2 epic set answers for each
    of its members."""
    from fno.agents.registry import TERMINAL_STATUSES, load_registry

    rows = load_registry(path=registry_path) if registry_path else load_registry()
    return sorted(
        {
            row.name
            for row in rows
            if getattr(row, "role_level", None) is not None
            and role_answers_to(getattr(row, "role_scope", None), scope)
            and getattr(row, "status", None) not in TERMINAL_STATUSES
        }
    )


class RolePromotionError(RuntimeError):
    """An attended in-place grant that refused without changing the registry."""


def _locked_identity(rows, row) -> Any:
    """Find the row resolved BEFORE the lock by name AND session; no answer refuses."""
    from fno.agents.spawn_overlay_client import SpawnOverlayUnavailable, spawn_overlay_call
    ids = ("harness_session_id", "cc_session_id", "short_id")
    session = next((getattr(row, f) for f in ids if getattr(row, f)), None)
    try:
        answer = spawn_overlay_call({
            "kind": "role-identity",
            "rows": [asdict(row) for row in rows],
            "expect": {"name": row.name, "harness_session_id": session},
        })
    except SpawnOverlayUnavailable as exc:
        raise RolePromotionError(f"identity check unavailable ({exc})") from exc
    return rows[answer["index"]] if answer.get("matched") else None


def emit_role_vacated(
    *,
    scope: Optional[str],
    level: Optional[int],
    holder: Optional[str],
    holder_session: Optional[str],
    grantor: Optional[str],
    cause: str,
    successor: Optional[str] = None,
) -> None:
    """Journal one role leaving its holder, after the registry write commits.

    An departure and a role lost to a bug are indistinguishable from outside
    until one of these lines lands, so the team answers from the record, never
    from testimony.
    """
    from fno.agents import events

    events.emit(
        "agent_role_vacated", scope=scope, level=level, holder=holder,
        holder_session=holder_session, grantor=grantor, cause=cause,
        successor=successor,
    )


def settle_spawn_role(
    rows: list,
    *,
    scope: str,
    plan: dict,
    exclude_name: Optional[str] = None,
    successor: Optional[str] = None,
    successor_harness: Optional[str] = None,
    successor_session: Optional[str] = None,
    successor_cwd: Optional[str] = None,
    stamp: bool = False,
    level: Optional[int] = None,
    grantor: Optional[str] = None,
) -> "tuple[list, str, list]":
    """Apply Rust's lock-time row updates and return vacated rows for journaling.
    The native owner checks occupancy and the successor's carried identity; a missing
    or malformed answer declines without changing the caller's rows.
    """
    from fno.agents.spawn_overlay_client import SpawnOverlayUnavailable, spawn_overlay_call

    try:
        answer = spawn_overlay_call({
            "kind": "spawn-team", "op": "apply", "scope": scope, "exclude_name": exclude_name,
            "plan": plan, "successor": successor, "rows": [asdict(row) for row in rows],
            "successor_identity": {"harness": successor_harness, "session_id": successor_session, "cwd": successor_cwd},
            "stamp": stamp, "level": level, "grantor": grantor,
        })
        outcome = answer["outcome"]
        if outcome not in ("granted", "succeeded", "declined"):
            raise ValueError("invalid spawn-team outcome")
        vacated = [(replace(rows[int(i)], **fields), cause) for i, cause, fields in answer["vacated"]]
        updates = {int(i): replace(rows[int(i)], **fields) for i, fields in answer["updates"].items()}
    except (SpawnOverlayUnavailable, LookupError, TypeError, ValueError):
        return rows, "declined", []
    return [updates.get(i, row) for i, row in enumerate(rows)], outcome, vacated


def plan_spawn_role(
    scope: str,
    caller_row,
    succession: bool,
    exclude_name: Optional[str] = None,
    proposed_name: Optional[str] = None,
) -> "tuple[Optional[str], Optional[dict]]":
    """Check authority, then ask team_settle for a pre-launch occupancy plan.
    Return the refusal and native answer. A failed read refuses before launch;
    settlement rechecks that plan under the registry lock.
    """
    grant_problem = grant_error(
        scope, caller_row, allow_terminal_recovery=True, allow_succession=succession,
    )
    if grant_problem is not None:
        return grant_problem, None
    from fno.agents.registry import load_registry
    from fno.agents.spawn_overlay_client import SpawnOverlayUnavailable, spawn_overlay_call

    caller_name = getattr(caller_row, "name", None)
    caller = {"kind": "agent", "name": caller_name} if caller_name else {"kind": "human"}
    try:
        rows = load_registry()
    except Exception as exc:
        return f"cannot decide role occupancy: the registry could not be read ({exc})", None
    payload = {
        "kind": "role-settle",
        "scope": scope,
        "succession": succession,
        "proposed_name": proposed_name,
        "caller": caller,
        "exclude_name": exclude_name,
        "rows": [asdict(row) for row in rows],
    }
    try:
        answer = spawn_overlay_call(payload)
    except SpawnOverlayUnavailable as exc:
        return f"cannot decide role occupancy: {exc}", None
    return answer.get("refusal"), answer


def _widen_answer(scope: str, caller, target_name: str) -> dict:
    """Rust's role-widen answer; a missing/old binary answers ``{}`` (fails closed)."""
    from fno.agents.spawn_overlay_client import SpawnOverlayUnavailable, spawn_overlay_call

    by_id = _graph_index() or {}
    fields = ("name", "status", "role_scope", "role_grantor",
              "harness_session_id", "cc_session_id", "pid")
    member_ids = dict.fromkeys(split_scope(scope) + split_scope(getattr(caller, "role_scope", None)))
    try:
        return spawn_overlay_call({
            "kind": "role-widen",
            "requested": scope,
            "target": target_name,
            "caller": {f: getattr(caller, f, None) for f in fields},
            "members": [by_id.get(m) for m in member_ids],
        })
    except SpawnOverlayUnavailable:
        return {}


def arm_promoted_missions(scope: Optional[str]) -> Optional[list[str]]:
    """Set mission_active on every open epic in a promoted scope. Operator rule:
    an epic with an owner is a mission, or no drain loop can see its children.
    Returns the epic ids newly armed, or None when the graph write failed."""
    armed: list[str] = []
    try:
        from fno.backlog.advance import (
            EVENT_MISSION_ACTIVATED,
            _emit,
            _set_mission_active,
        )
        from fno.graph.cli import _container_ids

        # ONE graph parse serves every member: a graph this rung could not
        # read answers None, and the per-call fallback keeps that machine
        # working one member at a time.
        by_id = _graph_index()
        entry_of = _graph_entry if by_id is None else by_id.get
        containers = _container_ids(list(by_id.values())) if by_id else set()
        for member in split_scope(scope):
            entry = entry_of(member) or {}
            if entry.get("type") != "epic":
                continue
            if entry.get("status") in ("done", "superseded"):
                continue
            if member not in containers:
                # advance_epic refuses a childless epic as not-a-container and
                # leaves the flag standing, which the drain then polls forever.
                # The dispatch lever arms it once children exist.
                continue
            if _set_mission_active(member, True):
                _emit(
                    EVENT_MISSION_ACTIVATED,
                    {"epic_id": member, "source": "role"},
                    None,
                )
                armed.append(member)
    except Exception as exc:  # noqa: BLE001 - the role already committed
        import sys

        print(
            f"role: WARNING: mission arming failed for scope {scope!r} ({exc}); "
            "arm it with: fno backlog advance --epic <id>",
            file=sys.stderr,
        )
        return None
    return armed


def journal_spawn_role(outcome: Optional[str], vacated: list, *, name, level, scope, grantor) -> None:
    """Journal one committed spawn write: a vacate line per cleared holder, one
    reown line per team child that followed the role, plus the grant line."""
    from fno.agents import events
    from fno.agents.spawn_overlay_client import spawn_overlay_call

    answer = spawn_overlay_call({
        "kind": "spawn-team", "op": "journal", "outcome": outcome,
        "vacated": [(asdict(row), cause) for row, cause in vacated],
        "name": name, "level": level, "scope": scope, "grantor": grantor,
    })
    for event in answer["events"]:
        events.emit(event["kind"], **event["data"])
    if answer["arm_missions"]:
        arm_promoted_missions(scope)


def reclaim_role(handle: Optional[str] = None) -> dict[str, Any]:
    """Return a transferred role to its recorded grantor.

    Reclaim is a registry transfer, not a spawn: the current holder is cleared
    and the live row named by ``role_grantor`` receives the same territory and
    level in one locked write. A caller may identify the current holder by
    ``handle`` from an attended shell; without one, the current session must be
    the holder. The grantor is treated as an opaque registry identity and is
    resolved against every session-id field the registry accepts.
    """
    from fno.agents.registry import (
        AgentResolutionError,
        TERMINAL_STATUSES,
        resolve_agent,
        update_registry,
    )

    caller = calling_agent_row()
    if handle:
        # The handle form is the attended operator's reach, and only that: a
        # registered agent that learns a peer's handle must not strip that
        # peer's role by name. The no-handle form below is the one agent
        # path, and it only ever expires the caller's own role. The
        # unreadable/unregistered sentinels refuse too - an identity the
        # registry cannot resolve to "attended human" gets no handle reach.
        if caller is not None:
            raise RolePromotionError(
                "reclaim by handle is an attended-shell action: run it "
                "without --handle inside the successor session, or from a shell "
                "holding no agent identity"
            )
        try:
            holder_snapshot = resolve_agent(handle).entry
        except AgentResolutionError as exc:
            raise RolePromotionError(str(exc)) from exc
    else:
        holder_snapshot = caller
    if holder_snapshot in (None, REGISTRY_UNREADABLE, AGENT_UNREGISTERED):
        raise RolePromotionError(
            "reclaim needs the current promoted holder: run it inside the successor "
            "session or pass that registered handle from an attended shell"
        )
    scope = getattr(holder_snapshot, "role_scope", None)
    level = getattr(holder_snapshot, "role_level", None)
    grantor_key = getattr(holder_snapshot, "role_grantor", None)
    if level is None or not isinstance(scope, str) or not scope.strip():
        raise RolePromotionError("this session holds no role to reclaim")
    if not grantor_key or grantor_key == "human":
        raise RolePromotionError(
            f"role over {scope!r} has no registry grantor to reclaim; "
            "a human grant cannot be returned automatically"
        )

    holder_name = holder_snapshot.name
    holder_session = (
        getattr(holder_snapshot, "harness_session_id", None)
        or getattr(holder_snapshot, "cc_session_id", None)
        or getattr(holder_snapshot, "short_id", None)
        or holder_name
    )
    receipt: dict[str, Any] = {}

    def _reclaim(rows: list) -> list:
        holder = _locked_identity(rows, holder_snapshot)
        if holder is None or holder.status in TERMINAL_STATUSES:
            raise RolePromotionError(
                f"cannot reclaim {scope!r}: current holder {holder_name!r} "
                "is no longer live"
            )
        if holder.role_scope != scope or holder.role_level != level:
            raise RolePromotionError(
                f"cannot reclaim {scope!r}: holder {holder_name!r} changed "
                "its role while reclaim was in flight"
            )
        target = next(
            (
                row
                for row in rows
                if row.name != holder_name
                and grantor_key
                in {
                    row.name,
                    getattr(row, "harness_session_id", None),
                    getattr(row, "cc_session_id", None),
                    getattr(row, "short_id", None),
                }
            ),
            None,
        )
        if target is None:
            raise RolePromotionError(
                f"cannot reclaim {scope!r}: recorded grantor {grantor_key!r} "
                "has no registry row; leave the role in place and re-register "
                "the grantor before retrying"
            )
        if target.status in TERMINAL_STATUSES:
            raise RolePromotionError(
                f"cannot reclaim {scope!r}: recorded grantor {target.name!r} "
                f"is terminal ({target.status}); a role cannot return to a dead row"
            )
        if target.role_scope is not None or target.role_level is not None:
            raise RolePromotionError(
                f"cannot reclaim {scope!r}: grantor {target.name!r} already "
                "holds a role; refusing to replace unrelated authority"
            )
        other = next(
            (
                row
                for row in rows
                if row.name not in {holder_name, target.name}
                and row.status not in TERMINAL_STATUSES
                and _role_rivals(row.role_scope, row.role_level, scope, level)
            ),
            None,
        )
        if other is not None:
            raise RolePromotionError(
                f"cannot reclaim {scope!r}: live row {other.name!r} already "
                f"holds overlapping territory ({other.role_scope!r})"
            )

        returned_by = (
            getattr(holder, "harness_session_id", None)
            or getattr(holder, "cc_session_id", None)
            or getattr(holder, "short_id", None)
            or holder.name
        )
        armed = False
        unarmed_reason = ""
        try:
            from fno.lead.state import arm_lead_manifest

            armed = (
                arm_lead_manifest(
                    scope,
                    getattr(target, "harness_session_id", None) or "",
                    owner_cwd=getattr(target, "cwd", None),
                    role_level=level,
                    role_scope=scope,
                    role_grantor=returned_by,
                    model=getattr(target, "requested_model", None),
                    harness=getattr(target, "harness", None),
                )
                is not None
            )
        except (OSError, ValueError) as exc:
            unarmed_reason = str(exc)
        for index, row in enumerate(rows):
            if row.name == holder_name:
                rows[index] = replace(
                    row,
                    role_level=None,
                    role_scope=None,
                    role_grantor=None,
                )
            elif row.name == target.name:
                rows[index] = replace(
                    row,
                    role_level=level,
                    role_scope=scope,
                    role_grantor=returned_by,
                )
        receipt.update(
            reclaimed=target.name,
            from_holder=holder_name,
            level=level,
            scope=scope,
            grantor=returned_by,
            lead_loop_armed=armed,
        )
        if unarmed_reason:
            receipt["lead_loop_unarmed_reason"] = unarmed_reason
        return rows

    update_registry(_reclaim)
    from fno.lead.state import remove_lead_manifest

    remove_lead_manifest(
        scope,
        owner_cwd=getattr(holder_snapshot, "cwd", None),
        expected_harness_session_id=holder_session,
    )
    return receipt


def promote_existing_session(handle: str, scopes: list[str]) -> dict[str, Any]:
    """Grant a role to one existing row from an attended human shell, or from
    an agent whose own role strictly contains the requested scope.

    Same-scope succession stays on ``spawn --promote``; this function closes the
    human workflow and the in-place re-scope of a live subordinate, where the
    useful target session already exists.

    A row that already holds a role is re-scoped, not refused: the new
    territory replaces the old in the one write below, and the receipt reports
    what was vacated. The live-holder check is the guard that matters here - it
    is what keeps two rows from ruling the same territory.
    """
    try:
        level, scope = resolve_role(scopes)
    except RoleScopeError as exc:
        raise RolePromotionError(str(exc)) from exc

    # Resolved before update_registry, never inside _stamp: this reads the
    # registry itself, and the closure runs under its lock.
    from fno.agents.registry import AgentResolutionError, TERMINAL_STATUSES, resolve_agent, update_registry
    caller = calling_agent_row()
    try:
        resolved_target = resolve_agent(handle).entry
    except AgentResolutionError as exc:
        raise RolePromotionError(
            f"{exc}. `fno agents list` shows every handle you can role."
        ) from exc
    target_name = resolved_target.name
    denial = grant_error(scope, caller, allow_succession=True)
    widen = _widen_answer(scope, caller, target_name) if caller is not None else {}
    if widen.get("widen") is not True and (denial is not None or (caller is not None and target_name == caller.name)):
        raise RolePromotionError(" ".join(filter(None, (denial, widen.get("hint"))))
            or "role self-edit refused: the role-widen answer was unavailable")
    if widen.get("widen") is True and target_name == caller.name and not widen.get("grantor"):
        raise RolePromotionError("role self-edit admitted with no grantor in the answer; update the fno-agents binary (`fno doctor update --rust`)")
    grantor_name = "human" if caller is None else caller.name
    recorded_grantor = widen.get("grantor") or grantor_name
    # `grant_error` blesses an equal scope because SPAWN succession vacates the
    # caller and stamps the successor in one write; this path only stamps the target,
    # so letting it through would leave two live roles and the holder scan
    # below would refuse naming the caller's OWN row. Refuse here, where the
    # remedy is reachable.
    if caller is not None and _same_territory(
        getattr(caller, "role_scope", None), scope
    ):
        raise RolePromotionError(
            f"refusing to role {handle!r}: {scope!r} is your OWN scope, and "
            "this verb only stamps the target, so it cannot hand a role over. "
            "Succession runs through `fno agents spawn --promote <scope> "
            "--hand-off` instead, which vacates you and stamps the new holder "
            "in a single registry write, so the scope is never doubly ruled "
            "and never briefly unruled."
        )
    # The authority check ran OUTSIDE the lock, so the grantor's role can move
    # before the stamp. Re-running grant_error under the lock would put graph
    # I/O on the lock, so carry the granted scope and re-assert it under the
    # lock as a plain compare. Fails closed: a grantor whose role moved
    # mid-call cannot grant what it no longer holds.
    granting_scope = None if caller is None else getattr(caller, "role_scope", None)

    receipt: dict[str, Any] = {}
    vacated_manifest_owner = ""
    vacated_owner_cwd = ""

    def _stamp(rows: list) -> list:
        nonlocal vacated_manifest_owner, vacated_owner_cwd
        if caller is not None:
            # The grantor re-asserted under the lock by name AND session, the
            # same pair match the stamp below uses; a rebound grantor name
            # cannot grant what the calling session no longer holds.
            live_caller = _locked_identity(rows, caller)
            if live_caller is not None and live_caller.status in TERMINAL_STATUSES:
                raise RolePromotionError(
                    f"refusing to role {target_name!r}: the grantor's STORED "
                    f"status is {live_caller.status!r}, which is terminal, so "
                    "authority ended before the registry write. Re-register the "
                    "grantor in its live session or use an attended shell."
                )
            live_scope = None if live_caller is None else live_caller.role_scope
            if not _same_territory(live_scope, granting_scope):
                raise RolePromotionError(
                    f"refusing to role {target_name!r}: this session's own role "
                    f"moved from {granting_scope!r} to {live_scope!r} while the "
                    "grant was in flight, so the authority it was checked against "
                    "no longer holds. Re-read your role with `fno agents team`, "
                    "then retry if it still contains the scope."
                )
        target = _locked_identity(rows, resolved_target)
        if target is None:
            raise RolePromotionError(
                f"no agent matching {handle!r}; the target disappeared before the "
                "grant committed. `fno agents list` shows the handles you can role."
            )
        if target.status in TERMINAL_STATUSES:
            # Name the field and a remedy that cannot contradict the refusal:
            # `fno agents list` renders this STORED status beside a
            # freshly-computed `live_status`, and pointing the caller there (as
            # this used to) shows a column saying the row is live.
            raise RolePromotionError(
                f"refusing to role {target.name!r}: its STORED status is "
                f"{target.status!r}, which is terminal. That is a recorded "
                "snapshot, not a live probe, so it can be stale for a session "
                "that is still running. Two ways out:\n"
                "  target is alive    run `fno agents register` IN the target "
                "session (it restamps the row idle in place), then retry\n"
                "  target really died `fno agents reconcile`, then role a live "
                "row instead; `fno agents list` shows stored status beside the "
                "computed live_status, and a disagreement means the stored one "
                "is stale"
            )

        # A row that already holds a role is re-scoped, not refused: the
        # replace() below overwrites the old territory in the same write that
        # stamps the new one, so the vacated scope frees atomically and no
        # reader ever sees two live roles or zero. Captured BEFORE the stamp
        # so the receipt can name what changed hands.
        vacated_scope = target.role_scope
        vacated_level = target.role_level
        vacated_manifest_owner = (
            target.harness_session_id or target.cc_session_id or target.short_id or ""
        )
        vacated_owner_cwd = target.cwd
        if (vacated_level is None) != (vacated_scope is None):
            # Half a role is unstampable by role_validation_error, so no
            # legal writer produces it; overwrite would erase the corruption
            # signal instead of surfacing it.
            raise RolePromotionError(
                f"refusing to role {target.name!r}: it holds half a role "
                f"(level={vacated_level!r}, scope={vacated_scope!r}), which "
                "no legal role produces. fno agents stop the row and "
                "fno agents rm it, then role the re-registered session."
            )

        # An agent grantor's own row legitimately overlaps the delegated scope
        # (grant_error verified a STRICT containment before the write), so the
        # caller is not a second ruler; every other RIVAL live row is. Rivalry
        # is ladder-aware, not bare overlap: a live portfolio over the scope's
        # project is the new lead's team, not a second ruler of it.
        delegating = {target.name} | ({grantor_name} if caller is not None else set())
        holder = next(
            (
                row
                for row in rows
                if row.name not in delegating
                and row.status not in TERMINAL_STATUSES
                and _role_rivals(row.role_scope, row.role_level, scope, level)
            ),
            None,
        )
        if holder is not None:
            raise RolePromotionError(
                f"refusing to role {target.name!r}: scope {scope!r} is already "
                f"held by live row {holder.name!r} (holding "
                f"{holder.role_scope!r}). Three ways out, cheapest "
                "first:\n"
                f"  re-scope the holder   fno agents org promote {holder.name} --scope "
                "<other territory>   (both sessions stay live; retry this "
                "command after)\n"
                "  holder looks dead     fno agents reconcile   (a row whose "
                "harness session is gone flips to orphaned, which frees the "
                "scope)\n"
                f"  end the holder        fno agents stop {holder.name}, then retry"
            )

        try:
            from fno.lead.state import arm_lead_manifest

            manifest_path = arm_lead_manifest(
                scope,
                target.harness_session_id or target.cc_session_id or target.short_id or "",
                owner_cwd=target.cwd,
                role_level=level,
                role_scope=scope,
                role_grantor=recorded_grantor,
                model=getattr(target, "requested_model", None),
                harness=target.harness,
            )
        except (OSError, ValueError) as exc:
            raise RolePromotionError(
                f"refusing to role {target.name!r}: lead manifest arming failed: {exc}"
            ) from exc

        for index, row in enumerate(rows):
            if row.name == target.name:
                rows[index] = replace(
                    row,
                    role_level=level,
                    role_scope=scope,
                    role_grantor=recorded_grantor,
                )
                break
        receipt.update(
            promoted=target.name,
            level=level,
            scope=scope,
            grantor=recorded_grantor,
            vacated_scope=vacated_scope,
            vacated_level=vacated_level,
            lead_loop_armed=manifest_path is not None,
        )
        return rows

    # The persisted rows ARE the lock-window snapshot the strand scan needs:
    # a post-release re-read could see a concurrent grant over the
    # just-freed scope and mislabel that successor as stranded.
    rows_after = update_registry(_stamp)
    # Clear the vacated manifest BEFORE arming missions: a role killed
    # between the registry commit and a slow arm would otherwise leave the
    # leftover on disk, listing as a phantom role.
    if receipt.get("vacated_scope") and not _same_territory(
        receipt["vacated_scope"], scope
    ):
        from fno.lead.state import remove_lead_manifest

        remove_lead_manifest(
            receipt["vacated_scope"],
            owner_cwd=vacated_owner_cwd,
            expected_harness_session_id=vacated_manifest_owner,
        )
    # The recorded team name follows the role (a re-scope used to strand it
    # on the vacated scope; candor landed anonymous twice on 2026-10-04).
    row_after = next((r for r in rows_after if r.name == target_name), None)
    receipt["team_name"] = _carry_team_name(receipt.get("vacated_scope"), scope, row_after, level)
    receipt["missions_armed"] = arm_promoted_missions(scope)
    try:
        receipt["stranded_subordinates"] = _stranded_subordinates(
            receipt["vacated_scope"], scope, target_name, rows_after
        )
    except Exception:
        # Advisory receipt data must never crash a role that committed.
        receipt["stranded_subordinates"] = None
    # The role TYPES the verb: the holder learns it terms through raw mail
    # typed as the operator would. Rendered per harness through the one
    # normalizer; an unknown harness keeps the `/fno:` spelling.
    target_row = next((r for r in rows_after if r.name == target_name), None)
    target_harness = getattr(target_row, "harness", None) if target_row else None
    address = target_name
    if target_row is not None:
        address = (
            getattr(target_row, "harness_session_id", None)
            or getattr(target_row, "cc_session_id", None)
            or getattr(target_row, "short_id", None)
            or target_name
        )
    from fno.agents.harness_map import DispatchResolveError, normalize_command

    try:
        verb = normalize_command(f"/fno:lead {scope}", target_harness or "")
    except DispatchResolveError:
        verb = f"/fno:lead {scope}"
    if caller is not None and target_name == caller.name:
        receipt["term_delivery"] = "skipped: self-edit, this session already terms"
    else:
        receipt["term_delivery"] = _send_term_verb(address, verb)
    return receipt


def _carry_team_name(vacated_scope: Optional[str], scope: str, row_after, level: int) -> str:
    """Team-name carry behind an in-place grant, via spawn-overlay kind
    ``team-rescope``: a recorded name moves to the landing scope, an unnamed
    team takes the row's people-shaped name. Registry-free (the payload
    names the holder session and level). Advisory: never raises; a store
    refusal is the receipt line naming what did not happen."""
    from fno.agents.spawn_overlay_client import SpawnOverlayUnavailable, spawn_overlay_call

    if row_after is None:
        return "unchanged"
    session = next((s for s in (getattr(row_after, f, None) for f in
                     ("harness_session_id", "cc_session_id", "short_id")) if s), "")
    try:
        answer = spawn_overlay_call({
            "kind": "team-rescope", "old_scope": vacated_scope or "",
            "new_scope": scope, "candidate": row_after.name,
            "holder_session": session, "level": level,
        })
    except SpawnOverlayUnavailable as exc:
        return f"unavailable: {exc}"
    if answer.get("named"):
        return str(answer["named"])
    return "carried" if answer.get("carried") else str(answer.get("reason") or "unchanged")


def _send_term_verb(address: str, verb: str) -> str:
    """Mail the term verb to a freshly promoted holder; never raises.

    Advisory receipt data: a failure is named on the receipt, because a promoted
    session that never receives the verb improvises the ritual. `--raw` types
    the payload as the operator would, so a slash arrives as a command.
    """
    import subprocess
    import sys

    try:
        proc = subprocess.run(  # noqa: S603 - fixed argv, no shell
            [sys.executable, "-m", "fno.cli", "agents", "mail",
             "send", address, verb, "--raw"],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return f"not delivered ({exc})"
    if proc.returncode == 0 and proc.stdout.strip():
        return proc.stdout.strip().splitlines()[-1]
    detail = (proc.stderr.strip() or proc.stdout.strip() or "no output").splitlines()[-1]
    return f"not delivered (rc={proc.returncode}: {detail})"


def _stranded_subordinates(
    vacated: Optional[str], new_scope: str, target_name: str, rows: list
) -> Optional[list[str]]:
    """Live rows whose role the vacated scope contained and the new one does not.

    Advisory receipt data about a re-scope: containment is enforced at grant
    time only, so a role granted out of the old territory keeps serving
    after its grantor moves away. Two deliberate answers beyond the list:

    ``[]`` on a no-op, widening, or FIRST-role move, where no territory
    actually left the new scope. ``None`` when the check could not run at all
    (graph unreadable, or an external tracker backend) - which is not the same
    answer as ``[]``, or the receipt would read as "verified no strands" on a
    machine it could not check. A row whose epic id the graph no longer holds
    is LISTED rather than nulled: containment for it is unknowable, and one
    stale promoted row must not silence the determinate answers for every
    other row. Computed by the caller AFTER the registry write returns, over
    the persisted rows, so no graph I/O runs under the lock and no concurrent
    grant can appear in the scan.
    """
    if vacated is None or _same_territory(vacated, new_scope):
        return []
    from fno.agents.registry import TERMINAL_STATUSES

    by_id = _graph_index()
    if by_id is None:
        return None
    stranded: list[str] = []
    for row in rows:
        if row.name == target_name or row.status in TERMINAL_STATUSES:
            continue
        members = split_scope(row.role_scope)
        # A single-member scope naming no project is an epic id whose
        # containment lives in the graph; without the graph it is UNKNOWABLE.
        # List it anyway: one stale row must not silence every determinate
        # answer, and a false name costs a glance while a missing one costs
        # the audit trail.
        unresolvable = (
            len(members) == 1
            and members[0] not in by_id
            and _canonical_project(members[0]) is None
        )
        if unresolvable or (
            scope_contains(
                vacated, row.role_scope, graph_entry=by_id.get
            ) and not scope_contains(new_scope, row.role_scope, graph_entry=by_id.get)
        ):
            stranded.append(row.name)
    return stranded


def role_validation_error(level: Any, scope: Any) -> Optional[str]:
    """Ask the native stamp validator; project resolution remains a caller fact."""
    from fno.agents.spawn_overlay_client import SpawnOverlayUnavailable, spawn_overlay_call

    try:
        return spawn_overlay_call({
            "kind": "spawn-team", "op": "validate", "level": None if level is None else str(level),
            "scope": scope if scope is None or isinstance(scope, str) else repr(scope),
            "level_repr": repr(level), "scope_repr": repr(scope),
            "level_is_int": isinstance(level, int) and not isinstance(level, bool),
            "scope_is_str": isinstance(scope, str),
            "projects": [m for m in split_scope(scope if isinstance(scope, str) else None)
                         if _canonical_project(m)],
        })["refusal"]
    except SpawnOverlayUnavailable as exc:
        return f"cannot validate promotion: {exc}"

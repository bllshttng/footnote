"""The crown vocabulary: what a scope is, what rung it implies, and what a
grantor may hand down. ``registry`` owns the three row fields; this module owns
their meaning and touches no file.

THE LADDER IS THREE RUNGS, EACH A FACT ABOUT THE SCOPE:

    0   several projects   scope names 2+ config projects (a portfolio)
    1   one project        scope names one config project
    2   one epic           scope is a backlog node with type == "epic"

No rung for an implementer: a non-epic node is work, not territory, so a crown
aimed at one is REFUSED - which is why no caller passes a level;
``derive_crown_level`` reads the rung off the scope, and a scope naming no
territory has none to read. Callers hand-type no altitude (the old surface let a
backwards ladder - 0 is the TOP - silently mint wrong authority).

CANONICALIZATION CONTRACT: the one-live-crown guard compares stored scopes by
exact equality, so ``resolve_crown`` guarantees every stamp is canonical
(aliases resolved, members sorted and deduped). The only gap is a pre-redesign
``--crown level=,scope=`` row storing a raw alias spelling - that path is
deleted and was unreleased; re-crown such a row rather than merging spellings.
"""
from __future__ import annotations

import os
from dataclasses import replace
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
    - an ``AgentEntry`` - a joined agent; :func:`grant_error` checks its crown.
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


def crown_reading(row) -> Optional[dict[str, Any]]:
    """The one rendering of a crown, or ``None`` when ``row`` holds none.

    Reads ``row.crown_label`` (``registry.AgentEntry.crown_label``) rather than
    re-deriving the "L{level} {scope}" string, so ``fno whoami``, ``fno agents
    whoami`` and ``fno agents top`` cannot drift into three different renderings
    of the same fact. Safe on the :func:`calling_agent_row` sentinels and on
    ``None``: neither carries ``crown_label``, so ``getattr`` answers ``None``
    rather than raising.
    """
    label = getattr(row, "crown_label", None)
    if label is None:
        return None
    grantor = getattr(row, "crown_grantor", None) or "human"
    level = getattr(row, "crown_level", None)
    scope = getattr(row, "crown_scope", None)
    return {
        "level": level,
        "scope": scope,
        "grantor": grantor,
        "label": label,
        "text": f"{label} (by {grantor})",
    }


def current_crown() -> Optional[dict[str, Any]]:
    """This session's crown reading, or ``None`` - never raises.

    ``fno whoami`` must render byte-for-byte unchanged for an attended human
    shell (AC8-EDGE), so any failure resolving the caller's identity or
    registry row degrades to "no crown" rather than surfacing.
    """
    try:
        return crown_reading(calling_agent_row())
    except Exception:
        return None


def canonical_scope(scopes: list[str]) -> str:
    """The stored form: one name, or sorted unique members joined.

    Canonical on purpose. The one-live-crown guard compares scopes by equality,
    so ``{web,etl}`` and ``{etl,web}`` must reduce to the SAME string or a second
    crown over one portfolio slips through spelled in a different order.
    """
    return SCOPE_SEPARATOR.join(sorted({s.strip() for s in scopes if s.strip()}))


def split_scope(scope: Optional[str]) -> list[str]:
    """The members of a stored scope; a single-name scope yields one element.

    Guards on type, not just truthiness: a corrupted registry row can carry a
    non-string ``crown_scope`` (e.g. a stray int from a hand-edit), and
    ``5.split(...)`` would raise past every caller here, including
    ``fno agents court``, which promises to exit 0 on a read.
    """
    if not scope or not isinstance(scope, str):
        return []
    return [s for s in (part.strip() for part in scope.split(SCOPE_SEPARATOR)) if s]


def _graph_entry(node_id: str) -> Optional[dict]:
    """The node record for ``node_id``, or None. Reads footnote-minted
    metadata (type/project) through the guarded default-backend reader: the
    crown ladder's epic rung classifies nodes footnote's own verbs typed, so
    an external selection degrades to None - the same path as an unknown id
    (configured projects, the top rungs, still resolve from settings)."""
    from fno.tracker.metadata import ExternalMetadataUnavailable, read_entries

    try:
        entries = read_entries("agents.crown")
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
    """Return the readable crown graph as an id index, or ``None``.

    ONE parse serves the availability probe and every per-row node lookup its
    callers make. ``read_entries`` raises ``ExternalMetadataUnavailable`` under
    an external tracker backend, so ``None`` is a distinct answer from an empty
    index: without it a containment or agreement check would silently degrade to
    "not contained" / "not in the graph" on a machine it never read.
    """
    from fno.tracker.metadata import read_entries

    try:
        entries = read_entries("agents.crown")
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
    `a` sit as two live crowns over one project (the guard compares scopes by
    equality), and naming both at once would read as a two-project portfolio
    instead of one project named twice.
    """
    try:
        from fno.projects.resolve import resolve_project_name

        return resolve_project_name(name)
    except Exception:
        return None


def resolve_crown(scopes: list[str]) -> "tuple[int, str]":
    """``scopes`` -> the (rung, stored scope) they imply, both derived together.

    ONE call rather than a derive-then-encode pair, because the two answers must
    agree and a caller holding them separately can mismatch them: the rung is
    counted over CANONICAL project names, so a scope encoded from the raw
    spelling would be a different territory than the one that was counted.

    Raises :class:`CrownScopeError` when the scopes name no territory - the
    refusal that keeps implementers uncrowned. Mixed scopes are refused rather
    than coerced: a portfolio is projects, so naming a project and an epic
    together is a mistake about what is being ruled, not a level-0 crown over
    both.
    """
    members = split_scope(canonical_scope(scopes))
    if not members:
        raise CrownScopeError("a crown needs a scope: name an epic or a project")

    # Resolve aliases FIRST, then dedupe: `-k alpha -k a` is one project spelled
    # twice, not a two-project portfolio.
    resolved = [(m, _canonical_project(m)) for m in members]
    projects = [canon for _, canon in resolved if canon]

    if len(members) > 1:
        unknown = [raw for raw, canon in resolved if not canon]
        if unknown:
            raise CrownScopeError(
                f"a multi-scope crown rules PROJECTS, but {', '.join(unknown)} "
                f"{'is not a configured project' if len(unknown) == 1 else 'are not configured projects'}. "
                "Name projects from your config, or pass a single epic instead."
            )
        scope = canonical_scope(projects)
        # One project named twice collapses to one project, not a portfolio.
        return (0 if len(split_scope(scope)) > 1 else 1), scope

    raw = members[0]
    if projects:
        return 1, projects[0]

    entry = _graph_entry(raw)
    if entry is None:
        raise CrownScopeError(
            f"{raw!r} is neither a configured project nor a backlog node; "
            "nothing to reign over (check for a typo)"
        )
    if entry.get("type") != "epic":
        raise CrownScopeError(
            f"{raw!r} is a {entry.get('type') or 'node'}, not an epic. "
            "Implementers get no crowns - a single node is work, not a territory. "
            f"If {raw} IS meant to be an epic: fno backlog update {raw} --type epic. "
            "Otherwise crown the epic above it, or its project."
        )
    return 2, raw


def derive_crown_level(scopes: list[str]) -> int:
    """The rung alone. Thin wrapper over :func:`resolve_crown` for callers that
    only need the altitude."""
    return resolve_crown(scopes)[0]


def scope_contains(
    outer: Optional[str],
    inner: Optional[str],
    *,
    graph_entry=None,
) -> bool:
    """Does a crown over ``outer`` strictly contain one over ``inner``?

    Real containment, not the honor system it replaces. The old rule could only
    check that two scopes DIFFERED, because scopes were opaque ids and
    project>epic>node containment was not derivable. Under this ladder it is: a
    project is in a portfolio by name, and an epic carries the project it belongs
    to, so a grantor can no longer hand down authority it does not hold.

    ``graph_entry`` overrides the per-call graph read (an ``id -> entry``
    callable), so a caller scanning many rows pays one graph parse instead of
    one per row.
    """
    outer_members = _canonical_members(outer)
    inner_members = _canonical_members(inner)
    if not outer_members or not inner_members:
        return False
    if inner_members == outer_members:
        return False  # a peer crown, not a subordinate one

    if len(inner_members) > 1:
        return inner_members < outer_members

    name = next(iter(inner_members))
    if name in outer_members:
        return True
    entry = (graph_entry or _graph_entry)(name)
    if not entry:
        return False
    # Canonicalize the entry's project before the comparison: graph intake stores
    # the project field RAW (the short_name alias a node was filed under), while
    # outer_members is canonicalized - a raw 'a' would never match a canonical
    # 'alpha', so a king over 'alpha' would be falsely refused an epic filed as
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
    """Why this caller may not bestow a crown over ``requested_scope``, or None.

    The rule the docs have always stated and nothing was enforcing after the
    promotion verb was deleted: you cannot hand down authority you do not hold.
    ``scope_contains`` existed but had no caller, so any spawned worker could
    mint portfolio-level authority for its child.

    Two grantor classes, matching what the verb used to accept, plus three
    failure modes that must fail closed:

    - **an attended human** (``caller_row`` is None - a shell with no agent
      identity in its environment) may grant any scope; there is nobody above a
      human to check against.
    - **an agent** must hold a live crown that STRICTLY contains the request.
      Uncrowned means it holds nothing to hand down. An equal scope is a
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
      permanent_dead) cannot bestow a crown: authority it can no longer exercise
      is not authority to hand down.

    ``allow_terminal_recovery`` admits ONLY a request for the SAME territory the
    terminal row already holds - handing the whole thing to an heir. A strict
    subset is still refused there, because narrowing from a dead grantor leaves
    the remainder ruled by nobody live. ``allow_succession`` is the separate
    live-holder exemption for an intentional same-scope transfer.
    """
    if caller_row is None:
        return None
    if caller_row is REGISTRY_UNREADABLE:
        return (
            "cannot verify the grantor's authority: the agent registry could not "
            "be read, so this session is treated as an agent whose authority is "
            "unknown, not as an attended human. A crown may not be bestowed when "
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
            getattr(caller_row, "crown_scope", None), requested_scope
        ):
            # A stale shell may still carry the identity of a terminal holder.
            # Spawn is the recovery path for that abandoned scope; it must not
            # require succession from a session that can no longer spawn.
            return None
        return (
            f"cannot verify the grantor's authority: its STORED status is "
            f"{status!r}, which is terminal, so it cannot bestow a crown. "
            "Re-register the grantor in its live session or use an attended shell."
        )
    holder = getattr(caller_row, "crown_scope", None)
    if not holder:
        return (
            "a crown is handed DOWN, and this session holds none: an uncrowned "
            "agent cannot bestow one. Ask a king whose scope contains "
            f"{requested_scope!r}, or spawn from an attended shell."
        )
    # SUCCESSION, not a grant. An equal scope is legal only when the spawn caller
    # explicitly names the transfer; otherwise the caller could strip itself by
    # accident while believing --crown was additive.
    if _same_territory(holder, requested_scope):
        if allow_succession:
            return None
        return (
            f"this session already holds its own crown over {requested_scope!r}; "
            "a same-scope spawn is a transfer, not a grant. Re-run with "
            "`--succeed` to name the succession explicitly, or choose a "
            "different scope so this session keeps its crown."
        )
    if not scope_contains(holder, requested_scope):
        return (
            f"this session's crown over {holder!r} neither contains nor equals "
            f"{requested_scope!r}, so it cannot bestow it. A grant must be a "
            "strict subset of what the grantor holds; an equal scope is allowed "
            "only as succession, which hands YOUR scope to an heir."
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


def crown_scope_matches(held: Optional[str], requested: Optional[str]) -> bool:
    """Will the row-keyed king readers accept a crown over ``held`` for
    ``requested``? Territory equality, aliases normalized.

    NOT a containment check: grant answers "may this crown bestow that scope"
    with the ladder, while the readers answer a string question
    (``kings/{crown_scope}.md`` built verbatim; ``done`` refuses on
    ``own != scope``), so a project crown does NOT satisfy an epic manifest.
    A first cut reused ``scope_contains`` and silenced exactly the state the
    warning exists to make loud. Reuse the rule that matches the QUESTION.
    A blank scope on either side answers False.
    """
    if not held or not requested:
        return False
    return _same_territory(held, requested)


class CrownPromotionError(RuntimeError):
    """An attended in-place grant that refused without changing the registry."""


def reclaim_crown(handle: Optional[str] = None) -> dict[str, Any]:
    """Return a transferred crown to its recorded grantor.

    Reclaim is a registry transfer, not a spawn: the current holder is cleared
    and the live row named by ``crown_grantor`` receives the same territory and
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
        # peer's crown by name. The no-handle form below is the one agent
        # path, and it only ever expires the caller's own crown. The
        # unreadable/unregistered sentinels refuse too - an identity the
        # registry cannot resolve to "attended human" gets no handle reach.
        if caller is not None:
            raise CrownPromotionError(
                "reclaim by handle is an attended-shell action: run it "
                "without --handle inside the heir session, or from a shell "
                "holding no agent identity"
            )
        try:
            holder_snapshot = resolve_agent(handle).entry
        except AgentResolutionError as exc:
            raise CrownPromotionError(str(exc)) from exc
    else:
        holder_snapshot = caller
    if holder_snapshot in (None, REGISTRY_UNREADABLE, AGENT_UNREGISTERED):
        raise CrownPromotionError(
            "reclaim needs the current crowned holder: run it inside the heir "
            "session or pass that registered handle from an attended shell"
        )
    scope = getattr(holder_snapshot, "crown_scope", None)
    level = getattr(holder_snapshot, "crown_level", None)
    grantor_key = getattr(holder_snapshot, "crown_grantor", None)
    if level is None or not isinstance(scope, str) or not scope.strip():
        raise CrownPromotionError("this session holds no crown to reclaim")
    if not grantor_key or grantor_key == "human":
        raise CrownPromotionError(
            f"crown over {scope!r} has no registry grantor to reclaim; "
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
        holder = next((row for row in rows if row.name == holder_name), None)
        if holder is None or holder.status in TERMINAL_STATUSES:
            raise CrownPromotionError(
                f"cannot reclaim {scope!r}: current holder {holder_name!r} "
                "is no longer live"
            )
        if holder.crown_scope != scope or holder.crown_level != level:
            raise CrownPromotionError(
                f"cannot reclaim {scope!r}: holder {holder_name!r} changed "
                "its crown while reclaim was in flight"
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
            raise CrownPromotionError(
                f"cannot reclaim {scope!r}: recorded grantor {grantor_key!r} "
                "has no registry row; leave the crown in place and re-register "
                "the grantor before retrying"
            )
        if target.status in TERMINAL_STATUSES:
            raise CrownPromotionError(
                f"cannot reclaim {scope!r}: recorded grantor {target.name!r} "
                f"is terminal ({target.status}); a crown cannot return to a dead row"
            )
        if target.crown_scope is not None or target.crown_level is not None:
            raise CrownPromotionError(
                f"cannot reclaim {scope!r}: grantor {target.name!r} already "
                "holds a crown; refusing to replace unrelated authority"
            )
        other = next(
            (
                row
                for row in rows
                if row.name not in {holder_name, target.name}
                and row.status not in TERMINAL_STATUSES
                and _same_territory(row.crown_scope, scope)
            ),
            None,
        )
        if other is not None:
            raise CrownPromotionError(
                f"cannot reclaim {scope!r}: live row {other.name!r} also "
                "holds the scope"
            )

        armed = False
        unarmed_reason = ""
        try:
            from fno.king.state import arm_king_manifest

            armed = (
                arm_king_manifest(
                    scope,
                    getattr(target, "harness_session_id", None) or "",
                    owner_pid=getattr(target, "pid", None),
                    owner_cwd=getattr(target, "cwd", None),
                )
                is not None
            )
        except (OSError, ValueError) as exc:
            unarmed_reason = str(exc)

        returned_by = (
            getattr(holder, "harness_session_id", None)
            or getattr(holder, "cc_session_id", None)
            or getattr(holder, "short_id", None)
            or holder.name
        )
        for index, row in enumerate(rows):
            if row.name == holder_name:
                rows[index] = replace(
                    row,
                    crown_level=None,
                    crown_scope=None,
                    crown_grantor=None,
                )
            elif row.name == target.name:
                rows[index] = replace(
                    row,
                    crown_level=level,
                    crown_scope=scope,
                    crown_grantor=returned_by,
                )
        receipt.update(
            reclaimed=target.name,
            from_holder=holder_name,
            level=level,
            scope=scope,
            grantor=returned_by,
            king_loop_armed=armed,
        )
        if unarmed_reason:
            receipt["king_loop_unarmed_reason"] = unarmed_reason
        return rows

    update_registry(_reclaim)
    from fno.king.state import remove_king_manifest

    remove_king_manifest(
        scope,
        owner_cwd=getattr(holder_snapshot, "cwd", None),
        expected_harness_session_id=holder_session,
    )
    return receipt


def promote_existing_session(handle: str, scopes: list[str]) -> dict[str, Any]:
    """Grant a crown to one existing row from an attended human shell, or from
    an agent whose own crown strictly contains the requested scope.

    Same-scope succession stays on ``spawn --crown``; this function closes the
    human workflow and the in-place re-scope of a live subordinate, where the
    useful target session already exists.

    A row that already holds a crown is re-scoped, not refused: the new
    territory replaces the old in the one write below, and the receipt reports
    what was vacated. The live-holder check is the guard that matters here - it
    is what keeps two rows from ruling the same territory.
    """
    try:
        level, scope = resolve_crown(scopes)
    except CrownScopeError as exc:
        raise CrownPromotionError(str(exc)) from exc

    # Resolved before update_registry, never inside _stamp: this reads the
    # registry itself, and the closure runs under its lock.
    caller = calling_agent_row()
    denial = grant_error(scope, caller, allow_succession=True)
    if denial is not None:
        raise CrownPromotionError(denial)
    grantor = "human" if caller is None else caller.name
    # `grant_error` blesses an equal scope because SPAWN succession vacates the
    # caller and stamps the heir in one write; this path only stamps the target,
    # so letting it through would leave two live crowns and the holder scan
    # below would refuse naming the caller's OWN row. Refuse here, where the
    # remedy is reachable.
    if caller is not None and _same_territory(
        getattr(caller, "crown_scope", None), scope
    ):
        raise CrownPromotionError(
            f"refusing to crown {handle!r}: {scope!r} is your OWN scope, and "
            "this verb only stamps the target, so it cannot hand a crown over. "
            "Succession runs through `fno agents spawn --crown --succeed` "
            "instead, which vacates you and stamps the heir in a single "
            "registry write, so the scope is never doubly ruled and never "
            "briefly unruled."
        )
    # The authority check ran OUTSIDE the lock, so the grantor's crown can move
    # before the stamp. Re-running grant_error under the lock would put graph
    # I/O on the lock, so carry the granted scope and re-assert it under the
    # lock as a plain compare. Fails closed: a grantor whose crown moved
    # mid-call cannot bestow what it no longer holds.
    granting_scope = None if caller is None else getattr(caller, "crown_scope", None)

    from fno.agents.registry import (
        AgentResolutionError,
        TERMINAL_STATUSES,
        resolve_agent,
        update_registry,
    )

    try:
        target_name = resolve_agent(handle).entry.name
    except AgentResolutionError as exc:
        raise CrownPromotionError(
            f"{exc}. `fno agents list` shows every handle you can crown."
        ) from exc

    # Never self-declared: once a king may grant, the grantor recorded on the
    # row could be the row itself. The succession refusal above fires only on
    # an EQUAL scope, so narrowing to a strict SUBSET would sail past it -
    # identity, not territory, is the test.
    if caller is not None and target_name == grantor:
        raise CrownPromotionError(
            f"refusing to crown {target_name!r}: that is this session, and a "
            "crown is stamped by a grantor, never self-declared. The row would "
            "record itself as its own grantor, which is exactly the claim an "
            "external reader cannot verify. Ask a king whose scope contains "
            f"{scope!r}, or crown a different row."
        )

    receipt: dict[str, Any] = {}
    vacated_manifest_owner = ""
    vacated_owner_cwd = ""

    def _stamp(rows: list) -> list:
        nonlocal vacated_manifest_owner, vacated_owner_cwd
        if caller is not None:
            live_caller = next((row for row in rows if row.name == grantor), None)
            if live_caller is not None and live_caller.status in TERMINAL_STATUSES:
                raise CrownPromotionError(
                    f"refusing to crown {target_name!r}: the grantor's STORED "
                    f"status is {live_caller.status!r}, which is terminal, so "
                    "authority ended before the registry write. Re-register the "
                    "grantor in its live session or use an attended shell."
                )
            live_scope = None if live_caller is None else live_caller.crown_scope
            if not _same_territory(live_scope, granting_scope):
                raise CrownPromotionError(
                    f"refusing to crown {target_name!r}: this session's own crown "
                    f"moved from {granting_scope!r} to {live_scope!r} while the "
                    "grant was in flight, so the authority it was checked against "
                    "no longer holds. Re-read your crown with `fno agents court`, "
                    "then retry if it still contains the scope."
                )
        target = next((row for row in rows if row.name == target_name), None)
        if target is None:
            raise CrownPromotionError(
                f"no agent matching {handle!r}; the target disappeared before the "
                "grant committed. `fno agents list` shows the handles you can crown."
            )
        if target.status in TERMINAL_STATUSES:
            # Name the field and a remedy that cannot contradict the refusal:
            # `fno agents list` renders this STORED status beside a
            # freshly-computed `live_status`, and pointing the caller there (as
            # this used to) shows a column saying the row is live.
            raise CrownPromotionError(
                f"refusing to crown {target.name!r}: its STORED status is "
                f"{target.status!r}, which is terminal. That is a recorded "
                "snapshot, not a live probe, so it can be stale for a session "
                "that is still running. Two ways out:\n"
                "  target is alive    run `fno agents register` IN the target "
                "session (it restamps the row idle in place), then retry\n"
                "  target really died `fno agents reconcile`, then crown a live "
                "row instead; `fno agents list` shows stored status beside the "
                "computed live_status, and a disagreement means the stored one "
                "is stale"
            )

        # A row that already holds a crown is re-scoped, not refused: the
        # replace() below overwrites the old territory in the same write that
        # stamps the new one, so the vacated scope frees atomically and no
        # reader ever sees two live crowns or zero. Captured BEFORE the stamp
        # so the receipt can name what changed hands.
        vacated_scope = target.crown_scope
        vacated_level = target.crown_level
        vacated_manifest_owner = (
            target.harness_session_id or target.cc_session_id or target.short_id or ""
        )
        vacated_owner_cwd = target.cwd
        if (vacated_level is None) != (vacated_scope is None):
            # Half a crown is unstampable by crown_validation_error, so no
            # legal writer produces it; overwrite would erase the corruption
            # signal instead of surfacing it.
            raise CrownPromotionError(
                f"refusing to crown {target.name!r}: it holds half a crown "
                f"(level={vacated_level!r}, scope={vacated_scope!r}), which "
                "no legal crown produces. fno agents stop the row and "
                "fno agents rm it, then crown the re-registered session."
            )

        holder = next(
            (
                row
                for row in rows
                if row.name != target.name
                and row.status not in TERMINAL_STATUSES
                and _same_territory(row.crown_scope, scope)
            ),
            None,
        )
        if holder is not None:
            raise CrownPromotionError(
                f"refusing to crown {target.name!r}: scope {scope!r} is already "
                f"held by live row {holder.name!r}. Three ways out, cheapest "
                "first:\n"
                f"  re-scope the holder   fno agents crown {holder.name} --scope "
                "<other territory>   (both sessions stay live; retry this "
                "command after)\n"
                "  holder looks dead     fno agents reconcile   (a row whose "
                "harness session is gone flips to orphaned, which frees the "
                "scope)\n"
                f"  end the holder        fno agents stop {holder.name}, then retry"
            )

        try:
            from fno.king.state import arm_king_manifest

            manifest_path = arm_king_manifest(
                scope,
                target.harness_session_id or target.cc_session_id or target.short_id or "",
                owner_pid=target.pid,
                owner_cwd=target.cwd,
            )
        except (OSError, ValueError) as exc:
            raise CrownPromotionError(
                f"refusing to crown {target.name!r}: king manifest arming failed: {exc}"
            ) from exc

        for index, row in enumerate(rows):
            if row.name == target.name:
                rows[index] = replace(
                    row,
                    crown_level=level,
                    crown_scope=scope,
                    crown_grantor=grantor,
                )
                break
        receipt.update(
            crowned=target.name,
            level=level,
            scope=scope,
            grantor=grantor,
            vacated_scope=vacated_scope,
            vacated_level=vacated_level,
            king_loop_armed=manifest_path is not None,
        )
        return rows

    # The persisted rows ARE the lock-window snapshot the strand scan needs:
    # a post-release re-read could see a concurrent grant over the
    # just-freed scope and mislabel that heir as stranded.
    rows_after = update_registry(_stamp)
    if receipt.get("vacated_scope") and not _same_territory(
        receipt["vacated_scope"], scope
    ):
        from fno.king.state import remove_king_manifest

        remove_king_manifest(
            receipt["vacated_scope"],
            owner_cwd=vacated_owner_cwd,
            expected_harness_session_id=vacated_manifest_owner,
        )
    try:
        receipt["stranded_subordinates"] = _stranded_subordinates(
            receipt["vacated_scope"], scope, target_name, rows_after
        )
    except Exception:
        # Advisory receipt data must never crash a crown that committed.
        receipt["stranded_subordinates"] = None
    # The crown TYPES the verb: the holder learns it reigns through raw mail
    # typed as the operator would. Plugin-qualified per harness (`$fno:` on
    # codex, `/fno:` elsewhere).
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
    prefix = "$fno:" if target_harness == "codex" else "/fno:"
    receipt["reign_delivery"] = _send_reign_verb(address, f"{prefix}reign {scope}")
    return receipt


def _send_reign_verb(address: str, verb: str) -> str:
    """Mail the reign verb to a freshly crowned holder; never raises.

    Advisory receipt data: a failure is named on the receipt, because a crowned
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
    """Live rows whose crown the vacated scope contained and the new one does not.

    Advisory receipt data about a re-scope: containment is enforced at grant
    time only, so a crown granted out of the old territory keeps reigning
    after its grantor moves away. Two deliberate answers beyond the list:

    ``[]`` on a no-op, widening, or FIRST-crown move, where no territory
    actually left the new scope. ``None`` when the check could not run at all
    (graph unreadable, or an external tracker backend) - which is not the same
    answer as ``[]``, or the receipt would read as "verified no strands" on a
    machine it could not check. A row whose epic id the graph no longer holds
    is LISTED rather than nulled: containment for it is unknowable, and one
    stale crowned row must not silence the determinate answers for every
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
        members = split_scope(row.crown_scope)
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
                vacated, row.crown_scope, graph_entry=by_id.get
            ) and not scope_contains(new_scope, row.crown_scope, graph_entry=by_id.get)
        ):
            stranded.append(row.name)
    return stranded


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

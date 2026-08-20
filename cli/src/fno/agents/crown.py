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

CANONICALIZATION CONTRACT: the one-live-crown guard (in ``dispatch`` and
``mux_spawn``) compares a stored ``crown_scope`` to the requested one by exact
equality, so it only stays correct while every stored scope IS canonical.
``resolve_crown`` guarantees that for every crown it stamps (aliases resolved via
``_canonical_project``, members sorted and deduped), so new-vs-new is safe. The
gap is migration-only: a crown stamped by the OLD ``--crown level=,scope=`` path
(before this redesign) could store a raw alias spelling that the equality guard
would not match against the canonical form. That old path is deleted here, and
the feature was unreleased, so no such row should exist outside development
registries - if one does, re-crown it rather than relying on the guard to merge
the spellings.
"""
from __future__ import annotations

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

#: Sentinel for "the caller carries an agent identity but no registry row matches
#: it." Distinct from ``None`` (an attended human with no identity at all) and
#: from :data:`REGISTRY_UNREADABLE` (the registry could not be read): the registry
#: WAS read, the caller claims to be an agent, but it is not joined - a
#: just-spawned worker before its row lands, or a session that has not run
#: ``/fno-me``. Such a caller holds no verified authority, so :func:`grant_error`
#: refuses with the heal rather than authorize like a human. This is the third
#: fail-open path: the first review's sentinel covered the registry EXCEPTION, and
#: ``_find_by_session`` returns ``None`` on a clean miss WITHOUT raising, so the
#: miss never reached that except branch.
AGENT_UNREGISTERED: Any = object()


def calling_agent_row():
    """The calling session's registry row.

    Four outcomes, and :func:`grant_error` treats each differently:

    - ``None`` - an attended human: no ``FNO_AGENT_SELF``, so no agent identity
      to check against. A human may grant any scope.
    - an ``AgentEntry`` - a joined agent; :func:`grant_error` checks its crown.
    - :data:`REGISTRY_UNREADABLE` - the caller HAS an agent identity, but the
      registry could not be read to resolve it. This is NOT an attended human,
      and flattening it to ``None`` would let a registry read failure promote
      any spawned worker to human authority and mint crowns it has no right to
      bestow - fail-open on the rule "you cannot hand down authority you do not
      hold." Surfaced as a sentinel so the caller refuses instead.
    - :data:`AGENT_UNREGISTERED` - the caller HAS an agent identity and the
      registry was read, but no row matches it (a just-spawned worker before its
      row lands, or a session that has not run ``/fno-me``). ``_find_by_session``
      returns ``None`` on this clean miss WITHOUT raising, so without this
      sentinel the miss would flow out as the attended-human ``None`` and
      authorize any grant - the same fail-open as the exception path, one branch
      over. Surfaced so :func:`grant_error` refuses with the heal instead.

    Resolved the same way ``fno whoami`` does, so "who am I" has one answer.
    """
    from fno.agents.registry import load_registry
    from fno.agents.whoami import _find_by_session
    from fno.harness_identity import resolve_harness_identity

    ident = resolve_harness_identity()
    if not ident.session_id or not ident.harness:
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
            "Crown the epic above it, or its project."
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


def grant_error(requested_scope: str, caller_row) -> Optional[str]:
    """Why this caller may not bestow a crown over ``requested_scope``, or None.

    The rule the docs have always stated and nothing was enforcing after the
    promotion verb was deleted: you cannot hand down authority you do not hold.
    ``scope_contains`` existed but had no caller, so any spawned worker could
    mint portfolio-level authority for its child.

    Two grantor classes, matching what the verb used to accept, plus two
    failure modes that must fail closed:

    - **an attended human** (``caller_row`` is None - a shell with no agent
      identity in its environment) may grant any scope; there is nobody above a
      human to check against.
    - **an agent** must hold a live crown that STRICTLY contains the request.
      Uncrowned means it holds nothing to hand down, and an equal scope would be
      a peer crown rather than a subordinate one - already refused by the
      one-live-crown rule, and refused here with a message that explains it.
    - **an unreadable registry** (``caller_row`` is
      :data:`REGISTRY_UNREADABLE`) is refused outright. The check cannot verify
      the caller holds anything, so it must not fall back to the most privileged
      answer (human); a registry read failure does not make the caller human.
    - **an unregistered agent** (``caller_row`` is :data:`AGENT_UNREGISTERED`)
      has an identity but no registry row. The registry was read and the caller
      is not in it, so it is an agent holding no verified authority, not a human;
      refused with the heal (run ``/fno-me`` to join, or wait for the row) rather
      than authorized.
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
    holder = getattr(caller_row, "crown_scope", None)
    if not holder:
        return (
            "a crown is handed DOWN, and this session holds none: an uncrowned "
            "agent cannot bestow one. Ask a king whose scope contains "
            f"{requested_scope!r}, or spawn from an attended shell."
        )
    # SUCCESSION, not a grant. Handing your own scope to an heir is the one case
    # where an equal scope is legal, because the caller does not keep it: the
    # one-live-crown guard vacates the caller in the same write that stamps the
    # heir. Checking strict containment alone would refuse exactly the abdication
    # this design requires - a king cannot hand off after exiting.
    if _same_territory(holder, requested_scope):
        return None
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


def _same_territory(a: Optional[str], b: Optional[str]) -> bool:
    """Do two stored scopes name the same territory, aliases normalized?"""
    left, right = _canonical_members(a), _canonical_members(b)
    return bool(left) and left == right


class CrownPromotionError(RuntimeError):
    """An attended in-place grant that refused without changing the registry."""


def promote_existing_session(handle: str, scopes: list[str]) -> dict[str, Any]:
    """Grant a crown to one existing row from an attended human shell.

    This is deliberately not a general crown writer. Agent-to-agent grants and
    same-scope succession stay on ``spawn --crown``; this function only closes
    the human workflow where the useful target session already exists.

    A row that already holds a crown is re-scoped, not refused: the new
    territory replaces the old in the one write below, and the receipt reports
    what was vacated. The live-holder check is the guard that matters here - it
    is what keeps two rows from ruling the same territory.
    """
    caller = calling_agent_row()
    if caller is not None:
        raise CrownPromotionError(
            "in-place coronation must run from an attended shell with no ambient "
            "agent identity. Run `fno agents register` in the target session, "
            "then run this crown command from another terminal."
        )

    try:
        level, scope = resolve_crown(scopes)
    except CrownScopeError as exc:
        raise CrownPromotionError(str(exc)) from exc

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

    receipt: dict[str, Any] = {}

    def _stamp(rows: list) -> list:
        target = next((row for row in rows if row.name == target_name), None)
        if target is None:
            raise CrownPromotionError(
                f"no agent matching {handle!r}; the target disappeared before the "
                "grant committed. `fno agents list` shows the handles you can crown."
            )
        if target.status in TERMINAL_STATUSES:
            raise CrownPromotionError(
                f"refusing to crown {target.name!r}: target status {target.status!r} "
                "is terminal. `fno agents list` shows which rows are live; crown "
                "one of those instead."
            )

        # A row that already holds a crown is re-scoped, not refused: the
        # replace() below overwrites the old territory in the same write that
        # stamps the new one, so the vacated scope frees atomically and no
        # reader ever sees two live crowns or zero. Captured BEFORE the stamp
        # so the receipt can name what changed hands.
        vacated_scope = target.crown_scope
        vacated_level = target.crown_level
        if vacated_level is not None and vacated_scope is None:
            # A level with no scope is unstampable by crown_validation_error,
            # so no legal writer produces it; overwrite would erase the
            # corruption signal instead of surfacing it.
            raise CrownPromotionError(
                f"refusing to crown {target.name!r}: it holds partial crown "
                f"metadata (level {vacated_level} with no scope), which no "
                "legal crown produces. fno agents stop the row and "
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

        for index, row in enumerate(rows):
            if row.name == target.name:
                rows[index] = replace(
                    row,
                    crown_level=level,
                    crown_scope=scope,
                    crown_grantor="human",
                )
                break
        receipt.update(
            crowned=target.name,
            level=level,
            scope=scope,
            grantor="human",
            vacated_scope=vacated_scope,
            vacated_level=vacated_level,
        )
        return rows

    # The persisted rows ARE the lock-window snapshot the strand scan needs:
    # a post-release re-read could see a concurrent grant over the
    # just-freed scope and mislabel that heir as stranded.
    rows_after = update_registry(_stamp)
    try:
        receipt["stranded_subordinates"] = _stranded_subordinates(
            receipt["vacated_scope"], scope, target_name, rows_after
        )
    except Exception:
        # Advisory receipt data must never crash a crown that committed.
        receipt["stranded_subordinates"] = None
    return receipt


def _stranded_subordinates(
    vacated: Optional[str], new_scope: str, target_name: str, rows: list
) -> Optional[list[str]]:
    """Live rows whose crown the vacated scope contained and the new one does not.

    Advisory receipt data about a re-scope: containment is enforced at grant
    time only, so a crown granted out of the old territory keeps reigning
    after its grantor moves away. Two deliberate answers beyond the list:

    ``[]`` on a no-op, widening, or FIRST-crown move, where no territory
    actually left the new scope. ``None`` when the check could not run (graph
    unreadable, or an external tracker backend) - which is not the same answer
    as ``[]``, or the receipt would read as "verified no strands" on a machine
    it could not check. Computed by the caller AFTER the registry write
    returns, over a rows snapshot taken inside the write's lock window, so
    no graph I/O runs under the lock and no concurrent grant can appear in
    the scan.
    """
    if vacated is None or _same_territory(vacated, new_scope):
        return []
    from fno.tracker.metadata import read_entries

    from fno.agents.registry import TERMINAL_STATUSES

    try:
        # One parse serves both the availability probe and every per-row
        # epic lookup: raises ExternalMetadataUnavailable under an external
        # tracker backend, where containment checks would silently degrade
        # to "not contained".
        entries = read_entries("agents.crown")
    except Exception:
        return None
    by_id = {
        e.get("id"): e for e in entries if isinstance(e, dict) and e.get("id")
    }
    stranded: list[str] = []
    for row in rows:
        if row.name == target_name or row.status in TERMINAL_STATUSES:
            continue
        members = split_scope(row.crown_scope)
        # A single-member scope that names no project is an epic id, whose
        # containment lives in the graph. If the graph does not hold it,
        # containment is UNKNOWABLE for this row, and letting
        # scope_contains answer False would print [] ("verified no
        # strands") about a row the check never evaluated.
        if (
            len(members) == 1
            and members[0] not in by_id
            and _canonical_project(members[0]) is None
        ):
            return None
        if scope_contains(
            vacated, row.crown_scope, graph_entry=by_id.get
        ) and not scope_contains(new_scope, row.crown_scope, graph_entry=by_id.get):
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

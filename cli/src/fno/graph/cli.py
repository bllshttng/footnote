"""fno graph CLI - typer subcommands for feature graph management.

Each subcommand delegates to fno.graph.{store,statuses,render,depends}
and preserves identical behavior to scripts/roadmap-tasks.py.

Exit codes:
    0  success
    1  user error (invalid input)
    2  runtime error (bad state, cycle detected)
    3  not found
    4  nothing to intake
"""
from __future__ import annotations

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import List, Literal, Optional

import typer

cli = typer.Typer(name="graph", help="Feature graph management", no_args_is_help=True)

# Nested triage sub-app: `fno backlog triage <verb>` (or the deprecated
# `fno graph triage <verb>` alias since backlog and graph share the same app).
from fno.graph.triage import cli as _triage_cli  # noqa: E402

cli.add_typer(_triage_cli, name="triage")

# Nested capture sub-app: `fno backlog capture <verb>`. The capture tier below
# idea nodes (markdown fu-* items, NOT graph nodes). Distinct from
# `fno mail` (cross-project messaging). `fno backlog inbox` is kept as a
# hidden deprecated alias (same Typer app, byte-identical behavior), mirroring
# the top-level `fno graph` -> `fno backlog` precedent.
from fno.backlog.capture import cli as _capture_cli  # noqa: E402

cli.add_typer(_capture_cli, name="capture")
cli.add_typer(_capture_cli, name="inbox", hidden=True)


def _live_claimed_node_ids() -> set[str]:
    """Node ids that currently hold a LIVE ``node:<id>`` claim.

    Selection-time enforcement (ab-fcf9cec5): a node another session is
    actively driving (a `/target` run or a walker-dispatched target) holds a
    live ``node:<id>`` claim, and must be skipped so two sessions never pick
    up the same node. Node claims live at the GLOBAL claims root
    (``~/.fno/claims`` via ``Path.home()``), mirroring the global graph
    at ``~/.fno/graph.json`` so the lock coordinates across worktrees.

    Best-effort: any fault in the claims subsystem degrades to an empty set
    so selection never breaks on it (identical to pre-enforcement behavior).
    Only LIVE claims filter; stale/expired/released ones do not.
    """
    try:
        from fno.claims.core import list_claims
        from fno.claims.io import global_claims_root
        # list_claims(prefix="node:") guarantees every key starts with "node:";
        # the isinstance guard keeps this robust if that contract ever changes.
        live = list_claims(prefix="node:", include_stale=False, root=global_claims_root())
        return {c["key"].removeprefix("node:") for c in live if isinstance(c.get("key"), str)}
    except Exception:
        return set()


def _has_unmerged_open_pr(e: dict) -> bool:
    """True when a node already carries a PR but is not yet closed (done) -
    i.e. work is in flight / in review, so it must NOT be re-selected for
    dispatch (ab-372130f6).

    A node only leaves the ready pool at merge-and-close (completed_at set ->
    recompute_statuses derives _status "done"). During the whole PR window
    pr_number is set but completed_at is None, so the status derivation still
    yields "ready"; this predicate is the missing selection-time guard,
    mirroring _live_claimed_node_ids() - the PID-based node claim dies when the
    builder session exits, leaving no in-flight signal behind.

    Excludes every not-done node that carries a pr_number, regardless of
    merge_status: an open unmerged PR (the originally-observed ab-58645f63
    case), a merged-but-reconcile-pending PR (do not re-dispatch already-merged
    work in the close gap), and a closed-without-merge PR (un-dispatchable until
    pr_number is cleared - see the plan's known edges) all mean "not fresh
    ready work".
    """
    if e.get("completed_at"):
        return False  # already done; status derivation bucketed it out of ready
    return bool(e.get("pr_number"))


@cli.callback()
def _graph_callback(
    ctx: typer.Context,
    json_output: bool = typer.Option(
        False,
        "--json", "-J",
        help="Output structured JSON to stdout. Diagnostics go to stderr.",
    ),
) -> None:
    from fno.handoff.output import merge_json_flag
    merge_json_flag(ctx, json_output)


def _graph_path() -> Path:
    """Return the active graph.json path (monkeypatch-friendly)."""
    from fno.graph._constants import GRAPH_JSON
    return GRAPH_JSON


def _archive_path() -> Path:
    from fno.graph._constants import GRAPH_ARCHIVE_JSON
    return GRAPH_ARCHIVE_JSON


def _briefs_dir() -> Path:
    from fno.graph._constants import BRIEFS_DIR
    return BRIEFS_DIR


# -- shared node construction --

_NodeFields = dict


def _scan_md_field(text: str, key: str) -> Optional[str]:
    """First ``<key>: <value>`` value in a target-state.md, matched-quote-stripped.

    Local mirror of ``fno.agents.whoami._scan_field`` so ``graph`` does not import
    ``agents`` (avoids an import cycle). ``None`` if the key is absent.
    """
    import re

    # ^\s* tolerates indentation; (.+) captures the whole value so a path/title
    # containing spaces is not truncated at the first space (\S+ would).
    pattern = re.compile(rf"^\s*{re.escape(key)}:\s*(.+)")
    for line in text.splitlines():
        match = pattern.match(line)
        if match:
            value = match.group(1).strip()
            if len(value) >= 2 and value[0] == value[-1] and value[0] in ('"', "'"):
                value = value[1:-1]
            return value
    return None


def _session_provenance(running_cwd: Optional[str] = None) -> dict:
    """Ambient parent-edge provenance for a node born inside a live session.

    Reads the running session's env + ``.fno/target-state.md`` and returns
    ``source_session_id`` / ``source_harness`` / ``source_cwd`` /
    ``source_node_id`` / ``source_plan_path``. Capture is AMBIENT, never
    volunteered (x-30f6): no caller passes anything. Every key degrades to
    ``None`` and the function NEVER raises (AC-EDGE).

    ``source_cwd`` is the originating SESSION's cwd, which is the key claude
    transcript dirs are slugged by -- distinct from the node's durable ``cwd``
    (the canonical project root). The read-back resolver needs the session cwd,
    so it is persisted separately rather than reusing the node's ``cwd``.

    Ownership of the manifest is proven exactly as ``whoami.find_held_node``
    does it: the manifest's ``claude_transcript_id`` must equal this process's
    ``CLAUDE_CODE_SESSION_ID``, so a stale / reused / foreign worktree manifest
    never leaks a node this session does not hold. Node + plan resolution is
    claude-only (the only proven transcript-resolver lane); codex/gemini stamp
    session + harness and degrade the rest.
    """
    cwd = running_cwd if running_cwd is not None else os.getcwd()

    session: Optional[str] = None
    harness: Optional[str] = None
    sid = (os.environ.get("CLAUDE_CODE_SESSION_ID") or "").strip()
    if sid:
        session, harness = sid, "claude"
    else:
        codex_sid = (os.environ.get("CODEX_SESSION_ID") or "").strip()
        gemini_sid = (os.environ.get("GEMINI_SESSION_ID") or "").strip()
        if codex_sid:
            session, harness = codex_sid, "codex"
        elif gemini_sid:
            session, harness = gemini_sid, "gemini"

    source_node_id: Optional[str] = None
    source_plan_path: Optional[str] = None
    if session and harness == "claude":
        try:
            text = (Path(cwd) / ".fno" / "target-state.md").read_text(encoding="utf-8")
            if _scan_md_field(text, "claude_transcript_id") == session:
                nid = _scan_md_field(text, "graph_node_id")
                if nid and nid.lower() != "null":
                    source_node_id = nid
                plan = _scan_md_field(text, "plan_path")
                if plan and plan.lower() != "null":
                    source_plan_path = plan
        except OSError:
            pass

    return {
        "source_session_id": session,
        "source_harness": harness,
        # session cwd is the transcript-resolver key; only meaningful with a session.
        "source_cwd": cwd if session else None,
        "source_node_id": source_node_id,
        "source_plan_path": source_plan_path,
    }


def _build_backlog_node(
    *,
    title: str,
    type_: str = "feature",
    parent: Optional[str] = None,
    project: Optional[str] = None,
    cwd: Optional[str] = None,
    priority: str = "p2",
    domain: str = "code",
    blocked_by: Optional[list[str]] = None,
    roadmap_id: Optional[str] = None,
    vision_path: Optional[str] = None,
    details: Optional[str] = None,
    size: Optional[str] = None,
    batch: Optional[str] = None,
    plan_path: Optional[str] = None,
) -> _NodeFields:
    """Build a backlog node dict shared by ``cmd_add`` and ``cmd_idea``.

    Centralizes the field set so a schema addition (e.g. a new graph
    field) shows up in every entry-creating verb at once. The returned
    dict has no ``id`` - the caller assigns one inside its locked mutator
    so duplicate-ID checks happen against the live snapshot.
    """
    from fno.graph._constants import ID_PREFIX  # noqa: F401 (kept for symmetry)
    # Parent-edge provenance (x-30f6): stamped ambiently from the running
    # session's env + manifest. Centralized here so every creator verb
    # (add/idea/decompose) self-describes its origin with no caller arg.
    prov = _session_provenance()
    return {
        "id": None,  # caller fills inside locked mutator
        "parent": parent,
        "title": title,
        "type": type_,
        "project": project,
        "cwd": cwd,
        "priority": priority,
        "domain": domain,
        "blocked_by": list(blocked_by or []),
        "session_id": None,
        "claimed_at": None,
        "completed_at": None,
        "has_brief": False,
        "compacted": False,
        "roadmap_id": roadmap_id,
        "vision_path": vision_path,
        "details": details,
        "size": size,
        "batch": batch,
        "cost_usd": None,
        "cost_sessions": [],
        "plan_path": plan_path,
        "pr_number": None,
        "pr_url": None,
        "merge_status": None,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "source_session_id": prov["source_session_id"],
        "source_harness": prov["source_harness"],
        "source_cwd": prov["source_cwd"],
        "source_node_id": prov["source_node_id"],
        "source_plan_path": prov["source_plan_path"],
    }


# -- add --

@cli.command("add")
def cmd_add(
    title: str = typer.Argument(..., help="Feature title"),
    domain: str = typer.Option("code", help="Domain profile"),
    priority: str = typer.Option("p2", "--priority", "-p", help="p0|p1|p2|p3"),
    blocked_by: Optional[str] = typer.Option(None, "--blocked-by", help="Comma-separated ab-IDs"),
    parent: Optional[str] = typer.Option(None, help="Parent node ab-ID"),
    type_: str = typer.Option("feature", "--type", "-t", help="Node type: roadmap|feature|task"),
    project: Optional[str] = typer.Option(
        None,
        help=(
            "Project name. Defaults to the project whose `path:` in "
            "settings.yaml matches the current working directory."
        ),
    ),
    cwd: Optional[str] = typer.Option(
        None,
        "--cwd",
        "-c",
        help="Project working directory. Defaults to the current working directory.",
    ),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id", help="Roadmap group ID"),
    vision_path: Optional[str] = typer.Option(None, "--vision-path", help="Source vision doc path"),
    details: Optional[str] = typer.Option(None, "--details", "-d", help="Implementation guidance"),
    description: Optional[str] = typer.Option(
        None,
        "--description",
        help=(
            "Alias for --details. Reads more naturally for an idea-stage "
            "row. Mutually exclusive with --details."
        ),
    ),
    size: Optional[str] = typer.Option(None, help="Size estimate: S|M|L"),
    batch: Optional[str] = typer.Option(None, help="Execution batch group"),
) -> None:
    from fno.graph._constants import PRIORITY_ORDER, mint_node_id
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import detect_project_from_settings, project_root_from_settings, repo_root

    if priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    if details is not None and description is not None:
        typer.echo(
            "Error: pass --details or --description, not both",
            err=True,
        )
        raise typer.Exit(code=1)
    resolved_details = details if details is not None else description

    # Store an absolute path so downstream `detect_project()` (which compares
    # against `repo_root()` via normpath) finds matches. A relative cwd like
    # "." would normpath to "." and silently fail to match any project.
    # No explicit --cwd: record the canonical main checkout (repo_root()), not
    # os.getcwd(). A backlog node is durable and outlives the worktree it was
    # filed from, so a worktree cwd would dangle once that worktree is archived.
    if cwd is not None:
        resolved_cwd = os.path.abspath(os.path.expanduser(cwd))
    elif project is not None:
        # Explicit --project: derive cwd from the work-map so project and cwd
        # are consistent even when filed from a foreign working directory.
        resolved_cwd = project_root_from_settings(project) or repo_root()
    else:
        resolved_cwd = repo_root()
    resolved_project = project
    if resolved_project is None:
        resolved_project = detect_project_from_settings(resolved_cwd)

    blockers: list[str] = []
    if blocked_by:
        blockers = [b.strip() for b in blocked_by.split(",") if b.strip()]

    new_id_holder: list[Optional[str]] = [None]

    def mutator(entries):
        new_id = mint_node_id({e.get("id") for e in entries})
        new_id_holder[0] = new_id
        node = _build_backlog_node(
            title=title,
            type_=type_,
            parent=parent,
            project=resolved_project,
            cwd=resolved_cwd,
            priority=priority,
            domain=domain,
            blocked_by=blockers,
            roadmap_id=roadmap_id,
            vision_path=vision_path,
            details=resolved_details,
            size=size,
            batch=batch,
        )
        node["id"] = new_id
        entries.append(node)
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(json.dumps({"id": new_id_holder[0], "title": title}, indent=2))


# -- idea (sugar verb) --

@cli.command("idea")
def cmd_idea(
    title: str = typer.Argument(..., help="Idea title - what is this?"),
    description: Optional[str] = typer.Option(
        None,
        "--description",
        "--details",
        "-d",
        help="Optional free-form description (stored in `details`).",
    ),
    priority: str = typer.Option("p2", "--priority", "-p", help="p0|p1|p2|p3"),
    project: Optional[str] = typer.Option(
        None,
        help=(
            "Project name. Defaults to the project whose `path:` in "
            "settings.yaml matches the current working directory."
        ),
    ),
    cwd: Optional[str] = typer.Option(
        None,
        "--cwd",
        "-c",
        help="Working directory. Defaults to the current working directory.",
    ),
) -> None:
    """Capture an idea (a plan-less backlog node) with minimal ceremony.

    Equivalent to `fno backlog add <title>` but signals intent to skip the
    spec/plan ceremony for now. The new node has no ``plan_path`` and so
    derives to ``_status: idea`` until a plan is associated (via
    ``fno backlog intake`` or by setting ``--plan-path`` on
    ``fno backlog update``).
    """
    from fno.graph._constants import PRIORITY_ORDER, mint_node_id
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import detect_project_from_settings, project_root_from_settings, repo_root

    if priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    # Store an absolute path so downstream `detect_project()` (which compares
    # against `repo_root()` via normpath) finds matches. A relative cwd like
    # "." would normpath to "." and silently fail to match any project.
    # No explicit --cwd: record the canonical main checkout (repo_root()), not
    # os.getcwd(). A backlog node is durable and outlives the worktree it was
    # filed from, so a worktree cwd would dangle once that worktree is archived.
    if cwd is not None:
        resolved_cwd = os.path.abspath(os.path.expanduser(cwd))
    elif project is not None:
        # Explicit --project: derive cwd from the work-map so project and cwd
        # are consistent even when filed from a foreign working directory.
        resolved_cwd = project_root_from_settings(project) or repo_root()
    else:
        resolved_cwd = repo_root()
    resolved_project = project
    if resolved_project is None:
        resolved_project = detect_project_from_settings(resolved_cwd)

    new_id_holder: list[Optional[str]] = [None]

    def mutator(entries):
        new_id = mint_node_id({e.get("id") for e in entries})
        new_id_holder[0] = new_id
        node = _build_backlog_node(
            title=title,
            project=resolved_project,
            cwd=resolved_cwd,
            priority=priority,
            details=description,
        )
        node["id"] = new_id
        entries.append(node)
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(json.dumps({"id": new_id_holder[0], "title": title}, indent=2))


# -- decompose (bounded epic -> group child nodes) --

@cli.command("decompose")
def cmd_decompose(
    ctx: typer.Context,
    epic_id: str = typer.Argument(..., help="Epic node ab-ID to decompose into group children"),
    groups: str = typer.Option(
        ...,
        "--groups",
        help=(
            "JSON array of {slug,title,waves,blocked_by_groups} group specs. "
            "Prefix '@' to read a file (--groups @groups.json) or pass '-' to read stdin."
        ),
    ),
    max_prs: Optional[int] = typer.Option(
        None,
        "--max-prs",
        help=(
            "Ceiling on group/PR count. Rejects when groups exceed it (N is a "
            "ceiling, not a quota). Defaults to config.blueprint.max_prs_per_epic."
        ),
    ),
    force: bool = typer.Option(
        False,
        "--force", "-F",
        help="Allow a re-decomposition that orphans an already-shipped group child node.",
    ),
) -> None:
    """Upsert group child nodes under an epic (atomic + idempotent).

    Each group becomes one child node (parent=epic, plan_path=<epic-doc>#group-<slug>)
    bundling 1+ execution waves into a single shippable PR. Re-running with the
    same slugs updates the existing children in place rather than duplicating.
    The whole decomposition lands in one locked graph mutation, so a bad spec
    leaves the graph exactly as it was (AC1-FR).
    """
    import sys as _sys
    from fno.graph._constants import mint_node_id
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node, _would_create_cycle
    from fno.graph._decompose import (
        DecomposeError,
        child_plan_path,
        find_orphans,
        is_shipped,
        plan_base,
        validate_groups,
    )
    from fno.handoff.output import emit_error, json_mode

    # 1. Read the --groups source ('@file', '-' stdin, or a JSON literal),
    #    keeping read vs parse failures distinct so the message names the cause.
    try:
        if groups == "-":
            raw = _sys.stdin.read()
        elif groups.startswith("@"):
            raw = Path(groups[1:]).expanduser().read_text(encoding="utf-8")
        else:
            raw = groups
    except OSError as e:
        emit_error(ctx, f"could not read --groups file {groups[1:]!r}: {e}")
        raise typer.Exit(code=1)
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError as e:
        emit_error(ctx, f"--groups is not valid JSON: {e}")
        raise typer.Exit(code=1)

    # 2. Resolve the ceiling: explicit --max-prs wins; else fall back to
    #    config.blueprint.max_prs_per_epic. load_settings() already returns the
    #    default (4) when config.blueprint is absent, so a raise here means the
    #    config is present but invalid - surface it rather than masking it with 4.
    if max_prs is None:
        from fno.config import load_settings
        try:
            max_prs = load_settings().config.blueprint.max_prs_per_epic
        except Exception as e:
            emit_error(ctx, f"could not read config.blueprint.max_prs_per_epic: {e}")
            raise typer.Exit(code=1)

    # 3. Validate the spec entirely before touching the graph (atomicity).
    try:
        norm = validate_groups(parsed, max_prs)
    except DecomposeError as e:
        emit_error(ctx, str(e))
        raise typer.Exit(code=e.exit_code)

    keep_slugs = {g["slug"] for g in norm}
    results: list[dict] = []
    epic_id_box: list[str] = [epic_id]

    def mutator(graph_entries):
        # Resolve the epic inside the locked snapshot so a corrupt graph
        # surfaces as exit 1 (via locked_mutate_graph) rather than masquerading
        # as "epic not found".
        live_epic = _find_node(graph_entries, epic_id)
        if live_epic is None:
            raise DecomposeError(f"epic node {epic_id} not found", exit_code=3)
        epic_resolved_id = live_epic["id"]
        epic_id_box[0] = epic_resolved_id
        base = plan_base(live_epic.get("plan_path"))
        # `base` (verbatim, possibly relative) is the node-identity key used by
        # child_plan_path below - DO NOT mutate it. For the set-expected
        # shell-out only, resolve a relative base against the epic's project
        # root (its stored cwd) so a decompose run from a subdirectory still
        # locates the doc on disk; fno.plan._stamp resolves relative paths against
        # the process cwd, which would otherwise false-"missing" and skip
        # writing the count (reintroducing early graduation).
        if base and not os.path.isabs(base):
            base_box[0] = os.path.join(live_epic.get("cwd") or os.getcwd(), base)
        else:
            base_box[0] = base

        # Refuse to orphan an already-shipped group child unless --force.
        orphans = find_orphans(graph_entries, epic_resolved_id, base, keep_slugs)
        shipped_orphans = [o for o in orphans if is_shipped(o)]
        if shipped_orphans and not force:
            ids = ", ".join(o["id"] for o in shipped_orphans)
            raise DecomposeError(
                f"re-decomposition would orphan already-shipped group node(s): {ids}. "
                "Re-run with --force to proceed, or keep their #group slugs.",
                exit_code=2,
            )

        # Pass 1: resolve each group to an existing or new child node.
        slug_to_id: dict[str, str] = {}
        plan_to_group: list[tuple[dict, dict]] = []  # (node, normalized group)
        for grp in norm:
            cpath = child_plan_path(base, grp["slug"])
            existing = next(
                (
                    e
                    for e in graph_entries
                    if e.get("parent") == epic_resolved_id
                    and e.get("plan_path") == cpath
                ),
                None,
            )
            if existing is not None:
                action = "updated"
                node = existing
            else:
                action = "created"
                node = _build_backlog_node(
                    title=grp["title"],
                    parent=epic_resolved_id,
                    project=live_epic.get("project"),
                    cwd=live_epic.get("cwd"),
                    priority=live_epic.get("priority", "p2"),
                    domain=live_epic.get("domain", "code"),
                    plan_path=cpath,
                )
                node["id"] = mint_node_id({e.get("id") for e in graph_entries})
                # Reuse the parent-setter's cycle detection on this path too
                # (plan Invariants, line 88). A freshly minted id cannot be an
                # ancestor of the epic, so this never trips for new nodes today;
                # it guards future paths that re-parent an existing node.
                if _would_create_cycle(graph_entries, node["id"], epic_resolved_id):
                    raise DecomposeError(
                        f"parenting group {grp['slug']!r} to {epic_resolved_id} would create a cycle",
                        exit_code=2,
                    )
                graph_entries.append(node)
            slug_to_id[grp["slug"]] = node["id"]
            plan_to_group.append((node, grp))
            results.append(
                {
                    "id": node["id"],
                    "slug": grp["slug"],
                    "waves": grp["waves"],
                    "action": action,
                }
            )

        # Pass 2: set titles + inter-group blocked_by now that all ids exist.
        for (node, grp), r in zip(plan_to_group, results):
            node["title"] = grp["title"]
            # Set details unconditionally so a re-decompose that clears a
            # group's waves does not leave stale wave metadata behind.
            node["details"] = (
                f"Waves {grp['waves']} of epic {epic_resolved_id}" if grp["waves"] else None
            )
            node["blocked_by"] = [slug_to_id[d] for d in grp["blocked_by_groups"]]
            r["blocked_by"] = list(node["blocked_by"])

        # Surface any unshipped orphans (slug dropped from the spec). They are
        # left in place, not deleted - deleting graph nodes is destructive.
        orphan_box[0] = [o["id"] for o in orphans]
        return graph_entries

    orphan_box: list[list[str]] = [[]]
    base_box: list = [None]
    try:
        locked_mutate_graph(_graph_path(), mutator)
    except DecomposeError as e:
        emit_error(ctx, str(e))
        raise typer.Exit(code=e.exit_code)

    epic_resolved_id = epic_id_box[0]
    orphan_ids = orphan_box[0]

    # 4. Report what happened (AC1-UI).
    if json_mode(ctx):
        typer.echo(json.dumps(
            {"epic": epic_resolved_id, "groups": results, "orphaned": orphan_ids},
            default=str,
        ))
    else:
        typer.echo(f"epic: {epic_resolved_id}")
        typer.echo(f"decomposed into {len(results)} group child node(s):")
        for r in results:
            waves = f" waves {r['waves']}" if r["waves"] else ""
            blk = f" blocked_by={r['blocked_by']}" if r["blocked_by"] else ""
            typer.echo(f"  {r['action']}: {r['id']} (#group-{r['slug']}){waves}{blk}")
        if orphan_ids:
            typer.echo(
                f"warning: {len(orphan_ids)} group child node(s) no longer in the spec, "
                f"left in place: {', '.join(orphan_ids)}",
                err=True,
            )

    # 5. Record the group count N on the shared epic doc so it graduates only
    #    after all N group PRs ship (not after the first). The graph mutation
    #    above is the source of truth and is NEVER rolled back; decompose also
    #    never exits non-zero on a stamp problem, because that would break
    #    pipelines (e.g. /blueprint group) for a best-effort stamp. A genuine
    #    write failure (the doc exists but could not be written) is surfaced as
    #    a loud, actionable stderr warning so it is not silent; environment
    #    skips (absent doc/script - which also can't be stamped at ship, so no
    #    early graduation) stay quiet.
    base = base_box[0]
    expected_count = len(results)
    if base and expected_count >= 1:
        status, detail = _set_expected_count(base, expected_count)
        if status == "failed":
            typer.echo(
                f"warning: could not record expected_url_count={expected_count} on "
                f"{base}: {detail}. The shared doc will graduate after the FIRST "
                f"group ships unless you run: fno plan set-expected --plan-path "
                f"{base} --count {expected_count}",
                err=True,
            )
        # status == "skipped": the doc or script is absent (an environment
        # condition that cannot cause early graduation - target can't stamp it
        # either). Proceed silently; the graph mutation already succeeded.


# -- intake --

def _intake_impl(
    plan_paths: Optional[List[str]] = None,
    from_list: Optional[str] = None,
    roadmap_id: Optional[str] = None,
    title: Optional[str] = None,
    priority: Optional[str] = None,
    deps: Optional[str] = None,
    points: Optional[int] = None,
    project: Optional[str] = None,
    force_new_roadmap: bool = False,
    batch: bool = False,
    force_batch: bool = False,
    dry_run: bool = False,
    claims: Optional[str] = None,
) -> None:
    """Implementation for the intake verb.

    Pulls an existing plan file into the backlog as a new node. Typer-parameter
    defaults are intentionally plain Python values here so the thin command
    wrapper can pass through already-parsed arguments. Kept as a separate
    `_intake_impl` (rather than inlined into `cmd_intake`) so the underlying
    `_intake.py` helpers can be exercised by tests without going through Typer.
    """
    from fno.graph._constants import PRIORITY_ORDER
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import (
        _prepare_intake, _build_intake_node,
        _collect_intake_paths, _validate_cli_deps, _match_plan_in_graph,
    )

    # Reject removed --batch flag
    if batch:
        typer.echo(
            "Error: `--batch` was removed. Use multi-path intake instead:\n"
            "  fno backlog intake plans/a.md plans/b.md plans/c.md\n"
            "  fno backlog intake plans/folder/*.md  # shell glob",
            err=True,
        )
        raise typer.Exit(code=1)

    # Build args-like namespace for reuse of shared intake logic
    class _Args:
        pass

    args = _Args()
    args.roadmap_id = roadmap_id
    args.title = title
    args.priority = priority
    args.deps = deps
    args.points = points
    args.force_new_roadmap = force_new_roadmap
    args.force_batch = force_batch
    args.dry_run = dry_run
    args.batch = False
    args.from_list = from_list
    args.plan_paths = plan_paths or []
    args.project = project

    if project is not None and (not isinstance(project, str) or not project.strip()):
        typer.echo("Error: --project must be a non-empty string", err=True)
        raise typer.Exit(code=1)

    if args.priority and args.priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{args.priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    all_paths = _collect_intake_paths_typer(plan_paths or [], from_list)
    if not all_paths:
        if from_list:
            label = "stdin" if from_list == "-" else from_list
            typer.echo(
                f"Error: --from {label} produced 0 usable paths "
                "(blank lines and '#' comments are skipped).",
                err=True,
            )
        else:
            typer.echo(
                "Error: no plan paths provided. Pass one or more positional "
                "arguments, or use --from FILE (or --from -).",
                err=True,
            )
        raise typer.Exit(code=1)

    if len(all_paths) > 1:
        _do_intake_multi(
            args, all_paths,
            roadmap_id=roadmap_id, batch_mode=False, dry_run=dry_run,
        )
        return

    # Single-path flow
    plan_path = all_paths[0]

    cli_deps: list[str] = (
        [d.strip() for d in deps.split(",") if d.strip()] if deps else []
    )

    entries = read_graph(_graph_path())

    if roadmap_id and not force_new_roadmap:
        has_roadmap = any(e.get("roadmap_id") == roadmap_id for e in entries)
        if not has_roadmap:
            typer.echo(
                f"unknown roadmap_id: {roadmap_id} "
                "(use /megawalk vision.md to create a roadmap first, "
                "pass --force-new-roadmap, or omit --roadmap-id to intake to the backlog)",
                err=True,
            )
            raise typer.Exit(code=2)

    _validate_cli_deps(cli_deps, entries)

    try:
        prep = _prepare_intake(
            plan_path, entries,
            roadmap_id=roadmap_id, cli_title=title,
            cli_priority=priority, cli_deps=cli_deps, cli_points=points,
            cli_project=project,
            cli_claim=claims,
        )
    except ValueError as e:
        typer.echo(f"Error: {e}", err=True)
        raise typer.Exit(code=1)
    if prep["status"] == "already":
        typer.echo(f'already intaked: {prep["id"]}')
        return

    spec = prep["node_spec"]

    if dry_run:
        verb = "claim" if prep["status"] == "claim" else "intake"
        typer.echo(f"{verb.capitalize()} preview (dry-run, no changes):")
        target = f' (claims {prep["id"]})' if prep["status"] == "claim" else ""
        typer.echo(f'  would {verb}: "{spec["title"]}"  (plan: {plan_path}){target}')
        if spec["deps"]:
            typer.echo(f'  blocked_by: {", ".join(spec["deps"])}')
        return

    # Emit "not in ledger" warning before mutating
    from fno.graph._intake import _lookup_ledger_entry
    if _lookup_ledger_entry(plan_path) is None:
        typer.echo("plan_path not in ledger.json - intake will continue anyway", err=True)

    if prep["status"] == "claim":
        claim_id = prep["id"]
        claim_source = prep["claim_source"]

        def claim_mutator(es):
            from fno.graph._intake import resolve_node_project_and_cwd

            for entry in es:
                if entry.get("id") != claim_id:
                    continue
                entry["plan_path"] = plan_path
                entry["title"] = spec["title"]
                if spec["deps"]:
                    merged = list(
                        dict.fromkeys([*entry.get("blocked_by", []), *spec["deps"]])
                    )
                    entry["blocked_by"] = merged
                # Only override priority if the plan supplied a non-default one.
                if spec.get("priority") and spec["priority"] != "p2":
                    entry["priority"] = spec["priority"]
                if spec.get("points") is not None:
                    entry["points"] = spec["points"]
                # Backfill project/cwd when the node was created via
                # `fno backlog new` (no plan path -> no auto-scope) and is
                # now being claimed by a plan that lives in a project repo.
                # Only fills nulls; never overwrites existing values.
                if entry.get("project") is None or entry.get("cwd") is None:
                    resolved_project, resolved_cwd, _ = resolve_node_project_and_cwd(
                        plan_path, project, es,
                    )
                    if entry.get("project") is None and resolved_project:
                        entry["project"] = resolved_project
                    if entry.get("cwd") is None and resolved_cwd:
                        entry["cwd"] = resolved_cwd
                # Promote idea -> ready by clearing any stale claimed_at.
                # _status is recomputed by recompute_statuses on the next read.
                entry["claimed_at"] = None
                break
            return es

        locked_mutate_graph(_graph_path(), claim_mutator)
        typer.echo(
            f'claimed {claim_id} via {claim_source}: "{spec["title"]}"'
        )
        return

    new_id_holder: list[Optional[str]] = [None]

    def mutator(es):
        node = _build_intake_node(spec, es)
        new_id_holder[0] = node["id"]
        es.append(node)
        return es

    locked_mutate_graph(_graph_path(), mutator)
    destination = roadmap_id if roadmap_id else "backlog"
    typer.echo(f'intake {new_id_holder[0]} -> {destination}: "{spec["title"]}"')

    try:
        from fno.graph._intake import (
            _warn_unknown_project, _find_node, _warn_similar_idea_titles,
        )
        post_entries = read_graph(_graph_path())
        node = _find_node(post_entries, new_id_holder[0] or "")
        landed_project = node.get("project") if node else None
        _warn_unknown_project(landed_project)
        # Safety net: if the new node strongly resembles an existing idea,
        # warn so the author can re-run with --claims to consolidate.
        _warn_similar_idea_titles(
            spec["title"], new_id_holder[0] or "", post_entries,
        )
    except Exception as e:
        # The mutation already committed; a stray failure in the warning
        # path must not surface as if the intake itself failed.
        sys.stderr.write(f"warning: post-intake project check failed: {e}\n")


@cli.command("intake")
def cmd_intake(
    plan_paths: Optional[List[str]] = typer.Argument(default=None, help="Plan paths"),
    from_list: Optional[str] = typer.Option(None, "--from", help="Read paths from FILE or '-' for stdin"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id", help="Target roadmap ID"),
    title: Optional[str] = typer.Option(None, "--title", "-t", help="Override derived title"),
    priority: Optional[str] = typer.Option(None, "--priority", "-p", help="p0|p1|p2|p3"),
    deps: Optional[str] = typer.Option(None, help="Comma-separated ab-IDs"),
    points: Optional[int] = typer.Option(None, help="Story point estimate"),
    project: Optional[str] = typer.Option(None, "--project", help="Override the project field (beats frontmatter and cwd inference)"),
    force_new_roadmap: bool = typer.Option(False, "--force-new-roadmap"),
    batch: bool = typer.Option(False, "--batch", hidden=True),
    force_batch: bool = typer.Option(False, "--force-batch"),
    dry_run: bool = typer.Option(False, "--dry-run", "-N"),
    claims: Optional[str] = typer.Option(
        None, "--claims",
        help=(
            "ab-XXXXXXXX of an existing idea-state node this plan implements. "
            "Updates the node in place rather than creating a new one. "
            "Beats any frontmatter 'claims:' value."
        ),
    ),
) -> None:
    """Pull in an existing plan file as a backlog node."""
    _intake_impl(
        plan_paths=plan_paths,
        from_list=from_list,
        roadmap_id=roadmap_id,
        title=title,
        priority=priority,
        deps=deps,
        points=points,
        project=project,
        force_new_roadmap=force_new_roadmap,
        batch=batch,
        force_batch=force_batch,
        dry_run=dry_run,
        claims=claims,
    )


# -- update --

@cli.command("update")
def cmd_update(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    completed: bool = typer.Option(False, "--completed", help="Mark as completed"),
    locked_by: Optional[str] = typer.Option(None, "--locked-by", help="Session ID ('null' to release)"),
    has_brief: Optional[str] = typer.Option(None, "--has-brief", help="Set has_brief flag"),
    plan_path: Optional[str] = typer.Option(None, "--plan-path", help="Plan directory path"),
    pr_number: Optional[str] = typer.Option(None, "--pr-number", help="PR number"),
    pr_url: Optional[str] = typer.Option(None, "--pr-url", help="PR URL"),
    merge_status: Optional[str] = typer.Option(None, "--merge-status", help="Merge status"),
    priority: Optional[str] = typer.Option(None, "--priority", "-p", help="New priority"),
    title: Optional[str] = typer.Option(None, "--title", "-t", help="Update display title"),
    project: Optional[str] = typer.Option(None, "--project", help="Reproject this node (use for migrating wrong-scope nodes)"),
    cwd: Optional[str] = typer.Option(None, "--cwd", "-c", help="Update cwd (pair with --project for migration)"),
    blocked_by: Optional[List[str]] = typer.Option(None, "--blocked-by", help="Replace blocked_by list"),
    add_blocker: Optional[List[str]] = typer.Option(None, "--add-blocker", help="Append blocker IDs"),
    remove_blocker: Optional[List[str]] = typer.Option(None, "--remove-blocker", help="Remove blocker IDs"),
    acknowledge_collisions: Optional[str] = typer.Option(
        None,
        "--acknowledge-collisions",
        help="Comma-separated ab-IDs of collisions deliberately accepted. Pass '__skipped_check__' to record a skipped check.",
    ),
    parent: Optional[str] = typer.Option(
        None,
        "--parent",
        help="Set parent node ID. Pass 'null' to clear (de-orphan to top-level). Validates target exists and rejects cycles.",
    ),
    completion_note: Optional[str] = typer.Option(
        None,
        "--completion-note",
        help="Append free-text completion note. Multiple calls append with ' + ' separator. Whitespace-only is a no-op. Pass 'null' to clear.",
    ),
    add_pr: Optional[int] = typer.Option(
        None,
        "--add-pr",
        help="Append a follow-up PR number to additional_prs. Pair with --add-pr-url / --add-pr-note for context. Re-adding an existing number updates that entry in place.",
    ),
    add_pr_url: Optional[str] = typer.Option(
        None,
        "--add-pr-url",
        help="URL for the --add-pr entry (optional).",
    ),
    add_pr_note: Optional[str] = typer.Option(
        None,
        "--add-pr-note",
        help="One-line note for the --add-pr entry (optional).",
    ),
    remove_pr: Optional[int] = typer.Option(
        None,
        "--remove-pr",
        help="Remove a PR entry from additional_prs by number. No-op if absent. The primary pr_number is unaffected (use --pr-number to change that).",
    ),
) -> None:
    from fno.graph._constants import PRIORITY_ORDER, has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import (
        _parse_blocker_list,
        _validate_blocker_ids,
        _find_node,
        _would_create_cycle,
    )

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    if priority is not None and priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    if project is not None and (not isinstance(project, str) or not project.strip()):
        typer.echo("Error: --project must be a non-empty string", err=True)
        raise typer.Exit(code=1)

    # Derive cwd from the work-map when --project is explicit but --cwd was
    # not given. Do this OUTSIDE the mutator so settings reads never happen
    # under the graph lock.
    derived_cwd_for_update: Optional[str] = None
    if project is not None and cwd is None:
        from fno.graph._intake import project_root_from_settings
        workmap_root = project_root_from_settings(project)
        if workmap_root is not None:
            derived_cwd_for_update = workmap_root
        else:
            typer.echo(
                f"warning: project '{project}' not in any settings.yaml work-map; cwd left unchanged",
                err=True,
            )

    if cwd is not None:
        if not isinstance(cwd, str) or not cwd.strip():
            typer.echo("Error: --cwd must be a non-empty string", err=True)
            raise typer.Exit(code=1)
        cwd = os.path.abspath(os.path.expanduser(cwd))

    replace_blockers = _parse_blocker_list(blocked_by)
    add_blockers = _parse_blocker_list(add_blocker)
    remove_blockers = _parse_blocker_list(remove_blocker)
    has_blocker_edit = bool(blocked_by is not None or add_blockers or remove_blockers)

    if blocked_by is not None and (add_blockers or remove_blockers):
        typer.echo(
            "Error: --blocked-by is mutually exclusive with --add-blocker/--remove-blocker",
            err=True,
        )
        raise typer.Exit(code=2)

    if add_pr is None and (add_pr_url is not None or add_pr_note is not None):
        typer.echo(
            "Error: --add-pr-url and --add-pr-note require --add-pr",
            err=True,
        )
        raise typer.Exit(code=2)

    def mutator(entries):
        node = _find_node(entries, task_id)
        if node is None:
            typer.echo(f"Error: graph node {task_id} not found", err=True)
            raise typer.Exit(code=1)

        if has_blocker_edit:
            if blocked_by is not None:
                desired = list(dict.fromkeys(replace_blockers))
                _validate_blocker_ids(desired, entries, task_id)
                node["blocked_by"] = desired
            else:
                current = list(node.get("blocked_by", []))
                _validate_blocker_ids(add_blockers, entries, task_id)
                for b in add_blockers:
                    if b not in current:
                        current.append(b)
                current = [b for b in current if b not in remove_blockers]
                node["blocked_by"] = current

        if completed:
            node["completed_at"] = datetime.now(timezone.utc).isoformat()
        if locked_by is not None:
            session = locked_by if locked_by != "null" else None
            node["session_id"] = session
            node["claimed_at"] = datetime.now(timezone.utc).isoformat() if session else None
        if has_brief is not None:
            node["has_brief"] = has_brief.lower() == "true"
        if plan_path is not None:
            node["plan_path"] = plan_path
        if pr_number is not None:
            node["pr_number"] = int(pr_number)
        if pr_url is not None:
            node["pr_url"] = pr_url
        if merge_status is not None:
            node["merge_status"] = merge_status
        if priority is not None:
            node["priority"] = priority
        if project is not None:
            node["project"] = project
        if cwd is not None:
            node["cwd"] = cwd
        elif derived_cwd_for_update is not None:
            node["cwd"] = derived_cwd_for_update
        if title is not None:
            new_title = title.strip()
            if not new_title:
                typer.echo("Error: --title cannot be empty or whitespace-only", err=True)
                raise typer.Exit(code=1)
            node["title"] = new_title
        if acknowledge_collisions is not None:
            ids = [x.strip() for x in acknowledge_collisions.split(",") if x.strip()]
            node["collisions_acknowledged"] = ids
        if completion_note is not None:
            if completion_note.lower() == "null":
                node["completion_note"] = None
            else:
                new_note = completion_note.strip()
                if new_note:
                    existing = node.get("completion_note")
                    if existing and str(existing).strip():
                        node["completion_note"] = f"{existing} + {new_note}"
                    else:
                        node["completion_note"] = new_note
        if add_pr is not None:
            existing_list = list(node.get("additional_prs") or [])
            entry = {"number": int(add_pr)}
            if add_pr_url is not None:
                entry["url"] = add_pr_url
            if add_pr_note is not None:
                entry["note"] = add_pr_note
            replaced = False
            for i, item in enumerate(existing_list):
                if isinstance(item, dict) and item.get("number") == entry["number"]:
                    merged = dict(item)
                    merged.update(entry)
                    existing_list[i] = merged
                    replaced = True
                    break
            if not replaced:
                existing_list.append(entry)
            node["additional_prs"] = existing_list
        if remove_pr is not None:
            existing_list = list(node.get("additional_prs") or [])
            node["additional_prs"] = [
                item for item in existing_list
                if not (isinstance(item, dict) and item.get("number") == int(remove_pr))
            ]
        if parent is not None:
            if parent.lower() == "null":
                node["parent"] = None
            else:
                target = _find_node(entries, parent)
                if target is None:
                    typer.echo(f"Error: parent node {parent} not found", err=True)
                    raise typer.Exit(code=1)
                if _would_create_cycle(entries, node["id"], target["id"]):
                    typer.echo(
                        f"Error: setting parent of {node['id']} to {target['id']} "
                        f"would create a cycle",
                        err=True,
                    )
                    raise typer.Exit(code=1)
                node["parent"] = target["id"]
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"Updated {task_id}")


# -- next --

@cli.command("next")
def cmd_next(
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
    parent: Optional[str] = typer.Option(
        None,
        "--parent",
        help="Restrict to transitive children of this epic node (ab-ID).",
    ),
    claim: Optional[str] = typer.Option(None, "--claim", help="Session ID to atomically claim"),
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Filter by project name"),
    all_: bool = typer.Option(False, "--all", "-A", help="Consider all projects"),
    include_ideas: bool = typer.Option(
        False,
        "--ideas",
        "-I",
        "--include-ideas",
        help="Also consider idea-stage rows (plan-less nodes) as claimable.",
    ),
    include_deferred: bool = typer.Option(
        False,
        "--include-deferred",
        help="Also consider deferred rows for explicit re-engagement.",
    ),
    mission: Optional[str] = typer.Option(
        None,
        "--mission",
        help=(
            "Restrict to nodes whose mission_id matches (megatron child walks: "
            "the walk works ONLY the mission's nodes)."
        ),
    ),
) -> None:
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import (
        detect_project, filter_by_project, make_selection_sort_key,
        descendants_of, _find_node,
    )

    result: list = [None]
    project_filter = project
    # Read the graph at most once for project detection AND parent
    # resolution (both need the full entry list).
    pre_entries = None
    if (not project_filter and not all_) or parent:
        pre_entries = read_graph(_graph_path())
    if not project_filter and not all_:
        project_filter = detect_project(pre_entries)

    # Epic-scope filter (C2, ab-facfaade): restrict candidates to the
    # transitive children of --parent. Resolve the parent id up-front so a
    # missing node is a hard error (AC2-ERR) and a childless node prints a
    # clear note while still returning null so the walker can fall back
    # (AC2-EDGE). The actual descendant SET is computed inside _pick_ready
    # from the entries it receives so that under --claim it reflects the
    # locked graph state, not a pre-read snapshot (avoids a TOCTOU where a
    # concurrent reparent could claim a node no longer in the subtree).
    parent_target_id: Optional[str] = None
    if parent:
        target = _find_node(pre_entries, parent)
        if target is None:
            typer.echo(f"Error: no such node '{parent}'", err=True)
            raise typer.Exit(code=1)
        parent_target_id = target["id"]
        if not descendants_of(pre_entries, parent_target_id):
            typer.echo(f"no children under {parent_target_id}", err=True)

    allowed = {"ready"}
    if include_ideas:
        allowed.add("idea")
    if include_deferred:
        allowed.add("deferred")

    def _pick_ready(entries):
        candidates = [e for e in entries if e.get("_status") in allowed]
        if roadmap_id:
            candidates = [e for e in candidates if e.get("roadmap_id") == roadmap_id]
        if mission:
            candidates = [e for e in candidates if e.get("mission_id") == mission]
        if parent_target_id is not None:
            scope = descendants_of(entries, parent_target_id)
            candidates = [e for e in candidates if e.get("id") in scope]
        candidates = filter_by_project(candidates, project_filter, all_)
        # Selection-time claim enforcement (ab-fcf9cec5): drop nodes a live
        # session already holds so a second pickup is impossible.
        claimed = _live_claimed_node_ids()
        if claimed:
            candidates = [e for e in candidates if e.get("id") not in claimed]
        # Drop READY nodes that already carry an unmerged open PR so a successor
        # dispatch (advance / megawalk, both shelling `fno backlog next`) never
        # re-builds already-PR'd work (ab-372130f6). The PID-based node claim
        # is gone once the builder session exits, so this PR-state guard is the
        # only in-flight signal left during the review window.
        #
        # Scoped to _status "ready": a deferred/idea row only appears here when
        # an operator explicitly asked for it via --include-deferred /
        # --include-ideas, and the defer contract says those resurface on
        # request. The guard is about AUTO re-selection of fresh ready work, not
        # explicit re-engagement, so it must not suppress an explicitly-included
        # paused PR-bearing node (codex PR #516 P2). The auto paths only ever
        # pass bare `next` (allowed == {"ready"}), so this scoping is a no-op
        # for them and the originally-observed bug node (ready + open PR) is
        # still caught.
        candidates = [
            e for e in candidates
            if e.get("_status") != "ready" or not _has_unmerged_open_pr(e)
        ]
        # Epics-first, then flat priority (C3, Locked Decision 7). Build the
        # key from the FULL graph so epic parents resolve even when filtered
        # out of the candidate set.
        candidates.sort(key=make_selection_sort_key(entries))
        return candidates

    def _node_summary(e):
        return {
            # slug leads (ab-f82e8083); `id` stays the canonical key right after.
            "slug": e.get("slug"),
            "id": e["id"], "title": e.get("title"),
            "priority": e.get("priority"), "domain": e.get("domain"),
            "project": e.get("project"), "cwd": e.get("cwd"),
            "size": e.get("size"), "plan_path": e.get("plan_path"),
            "mission_id": e.get("mission_id"),
            "mission_wave": e.get("mission_wave"),
            "mission_slug": e.get("mission_slug"),
            "mission_from_msg_id": e.get("mission_from_msg_id"),
        }

    if claim:
        def mutator(entries):
            candidates = _pick_ready(entries)
            if candidates:
                winner = candidates[0]
                winner["session_id"] = claim
                winner["claimed_at"] = datetime.now(timezone.utc).isoformat()
                result[0] = _node_summary(winner)
            return entries
        locked_mutate_graph(_graph_path(), mutator)
    else:
        entries = read_graph(_graph_path())
        candidates = _pick_ready(entries)
        if candidates:
            result[0] = _node_summary(candidates[0])

    typer.echo(json.dumps(result[0], indent=2) if result[0] else "null")


# -- ready --

@cli.command("ready")
def cmd_ready(
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Filter by project name"),
    all_: bool = typer.Option(False, "--all", "-A", help="Show all projects"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
    parent: Optional[str] = typer.Option(
        None,
        "--parent",
        help="Restrict to transitive children of this epic node (ab-ID).",
    ),
    include_ideas: bool = typer.Option(
        False,
        "--ideas",
        "-I",
        "--include-ideas",
        help="Also list idea-stage rows (plan-less nodes) alongside ready ones.",
    ),
    include_deferred: bool = typer.Option(
        False,
        "--include-deferred",
        help="Also list deferred rows for explicit re-engagement.",
    ),
) -> None:
    from fno.graph.store import read_graph
    from fno.graph._intake import (
        filter_by_project, make_selection_sort_key, descendants_of, _find_node,
    )

    entries = read_graph(_graph_path())
    allowed = {"ready"}
    if include_ideas:
        allowed.add("idea")
    if include_deferred:
        allowed.add("deferred")
    ready = [e for e in entries if e.get("_status") in allowed]
    ready = filter_by_project(ready, project, all_)
    if roadmap_id:
        ready = [e for e in ready if e.get("roadmap_id") == roadmap_id]
    # Epic-scope filter (C2, ab-facfaade): transitive children of --parent.
    if parent:
        target = _find_node(entries, parent)
        if target is None:
            typer.echo(f"Error: no such node '{parent}'", err=True)
            raise typer.Exit(code=1)
        scope = descendants_of(entries, target["id"])
        if not scope:
            typer.echo(f"no children under {target['id']}", err=True)
        ready = [e for e in ready if e.get("id") in scope]
    # Selection-time claim enforcement (ab-fcf9cec5): hide nodes a live
    # session already holds (same rule as `graph next`).
    claimed = _live_claimed_node_ids()
    if claimed:
        ready = [e for e in ready if e.get("id") not in claimed]
    # Same in-flight guard as `next` (ab-372130f6): a human / megawalk `ready`
    # listing must not present an already-PR'd node as actionable work.
    # cmd_ready keeps its own inline status/claim filter (it does not route
    # through _pick_ready), so the guard is applied here too for parity.
    # Scoped to _status "ready" so an explicitly --include-deferred / -ideas
    # paused PR-bearing node still lists (the defer contract resurfaces those
    # on request; codex PR #516 P2).
    ready = [
        e for e in ready
        if e.get("_status") != "ready" or not _has_unmerged_open_pr(e)
    ]
    # Epics-first, then flat priority (C3, Locked Decision 7); key built
    # from the full graph so epic parents always resolve.
    ready.sort(key=make_selection_sort_key(entries))

    output = [{
        # slug leads (ab-f82e8083) so a `ready` list / clipboard is readable.
        "slug": e.get("slug"),
        "id": e["id"], "title": e.get("title"), "priority": e.get("priority"),
        "domain": e.get("domain"), "project": e.get("project"),
        "cwd": e.get("cwd"), "parent": e.get("parent"),
    } for e in ready]

    typer.echo(json.dumps(output, indent=2))


# -- get --

@cli.command("get")
def cmd_get(
    id: str = typer.Argument(
        ...,
        help="Node ab-id, slug, or bare 8-hex (e.g. ab-ff6f96e0 | dashless-spawn | ff6f96e0)",
    ),
    field: Optional[str] = typer.Option(None, help="Print only this field"),
) -> None:
    from fno.graph.store import read_graph
    from fno.graph.fuzzy import resolve_node

    entries = read_graph(_graph_path())
    # Deterministic resolution tiers 1-3 (ab-f82e8083): exact ab-id, exact slug,
    # bare-8-hex re-prefix. A slug/bare-hex argument resolves to the same node
    # an ab-id would, so the spawn VALIDATE step (`fno backlog get "$node"`)
    # accepts every exact entry form.
    match = resolve_node(id, entries)
    if match.kind == "exact":
        e = match.candidates[0]
        from fno.graph._intake import project_root_from_settings
        root = project_root_from_settings(e["project"]) if e.get("project") else None
        e["_resolved_cwd"] = root or e.get("cwd")
        if field:
            value = e.get(field)
            if value is None:
                typer.echo("null")
            elif isinstance(value, (list, dict)):
                typer.echo(json.dumps(value))
            else:
                typer.echo(value)
        else:
            typer.echo(json.dumps(e, indent=2))
        return
    typer.echo(f"No node matching '{id}' (id/slug/bare-hex) in {_graph_path()}", err=True)
    raise typer.Exit(code=1)


# -- provenance --

@cli.command("provenance")
def cmd_provenance(
    id: str = typer.Argument(
        ...,
        help="Node ab-id, slug, or bare 8-hex",
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit machine-readable JSON instead of human summary"
    ),
) -> None:
    """Show provenance pointers for a node and resolve transcripts where possible.

    Reads two provenance edges stored on the node:

      node-birth edge  source_session_id + source_harness + source_cwd
      spawn edge       spawned_by_session + spawned_by_harness + spawned_by_cwd

    For each edge that carries a session id the resolver is run (claude only;
    codex/gemini/etc. return resolved=False). Read-only: no graph mutation.
    """
    from fno.graph.store import read_graph
    from fno.graph.fuzzy import resolve_node
    from fno.provenance.resolver import resolve_transcript, _DEFAULT_PROJECTS_ROOT

    entries = read_graph(_graph_path())
    match = resolve_node(id, entries)
    if match.kind != "exact":
        typer.echo(f"No node matching '{id}' in {_graph_path()}", err=True)
        raise typer.Exit(code=1)

    e = match.candidates[0]
    node_id = e["id"]

    # node-birth edge: resolve against the originating SESSION cwd
    # (source_cwd), NOT the node's durable project `cwd`. Claude transcript dirs
    # are slugged by the session cwd, so a node filed from a worktree resolves
    # only via source_cwd; fall back to `cwd` for legacy pre-source_cwd nodes.
    birth_session = e.get("source_session_id")
    birth_harness = e.get("source_harness")
    birth_cwd = e.get("source_cwd") or e.get("cwd")
    birth_result = None
    if birth_session:
        birth_result = resolve_transcript(
            birth_harness, birth_session, birth_cwd,
            projects_root=_DEFAULT_PROJECTS_ROOT,
        )

    # spawn edge: uses spawned_by_cwd
    spawn_session = e.get("spawned_by_session")
    spawn_harness = e.get("spawned_by_harness")
    spawn_cwd = e.get("spawned_by_cwd")
    spawn_result = None
    if spawn_session:
        spawn_result = resolve_transcript(
            spawn_harness, spawn_session, spawn_cwd,
            projects_root=_DEFAULT_PROJECTS_ROOT,
        )

    if json_out:
        import dataclasses

        def _edge(label: str, result) -> dict:
            if result is None:
                return {"edge": label, "session_id": None, "resolved": False}
            d = dataclasses.asdict(result)
            d["edge"] = label
            return d

        output = {
            "node_id": node_id,
            "title": e.get("title"),
            "edges": [
                _edge("node_birth", birth_result),
                _edge("spawn", spawn_result),
            ],
        }
        typer.echo(json.dumps(output, indent=2))
        return

    # Human-readable summary
    lines = [f"provenance for {node_id}: {e.get('title', '')}"]

    def _fmt_edge(label: str, result, session: Optional[str], harness: Optional[str]) -> None:
        if session is None:
            lines.append(f"  {label}: (none)")
            return
        lines.append(f"  {label}:")
        lines.append(f"    session:  {session}")
        lines.append(f"    harness:  {harness or '(unknown)'}")
        if result is None:
            lines.append("    transcript: (not resolved)")
        elif result.resolved:
            ambig = " [ambiguous match]" if result.ambiguous else ""
            lines.append(f"    transcript: {result.transcript_path}{ambig}")
        else:
            reason = result.reason or "not-found"
            lines.append(f"    transcript: (unresolved - {reason})")

    _fmt_edge("node-birth", birth_result, birth_session, birth_harness)
    _fmt_edge("spawn", spawn_result, spawn_session, spawn_harness)

    typer.echo("\n".join(lines))


# -- backfill-slugs --

@cli.command("backfill-slugs")
def cmd_backfill_slugs() -> None:
    """Assign a title-derived slug to every node lacking one (ab-f82e8083).

    A one-time, idempotent, lock-safe pass: existing slugs are never rewritten,
    so re-running changes nothing. (Every graph mutation also slugs new nodes
    automatically via ensure_slugs; this verb is the explicit operator trigger
    that backfills the whole legacy graph in one call.)
    """
    from fno.graph.store import locked_mutate_graph
    from fno.graph.slug import ensure_slugs

    assigned = [0]

    def mutator(entries):
        assigned[0] = ensure_slugs(entries)
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(json.dumps({"slugs_assigned": assigned[0]}, indent=2))


# -- view --

@cli.command("view")
def cmd_view() -> None:
    """Render the backlog as HTML and open it with the system's default handler.

    Always rerenders before opening so the file reflects current graph.json
    state even if the auto-render hook hasn't fired since the last edit. The
    file lives at ``~/.fno/graph.html`` and is opened via ``open`` on
    macOS, ``xdg-open`` on Linux, ``os.startfile`` on Windows - whichever
    handler the OS has registered for ``.html`` takes over from there
    (browser, yazi, anything else).

    Set ``FNO_NO_OPEN=1`` to skip the launch step and just print the path -
    useful for scripts, CI, and tests.
    """
    import platform
    import shutil
    import subprocess

    from fno.graph._constants import GRAPH_HTML
    from fno.graph.render_html import render_graph_html
    from fno.graph.store import read_graph

    entries = read_graph(_graph_path())
    render_graph_html(entries, GRAPH_HTML)
    typer.echo(str(GRAPH_HTML))

    if os.environ.get("FNO_NO_OPEN") == "1":
        return

    system = platform.system()
    try:
        if system == "Darwin":
            subprocess.run(["open", str(GRAPH_HTML)], check=False)
        elif system == "Windows":
            os.startfile(str(GRAPH_HTML))  # type: ignore[attr-defined]
        else:
            opener = shutil.which("xdg-open") or shutil.which("wslview")
            if opener:
                subprocess.run([opener, str(GRAPH_HTML)], check=False)
            else:
                typer.echo(
                    "No xdg-open / wslview found; file rendered but not opened.",
                    err=True,
                )
    except OSError as e:
        typer.echo(f"Could not launch opener: {e}", err=True)


# -- tree --

@cli.command("tree")
def cmd_tree(
    project: Optional[str] = typer.Option(None, help="Filter by project"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
) -> None:
    from fno.graph.store import read_graph
    from fno.graph._intake import _graph_sort_key_fn

    entries = read_graph(_graph_path())

    if project:
        entries = [e for e in entries if e.get("project") == project]
    if roadmap_id:
        entries = [e for e in entries if e.get("roadmap_id") == roadmap_id]

    if not entries:
        typer.echo("No graph entries found.")
        return

    STATUS_ICONS = {
        "done": "[x]",
        "claimed": "[~]",
        "ready": "[ ]",
        "blocked": "[B]",
        "idea": "[i]",
        "deferred": "[d]",
    }
    id_to_entry = {e["id"]: e for e in entries}
    children_map: dict = {}
    for e in entries:
        parent = e.get("parent")
        children_map.setdefault(parent, []).append(e)

    def print_tree(parent_id, indent=0):
        children = children_map.get(parent_id, [])
        children.sort(key=_graph_sort_key_fn)
        for child in children:
            icon = STATUS_ICONS.get(child.get("_status", "ready"), "[ ]")
            title = child.get("title", "?")
            eid = child["id"]
            prefix = "  " * indent
            suffix = ""
            if child.get("pr_number"):
                suffix += f" PR#{child['pr_number']}"
            if child.get("cost_usd"):
                suffix += f" ${child['cost_usd']:.2f}"
            typer.echo(f"{prefix}{icon} {eid} {title}{suffix}")
            print_tree(eid, indent + 1)

    roots = [e for e in entries if e.get("parent") is None or e.get("parent") not in id_to_entry]
    for root in sorted(roots, key=_graph_sort_key_fn):
        icon = STATUS_ICONS.get(root.get("_status", "ready"), "[ ]")
        title = root.get("title", "?")
        eid = root["id"]
        suffix = ""
        if root.get("pr_number"):
            suffix += f" PR#{root['pr_number']}"
        if root.get("cost_usd"):
            suffix += f" ${root['cost_usd']:.2f}"
        typer.echo(f"{icon} {eid} {title}{suffix}")
        print_tree(eid, 1)


# -- status --

@cli.command("status")
def cmd_status(
    project: Optional[str] = typer.Option(None, help="Filter by project"),
    all_: bool = typer.Option(False, "--all", "-A", help="Show all projects"),
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
) -> None:
    from fno.graph.store import read_graph
    from fno.graph._intake import detect_project

    entries = read_graph(_graph_path())

    if not entries:
        typer.echo("No graph entries found. Run /megawalk vision.md to generate a roadmap.")
        return

    if roadmap_id:
        entries = [e for e in entries if e.get("roadmap_id") == roadmap_id]

    if project:
        projects = {project: [e for e in entries if e.get("project") == project]}
    elif all_:
        projects: dict = {}
        for e in entries:
            proj = e.get("project") or "(no project)"
            projects.setdefault(proj, []).append(e)
    else:
        proj = detect_project(entries)
        if proj:
            projects = {proj: [e for e in entries if e.get("project") == proj]}
        else:
            projects = {"(all)": entries}

    global_done = 0
    global_total = 0
    global_cost = 0.0

    for proj_name, proj_entries in sorted(projects.items()):
        features = [e for e in proj_entries if e.get("type") == "feature"]
        done = sum(1 for e in features if e.get("_status") == "done")
        claimed = sum(1 for e in features if e.get("_status") == "claimed")
        ready = sum(1 for e in features if e.get("_status") == "ready")
        ideas = sum(1 for e in features if e.get("_status") == "idea")
        blocked = sum(1 for e in features if e.get("_status") == "blocked")
        deferred = sum(1 for e in features if e.get("_status") == "deferred")
        total = len(features)
        cost = sum(e.get("cost_usd", 0) or 0 for e in features)

        global_done += done
        global_total += total
        global_cost += cost

        if all_:
            ideas_suffix = f", ideas: {ideas}" if ideas else ""
            deferred_suffix = f", deferred: {deferred}" if deferred else ""
            typer.echo(
                f"\n=== {proj_name} ({done}/{total} done{ideas_suffix}{deferred_suffix}, ${cost:.2f}) ==="
            )
        else:
            typer.echo(f"Project: {proj_name}")
            roadmaps = [e for e in proj_entries if e.get("type") == "roadmap"]
            if roadmaps:
                typer.echo(f"Roadmap: {roadmaps[0].get('roadmap_id', '?')} ({roadmaps[0].get('title', '?')})")
            ideas_suffix = (
                f" | ideas: {ideas} (use 'fno backlog ready --ideas' to list)"
                if ideas else ""
            )
            # Active-most → inactive-most ordering: done | claimed | ready
            # | ideas | blocked | deferred. Deferred is the only state that
            # requires an explicit `--include-deferred` to re-surface, so it
            # belongs at the tail.
            deferred_suffix = (
                f" | deferred: {deferred} (use 'fno backlog ready --include-deferred' to list)"
                if deferred else ""
            )
            typer.echo(
                f"Progress: {done}/{total} done | {claimed} claimed | {ready} ready"
                f"{ideas_suffix} | {blocked} blocked{deferred_suffix}"
            )
            typer.echo(f"Cost: ${cost:.2f}")
            typer.echo("")

        typer.echo(f"{'ID':<14} {'Title':<30} {'Status':<10} {'Priority':<10} {'Cost':>8}  {'PR'}")
        typer.echo("-" * 85)
        for e in features:
            eid = e.get("id", "?")
            title = (e.get("title", "?"))[:28]
            st = e.get("_status", "?")
            pri = e.get("priority", "?")
            c = f"${e.get('cost_usd', 0) or 0:.2f}"
            pr = f"#{e.get('pr_number')}" if e.get("pr_number") else "-"
            typer.echo(f"{eid:<14} {title:<30} {st:<10} {pri:<10} {c:>8}  {pr}")

    if all_ and len(projects) > 1:
        typer.echo(f"\nTotal: {global_done}/{global_total} done, ${global_cost:.2f}")


# -- briefs --

@cli.command("briefs")
def cmd_briefs(
    limit: int = typer.Option(5, help="Number of briefs to load"),
) -> None:
    from fno.graph.store import read_graph

    entries = read_graph(_graph_path())
    done_with_briefs = [
        e for e in entries
        if e.get("_status") == "done" and e.get("has_brief")
    ]
    done_with_briefs.sort(key=lambda e: e.get("completed_at", ""), reverse=True)

    recent = done_with_briefs[:limit]
    older = done_with_briefs[limit:]
    briefs_dir = _briefs_dir()

    output = []
    for e in recent:
        brief_path = briefs_dir / f"{e['id']}.md"
        brief_text = ""
        if brief_path.exists():
            brief_text = brief_path.read_text().strip()
        output.append({
            "id": e["id"],
            "title": e.get("title"),
            "completed_at": e.get("completed_at"),
            "brief": brief_text,
        })

    for e in older:
        brief_path = briefs_dir / f"{e['id']}.md"
        summary = ""
        if brief_path.exists():
            lines = brief_path.read_text().strip().split("\n")
            summary = lines[0] if lines else ""
        output.append({
            "id": e["id"],
            "title": e.get("title"),
            "completed_at": e.get("completed_at"),
            "brief": f"(compressed) {summary}",
        })

    typer.echo(json.dumps(output, indent=2))


# -- validate --

@cli.command("validate")
def cmd_validate(
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
) -> None:
    from fno.graph.store import read_graph

    entries = read_graph(_graph_path())
    if roadmap_id:
        entries = [e for e in entries if e.get("roadmap_id") == roadmap_id]

    if not entries:
        typer.echo("No entries to validate.")
        return

    errors = []
    warnings = []
    id_set = {e["id"] for e in entries}

    for e in entries:
        for blocker_id in e.get("blocked_by", []):
            if blocker_id not in id_set:
                errors.append(f"{e['id']} blocked by unknown node {blocker_id}")

    id_to_entry = {e["id"]: e for e in entries}
    visited: set = set()
    rec_stack: set = set()

    def _has_cycle(node_id):
        visited.add(node_id)
        rec_stack.add(node_id)
        node = id_to_entry.get(node_id)
        if node:
            for blocker_id in node.get("blocked_by", []):
                if blocker_id not in visited:
                    if _has_cycle(blocker_id):
                        return True
                elif blocker_id in rec_stack:
                    errors.append(f"Circular dependency involving {node_id} and {blocker_id}")
                    return True
        rec_stack.discard(node_id)
        return False

    for e in entries:
        if e["id"] not in visited:
            _has_cycle(e["id"])

    seen_ids: set = set()
    for e in entries:
        eid = e.get("id")
        if eid in seen_ids:
            errors.append(f"Duplicate ID: {eid}")
        seen_ids.add(eid)

    for e in entries:
        parent = e.get("parent")
        if parent and parent not in id_set:
            warnings.append(f"{e['id']} has parent {parent} not in graph")

    if errors:
        typer.echo(f"ERRORS ({len(errors)}):")
        for err in errors:
            typer.echo(f"  - {err}")
    if warnings:
        typer.echo(f"WARNINGS ({len(warnings)}):")
        for warn in warnings:
            typer.echo(f"  - {warn}")
    if not errors and not warnings:
        typer.echo(f"OK: {len(entries)} entries, no issues found.")

    if errors:
        raise typer.Exit(code=1)


# -- cost --

@cli.command("cost")
def cmd_cost(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    session: Optional[str] = typer.Option(None, "--session-id", help="Session ID"),
    session_legacy: Optional[str] = typer.Option(
        None, "--session", hidden=True, help="[DEPRECATED] alias for --session-id."
    ),
    amount: str = typer.Option(..., "--amount", help="Cost in USD"),
) -> None:
    import click

    from fno._flag_aliases import merge_deprecated_alias
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph

    session = merge_deprecated_alias(
        session, session_legacy, canonical_flag="--session-id", legacy_flag="--session"
    )
    # --session-id is required; the merge returns None only when NEITHER
    # spelling was passed (the hidden alias forces a None default here).
    if session is None:
        raise click.UsageError("Missing option '--session-id'.")

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    try:
        amount_f = float(amount)
    except ValueError:
        typer.echo(f"Error: amount must be a number, got '{amount}'", err=True)
        raise typer.Exit(code=1)

    def mutator(entries):
        for e in entries:
            if e.get("id") == task_id:
                sessions = e.get("cost_sessions", [])
                sessions.append({
                    "session_id": session,
                    "cost_usd": round(amount_f, 2),
                    "timestamp": datetime.now(timezone.utc).isoformat(),
                })
                e["cost_sessions"] = sessions
                e["cost_usd"] = round(sum(s["cost_usd"] for s in sessions), 2)
                return entries
        typer.echo(f"Error: feature {task_id} not found", err=True)
        raise typer.Exit(code=1)

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"Recorded ${amount_f:.2f} for {task_id} (session {session})")


# -- remove --

@cli.command("remove")
def cmd_remove(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    force: bool = typer.Option(False, "--force", "-F", help="Skip cascade warning"),
) -> None:
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import _find_node, _find_dependents

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    entries = read_graph(_graph_path())
    dependents = _find_dependents(entries, task_id)
    if dependents and not force:
        typer.echo(f"Removing {task_id} will orphan blocked_by in: {', '.join(dependents)}")
        typer.echo("Use --force to confirm.")
        raise typer.Exit(code=1)

    def mutator(entries):
        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        for e in entries:
            if task_id in e.get("blocked_by", []):
                e["blocked_by"].remove(task_id)
        return [e for e in entries if e.get("id") != task_id]

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"Removed {task_id}" + (f" (orphaned deps in {dependents})" if dependents else ""))


# -- defer / undefer --
#
# ``defer`` records a first-class pause on a backlog node via dedicated
# ``deferred_at`` + ``deferred_reason`` fields. The cascade derives
# ``_status: deferred`` from those fields so the node disappears from the
# default ``ready`` / ``next`` candidate sets and from triage proposals,
# but resurfaces with ``--include-deferred``. Reversal is via ``undefer``
# (idempotent: clearing already-clear state warns but exits 0).
#
# Predates the ``completed_at: "deferred:<ts>"`` workaround; ``recompute_statuses``
# auto-migrates the prefix to the new schema, so callers should never see
# the old shape after one mutation.

@cli.command("defer")
def cmd_defer(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    reason: str = typer.Option(
        ...,
        "--reason", "-R",
        help="Why this node is being deferred (free text, surfaced in triage).",
    ),
) -> None:
    """Mark a backlog node as deferred. Sets ``deferred_at`` + ``deferred_reason``."""
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node, _find_dependents

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    # Strip and validate the reason at the CLI boundary so direct invocation
    # cannot land an empty-reason deferral. The triage validator already
    # rejects blank reasons; matching that contract here keeps both write
    # paths producing identically-shaped graph state.
    cleaned_reason = reason.strip()
    if not cleaned_reason:
        typer.echo("Error: --reason cannot be blank", err=True)
        raise typer.Exit(code=1)

    def mutator(entries):
        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        dependents = _find_dependents(entries, task_id)
        if dependents:
            typer.echo(
                f"WARN: Deferring {task_id} blocks: {', '.join(dependents)}",
                err=True,
            )
        node["session_id"] = None
        node["claimed_at"] = None
        # Clear completed_at so the cascade can flip to deferred. Without
        # this, deferring an already-done node is a silent no-op because
        # the precedence ladder is `done > deferred` - completed_at would
        # keep _status pinned to done. Symmetric with cmd_done, which
        # clears deferred_at on the reverse transition.
        node["completed_at"] = None
        node["deferred_at"] = datetime.now(timezone.utc).isoformat()
        node["deferred_reason"] = cleaned_reason
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f'Deferred {task_id}: "{cleaned_reason}"')


# -- queue / unqueue / queued --
#
# ``queue`` is the user-facing triage marker for "I'm pulling this off
# the backlog and intend to work on it next" (e.g. "tomorrow I'm going
# to queue x, y, z"). Orthogonal to ``_status``: a queued node still has
# ``_status: ready`` so ``fno backlog ready`` keeps surfacing it. The
# kanban renderer reads ``queued_at`` separately and promotes the card
# into the Now column (between ``claimed`` and the priority-driven
# promotion rule).
#
# Cleared automatically by ``cmd_done``; reversible via ``unqueue``.

def _expand_id_args(raw_ids: list[str]) -> list[str]:
    """Flatten a list of CLI args into individual node IDs.

    Accepts both space-separated args (``ab-X ab-Y``) and comma-
    separated bundles (``ab-X,ab-Y``) so end-of-day batch triage feels
    natural: ``fno backlog queue ab-X,ab-Y ab-Z`` is valid. Preserves
    first-occurrence order, dedupes ALL repeats (a ``seen`` set drops
    any id already encountered, not just adjacent ones), strips
    whitespace.
    """
    out: list[str] = []
    seen: set[str] = set()
    for raw in raw_ids:
        for part in str(raw).split(","):
            tid = part.strip()
            if not tid:
                continue
            if tid in seen:
                continue
            seen.add(tid)
            out.append(tid)
    return out


@cli.command("queue")
def cmd_queue(
    task_ids: List[str] = typer.Argument(
        ...,
        help="Feature IDs (ab-XXXXXXXX). Multiple via space and/or comma: 'ab-X,ab-Y ab-Z'.",
    ),
    reason: Optional[str] = typer.Option(
        None,
        "--reason", "-R",
        help="Why these nodes are being queued (applies to all). Free text, surfaced on the card.",
    ),
) -> None:
    """Queue one or more backlog nodes for action. Sets ``queued_at`` + optional ``queued_reason``.

    Atomic across the batch: if any ID is unknown, none of the nodes
    are queued. Same reason applies to every ID in the batch.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    ids = _expand_id_args(task_ids)
    if not ids:
        typer.echo("Error: at least one task_id is required", err=True)
        raise typer.Exit(code=1)
    for tid in ids:
        if not has_node_id_prefix(tid):
            typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{tid}'", err=True)
            raise typer.Exit(code=1)

    cleaned_reason = (reason or "").strip() or None

    def mutator(entries):
        missing = [tid for tid in ids if _find_node(entries, tid) is None]
        if missing:
            typer.echo(
                f"Error: feature(s) not found: {', '.join(missing)}",
                err=True,
            )
            raise typer.Exit(code=1)
        now = datetime.now(timezone.utc).isoformat()
        for tid in ids:
            node = _find_node(entries, tid)
            node["queued_at"] = now
            node["queued_reason"] = cleaned_reason
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    suffix = f': "{cleaned_reason}"' if cleaned_reason else ""
    for tid in ids:
        typer.echo(f"Queued {tid}{suffix}")


@cli.command("unqueue")
def cmd_unqueue(
    task_ids: List[str] = typer.Argument(
        ...,
        help="Feature IDs (ab-XXXXXXXX). Multiple via space and/or comma: 'ab-X,ab-Y ab-Z'.",
    ),
) -> None:
    """Clear queued state on one or more backlog nodes. Idempotent.

    Atomic across the batch: if any ID is unknown, none are cleared.
    Reports each ID's prior state; warns (non-fatally) for IDs that
    were not actually queued.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    ids = _expand_id_args(task_ids)
    if not ids:
        typer.echo("Error: at least one task_id is required", err=True)
        raise typer.Exit(code=1)
    for tid in ids:
        if not has_node_id_prefix(tid):
            typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{tid}'", err=True)
            raise typer.Exit(code=1)

    not_queued: list[str] = []

    def mutator(entries):
        missing = [tid for tid in ids if _find_node(entries, tid) is None]
        if missing:
            typer.echo(
                f"Error: feature(s) not found: {', '.join(missing)}",
                err=True,
            )
            raise typer.Exit(code=1)
        for tid in ids:
            node = _find_node(entries, tid)
            if not node.get("queued_at"):
                not_queued.append(tid)
            node["queued_at"] = None
            node["queued_reason"] = None
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    for tid in not_queued:
        typer.echo(f"warning: {tid} was not queued", err=True)
    for tid in ids:
        typer.echo(f"Unqueued {tid}")


def _pick_format_line(entry: dict, id_to_entry: dict[str, dict] | None = None) -> str:
    """Format a graph entry as a single fzf row.

    Shape: ``[marker] kind priority project ab-id title [blockers]``.

    Marker semantics:
      ``[ ]`` ready / idea, not queued
      ``[Q]`` queued (already on tomorrow's plate)
      ``[B]`` blocked by an open dependency, not queued
      ``[Q!]`` queued AND blocked (will fire once unblocked)

    Kind column: ``plan`` if a plan_path exists, else ``idea``.
    Useful to spot pre-plan rows that need /think+/blueprint before
    target can pick them up.

    For blocked rows, the open blocker IDs are appended to the title so
    you can see at a glance why something is gated - useful when you
    want to queue A and the nodes it blocks together.
    """
    is_queued = bool(entry.get("queued_at"))
    is_blocked = entry.get("_status") == "blocked"
    if is_queued and is_blocked:
        marker = "[Q!]"
    elif is_queued:
        marker = "[Q]"
    elif is_blocked:
        marker = "[B]"
    else:
        marker = "[ ]"
    kind = "plan" if entry.get("plan_path") else "idea"
    prio = entry.get("priority") or "p2"
    project = (entry.get("project") or "-")
    if len(project) > 22:
        project = project[:21] + "."
    title = (entry.get("title") or "").replace("\n", " ").strip() or "(untitled)"
    if len(title) > 75:
        title = title[:74] + "."
    blocker_suffix = ""
    if is_blocked and id_to_entry is not None:
        open_blockers: list[str] = []
        for bid in entry.get("blocked_by", []) or []:
            if not isinstance(bid, str):
                continue
            b = id_to_entry.get(bid)
            if b and not b.get("completed_at"):
                open_blockers.append(bid)
        if open_blockers:
            blocker_suffix = f"  (blocked by {','.join(open_blockers)})"
    marker_col = f"{marker:5s}"  # pad to width 5 so [Q!] doesn't shift columns
    return f"{marker_col} {kind}  {prio}  {project:22s}  {entry['id']}  {title}{blocker_suffix}"


def _pick_extract_id(line: str) -> str | None:
    """Recover the node ID from a picker row. None if not found.

    Extraction from a free-form row, so use the STRICT well-formed matcher (not
    the liberal prefix pre-check) - otherwise a non-id token that merely starts
    with the prefix (e.g. a project name ``fno-cli``) would be misread as an id.
    """
    from fno.graph._constants import is_wellformed_node_id

    for tok in line.split():
        if is_wellformed_node_id(tok):
            return tok
    return None


def _tsv_safe(s: str | None) -> str:
    """Strip TSV-breaking characters from a candidate field."""
    if not s:
        return ""
    return str(s).replace("\t", " ").replace("\n", " ").replace("\r", " ")


_PICK_RENDER_AWK = r"""
# Reads two files:
#   ARGV[1] = pending.txt (lines: "Q ab-xxxx" / "U ab-xxxx" / "T ab-xxxx",
#                          plus an initial "# pending" sentinel)
#   ARGV[2] = cands.tsv   (tab-delimited candidate snapshot)
# Emits TAB-delimited fzf rows: "<url>\t<id>\t<visible row>".
BEGIN { FS = "\t" }
# First file: record intents in order so T can flip the current running
# state (Q then T = back to original) rather than the immutable initial.
NR == FNR {
    if (length($0) >= 3) {
        kind = substr($0, 1, 1)
        if (kind == "Q" || kind == "U" || kind == "T") {
            rest = substr($0, 3)
            gsub(/[ \t\r\n]+$/, "", rest)
            gsub(/^[ \t]+/, "", rest)
            if (rest != "") {
                cnt = ++pending_count[rest]
                pending_seq[rest "|" cnt] = kind
            }
        }
    }
    next
}
# Second file: walk per-id intents in order to compute effective state.
{
    id = $1; title = $2; prio = $3; project = $4; status = $5
    q_initial = ($6 == "1") ? 1 : 0
    plan_path = $7; blocked_by = $8; url = $9

    queued = q_initial
    n = pending_count[id]
    for (i = 1; i <= n; i++) {
        k = pending_seq[id "|" i]
        if (k == "Q") queued = 1
        else if (k == "U") queued = 0
        else if (k == "T") queued = (1 - queued)
    }

    is_blocked = (status == "blocked")
    if (queued && is_blocked)       marker = "[Q!]"
    else if (queued)                 marker = "[Q]"
    else if (is_blocked)             marker = "[B]"
    else                             marker = "[ ]"

    kind_col = (plan_path == "") ? "idea" : "plan"

    if (length(project) > 22) project = substr(project, 1, 21) "."
    if (length(title)   > 75) title   = substr(title,   1, 74) "."

    while (length(marker) < 5)   marker = marker " "
    while (length(project) < 22) project = project " "

    blocker_suffix = ""
    if (is_blocked && blocked_by != "")
        blocker_suffix = "  (blocked by " blocked_by ")"

    printf "%s\t%s\t%s %s  %s  %s  %s  %s%s\n", \
        url, id, marker, kind_col, prio, project, id, title, blocker_suffix
}
"""


@cli.command("pick")
def cmd_pick(
    project: Optional[str] = typer.Option(None, help="Filter by project name"),
    all_: bool = typer.Option(False, "--all", "-A", help="Show all projects (default: current cwd)"),
    include_ideas: bool = typer.Option(
        True,
        "--ideas/--no-ideas",
        help="Include idea-stage rows alongside ready ones (default: yes).",
    ),
    include_blocked: bool = typer.Option(
        False,
        "--blocked/--no-blocked",
        "-b",
        help="Also show blocked rows so you can queue a node + its blocked dependents together. Open blockers are shown inline. Default: off.",
    ),
    reason: Optional[str] = typer.Option(
        None,
        "--reason", "-R",
        help="Reason applied to every newly-queued node (optional).",
    ),
) -> None:
    """Interactively manage the backlog queue via fzf with live marker updates.

    Pressing keys updates the marker in real time via fzf's reload
    action. So pressing ``q`` on a ``[ ]`` row flips it to ``[Q]``
    in-place; pressing ``u`` on a ``[Q]`` row flips it back to ``[ ]``.

      q       queue this row     -> marker becomes [Q]
      u       unqueue this row   -> marker becomes [ ]
      space   toggle this row    -> marker flips
      o       open plan in Obsidian (idea rows: no-op)
      Enter   commit all pending marker changes atomically
      Ctrl-C  cancel; no marks land on the graph
      type    fuzzy-filter the visible rows

    Markers reflect the effective state INCLUDING pending changes.
    Latest mark per row wins, so you can change your mind by pressing
    the opposite key. Idempotent: re-queuing an already-queued row is
    a no-op on commit.
    """
    import os as _os
    import platform
    import shlex
    import shutil
    import subprocess
    import tempfile

    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import filter_by_project, _find_node, _graph_sort_key_fn
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.render_html import _load_obsidian_vault, _obsidian_url

    fzf = shutil.which("fzf")
    if not fzf:
        typer.echo(
            "Error: fzf not found on PATH. Install with `brew install fzf` "
            "(macOS) or your package manager.",
            err=True,
        )
        raise typer.Exit(code=1)
    awk_bin = shutil.which("awk")
    if not awk_bin:
        typer.echo("Error: awk not found on PATH (needed for live marker updates).", err=True)
        raise typer.Exit(code=1)

    entries = read_graph(_graph_path())
    allowed = {"ready"}
    if include_ideas:
        allowed.add("idea")
    if include_blocked:
        allowed.add("blocked")
    candidates = [e for e in entries if e.get("_status") in allowed]
    candidates = filter_by_project(candidates, project, all_)

    if not candidates:
        scope = "/".join(sorted(allowed))
        typer.echo(f"No {scope} rows to pick from in this scope.")
        return

    # Sort queued rows to the TOP, then by priority within each cluster.
    currently_queued = {e["id"] for e in candidates if e.get("queued_at")}
    candidates.sort(
        key=lambda e: (0 if e["id"] in currently_queued else 1, _graph_sort_key_fn(e))
    )

    vault = _load_obsidian_vault()
    open_cmd = "open" if platform.system() == "Darwin" else (
        shutil.which("xdg-open") or shutil.which("wslview") or "xdg-open"
    )

    # Tempfiles:
    #   cands.tsv  : the immutable snapshot of candidates the picker reads
    #   pending.txt: empty file the keybinds append intents to
    #   awk.script : the renderer logic invoked by fzf reload
    fd_cand, cand_path = tempfile.mkstemp(prefix="abi-pick-", suffix=".cands.tsv")
    fd_pend, pend_path = tempfile.mkstemp(prefix="abi-pick-", suffix=".pending.txt")
    fd_awk, awk_path = tempfile.mkstemp(prefix="abi-pick-", suffix=".awk")
    # Seed pending.txt with a sentinel comment line. Awk's NR==FNR test
    # misfires when the first file is empty (FNR resets at file
    # boundary so the first record of file 2 also has NR==FNR), and
    # then candidate rows get mistakenly parsed as pending intents.
    # Any non-intent line is silently skipped by the renderer.
    with _os.fdopen(fd_pend, "w") as f:
        f.write("# pending\n")

    try:
        with _os.fdopen(fd_cand, "w") as f:
            for e in candidates:
                url = ""
                if vault and e.get("plan_path"):
                    built = _obsidian_url(vault, e["plan_path"])
                    if built:
                        url = built
                blockers = ",".join(
                    b for b in (e.get("blocked_by") or []) if isinstance(b, str)
                )
                row_fields = [
                    e["id"],
                    _tsv_safe(e.get("title") or ""),
                    e.get("priority") or "p2",
                    _tsv_safe(e.get("project") or "-"),
                    e.get("_status") or "ready",
                    "1" if e.get("queued_at") else "0",
                    _tsv_safe(e.get("plan_path") or ""),
                    blockers,
                    url,
                ]
                f.write("\t".join(row_fields) + "\n")
        with _os.fdopen(fd_awk, "w") as f:
            f.write(_PICK_RENDER_AWK)

        qa = shlex.quote(awk_bin)
        qs = shlex.quote(awk_path)
        qp = shlex.quote(pend_path)
        qc = shlex.quote(cand_path)
        render_cmd = f"{qa} -f {qs} {qp} {qc}"

        header_lines = [
            f"q=queue  u=unqueue  space=toggle  o=open plan  Enter=commit  Ctrl-C=cancel  "
            f"({len(candidates)} rows, {len(currently_queued)} queued initially)",
            "Markers update in-place as you press keys: [ ] not queued  [Q] queued  [B] blocked  [Q!] queued+blocked",
        ]
        if not vault:
            header_lines.append(
                "(set config.obsidian.vault in settings.yaml to enable 'o' opener)"
            )
        header = "\n".join(header_lines)

        # Initial row set: run the renderer once with empty pending.
        initial = subprocess.run(
            [awk_bin, "-f", awk_path, pend_path, cand_path],
            capture_output=True, text=True, check=False,
        ).stdout

        proc = subprocess.run(
            [
                fzf,
                "--no-multi",
                "--delimiter", "\t",
                "--with-nth", "3..",
                "--nth", "3..",
                # Q/U/T keybinds: append intent line to pending.txt, then
                # reload the row list from awk. Cursor preserves via fzf's
                # default reload behavior; +down advances to next row.
                "--bind", f"q:execute-silent(printf 'Q %s\\n' {{2}} >> {qp})+reload({render_cmd})+down",
                "--bind", f"u:execute-silent(printf 'U %s\\n' {{2}} >> {qp})+reload({render_cmd})+down",
                "--bind", f"space:execute-silent(printf 'T %s\\n' {{2}} >> {qp})+reload({render_cmd})+down",
                "--bind", "enter:accept",
                "--bind", f"o:execute-silent(u={{1}}; [[ -n \"$u\" ]] && {open_cmd} \"$u\")",
                "--header", header,
                "--prompt", "pick> ",
                "--height", "85%",
                "--reverse",
                "--no-sort",
            ],
            input=initial,
            text=True,
            capture_output=True,
        )

        # rc 130 = Ctrl-C / Esc - drop pending unread.
        if proc.returncode == 130:
            typer.echo("Cancelled.")
            return

        # Parse pending.txt INSIDE the try so we read it before the
        # finally block deletes it. Preserve order so T flips the
        # running state (matches what awk renders to the screen).
        ordered_intents: list[tuple[str, str]] = []
        try:
            with open(pend_path) as f:
                for raw in f:
                    if len(raw) < 3:
                        continue
                    kind = raw[0]
                    if kind not in ("Q", "U", "T"):
                        continue
                    rest = raw[1:].strip()
                    if has_node_id_prefix(rest):
                        ordered_intents.append((rest, kind))
        except OSError:
            pass
    finally:
        for path in (cand_path, pend_path, awk_path):
            try:
                _os.unlink(path)
            except OSError:
                pass

    if not ordered_intents:
        typer.echo("No changes.")
        return

    final_queued: dict[str, bool] = {}
    for tid, kind in ordered_intents:
        if kind == "Q":
            final_queued[tid] = True
        elif kind == "U":
            final_queued[tid] = False
        elif kind == "T":
            prev = final_queued.get(tid, tid in currently_queued)
            final_queued[tid] = not prev

    to_queue: list[str] = []
    to_unqueue: list[str] = []
    for tid, want_queued in final_queued.items():
        was_queued = tid in currently_queued
        if want_queued and not was_queued:
            to_queue.append(tid)
        elif was_queued and not want_queued:
            to_unqueue.append(tid)

    if not to_queue and not to_unqueue:
        typer.echo("No changes (marks ended at original state).")
        return

    cleaned_reason = (reason or "").strip() or None

    # Capture mutator outputs via a dict in the enclosing scope rather
    # than function attributes (clearer than mutator.x = ... pattern,
    # per Gemini review on PR #253).
    results: dict[str, list[str]] = {"queued_applied": [], "unqueued_applied": []}

    def mutator(graph_entries):
        now = datetime.now(timezone.utc).isoformat()
        queued_applied: list[str] = []
        unqueued_applied: list[str] = []
        for tid in to_queue:
            # Silent skip when a node is missing or already in the target
            # state. A node disappearing between the picker snapshot read
            # and the lock acquisition is a tolerable race - aborting the
            # whole batch over it would lose the user's other valid marks.
            node = _find_node(graph_entries, tid)
            if not node or node.get("queued_at"):
                continue
            node["queued_at"] = now
            if cleaned_reason:
                node["queued_reason"] = cleaned_reason
            queued_applied.append(tid)
        for tid in to_unqueue:
            node = _find_node(graph_entries, tid)
            if not node or not node.get("queued_at"):
                continue
            node["queued_at"] = None
            node["queued_reason"] = None
            unqueued_applied.append(tid)
        results["queued_applied"] = queued_applied
        results["unqueued_applied"] = unqueued_applied
        return graph_entries

    locked_mutate_graph(_graph_path(), mutator)
    queued_applied = results["queued_applied"]
    unqueued_applied = results["unqueued_applied"]

    suffix = f': "{cleaned_reason}"' if cleaned_reason else ""
    for tid in queued_applied:
        typer.echo(f"Queued {tid}{suffix}")
    for tid in unqueued_applied:
        typer.echo(f"Unqueued {tid}")
    if queued_applied or unqueued_applied:
        typer.echo(
            f"({len(queued_applied)} queued, {len(unqueued_applied)} unqueued)"
        )
    else:
        typer.echo("(no changes)")


@cli.command("queued")
def cmd_queued(
    project: Optional[str] = typer.Option(None, help="Filter by project name"),
    all_: bool = typer.Option(False, "--all", "-A", help="Show all projects"),
) -> None:
    """List nodes the user has queued for action. JSON output, sorted by priority."""
    from fno.graph.store import read_graph
    from fno.graph._intake import filter_by_project, _graph_sort_key_fn

    entries = read_graph(_graph_path())
    queued = [e for e in entries
              if e.get("queued_at")
              and not e.get("completed_at")
              and not e.get("deferred_at")]
    queued = filter_by_project(queued, project, all_)
    queued.sort(key=_graph_sort_key_fn)

    output = [{
        "id": e["id"], "title": e.get("title"), "priority": e.get("priority"),
        "project": e.get("project"), "queued_at": e.get("queued_at"),
        "queued_reason": e.get("queued_reason"), "_status": e.get("_status"),
    } for e in queued]
    typer.echo(json.dumps(output, indent=2))


@cli.command("undefer")
def cmd_undefer(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
) -> None:
    """Clear deferred state on a backlog node. Idempotent."""
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    was_deferred_holder: list[bool] = [False]

    def mutator(entries):
        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        was_deferred_holder[0] = bool(node.get("deferred_at"))
        node["deferred_at"] = None
        node["deferred_reason"] = None
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    if was_deferred_holder[0]:
        # Mark a streak-reset boundary so the failed-node cascade (#34) gives a
        # human-recovered node a clean slate: it needs N FRESH consecutive
        # failures before auto-defer re-triggers (AC5-FR). The reader keys on
        # data.unit_id; the flat agents envelope it writes is accepted too.
        # Best-effort - a failed emit only means the node keeps its pre-undefer
        # streak, never a crash in undefer.
        try:
            from fno.agents.events import emit as _emit_event
            from fno.graph.failure import events_path as _events_path

            # Write to the SAME log the streak reader consumes (the walker's
            # $HOME/.fno mirror), not agents.events' state_dir default, so
            # reader and emitter agree even under a customized config.state_dir.
            _emit_event("node_undeferred", path=_events_path(), unit_id=task_id)
        except Exception:
            pass

    if not was_deferred_holder[0]:
        typer.echo(f"warning: {task_id} was not deferred", err=True)
    typer.echo(f"Undeferred {task_id}")


# -- done --

def _apply_completion_fields(node: dict) -> None:
    """Set the fields that mark a node done.

    Shared by ``done`` and ``reconcile`` so both close paths stay in
    lockstep. The caller owns the idempotency check (skip when
    ``completed_at`` is already set). ``recompute_statuses`` derives
    ``_status: done`` from ``completed_at`` and unblocks dependents.
    """
    node["session_id"] = None
    node["claimed_at"] = None
    # Done dominates deferred per the cascade. Clear any deferred/queued state
    # so the row presents as cleanly done with no ghost fields.
    node["deferred_at"] = None
    node["deferred_reason"] = None
    node["queued_at"] = None
    node["queued_reason"] = None
    node["completed_at"] = datetime.now(timezone.utc).isoformat()


def _stamp_and_graduate_plan(
    plan_path: str,
    *,
    url: Optional[str] = None,
    session_id: Optional[str] = None,
) -> bool:
    """Best-effort: stamp a plan ``shipped`` (when a ship URL is known) then graduate.

    The completion path (``done``/``reconcile``) closes a node because its PR
    landed. ``graduate`` ALONE is a no-op on a plan that never went through
    target's ship gate: ``cmd_graduate`` returns early unless ``status`` is
    already ``shipped``, so a never-stamped plan's frontmatter would never record
    the ship (ab-bd9f476c). When a concrete PR ``url`` is available we first
    ``stamp`` the plan (sets ``shipped_at`` + ``status: shipped`` + records the
    URL and session id) and THEN ``graduate`` (flips ``shipped -> done`` once the
    URL count is met). Without a URL we fall back to graduate-only - the prior
    behavior - rather than assert a ship we cannot evidence (e.g. a forced close
    on an advisory node with no PR).

    Returns True when a stamp/graduate actually ran successfully (the relevant
    verb exited 0); False when the run failed. Non-fatal: every failure warns
    and returns False, never raising, so a node close is never aborted by a
    stamp problem.

    Shared by ``done`` and ``reconcile``. The stamper is the in-package
    ``fno.plan._stamp`` module, run under the same interpreter as fno, so it
    resolves whether the package runs from the repo (editable install) or a
    uv-installed venv.
    """
    import subprocess

    def _run(verb_args: list[str]):
        try:
            # sys.executable + ``-m fno.plan._stamp``: run the stamp under the
            # same interpreter/venv as fno so it sees the same deps, and avoid
            # failing where the binary is named "python".
            return subprocess.run(
                [sys.executable, "-m", "fno.plan._stamp", *verb_args],
                check=False,
                capture_output=True,
                text=True,
                # Bound the stamp so a hung subprocess never blocks a node close
                # (gemini, PR #474). A timeout raises and is caught below ->
                # treated as a failed run, non-fatal.
                timeout=30,
            )
        except Exception as e:  # spawn failure / timeout: warn, treat as a failed run
            typer.echo(
                f"warning: fno.plan._stamp {verb_args[0]} failed to run: {e}",
                err=True,
            )
            return None

    # Stamp shipped first when we have a concrete ship URL. Without the stamp,
    # graduate below would no-op on a never-shipped plan.
    stamped_shipped = False
    if url:
        sid = session_id or "backlog-close"
        res = _run(["stamp", "--plan-path", plan_path, "--session-id", sid, "--url", url])
        if res is None:
            return False
        if res.returncode != 0:
            typer.echo(
                f"warning: fno.plan._stamp stamp exited {res.returncode}"
                f"{f' - stderr: {res.stderr.strip()}' if res.stderr else ''}",
                err=True,
            )
            return False
        stamped_shipped = True

    res = _run(["graduate", "--plan-path", plan_path])
    if res is None:
        # A successful stamp already recorded the ship; report that win even if
        # the graduate spawn failed.
        return stamped_shipped
    if res.returncode != 0:
        # Surface the script's own error so a broken stamp run is diagnosable
        # instead of silently eaten.
        typer.echo(
            f"warning: fno.plan._stamp graduate exited {res.returncode}"
            f"{f' - stderr: {res.stderr.strip()}' if res.stderr else ''}",
            err=True,
        )
        return stamped_shipped
    return True


# Closed set of outcomes from _set_expected_count, so the call site's
# `status == "failed"` compare is type-checked rather than a free-form string.
SetExpectedStatus = Literal["ok", "skipped", "failed"]


def _set_expected_count(plan_path: str, count: int) -> tuple[SetExpectedStatus, str]:
    """Authoritatively write expected_url_count=count onto a plan's frontmatter.

    Used by ``decompose`` so a shared epic-decomposition doc graduates only
    after all N group PRs ship, not after the first. Runs the in-package
    ``fno.plan._stamp`` ``set-expected`` verb (the same sys.executable pattern
    as ``_graduate_plan``) to keep plan-frontmatter I/O in its single owner and
    this graph CLI graph-agnostic about frontmatter format.

    Returns ``(status, detail)`` where status is a ``SetExpectedStatus``:

    - ``"ok"``      - the count was written.
    - ``"skipped"`` - benign: the count could not be written for a reason that
      PROVABLY does NOT create the early-graduation risk: the base doc does not
      exist (set-expected exit 3). target also cannot stamp the doc at ship time,
      so it never graduates early. Mirrors ``_graduate_plan``'s best-effort,
      non-fatal philosophy. The caller proceeds silently.
    - ``"failed"``  - a real risk that must be surfaced: either the module RAN
      and reported a write failure on a doc it could read (e.g. malformed
      frontmatter; set-expected exit 1/2), OR the spawn itself raised. A spawn
      failure is INDETERMINATE - unlike an absent doc it does not prove the doc
      is unstampable at ship, so it could mask early graduation; the caller
      surfaces it as a loud, actionable stderr warning.

    The caller never rolls back the graph and never exits non-zero on any of
    these outcomes (group nodes are the source of truth, and a non-zero exit
    would break pipelines that call decompose for a best-effort stamp).
    """
    import subprocess
    try:
        result = subprocess.run(
            [
                sys.executable, "-m", "fno.plan._stamp", "set-expected",
                "--plan-path", plan_path,
                "--count", str(count),
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            return "ok", ""
        # Exit 3 == base doc absent: benign (cannot be stamped at ship either).
        if result.returncode == 3:
            return "skipped", result.stderr.strip()
        # Any other non-zero means the module ran but could not write a doc it
        # could see (malformed frontmatter, etc.) - the real degradation.
        return "failed", result.stderr.strip() or f"exit {result.returncode}"
    except Exception as e:  # noqa: BLE001 - report any spawn failure to the caller
        # A spawn failure is indeterminate: unlike an absent doc it does not
        # prove the doc is unstampable at ship, so treat it as a surfaced
        # failure rather than a silent skip.
        return "failed", f"set-expected spawn failed: {e}"


# -- gh cross-check helpers (injectable for tests) --
# These module-level callables are replaced by test stubs via monkeypatch.

def _done_gh_query(pr_number, **kwargs):
    """Query gh for PR merge state. Delegates to reconcile's canonical helper."""
    from fno.graph._reconcile import query_pr_merge_state
    return query_pr_merge_state(pr_number, **kwargs)


def _done_ci_query(pr_number, *, repo=None, cwd=None):
    """Query gh pr checks for a PR and return the list of check objects.

    Each object has at least {"name": str, "state": str, "bucket": str}.
    Raises ReconcileError on any subprocess failure. Returns [] when gh
    reports no checks configured.
    """
    import json as _json
    import subprocess
    import shutil
    from fno.graph._reconcile import GH_QUERY_TIMEOUT_S, ReconcileError

    if shutil.which("gh") is None:
        raise ReconcileError("gh CLI not found on PATH")

    cmd = ["gh", "pr", "checks", str(pr_number), "--json", "name,state,bucket"]
    if repo:
        cmd = ["gh", "pr", "checks", str(pr_number), "--repo", repo, "--json", "name,state,bucket"]
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            check=False,
            timeout=GH_QUERY_TIMEOUT_S,
            cwd=cwd,
        )
    except subprocess.TimeoutExpired as exc:
        raise ReconcileError(
            f"gh pr checks #{pr_number} timed out after {GH_QUERY_TIMEOUT_S}s"
        ) from exc
    except OSError as exc:
        raise ReconcileError(f"gh pr checks subprocess failed: {exc}") from exc

    if result.returncode != 0:
        raise ReconcileError(
            f"gh pr checks #{pr_number} failed (rc={result.returncode}): "
            f"{(result.stderr or '').strip()}"
        )

    try:
        data = _json.loads(result.stdout or "[]")
    except _json.JSONDecodeError as exc:
        raise ReconcileError(f"gh pr checks stdout was not JSON: {exc}") from exc

    if isinstance(data, list):
        return data
    # gh can return a wrapping object on some versions; be defensive
    return []


def _ci_is_green(checks: list) -> tuple[bool, str]:
    """Return (is_green, reason_if_not_green) using bucket semantics.

    Mirrors loopcheck.rs bucket evaluation:
      - any fail/cancel bucket -> not green
      - any non-pass/non-skipping bucket -> pending, not green
      - empty list -> not green (no checks configured = no evidence)
      - all pass/skipping -> green
    """
    if not checks:
        return False, "no CI checks configured"

    # Filter to dict items only; non-dict elements (e.g. strings from
    # unexpected gh output) are ignored rather than raising AttributeError.
    dict_checks = [c for c in checks if isinstance(c, dict)]
    if not dict_checks:
        return False, "no CI checks configured"

    for check in dict_checks:
        bucket = (check.get("bucket") or "").lower()
        if bucket in ("fail", "cancel"):
            name = check.get("name") or "unknown"
            return False, f"fail={name} bucket={bucket}"

    pending = [
        check for check in dict_checks
        if (check.get("bucket") or "").lower() not in ("pass", "skipping")
    ]
    if pending:
        names = ",".join(c.get("name", "?") for c in pending[:3])
        return False, f"pending checks: {names}"

    return True, ""


@cli.command("done")
def cmd_done(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    skip_stamp: bool = typer.Option(
        False,
        "--skip-stamp",
        help="Skip plan stamp even if plan_path is set",
    ),
    force: bool = typer.Option(
        False,
        "--force",
        "-F",
        help="Bypass gh cross-check. Requires --reason.",
    ),
    reason: Optional[str] = typer.Option(
        None,
        "--reason",
        "-R",
        help="Required when --force is used. Explains why the cross-check is bypassed.",
    ),
) -> None:
    """Mark a node complete.

    Sets ``completed_at`` to an ISO timestamp; ``recompute_statuses`` derives
    ``_status: done`` from that field and unblocks any dependents.

    Before mutation, a gh cross-check verifies that at least one referenced PR
    is MERGED or OPEN with green CI. If no evidence is found the command
    refuses and exits 3 (distinct from validation errors on exit 1/2).

    Exit codes:
        0  success (node closed)
        1  validation error (bad id, node not found)
        2  usage error (--force without --reason)
        3  gh cross-check refused: no merged/green evidence (retryable when
           the PR merges or CI goes green; walker treats this as Parked)
        4  gh outage: subprocess failure / timeout / parse error; retryable
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph, read_graph
    from fno.graph._intake import _find_node
    from fno.graph._reconcile import (
        node_pr_refs,
        repo_slug_from_url,
        ReconcileError,
    )

    if not has_node_id_prefix(task_id):
        typer.echo(
            f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'",
            err=True,
        )
        raise typer.Exit(code=1)

    # Usage guard: --force requires --reason
    if force and not reason:
        typer.echo(
            "Error: --force requires --reason TEXT (explain why the cross-check is bypassed)",
            err=True,
        )
        raise typer.Exit(code=2)

    # -- Step 1: Idempotency + node-lookup read (outside the lock) --
    # We must discover idempotency and PR refs before acquiring the lock, so
    # that gh I/O (which can be slow) never blocks other graph mutations.
    entries = read_graph(_graph_path())
    node = _find_node(entries, task_id)
    if not node:
        typer.echo(f"Error: feature {task_id} not found", err=True)
        raise typer.Exit(code=1)

    # Idempotency first: already done -> short-circuit with NO gh read (AC4-EDGE)
    if node.get("completed_at"):
        typer.echo(f"{task_id} is already done", err=True)
        return

    # -- Step 2: gh cross-check (outside the lock) --
    refs = node_pr_refs(node)

    # The PR url that evidences the close, captured so the plan stamp records
    # the actual ship (ab-bd9f476c). None when there is no PR ref / no evidence.
    evidence_pr_url: Optional[str] = None

    if refs and not force:
        # There are PR references; require evidence before closing.
        first_pr_number, first_pr_url = refs[0]
        repo = repo_slug_from_url(first_pr_url)
        cwd = node.get("cwd")

        # Try each ref in order; the first one that gives us evidence wins.
        evidence_found = False
        refusal_reason: Optional[str] = None
        outage_error: Optional[str] = None

        for pr_number, pr_url in refs:
            pr_repo = repo_slug_from_url(pr_url) or repo
            pr_cwd = cwd if pr_repo is None else None

            try:
                pr_state = _done_gh_query(pr_number, repo=pr_repo, cwd=pr_cwd)
            except ReconcileError as exc:
                outage_error = str(exc)
                continue

            if pr_state.state == "MERGED":
                evidence_found = True
                evidence_pr_url = pr_url
                break

            if pr_state.state == "OPEN":
                # Check CI green
                try:
                    checks = _done_ci_query(pr_number, repo=pr_repo, cwd=pr_cwd)
                except ReconcileError as exc:
                    outage_error = str(exc)
                    continue
                green, ci_reason = _ci_is_green(checks)
                if green:
                    evidence_found = True
                    evidence_pr_url = pr_url
                    break
                refusal_reason = (
                    f"PR #{pr_number} state=OPEN, CI not green: {ci_reason}"
                )
            else:
                # CLOSED (not merged) or UNKNOWN
                refusal_reason = (
                    f"PR #{pr_number} state={pr_state.state} (not merged)"
                )

        if not evidence_found:
            if outage_error and refusal_reason is None:
                # Only gh failures, no policy refusal -> retryable outage
                typer.echo(
                    f"Error: gh cross-check failed for {task_id}: {outage_error}\n"
                    f"The check is retryable once gh is available again. Node stays open.",
                    err=True,
                )
                raise typer.Exit(code=4)

            if outage_error:
                # Partial: some PRs queryable (policy failure), some not (outage).
                # Treat as a retryable outage (most conservative outcome; AC3-FR).
                typer.echo(
                    f"Error: gh cross-check partially failed for {task_id} "
                    f"(gh outage on some PRs: {outage_error}). "
                    f"Retry after gh recovers. Node stays open.",
                    err=True,
                )
                raise typer.Exit(code=4)

            # Pure policy refusal - print the specific fact
            msg = refusal_reason or f"PR #{first_pr_number}: no merged/green evidence"
            typer.echo(
                f"Refused: {task_id} cross-check failed: {msg}\n"
                f"Use --force --reason TEXT to bypass.",
                err=True,
            )
            # Emit refusal event (best-effort)
            try:
                from fno import events as _evts
                event = _evts.backlog_done_refused(
                    node_id=task_id,
                    pr_number=first_pr_number,
                    reason=msg,
                )
                _evts.append_event(event)
            except Exception:
                pass
            raise typer.Exit(code=3)

    # -- Step 3: Force path - proceed and journal loudly --
    if force and refs:
        first_pr_number, first_pr_url = refs[0]
        # A forced close still names a PR; stamp the plan against it so the ship
        # is recorded even when the cross-check was bypassed (ab-bd9f476c).
        evidence_pr_url = first_pr_url
        pr_repo = repo_slug_from_url(first_pr_url)
        # Best-effort: try to read the current PR state for journaling
        try:
            force_pr_state_obj = _done_gh_query(first_pr_number, repo=pr_repo)
            force_pr_state = force_pr_state_obj.state
        except Exception:
            force_pr_state = "UNKNOWN"
        typer.echo(
            f"Warning: force-closing {task_id} (reason: {reason}). "
            f"PR #{first_pr_number} state={force_pr_state}.",
            err=True,
        )
        # Emit forced-close event (best-effort)
        try:
            from fno import events as _evts
            event = _evts.backlog_done_forced(
                node_id=task_id,
                force_reason=reason,
                pr_number=first_pr_number,
                pr_state=force_pr_state,
            )
            _evts.append_event(event)
        except Exception:
            pass
    elif force:
        # --force with no refs: just log (advisory node)
        typer.echo(
            f"Warning: force flag set on advisory node {task_id} (reason: {reason}); no PR refs to check.",
            err=True,
        )

    # -- Step 4: Mutation under the lock --
    plan_path_out: list = [None]
    already_holder: list = [False]

    def mutator(entries):
        n = _find_node(entries, task_id)
        if not n:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        existing = n.get("completed_at") or ""
        # Idempotent: a prior real completion is a no-op.
        if existing:
            already_holder[0] = True
            return entries
        _apply_completion_fields(n)
        plan_path_out[0] = n.get("plan_path")
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    if already_holder[0]:
        typer.echo(f"{task_id} is already done", err=True)
        return

    typer.echo(f"Marked {task_id} done")

    # Operator-authority matrix (LD3/LD29): `fno backlog done` is an allowed
    # action during a drive window, but audit-tag it so the trail attributes
    # the completion to the operator rather than the LLM. Best-effort.
    try:
        from fno.agents.drive_authority import (
            emit_operator_initiated,
            is_drive_authority_active,
        )

        if is_drive_authority_active():
            emit_operator_initiated(
                "backlog_done_operator_initiated",
                source="backlog",
                task_id=task_id,
            )
    except Exception:
        pass

    if plan_path_out[0] and not skip_stamp:
        # Stamp the plan shipped (against the evidencing PR) THEN graduate, so a
        # plan that never went through target's ship gate still records the ship
        # rather than getting a graduate no-op (ab-bd9f476c).
        _stamp_and_graduate_plan(
            plan_path_out[0],
            url=evidence_pr_url,
            session_id=node.get("session_id"),
        )


# -- reconcile (close merged-PR drift) --

@cli.command("advance")
def cmd_advance(
    closed: Optional[str] = typer.Option(
        None,
        "--closed",
        help="The just-merged node id whose close triggered this advance (AC1-RACE keying).",
    ),
    project: Optional[str] = typer.Option(
        None, "--project", "-p", help="Restrict next-node selection to this project."
    ),
    json_out: bool = typer.Option(
        False, "--json", "-J", help="Emit the decision as JSON."
    ),
    verbose: bool = typer.Option(
        False, "--verbose", help="Print the dispatch decision to stderr."
    ),
) -> None:
    """Dispatch a fresh /target no-merge worker for the next now-unblocked node.

    Merge-triggered auto-continue (ab-3cd195b6). Opt-in and non-fatal: when
    auto-continue is disabled it emits advance_skipped{disabled} and dispatches
    nothing. Driven by the merge event (reconcile / post-merge), so megawalk,
    /target, and /megatron all inherit it without driver-specific code. Always
    exits 0 (a dispatch decision is never an error to the host op).
    """
    from fno.backlog.advance import advance as _advance

    try:
        result = _advance(closed_node_id=closed, project=project, verbose=verbose)
    except Exception as exc:  # noqa: BLE001 - the contract is "always exits 0"
        # advance() is designed non-fatal (every path emits + returns), but the
        # CLI entrypoint must never traceback on an unforeseen escape: a dispatch
        # decision is not an error to whoever invoked the verb. Report on stderr
        # and exit 0.
        typer.echo(f"advance: unexpected error (non-fatal): {exc}", err=True)
        return
    if json_out:
        typer.echo(
            json.dumps(
                {
                    "decision": result.decision,
                    "event": result.event,
                    "reason": result.reason,
                    "node_id": result.node_id,
                    "short_id": result.short_id,
                },
                indent=2,
            )
        )
    else:
        parts = [result.decision]
        if result.node_id:
            parts.append(result.node_id)
        if result.reason:
            parts.append(f"reason={result.reason}")
        if result.short_id:
            parts.append(f"short_id={result.short_id}")
        typer.echo(" ".join(parts))


@cli.command("reconcile")
def cmd_reconcile(
    dry_run: bool = typer.Option(
        False,
        "--dry-run", "-N",
        help="Report candidates only; mutate nothing (graph stays byte-identical).",
    ),
    node: Optional[str] = typer.Option(
        None,
        "--node",
        help="Restrict the scan to a single node id (ab-XXXXXXXX).",
    ),
    json_out: bool = typer.Option(
        False,
        "--json", "-J",
        help="Emit structured JSON instead of a human summary.",
    ),
) -> None:
    """Close open backlog nodes whose PR has merged outside the ship gate.

    The completion ritual (stamp plan -> mark node ``done`` -> capture
    follow-ups) runs automatically only through ``/target``'s ship gate or
    ``scripts/lib/pr-merge.sh``. A PR merged any other way (manual GitHub
    merge, bare ``gh pr merge``) leaves the node open. This verb detects that
    drift and closes it mechanically: mark done, best-effort stamp the plan,
    and drop a retro sentinel so a later session captures follow-ups. It never
    auto-creates inbox lines or backlog nodes, never auto-resumes work, and
    never clobbers a node that is already done.
    """
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import _find_node
    from fno.graph._reconcile import (
        emit_session_satisfied_for_record,
        scan_merge_drift,
        write_retro_sentinel,
    )
    from fno.paths import retro_pending_dir

    entries = read_graph(_graph_path())
    records = scan_merge_drift(entries, node_id=node)

    closeable = [r for r in records if r.closeable]
    failures = [r for r in records if r.error is not None]

    closed: list[dict] = []

    if not dry_run and closeable:
        # Apply every close in ONE locked mutation rather than locking once
        # per node: locked_mutate_graph acquires a file lock and rewrites the
        # whole graph, so a per-node loop is O(N) lock+rewrite cycles. The
        # mutator collects the records it actually closed (idempotency: a node
        # closed or removed out-of-band between the read-only scan and the lock
        # is skipped) so the post-lock work only touches genuinely-closed nodes.
        actually_closed: list = []

        def mutator(entries):
            actually_closed.clear()
            for record in closeable:
                node_obj = _find_node(entries, record.node_id)
                if node_obj and not node_obj.get("completed_at"):
                    _apply_completion_fields(node_obj)
                    actually_closed.append(record)
            return entries

        locked_mutate_graph(_graph_path(), mutator)

        # Post-mutation work outside the lock (mirrors `done`): stamp the plan
        # and drop the retro sentinel for each node we actually closed.
        for record in actually_closed:
            # Reflect the real stamp outcome: the helper is best-effort and may
            # no-op (missing script) or fail, so don't claim it stamped. Pass the
            # merged PR url so a plan that never went through the ship gate is
            # stamped shipped->done rather than getting a graduate no-op
            # (ab-bd9f476c).
            stamped = bool(record.plan_path) and _stamp_and_graduate_plan(
                record.plan_path,
                url=record.pr_url,
                session_id=record.session_id,
            )

            # Sentinel write is best-effort: the node is already closed in the
            # graph, so a failed sentinel must not abort the loop and strand
            # later records. Warn and continue, mirroring _graduate_plan.
            sentinel_str = None
            try:
                sentinel_str = str(
                    write_retro_sentinel(record, sentinel_dir=retro_pending_dir())
                )
            except OSError as exc:
                typer.echo(
                    f"warning: closed {record.node_id} but failed to write its "
                    f"retro sentinel: {exc}",
                    err=True,
                )

            # Hand the auto-complete signal to the owning target session (Group 1
            # / ab-f7f8bc53): an out-of-band merge bypassed pr-merge.sh's emit, so
            # the owning session is still IN_PROGRESS and its stop hook would hard
            # re-block. Best-effort + non-fatal: a failure here logs and continues
            # (the defensive stop-hook probe is the backstop).
            emit_session_satisfied_for_record(record)

            closed.append({
                "node_id": record.node_id,
                "pr_number": record.pr_number,
                "pr_url": record.pr_url,
                "plan_stamped": stamped,
                "sentinel": sentinel_str,
            })

            # Merge-triggered auto-continue (ab-3cd195b6 / task 2.1): now that
            # this node's close has committed (AC1-RACE ordering: advance runs
            # only AFTER the locked_mutate_graph above), dispatch a fresh
            # /target no-merge worker for the next now-unblocked node IF
            # auto-continue is armed for the project. advance gates on
            # enablement internally (a no-op advance_skipped{disabled} when
            # off) and is strictly non-fatal: a failed advance never fails the
            # reconcile sweep. Project-scoped per the closed node's project.
            try:
                _adv_node = _find_node(entries, record.node_id)
                _adv_project = _adv_node.get("project") if _adv_node else None
                # Resolve auto-continue state against the CLOSED NODE's project
                # context, not the reconcile's cwd (codex P2): a full-graph
                # reconcile run from project A can close a node belonging to
                # project B, and B's campaign-arm marker lives under B's root.
                _adv_cwd = _adv_node.get("cwd") if _adv_node else None
                from fno.backlog.advance import advance as _advance

                _advance(
                    closed_node_id=record.node_id,
                    project=_adv_project,
                    project_root=Path(_adv_cwd) if _adv_cwd else None,
                )
            except Exception as _adv_exc:  # noqa: BLE001 - never abort the sweep
                typer.echo(
                    f"warning: auto-continue advance after closing "
                    f"{record.node_id} failed: {_adv_exc}",
                    err=True,
                )

    if json_out:
        payload = {
            "dry_run": dry_run,
            "candidates": [
                {
                    "node_id": r.node_id,
                    "pr_number": r.pr_number,
                    "pr_url": r.pr_url,
                    "plan_path": r.plan_path,
                }
                for r in closeable
            ],
            "closed": closed,
            "failures": [
                {"node_id": r.node_id, "pr_number": r.pr_number, "error": r.error}
                for r in failures
            ],
        }
        typer.echo(json.dumps(payload, indent=2))
        # Unresolved PR queries are a partial failure: signal it so unattended
        # callers can detect it from the exit code, not just the JSON body.
        if failures:
            raise typer.Exit(code=4)
        return

    if not closeable and not failures:
        typer.echo("No merged-PR drift found. Backlog is in sync.")
        return

    if dry_run:
        typer.echo(f"Would close {len(closeable)} node(s) (dry-run, nothing mutated):")
        for r in closeable:
            typer.echo(f"  {r.node_id}  PR #{r.pr_number} MERGED  {r.pr_url or ''}".rstrip())
    else:
        typer.echo(f"Closed {len(closed)} node(s):")
        for c in closed:
            stamp_note = " (plan stamped)" if c["plan_stamped"] else ""
            typer.echo(f"  {c['node_id']}  PR #{c['pr_number']}{stamp_note}")
        if closed:
            typer.echo(f"Retro sentinels written under {retro_pending_dir()}")

    if failures:
        typer.echo(f"{len(failures)} node(s) could not be resolved:", err=True)
        for r in failures:
            typer.echo(f"  {r.node_id}  PR #{r.pr_number}: {r.error}", err=True)
        # Partial reconcile: non-zero exit so callers can detect it.
        raise typer.Exit(code=4)


# -- maintain (recurring backlog + kanban hygiene sweep) --

@cli.command("maintain")
def cmd_maintain(
    apply: bool = typer.Option(
        False,
        "--apply",
        help=(
            "Apply the DETERMINISTIC legs (re-scope drift, prune pytest leaks). "
            "The judgment legs (dedup, drain-stale, cap-Now) are ALWAYS "
            "proposal-only regardless of this flag."
        ),
    ),
    json_out: bool = typer.Option(
        False,
        "--json", "-J",
        help="Emit structured JSON instead of a human summary.",
    ),
) -> None:
    """Keep graph.json + the kanban board clean by composing existing verbs.

    Six legs (ab-9c144a4c). Two are deterministic and apply under ``--apply``:
    re-scope project/cwd drift, and prune pytest-temp leak nodes. Three are
    judgment calls and only ever PROPOSE (never mutate, regardless of
    ``--apply``): surface near-duplicate idea titles, propose a reversible
    ``defer`` for stale ideas, and report a Now column over its WIP cap. The
    last leg appends a summary to health-history so ``triage trend`` shows the
    board trending cleaner.

    Loop form: ``/loop 1d fno backlog maintain --apply``.

    Best-effort: a malformed row is skipped, a single failed apply does not
    abort the rest, and an empty graph is a clean no-op.
    """
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph.statuses import recompute_statuses
    from fno.graph._intake import _find_node
    from fno.graph.render import _kanban_column
    from fno.graph.render_html import _load_wip_caps
    from fno.graph import maintain as _maintain

    # Read once and derive _status so the judgment legs see accurate states
    # (read_graph applies defaults but does not run the cascade).
    entries = recompute_statuses(read_graph(_graph_path()))

    # Apply legs must never touch a node a live target session is driving.
    claimed = _live_claimed_node_ids()

    # --- detect (all read-only) ---
    workspaces = _maintain.load_workspaces()
    rescope_fixes = _maintain.detect_rescope_fixes(entries, workspaces)
    prune_ids = _maintain.detect_temp_leaks(entries)
    dup_groups = _maintain.detect_dup_groups(entries)

    try:
        from fno.config import load_settings

        _maintain_cfg = load_settings().config.backlog.maintain
        staleness_days = _maintain_cfg.staleness_days
        max_failed_attempts = _maintain_cfg.max_failed_attempts
    except Exception:
        staleness_days = 30
        max_failed_attempts = 3
    stale = _maintain.detect_stale_ideas(entries, staleness_days)

    now_cap = _load_wip_caps().get("now", 20)
    overflow = _maintain.now_overflow(entries, now_cap, _kanban_column)

    # Leg 7: auto-defer failure-prone nodes (#34). Derive the streak from the
    # walker's existing node_failed/node_closed events (Locked Decision #4).
    from fno.graph import failure as _failure

    events = _failure.read_events()
    defer_cands = _maintain.detect_failure_defers(entries, events, max_failed_attempts)
    # Blast-radius guard (Open Question #2): cap per-run auto-defers so a
    # provider-outage mass-failure cannot defer half the board. Truncate the
    # lowest-streak candidates and ALWAYS log the drop (no silent cap).
    defer_truncated = 0
    if len(defer_cands) > _maintain.AUTO_DEFER_BLAST_CAP:
        defer_cands = sorted(defer_cands, key=lambda d: (-d.streak, d.node_id))
        defer_truncated = len(defer_cands) - _maintain.AUTO_DEFER_BLAST_CAP
        defer_cands = defer_cands[: _maintain.AUTO_DEFER_BLAST_CAP]

    # --- apply (deterministic legs only) ---
    applied_rescope: list[str] = []
    applied_prune: list[str] = []
    applied_defers: list[dict] = []
    skipped_claimed: list[str] = []

    if apply and (rescope_fixes or prune_ids or defer_cands):
        # Batch every change under ONE locked mutation so the board renders once,
        # not per node (Domain Pitfall). Each item is guarded so one failure
        # never strands the rest (AC1-ERR).
        def mutator(ents):
            applied_rescope.clear()
            applied_prune.clear()
            applied_defers.clear()
            skipped_claimed.clear()
            prune_set: set[str] = set()
            for fix in rescope_fixes:
                if fix.node_id in claimed:
                    skipped_claimed.append(fix.node_id)
                    continue
                try:
                    n = _find_node(ents, fix.node_id)
                    if not n:
                        continue
                    # Only project/cwd are ever touched - never priority/status.
                    n["project"] = fix.new_project
                    n["cwd"] = fix.new_cwd
                    applied_rescope.append(fix.node_id)
                except Exception as exc:  # noqa: BLE001 - one bad row must not abort
                    typer.echo(
                        f"warning: re-scope of {fix.node_id} failed: {exc}", err=True
                    )
            for nid in prune_ids:
                if nid in claimed:
                    skipped_claimed.append(nid)
                    continue
                prune_set.add(nid)
                applied_prune.append(nid)
            if prune_set:
                # Mirror `remove`: drop the node AND clean dangling blocked_by refs.
                for e in ents:
                    blocked = e.get("blocked_by")
                    if blocked:
                        e["blocked_by"] = [b for b in blocked if b not in prune_set]
                ents = [e for e in ents if e.get("id") not in prune_set]
            # Leg 7: auto-defer failure-prone nodes (#34). Mirrors cmd_defer's
            # field-set. Re-check live state INSIDE the lock (Concurrency): a
            # node done or deferred between the read and now must not be touched.
            # Auto-defer is a state change (unlike rescope/prune's project-cwd
            # touch-ups), so it also RE-SAMPLES live claims inside the lock - a
            # node a session claimed between the pre-lock read and now must not
            # be deferred ("claimed between read and write", Failure Modes /
            # Concurrency). Best-effort: union with the pre-lock set.
            defer_claimed = claimed | _live_claimed_node_ids()
            for cand in defer_cands:
                if cand.node_id in defer_claimed:
                    skipped_claimed.append(cand.node_id)
                    continue
                try:
                    n = _find_node(ents, cand.node_id)
                    if not n:
                        continue
                    if n.get("completed_at") or n.get("deferred_at"):
                        continue  # raced to done/deferred; leave it
                    reason = (
                        f"{_failure.AUTO_FAILURE_SENTINEL} {cand.streak} "
                        f"consecutive failed attempts"
                    )
                    # Mirror cmd_defer: clear claim/completion so the cascade
                    # derives _status: deferred, then set the deferred fields.
                    n["session_id"] = None
                    n["claimed_at"] = None
                    n["completed_at"] = None
                    n["deferred_at"] = datetime.now(timezone.utc).isoformat()
                    n["deferred_reason"] = reason
                    applied_defers.append(
                        {"node_id": cand.node_id, "streak": cand.streak, "reason": reason}
                    )
                except Exception as exc:  # noqa: BLE001 - one bad row must not abort
                    typer.echo(
                        f"warning: auto-defer of {cand.node_id} failed: {exc}", err=True
                    )
            return ents

        locked_mutate_graph(_graph_path(), mutator)

    # --- report leg: append a summary to health-history (best-effort) ---
    report = {
        "scope": "maintain",
        "applied": apply,
        "rescoped": len(applied_rescope) if apply else len(rescope_fixes),
        "pruned": len(applied_prune) if apply else len(prune_ids),
        "dedup_groups": len(dup_groups),
        "stale_ideas": len(stale),
        "now_overflow": list(overflow) if overflow else None,
        "skipped_claimed": len(skipped_claimed),
        "auto_deferred": len(applied_defers) if apply else len(defer_cands),
        # Carry node + reason so a sweep's auto-defers are never silent (a
        # silent auto-defer is a design bug, per UI State Machines).
        "auto_deferred_nodes": applied_defers
        if apply
        else [{"node_id": c.node_id, "streak": c.streak} for c in defer_cands],
        "auto_defer_truncated": defer_truncated,
    }
    try:
        from fno.health_monitor import append_history

        append_history(report, [])
    except Exception as exc:  # noqa: BLE001 - report leg is non-fatal
        typer.echo(f"warning: maintain health-history append failed: {exc}", err=True)

    if json_out:
        payload = {
            "applied": apply,
            "rescope": {
                "applied": applied_rescope if apply else [],
                "candidates": [
                    {
                        "node_id": f.node_id,
                        "new_project": f.new_project,
                        "new_cwd": f.new_cwd,
                    }
                    for f in rescope_fixes
                ],
            },
            "prune": {
                "applied": applied_prune if apply else [],
                "candidates": prune_ids,
            },
            "dedup_groups": dup_groups,
            "stale_ideas": [{"node_id": s.node_id, "age_days": s.age_days} for s in stale],
            "now_overflow": list(overflow) if overflow else None,
            "skipped_claimed": skipped_claimed,
            "auto_defer": {
                "applied": applied_defers if apply else [],
                "candidates": [
                    {"node_id": c.node_id, "streak": c.streak} for c in defer_cands
                ],
                "truncated": defer_truncated,
            },
        }
        typer.echo(json.dumps(payload, indent=2))
        return

    # --- human per-leg summary (a no-op run is visibly distinct, AC1-UI) ---
    if apply:
        typer.echo(
            f"re-scoped {len(applied_rescope)} | pruned {len(applied_prune)} | "
            f"auto-deferred {len(applied_defers)} | "
            f"dedup-groups {len(dup_groups)} | stale-ideas {len(stale)} | "
            f"now-overflow {'yes' if overflow else 'no'} | "
            f"skipped-claimed {len(skipped_claimed)}"
        )
    else:
        typer.echo(
            f"re-scope candidates {len(rescope_fixes)} | prune candidates "
            f"{len(prune_ids)} | auto-defer candidates {len(defer_cands)} | "
            f"dedup-groups {len(dup_groups)} | stale-ideas "
            f"{len(stale)} | now-overflow {'yes' if overflow else 'no'}  "
            f"(run with --apply to apply the deterministic legs)"
        )

    for f in rescope_fixes:
        verb = "re-scoped" if (apply and f.node_id in applied_rescope) else "would re-scope"
        typer.echo(f"  {verb} {f.node_id} -> project={f.new_project} cwd={f.new_cwd}")
    for nid in prune_ids:
        verb = "pruned" if (apply and nid in applied_prune) else "would prune (temp-cwd leak)"
        typer.echo(f"  {verb} {nid}")
    if apply:
        for d in applied_defers:
            typer.echo(
                f"  auto-deferred {d['node_id']} ({d['streak']} consecutive "
                f"failures): {d['reason']}"
            )
    else:
        for c in defer_cands:
            typer.echo(
                f"  would auto-defer {c.node_id} ({c.streak} consecutive failures, "
                f">= {max_failed_attempts}): fno backlog undefer {c.node_id} to recover"
            )
    if defer_truncated:
        typer.echo(
            f"  NOTE: auto-defer blast cap hit - {defer_truncated} further "
            f"candidate(s) NOT deferred this run "
            f"(cap {_maintain.AUTO_DEFER_BLAST_CAP}); re-run to continue"
        )
    for group in dup_groups:
        typer.echo(f"  near-duplicate ideas (merge/supersede by hand): {', '.join(group)}")
    for s in stale:
        typer.echo(
            f"  stale idea {s.node_id} ({s.age_days}d): "
            f"fno backlog defer {s.node_id} --reason 'stale >{staleness_days}d, drained by maintain'"
        )
    if overflow:
        count, cap = overflow
        typer.echo(
            f"  Now over WIP cap ({count} > {cap}): run `fno backlog triage propose` "
            f"to demote lower-priority work (never auto-reprioritized)"
        )
    if skipped_claimed:
        typer.echo(
            f"  skipped {len(skipped_claimed)} live-claimed node(s): "
            f"{', '.join(skipped_claimed)}"
        )


# -- reprioritize --

@cli.command("reprioritize")
def cmd_reprioritize(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    priority: str = typer.Argument(..., help="New priority: p0|p1|p2|p3"),
) -> None:
    from fno.graph._constants import PRIORITY_ORDER, has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    if priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    old_holder: list = [None]

    def mutator(entries):
        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        old_holder[0] = node.get("priority", "p2")
        node["priority"] = priority
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"Reprioritized {task_id}: {old_holder[0]} -> {priority}")


# -- rank --

@cli.command("rank")
def cmd_rank(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX) to rank"),
    top: bool = typer.Option(
        False, "--top", help="Pin to the front of its (column, project) lane"
    ),
    bottom: bool = typer.Option(
        False, "--bottom", help="Send to the back of the ranked band in its lane"
    ),
    before: Optional[str] = typer.Option(
        None, "--before", help="Place just before a ranked anchor in the same lane"
    ),
    after: Optional[str] = typer.Option(
        None, "--after", help="Place just after a ranked anchor in the same lane"
    ),
    clear: bool = typer.Option(
        False, "--clear", help="Clear the rank (rejoin the unranked priority flow)"
    ),
) -> None:
    """Curate a node's position within its (column, project) board lane.

    Rank is a nullable float ordered ahead of the (priority, created_at)
    fallback within a lane; it never changes a node's column. ``--before`` /
    ``--after`` require a *ranked* anchor in the same lane - seed one with
    ``--top`` first. Float midpoints mean inserts never renumber siblings.
    """
    import math

    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node
    from fno.graph.render import _kanban_column, _project_key

    if not has_node_id_prefix(task_id):
        typer.echo(f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'", err=True)
        raise typer.Exit(code=1)

    chosen = [
        name
        for name, on in (
            ("--top", top),
            ("--bottom", bottom),
            ("--before", before is not None),
            ("--after", after is not None),
            ("--clear", clear),
        )
        if on
    ]
    if len(chosen) != 1:
        typer.echo(
            "Error: pass exactly one of --top / --bottom / --before <id> / "
            "--after <id> / --clear",
            err=True,
        )
        raise typer.Exit(code=1)

    anchor_id = before if before is not None else after
    if anchor_id is not None and not has_node_id_prefix(anchor_id):
        typer.echo(f"Error: anchor must be a <prefix>-<4..8 hex> node id, got '{anchor_id}'", err=True)
        raise typer.Exit(code=1)

    result: dict = {}

    def _is_ranked(e: dict) -> bool:
        # Match render._rank_band: a non-finite OR huge-int rank (from a
        # hand-edited graph.json) is treated as unranked, so a poisoned peer
        # can't corrupt the --top/--bottom/midpoint arithmetic or persist a
        # NaN/inf rank. float() guards the OverflowError a giant int raises.
        r = e.get("rank")
        if isinstance(r, bool) or not isinstance(r, (int, float)):
            return False
        try:
            return math.isfinite(float(r))
        except (OverflowError, ValueError):
            return False

    def _lane(e: dict) -> tuple:
        return (_kanban_column(e), _project_key(e))

    def _lane_label(e: dict) -> str:
        col, proj = _lane(e)
        return f"{col or '(off-board)'}/{proj}"

    def mutator(entries):
        node = _find_node(entries, task_id)
        if not node:
            typer.echo(f"Error: feature {task_id} not found", err=True)
            raise typer.Exit(code=1)
        # _find_node fuzzy-resolves partial ids (e.g. `ab-9728`); compare on
        # the RESOLVED id everywhere below so the target is excluded from its
        # own peer set and the self-anchor guard fires for partial input.
        tid = node.get("id") or task_id

        if clear:
            node["rank"] = None
            result.update(action="--clear", rank=None, lane=_lane_label(node), id=tid)
            return entries

        target_lane = _lane(node)
        # Lane peers exclude the target; ranked peers (anchor included) sorted
        # ascending give us the band to insert into.
        peers = [
            e for e in entries if e.get("id") != tid and _lane(e) == target_lane
        ]
        ranked = sorted((e for e in peers if _is_ranked(e)), key=lambda e: e["rank"])

        if top:
            new_rank = (ranked[0]["rank"] - 1.0) if ranked else 0.0
            action = "--top"
        elif bottom:
            new_rank = (ranked[-1]["rank"] + 1.0) if ranked else 0.0
            action = "--bottom"
        else:
            anchor = _find_node(entries, anchor_id)
            if not anchor:
                typer.echo(f"Error: anchor {anchor_id} not found", err=True)
                raise typer.Exit(code=1)
            if anchor.get("id") == tid:
                typer.echo("Error: cannot rank a node relative to itself", err=True)
                raise typer.Exit(code=1)
            if _lane(anchor) != target_lane:
                typer.echo(
                    f"Error: cross-lane rank rejected: {task_id} is in "
                    f"{_lane_label(node)} but anchor {anchor_id} is in "
                    f"{_lane_label(anchor)}. Rank is scoped per (column, project) lane.",
                    err=True,
                )
                raise typer.Exit(code=1)
            if not _is_ranked(anchor):
                typer.echo(
                    f"Error: anchor {anchor_id} is unranked; rank it first "
                    f"(e.g. `fno backlog rank {anchor_id} --top`) or use --top/--bottom.",
                    err=True,
                )
                raise typer.Exit(code=1)
            anchor_rank = float(anchor["rank"])
            if before is not None:
                lowers = [e["rank"] for e in ranked if e["rank"] < anchor_rank]
                lo = max(lowers) if lowers else None
                new_rank = (anchor_rank - 1.0) if lo is None else (lo + anchor_rank) / 2.0
                action = f"--before {anchor_id}"
            else:
                highers = [e["rank"] for e in ranked if e["rank"] > anchor_rank]
                hi = min(highers) if highers else None
                new_rank = (anchor_rank + 1.0) if hi is None else (anchor_rank + hi) / 2.0
                action = f"--after {anchor_id}"

        node["rank"] = new_rank
        result.update(action=action, rank=new_rank, lane=_lane_label(node), id=tid)
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    if result.get("action") == "--clear":
        typer.echo(
            f"Cleared rank on {result['id']} (rejoined the unranked flow in {result['lane']})"
        )
    else:
        typer.echo(
            f"Ranked {result['id']} {result['action']} (rank={result['rank']}) in {result['lane']}"
        )


# -- archive --

@cli.command("archive")
def cmd_archive(
    roadmap_id: Optional[str] = typer.Option(None, "--roadmap-id"),
) -> None:
    from fno.graph.store import locked_mutate_graph
    from fno.graph.store import _apply_graph_defaults, _read_json, _write_json
    from fno.graph._constants import GRAPH_ARCHIVE_JSON

    archived_count: list = [0]

    def mutator(entries):
        if roadmap_id:
            to_archive = [e for e in entries if e.get("_status") == "done" and e.get("roadmap_id") == roadmap_id]
            remaining = [e for e in entries if not (e.get("_status") == "done" and e.get("roadmap_id") == roadmap_id)]
        else:
            to_archive = [e for e in entries if e.get("_status") == "done"]
            remaining = [e for e in entries if e.get("_status") != "done"]

        if not to_archive:
            return entries

        archived_count[0] = len(to_archive)
        archived_ids = {e["id"] for e in to_archive}

        for e in remaining:
            blocked = e.get("blocked_by", [])
            if blocked:
                e["blocked_by"] = [b for b in blocked if b not in archived_ids]

        archive_path = _archive_path()
        from fno.graph.store import GraphCorruptError
        try:
            archive_entries = _apply_graph_defaults(_read_json(archive_path))
        except GraphCorruptError:
            typer.echo(f"Warning: {archive_path} corrupt, starting fresh archive", err=True)
            archive_entries = []
        archive_entries.extend(to_archive)
        archive_path.parent.mkdir(parents=True, exist_ok=True)
        _write_json(archive_entries, archive_path)

        return remaining

    locked_mutate_graph(_graph_path(), mutator)
    if archived_count[0]:
        typer.echo(f"Archived {archived_count[0]} done features to {_archive_path()}")
    else:
        typer.echo("No done features to archive.")


# -- Internal helpers for intake / update (avoid circular imports) --

def _collect_intake_paths_typer(plan_paths: list[str], from_list: Optional[str]) -> list[str]:
    """Build the path list for intake from positional args + --from."""
    paths: list[str] = []
    if from_list:
        if from_list == "-":
            import sys
            raw = sys.stdin.read()
        else:
            try:
                raw = Path(from_list).read_text()
            except OSError as e:
                typer.echo(f"Error: --from {from_list}: {e}", err=True)
                raise typer.Exit(code=1)
        for line in raw.splitlines():
            s = line.strip()
            if not s or s.startswith("#"):
                continue
            paths.append(s)
    for p in plan_paths or []:
        if "," in p and not os.path.exists(p):
            for part in p.split(","):
                part = part.strip()
                if part:
                    paths.append(part)
        else:
            paths.append(p)
    return paths


def _do_intake_multi(args, all_paths: list[str], *, roadmap_id, batch_mode, dry_run) -> None:
    """Multi-path intake flow delegating to intake helpers."""
    from fno.graph._constants import PRIORITY_ORDER
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import (
        _prepare_intake, _build_intake_node, _validate_cli_deps,
        _folder_is_single_feature_plan_fn, _numbered_plan_files_fn,
    )
    from fno.graph.depends import _derive_title

    cli_deps: list[str] = (
        [d.strip() for d in args.deps.split(",") if d.strip()] if args.deps else []
    )
    _validate_cli_deps(cli_deps, read_graph(_graph_path()))

    force_batch = getattr(args, "force_batch", False)
    resolved: list[dict] = []
    for raw in all_paths:
        if not os.path.exists(raw):
            resolved.append({"path": raw, "files": [], "status": "missing"})
            continue
        if os.path.isdir(raw) and batch_mode:
            if _folder_is_single_feature_plan_fn(Path(raw)) and not force_batch:
                resolved.append({"path": raw, "files": [], "status": "single_feature"})
                continue
            candidates = _numbered_plan_files_fn(Path(raw))
            if not candidates:
                resolved.append({"path": raw, "files": [], "status": "empty_batch"})
                continue
            resolved.append({"path": raw, "files": [str(c) for c in candidates], "status": "ready"})
        else:
            resolved.append({"path": raw, "files": [raw], "status": "ready"})

    concrete_files = [f for r in resolved if r["status"] == "ready" for f in r["files"]]
    if not concrete_files:
        for r in resolved:
            if r["status"] == "missing":
                typer.echo(f"warning: not found, skipped: {r['path']}", err=True)
            elif r["status"] == "empty_batch":
                typer.echo(f"warning: --batch folder has no [0-9][0-9]-*.md files: {r['path']}", err=True)
            elif r["status"] == "single_feature":
                typer.echo(
                    f"warning: --batch refused for {r['path']} - INDEX has "
                    "wave/phase structure (single-feature plan). Drop --batch "
                    "or pass --force-batch to override.",
                    err=True,
                )
        typer.echo(
            f"Error: nothing to intake (0 of {len(all_paths)} paths resolved)",
            err=True,
        )
        raise typer.Exit(code=4)

    preview_entries = read_graph(_graph_path())
    if roadmap_id and not args.force_new_roadmap:
        has_roadmap = any(e.get("roadmap_id") == roadmap_id for e in preview_entries)
        if not has_roadmap:
            typer.echo(
                f"unknown roadmap_id: {roadmap_id} "
                "(use /megawalk vision.md to create a roadmap first, "
                "pass --force-new-roadmap, or omit --roadmap-id to intake to the backlog)",
                err=True,
            )
            raise typer.Exit(code=2)

    if dry_run:
        typer.echo(f"Multi-intake preview (dry-run, no changes): {len(all_paths)} paths:")
        for r in resolved:
            if r["status"] == "missing":
                typer.echo(f"  warning: not found, skipped: {r['path']}")
                continue
            if r["status"] in ("empty_batch", "single_feature"):
                typer.echo(f"  warning: skipped: {r['path']}")
                continue
            for f in r["files"]:
                t = _derive_title(Path(f), args.title) if os.path.isfile(f) else os.path.basename(f.rstrip(os.sep))
                typer.echo(f'  would intake: "{t}"  (plan: {f})')
        typer.echo(f"{len(concrete_files)} plans would be intaked. Run without --dry-run to apply.")
        return

    typer.echo(f"Multi-intake {len(concrete_files)} plans:")
    tallies = {"intaked": 0, "already": 0}
    cli_project = getattr(args, "project", None)
    landed_projects: set[str] = set()

    def mutator(es):
        for r in resolved:
            if r["status"] != "ready":
                if r["status"] == "missing":
                    typer.echo(f"  warning: not found, skipped: {r['path']}")
                elif r["status"] in ("empty_batch", "single_feature"):
                    typer.echo(f"  warning: skipped: {r['path']}")
                continue
            for f in r["files"]:
                prep = _prepare_intake(
                    f, es,
                    roadmap_id=roadmap_id, cli_title=args.title,
                    cli_priority=args.priority, cli_deps=cli_deps,
                    cli_points=args.points,
                    cli_project=cli_project,
                )
                if prep["status"] == "already":
                    tallies["already"] += 1
                    typer.echo(f'  already intaked {prep["id"]}: "{prep["title"]}"  ({f})')
                    continue
                node = _build_intake_node(prep["node_spec"], es)
                es.append(node)
                tallies["intaked"] += 1
                typer.echo(f'  intake {node["id"]}: "{node["title"]}"  ({f})')
                if isinstance(node.get("project"), str):
                    landed_projects.add(node["project"])
        return es

    locked_mutate_graph(_graph_path(), mutator)

    from fno.graph._intake import _warn_unknown_project, _list_known_projects
    known = _list_known_projects()
    for proj in sorted(landed_projects):
        _warn_unknown_project(proj, known=known)

    missing = sum(1 for r in resolved if r["status"] in ("missing", "empty_batch", "single_feature"))
    typer.echo(
        f'\n{tallies["intaked"]} newly intaked, '
        f'{tallies["already"]} already intaked, '
        f'{missing} skipped.'
    )
    if tallies["intaked"] + tallies["already"] == 0:
        raise typer.Exit(code=4)


# -- find --


@cli.command("find")
def cmd_find(
    query: str = typer.Argument(..., help="ab-id / id-prefix / slug / bare-hex / free-text description"),
    domain: Optional[str] = typer.Option(None, "--domain", "-d", help="Filter by domain"),
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Filter by project"),
    status: Optional[str] = typer.Option(None, "--status", "-s", help="Filter by _status"),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit JSON array"),
) -> None:
    """Search graph entries: exact id/slug/bare-hex, else high-recall over title+slug+details.

    The describe-it candidate generator (ab-f82e8083): a free-text query matches
    across title, slug, AND details so the model has the recall it needs to rank
    a fuzzy description. An ``ab-`` query keeps the existing id/prefix resolution
    (resolve_id) byte-for-byte so `done`/intake callers are unaffected.
    """
    from fno.graph.fuzzy import resolve_id, resolve_node, search_entries
    from fno.graph.slug import format_handle
    from fno.graph.store import read_graph

    entries = read_graph(_graph_path())
    q = (query or "").strip()

    # Exact resolution first (id / slug / bare-hex). Trying resolve_node BEFORE
    # the ab- prefix branch is deliberate: a title can slugify to an `ab-`-led
    # slug (e.g. "AB test cleanup" -> `ab-test-cleanup`), which resolve_id would
    # reject as a malformed id; the exact-slug tier catches it so `find` and
    # `get` resolve the same slug (codex P2).
    node = resolve_node(query, entries)
    if node.kind == "exact":
        matched: list[dict] = list(node.candidates)
    elif q.startswith("ab-"):
        # Canonical id / id-prefix path - unchanged (resolve_id owns it).
        match = resolve_id(query, entries)
        if match.kind == "ambiguous":
            matched = list(match.candidates)
        elif match.kind in {"exact", "fuzzy", "branch_derived"}:
            matched = [e for e in entries if e.get("id") == match.id]
        else:
            matched = []
    else:
        # High-recall describe-it search over title+slug+details.
        matched = search_entries(query, entries, fields=("title", "slug", "details"))

    def _passes_filters(e: dict) -> bool:
        if domain is not None and e.get("domain") != domain:
            return False
        if project is not None and e.get("project") != project:
            return False
        if status is not None and e.get("_status") != status:
            return False
        return True

    matched = [e for e in matched if _passes_filters(e)]

    if not matched:
        typer.echo(f"fno find: no matches for {query!r}", err=True)
        raise typer.Exit(code=1)

    if json_output:
        typer.echo(json.dumps(matched, indent=2))
        return

    for e in matched:
        # Lead with the slug-forward handle (`slug (ab-id)`, or `(ab-id)` when
        # unslugged); the canonical hex stays present and copyable (ab-f82e8083).
        typer.echo(
            "\t".join([
                format_handle(e),
                e.get("_status", "?"),
                e.get("domain", "?"),
                e.get("project", "-") or "-",
                e.get("title", ""),
            ])
        )


# -- new --


@cli.command("new")
def cmd_new(
    title: str = typer.Argument(..., help="Title of the new entry"),
    domain: str = typer.Option("code", "--domain", help="Domain (fuzzy-suggested against history)"),
    project: Optional[str] = typer.Option(
        None, "--project",
        help="Project name. Defaults to current git repo's basename; pass --unscoped to skip auto-scope.",
    ),
    priority: str = typer.Option("p2", "--priority", help="p0|p1|p2|p3"),
    unscoped: bool = typer.Option(
        False, "--unscoped",
        help="Create with project=null and cwd=null. Default auto-scopes to current git repo.",
    ),
    force_domain: bool = typer.Option(
        False, "--force-domain",
        help="Skip the fuzzy domain suggestion and use --domain verbatim.",
    ),
    source_kind: str = typer.Option(
        "organic", "--source-kind",
        help="organic|from_inbox|from_observation|from_supervisor",
    ),
    source_project: Optional[str] = typer.Option(None, "--source-project", help="Source project name"),
    source_session_id: Optional[str] = typer.Option(None, "--source-session-id", help="Source session ID"),
    source_inbox_msg: Optional[str] = typer.Option(None, "--source-inbox-msg", help="Source inbox message ID"),
) -> None:
    """Create a new graph entry without a plan file.

    Auto-scopes project and cwd from the current git repo by default. Pass
    --unscoped to opt out (e.g. for cross-project ideas with no clear home).
    --project always overrides the auto-detected name when both are present.
    """
    from fno.graph._constants import PRIORITY_ORDER, mint_node_id
    from fno.graph.fuzzy import suggest_domain
    from fno.graph.store import read_graph, locked_mutate_graph

    _VALID_SOURCE_KINDS = {"organic", "from_inbox", "from_observation", "from_supervisor"}
    if source_kind not in _VALID_SOURCE_KINDS:
        typer.echo(
            f"Error: invalid --source-kind '{source_kind}'. "
            f"Must be one of: {', '.join(sorted(_VALID_SOURCE_KINDS))}",
            err=True,
        )
        raise typer.Exit(code=1)

    if priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    entries = read_graph(_graph_path())

    if not force_domain:
        sugg = suggest_domain(domain, entries)
        if sugg.confidence == "fuzzy" and sugg.match != domain:
            typer.echo(
                f"fno new: did you mean --domain {sugg.match}? "
                f"Pass --domain {sugg.match} or add --force-domain to keep {domain!r}.",
                err=True,
            )
            raise typer.Exit(code=2)
        # 'exact' and 'new' pass through silently.

    # Auto-scope from current git repo unless --unscoped is set. --project
    # always overrides the auto-detected basename. Skipping the auto-scope
    # gives us back the pre-fix behavior for the rare global-idea case.
    #
    # Uses the shared resolve_git_roots() helper: linked worktrees record the
    # canonical main checkout as cwd (a durable node outlives its worktree)
    # while keeping the canonical repo basename as project (so all worktrees
    # of the same repo share one project name).
    #
    # When --project is explicit, derive cwd from the work-map first regardless
    # of --unscoped. An explicit project is a stronger signal than the
    # auto-scope default.
    resolved_project = project
    resolved_cwd: Optional[str] = None
    if project is not None:
        from fno.graph._intake import project_root_from_settings
        resolved_cwd = project_root_from_settings(project)
        # resolved_project stays as-is (the explicit flag value)
    if resolved_cwd is None and not unscoped:
        from fno.graph._intake import resolve_git_roots
        derived_name, canonical_root = resolve_git_roots()
        if canonical_root:
            resolved_cwd = canonical_root
            if resolved_project is None:
                resolved_project = derived_name

    new_id_holder: list[Optional[str]] = [None]

    def mutator(es: list[dict]) -> list[dict]:
        new_id = mint_node_id({e.get("id") for e in es})
        new_id_holder[0] = new_id
        node = {
            "id": new_id,
            "parent": None,
            "title": title,
            "type": "feature",
            "project": resolved_project,
            "cwd": resolved_cwd,
            "priority": priority,
            "domain": domain,
            "blocked_by": [],
            "session_id": None,
            "claimed_at": None,
            "completed_at": None,
            "has_brief": False,
            "compacted": False,
            "roadmap_id": None,
            "vision_path": None,
            "details": None,
            "size": None,
            "batch": None,
            "cost_usd": None,
            "cost_sessions": [],
            "plan_path": None,
            "pr_number": None,
            "pr_url": None,
            "merge_status": None,
            "artifact_url": None,
            "completion_note": None,
            "source": "abi-new",
            "created_at": datetime.now(timezone.utc).isoformat(),
            "source_kind": source_kind,
            "source_project": source_project,
            "source_session_id": source_session_id,
            "source_inbox_msg": source_inbox_msg,
        }
        es.append(node)
        return es

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(new_id_holder[0])


# -- rehash --

@cli.command("rehash")
def cmd_rehash(
    revert: bool = typer.Option(
        False,
        "--revert",
        help="Restore graph.json from the latest backup instead of rehashing.",
    ),
) -> None:
    """Acknowledge an external edit to graph.json by rehashing the sidecar (default).

    With --revert: locate the most recent graph.json.bak.* backup and restore it,
    then update the sidecar to match.
    """
    import hashlib
    import shutil
    import tempfile

    path = _graph_path()

    if revert:
        # Find the most-recent .bak.* file
        backups = sorted(path.parent.glob(f"{path.name}.bak.*"))
        if not backups:
            typer.echo(
                f"No backups found for {path}. Cannot revert.", err=True
            )
            raise typer.Exit(code=1)
        latest_backup = backups[-1]
        # Atomic restore: temp + rename
        tmp_fd, tmp_path_str = tempfile.mkstemp(dir=path.parent, suffix=".tmp")
        try:
            with os.fdopen(tmp_fd, "wb") as f:
                f.write(latest_backup.read_bytes())
            os.replace(tmp_path_str, str(path))
        except Exception:
            Path(tmp_path_str).unlink(missing_ok=True)
            raise
        typer.echo(f"Reverted graph.json from {latest_backup.name}")

    # Rehash sidecar to match current (possibly just-restored) content
    if not path.exists():
        typer.echo(f"graph.json not found at {path}", err=True)
        raise typer.Exit(code=1)

    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    sidecar = Path(str(path) + ".sha256")
    tmp_fd2, tmp_path2 = tempfile.mkstemp(dir=path.parent, suffix=".sha256.tmp")
    try:
        with os.fdopen(tmp_fd2, "w") as f:
            f.write(digest + "\n")
        os.replace(tmp_path2, str(sidecar))
    except Exception:
        Path(tmp_path2).unlink(missing_ok=True)
        raise
    typer.echo(f"Reconciled to hash {digest[:8]}")


# ---------------------------------------------------------------------------
# collisions sub-app: file-overlap detection between plans
# ---------------------------------------------------------------------------

collisions_app = typer.Typer(
    name="collisions",
    help="Plan collision queries (file-overlap detection)",
    no_args_is_help=True,
)


@collisions_app.command("check")
def cmd_collisions_check(
    plan_path: Path = typer.Argument(..., help="Plan file or folder to check"),
    self_id: Optional[str] = typer.Option(
        None, "--self-id", help="Skip this node ID when comparing (excludes self-collision)",
    ),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit structured JSON instead of human text"),
) -> None:
    """Check a plan against all pending nodes for file collisions.

    Severity thresholds resolve from project then user ``settings.yaml`` and
    fall back to v1 defaults. Recommended actions are inferred deterministically
    from set relationships and plan ages.
    """
    from dataclasses import asdict

    from fno.graph.collision import find_collisions
    from fno.graph.store import read_graph

    entries = read_graph(_graph_path())
    collisions = find_collisions(plan_path, entries, self_id=self_id)

    if json_output:
        # Drop the private _other_created_at field from JSON output.
        payload = []
        for c in collisions:
            d = asdict(c)
            d.pop("_other_created_at", None)
            payload.append(d)
        typer.echo(json.dumps(payload, indent=2))
        return

    if not collisions:
        typer.echo(f"No collisions found for {plan_path}")
        return

    for c in collisions:
        typer.echo(f"[{c.severity.upper()}] {c.with_node_id} ({c.with_node_title})")
        typer.echo(f"  shared: {', '.join(c.shared_files)}")
        typer.echo(f"  recommended: {c.recommended_action}")
        typer.echo(f"  rationale: {c.rationale}")
        typer.echo("")


cli.add_typer(collisions_app, name="collisions")


# ---------------------------------------------------------------------------
# supersede: mark old node as replaced by a new node; auto-defer the old one
# ---------------------------------------------------------------------------


@cli.command("supersede")
def cmd_supersede(
    new_id: str = typer.Argument(..., help="The new node ID that replaces the old"),
    replaces: str = typer.Option(..., "--replaces", help="The old node ID being superseded"),
    reason: str = typer.Option(..., "--reason", "-R", help="Why supersede (free text, surfaces in triage)"),
) -> None:
    """Mark ``replaces`` as superseded by ``new_id``; defer ``replaces`` automatically.

    Sets ``superseded_by`` on the old node and appends to ``supersedes`` on
    the new node. Also sets ``deferred_at`` + ``deferred_reason`` on the old
    node so it stops appearing in active lists.
    """
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    if not has_node_id_prefix(new_id):
        typer.echo(f"Error: new_id must be a <prefix>-<4..8 hex> node id, got '{new_id}'", err=True)
        raise typer.Exit(code=1)
    if not has_node_id_prefix(replaces):
        typer.echo(f"Error: --replaces must be a <prefix>-<4..8 hex> node id, got '{replaces}'", err=True)
        raise typer.Exit(code=1)
    if new_id == replaces:
        typer.echo("Error: cannot supersede self", err=True)
        raise typer.Exit(code=1)

    cleaned_reason = reason.strip()
    if not cleaned_reason:
        typer.echo("Error: --reason cannot be blank", err=True)
        raise typer.Exit(code=1)

    def mutator(entries):
        new_node = _find_node(entries, new_id)
        old_node = _find_node(entries, replaces)
        if new_node is None:
            typer.echo(f"Error: new node {new_id} not found", err=True)
            raise typer.Exit(code=1)
        if old_node is None:
            typer.echo(f"Error: old node {replaces} not found", err=True)
            raise typer.Exit(code=1)

        # Refuse to supersede a shipped or already-superseded node. Without
        # this guard, the mutation would silently clear the old node's
        # completed_at to let the precedence cascade flip to superseded -
        # erasing the ship timestamp on a shipped plan and destroying
        # forensic history. Use a follow-up node instead.
        if old_node.get("completed_at") or old_node.get("_status") == "done":
            typer.echo(
                f"Error: cannot supersede {replaces}: it is already shipped "
                f"(_status=done). Open a follow-up node instead.",
                err=True,
            )
            raise typer.Exit(code=1)
        if old_node.get("superseded_by"):
            typer.echo(
                f"Error: cannot supersede {replaces}: it is already superseded "
                f"by {old_node['superseded_by']}. Resolve the existing supersede chain first.",
                err=True,
            )
            raise typer.Exit(code=1)

        supersedes = list(new_node.get("supersedes") or [])
        if replaces not in supersedes:
            supersedes.append(replaces)
        new_node["supersedes"] = supersedes
        old_node["superseded_by"] = new_id
        old_node["session_id"] = None
        old_node["claimed_at"] = None
        old_node["deferred_at"] = datetime.now(timezone.utc).isoformat()
        old_node["deferred_reason"] = f"superseded by {new_id}: {cleaned_reason}"
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"superseded {replaces} with {new_id}")

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

from fno.harness_identity import resolve_harness_identity

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

cli.add_typer(_capture_cli, name="capture", hidden=True)
cli.add_typer(_capture_cli, name="inbox", hidden=True)

# Nested batch sub-app: `fno backlog batch <verb>`. Batch-lane state
# (.fno/batches/<domain>.json) — coalesce same-domain nodes into one PR.
from fno.backlog.batch import cli as _batch_cli  # noqa: E402

cli.add_typer(_batch_cli, name="batch", hidden=True)


# Selection-time enforcement (ab-fcf9cec5): a node another session is actively
# driving holds a live ``node:<id>`` claim and must be skipped so two sessions
# never pick up the same node. The implementation is homed in graph/statuses.py
# (so the board renderers can share it without a cli<->render cycle); re-exported
# under the original module-global name that existing tests monkeypatch.
from fno.graph.statuses import live_claimed_node_ids as _live_claimed_node_ids  # noqa: E402


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


def _is_batched_member(e: dict) -> bool:
    """True when a node is already committed to an open batch (batch-lane Wave 2).

    A batched member has its atomic commits on a shared batch branch and ships as
    part of the batch PR, not its own. It must NOT be re-selected for dispatch
    (else the daemon would spawn a second worker for work already on the branch).
    The mark is the graph `batch` field, set by `/target batched` via
    `fno backlog update --batch <id>` and cleared (`--batch null`) on abandon so
    the node resurfaces for an individual ship. Mirrors `_has_unmerged_open_pr`:
    an in-flight signal that survives the builder session's PID-claim dying.
    """
    return bool(e.get("batch"))


def _container_ids(entries: list[dict]) -> set[str]:
    """Ids of nodes that are some other node's ``parent`` - i.e. epics/containers.

    A container is never directly buildable: its work lives in its decomposed
    children, and it carries no PR of its own. So every work-SELECTION surface
    drops it from the candidate pool - `next`/`ready`/`--all-ready` build the
    leaves, never the box (x-33b2). Shared by `next` (_pick_ready) and `ready`
    (cmd_ready) so the two surfaces cannot drift; advance_dependents applies the
    same rule on the merge edge-following path.

    No "keep the all-done epic selectable" exception is needed: an epic closes
    automatically via ``_cascade_close_parents`` on the merge that finishes its
    last child (uniform across projects), so it is already ``done`` - never a
    lingering ``ready`` container that selection would have to surface for
    closure. That replaces the old "walker closes the epic via next" path
    (loop_megawalk.rs grilled-decision-9), which conflicted with never building a
    container.
    """
    return {
        e.get("parent") for e in entries
        if isinstance(e, dict) and isinstance(e.get("parent"), str)
    }


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


# -- relatedness sidecar (`fno backlog relatedness build|get`) --
# A node-to-node relatedness map read by x-9ed6's offer path and /triage.
# Sidecar, not a graph mutation, so `build` writes unconditionally.

_relatedness_cli = typer.Typer(
    name="relatedness",
    help="Node-to-node relatedness map (sidecar next to graph.json).",
    no_args_is_help=True,
)


def _relatedness_path() -> Path:
    from fno.paths import relatedness_json
    return relatedness_json()


@_relatedness_cli.command("build")
def cmd_relatedness_build(
    project: Optional[str] = typer.Option(None, "--project", "-p", help="Restrict the corpus to this project."),
    judge: bool = typer.Option(False, "--judge", help="Haiku pairwise refinement (v2, opt-in); v1 is deterministic-only."),
    top_k: int = typer.Option(5, "--top-k", "-K", help="Edges persisted per node."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit the built map as JSON."),
) -> None:
    """Build the relatedness sidecar from graph signals (read-only on the graph)."""
    from fno.graph.store import read_graph
    from fno.graph import relatedness as _r

    entries = read_graph(_graph_path())
    if project is not None:
        entries = [e for e in entries if e.get("project") == project]
    mapping = _r.build_map(entries, k=top_k)
    if judge:
        # Degrade, never abort: v1 has no judge layer, so note and write the
        # deterministic map (AC6 posture - LLM absence never blocks the write).
        typer.echo("note: --judge (haiku refinement) not implemented in v1; wrote deterministic map.", err=True)
    path = _relatedness_path()
    _r.write_map(path, mapping)
    if json_output:
        typer.echo(json.dumps(mapping, indent=2))
    else:
        edges = sum(len(v) for v in mapping.values())
        typer.echo(f"relatedness: {len(mapping)} nodes, {edges} edges -> {path}")


@_relatedness_cli.command("get")
def cmd_relatedness_get(
    node_id: str = typer.Argument(..., help="Node id to fetch related nodes for."),
    top_k: int = typer.Option(5, "--top-k", "-K", help="Max related nodes to return."),
    json_output: bool = typer.Option(False, "--json", "-J", help="Emit a JSON array."),
) -> None:
    """Print the top related nodes for one node (the x-9ed6 consumer API).

    No map -> exit non-zero, empty stdout (the caller's fallback signal).
    Present map, no edges -> exit 0, empty list. The two are distinct (AC3).
    """
    from fno.graph import relatedness as _r

    try:
        edges = _r.get_related(_relatedness_path(), node_id, k=top_k)
    except _r.NoMapError:
        raise typer.Exit(code=1)
    if json_output:
        typer.echo(json.dumps(edges, indent=2))
    else:
        for r in edges:
            typer.echo(f"{r['id']}\t{r['score']}\t{r['reason']}")


cli.add_typer(_relatedness_cli, name="relatedness", hidden=True)


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

    identity = resolve_harness_identity()
    session = identity.session_id
    harness = identity.harness

    source_node_id: Optional[str] = None
    source_plan_path: Optional[str] = None
    if session and harness == "claude":
        try:
            text = (Path(cwd) / ".fno" / "target-state.md").read_text(encoding="utf-8")
            # Current key is claude_session_id; old-key fallback for one release.
            manifest_claude_sid = _scan_md_field(text, "claude_session_id") or _scan_md_field(
                text, "claude_transcript_id"
            )
            if manifest_claude_sid == session:
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
    tags: Optional[list[str]] = None,
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
        "tags": list(tags or []),
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


def _create_node_impl(
    *,
    title: str,
    type_: str = "feature",
    parent: Optional[str] = None,
    project: Optional[str] = None,
    cwd: Optional[str] = None,
    priority: str = "p2",
    domain: str = "code",
    blocked_by: Optional[str] = None,
    roadmap_id: Optional[str] = None,
    vision_path: Optional[str] = None,
    details: Optional[str] = None,
    description: Optional[str] = None,
    size: Optional[str] = None,
    batch: Optional[str] = None,
    tags: Optional[list[str]] = None,
) -> None:
    """Shared create-a-backlog-node body for ``cmd_add`` and ``cmd_idea``.

    Both verbs create a plan-less node (which derives to ``_status: idea``);
    ``idea`` is just sugar for ``add``. Centralizing the body keeps their flag
    sets and behavior from drifting - the divergence that used to force a second
    ``fno backlog update`` just to set parent/size/domain on a fresh idea.
    """
    from fno.graph._constants import PRIORITY_ORDER, mint_node_id
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import (
        detect_project_from_settings,
        project_root_from_settings,
        repo_root,
    )

    if priority not in PRIORITY_ORDER:
        typer.echo(
            f"Error: invalid priority '{priority}'. "
            f"Must be: {', '.join(PRIORITY_ORDER.keys())}",
            err=True,
        )
        raise typer.Exit(code=1)

    if details is not None and description is not None:
        typer.echo("Error: pass --details or --description, not both", err=True)
        raise typer.Exit(code=1)
    resolved_details = details if details is not None else description

    from fno.graph._constants import normalize_tag
    try:
        resolved_tags = list(dict.fromkeys(normalize_tag(t) for t in (tags or [])))
    except ValueError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=1)

    # Store an absolute path so downstream `detect_project()` (which compares
    # against `repo_root()` via normpath) finds matches. No explicit --cwd:
    # record the canonical main checkout (repo_root()), not os.getcwd() - a
    # backlog node outlives the worktree it was filed from.
    if cwd is not None:
        resolved_cwd = os.path.abspath(os.path.expanduser(cwd))
    elif project is not None:
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
    node_holder: list[Optional[dict]] = [None]

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
            tags=resolved_tags,
        )
        node["id"] = new_id
        # Enforce the epic-nesting cap on the create path too, or `add --type
        # epic --parent <nested-epic>` would slip a 3rd epic level past the same
        # guard cmd_update applies (x-6c2b). Scoped to a real cap violation: a
        # non-epic, or a parent that does not resolve, keeps the existing lenient
        # pass-through (add/idea has never hard-validated --parent).
        if parent:
            from fno.graph._intake import _find_node, _would_exceed_epic_depth
            from fno.graph._constants import EPIC_NEST_MAX_DEPTH

            target = _find_node(entries, parent)
            if target is not None and _would_exceed_epic_depth(entries, node, target):
                typer.echo(
                    f"Error: parenting epic {new_id} under {target['id']} would "
                    f"exceed the {EPIC_NEST_MAX_DEPTH}-level cap (mission -> epic "
                    f"-> leaf); an epic may nest only under a top-level mission",
                    err=True,
                )
                raise typer.Exit(code=1)
        entries.append(node)
        node_holder[0] = node
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    # Born-with-why: route births through the shared birth hook. Gate-first and
    # strictly non-fatal, so a gate-OFF install is a no-op and a dispatch
    # failure never wedges the filing of the node above.
    if node_holder[0] is not None:
        try:
            from fno.provenance.spawn_think import on_node_born

            on_node_born(node_holder[0])
        except Exception:  # noqa: BLE001 - born-with-why is additive; never block birth
            pass

    typer.echo(json.dumps({"id": new_id_holder[0], "title": title}, indent=2))


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
    tag: Optional[List[str]] = typer.Option(
        None, "--tag", hidden=True, help="Tag (repeatable, lowercase-kebab)."
    ),
) -> None:
    _create_node_impl(
        title=title,
        type_=type_,
        parent=parent,
        project=project,
        cwd=cwd,
        priority=priority,
        domain=domain,
        blocked_by=blocked_by,
        roadmap_id=roadmap_id,
        vision_path=vision_path,
        details=details,
        description=description,
        size=size,
        batch=batch,
        tags=tag,
    )


# -- idea (sugar verb) --

@cli.command("idea")
def cmd_idea(
    title: str = typer.Argument(..., help="Idea title - what is this?"),
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
        help="Working directory. Defaults to the current working directory.",
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
    tag: Optional[List[str]] = typer.Option(
        None, "--tag", hidden=True, help="Tag (repeatable, lowercase-kebab)."
    ),
) -> None:
    """Capture an idea (a plan-less backlog node) with minimal ceremony.

    Equivalent to `fno backlog add <title>` but signals intent to skip the
    spec/plan ceremony for now. The new node has no ``plan_path`` and so
    derives to ``_status: idea`` until a plan is associated (via
    ``fno backlog intake`` or by setting ``--plan-path`` on
    ``fno backlog update``). Shares ``add``'s full option set so a fresh idea
    can carry parent/size/domain without a follow-up ``fno backlog update``.
    """
    _create_node_impl(
        title=title,
        type_=type_,
        parent=parent,
        project=project,
        cwd=cwd,
        priority=priority,
        domain=domain,
        blocked_by=blocked_by,
        roadmap_id=roadmap_id,
        vision_path=vision_path,
        details=details,
        description=description,
        size=size,
        batch=batch,
        tags=tag,
    )


# -- decompose (bounded epic -> group child nodes) --

@cli.command("decompose", hidden=True)
def cmd_decompose(
    ctx: typer.Context,
    epic_id: str = typer.Argument(..., help="Epic node ab-ID to decompose into group children"),
    groups: str = typer.Option(
        ...,
        "--groups",
        help=(
            "JSON array of {slug,title,waves,blocked_by_groups[,project][,cwd]} "
            "group specs. Optional per-group project/cwd route a child into a "
            "different repo (multi-repo decomposition): project resolves its cwd "
            "from the settings work-map; an explicit cwd overrides; absent -> "
            "inherit the epic's repo. "
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
    plans: str = typer.Option(
        "separate",
        "--plans",
        help=(
            "Per-child plan packaging. Only 'separate' is supported: scaffold a "
            "self-contained quick-plan stub per child and repoint its plan_path "
            "to that file (one plan == one PR == one node). The former 'fragment' "
            "packaging (a <epic-doc>#group-<slug> section of a shared doc) was "
            "removed - it is still recognized on existing children for idempotent "
            "re-decompose, but never authored."
        ),
    ),
) -> None:
    """Upsert group child nodes under an epic (atomic + idempotent).

    Each group becomes one child node (parent=epic) bundling 1+ execution waves
    into a single shippable PR, with its own self-contained
    <stem>.group-<slug>.md quick-plan (the only packaging). Re-running with the
    same slugs updates the existing children in place rather than duplicating,
    keyed on the slug - and a child still on the legacy <epic-doc>#group-<slug>
    fragment form is repointed to its separate file. The whole decomposition
    lands in one locked graph mutation, so a bad spec leaves the graph exactly
    as it was (AC1-FR).
    """
    import sys as _sys
    from fno.graph._constants import mint_node_id
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node, _would_create_cycle
    from fno.graph._decompose import (
        DecomposeError,
        canonical_child_plan_path,
        child_plan_path,
        classify_group_dep,
        extract_contract_versions,
        extract_why_digest,
        find_orphans,
        is_shipped,
        plan_base,
        scaffold_separate_plan,
        separate_plan_path,
        validate_groups,
    )
    from fno.handoff.output import emit_error, json_mode

    if plans == "fragment":
        emit_error(
            ctx,
            "--plans fragment was removed; 'separate' is now the only packaging "
            "(one plan == one PR == one node). Drop the flag or pass --plans separate.",
        )
        raise typer.Exit(code=1)
    if plans != "separate":
        emit_error(ctx, f"--plans must be 'separate' (got {plans!r})")
        raise typer.Exit(code=1)
    separate = True

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
            max_prs = load_settings().blueprint.max_prs_per_epic
        except Exception as e:
            emit_error(ctx, f"could not read config.blueprint.max_prs_per_epic: {e}")
            raise typer.Exit(code=1)

    # 3. Validate the spec entirely before touching the graph (atomicity).
    try:
        norm = validate_groups(parsed, max_prs)
    except DecomposeError as e:
        emit_error(ctx, str(e))
        raise typer.Exit(code=e.exit_code)

    # 3b. Resolve per-group repo routing OUTSIDE the graph lock (settings reads
    #     never happen under the lock, mirroring `update`). A group with an
    #     explicit cwd uses it as-is; a group with only a project derives its
    #     cwd from the work-map and is REFUSED (atomically, before any write) if
    #     that project is unmapped - guessing a cwd would silently record foreign
    #     work under the wrong repo and break spawn-into-project. No project/cwd
    #     -> (None, None) = inherit the epic's repo (the single-repo default).
    from fno.graph._intake import project_root_from_settings
    slug_route: dict[str, tuple[Optional[str], Optional[str]]] = {}
    for grp in norm:
        gproj, gcwd = grp["project"], grp["cwd"]
        if gcwd is not None:
            slug_route[grp["slug"]] = (gproj, os.path.abspath(os.path.expanduser(gcwd)))
        elif gproj is not None:
            root = project_root_from_settings(gproj)
            if root is None:
                emit_error(
                    ctx,
                    f"group {grp['slug']!r} project {gproj!r} is not in any "
                    "settings.yaml work-map; add it there or pass an explicit cwd",
                )
                raise typer.Exit(code=1)
            slug_route[grp["slug"]] = (gproj, root)
        else:
            slug_route[grp["slug"]] = (None, None)

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
        verbatim_base_box[0] = base  # the relative base, for the source_doc seed
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
        # Stash the epic's cwd (US4) so the post-lock scaffold step can resolve an
        # inherited child's child_root outside the lock (mirrors base_box).
        epic_cwd_box[0] = live_epic.get("cwd")

        # Read the epic doc's pinned interface-contract version(s). The doc is
        # the single source of truth: a `contract`-tier group is eligible only
        # when the doc pins a `## Interface Contract` (G1); with no pin every
        # `contract` request falls back to `hard` (AC2-HP). A missing/unreadable
        # doc -> no pin -> all hard (fail-safe; the downgrade is reported, never
        # silent). Local file read under the lock is trivial (the doc is small).
        pinned_versions: set[int] = set()
        if base_box[0]:
            try:
                pinned_versions = extract_contract_versions(
                    Path(base_box[0]).read_text(encoding="utf-8")
                )
            except (OSError, UnicodeDecodeError):
                # No readable doc -> no pin -> contract falls back to hard. Never
                # hard-fail decompose on a doc-read issue (mirrors the stamp path).
                pinned_versions = set()

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
            frag_path = child_plan_path(base, grp["slug"])
            sep_path = separate_plan_path(base, grp["slug"])
            # Tolerant lookup: identity is the durable group_slug (x-edf7 US2), so
            # a child born unlinked (no plan_path yet) is still found; the legacy
            # plan_path match (fragment or separate form) upserts a pre-field child
            # in place instead of duplicating (idempotent on slug across migration).
            existing = next(
                (
                    e
                    for e in graph_entries
                    if e.get("parent") == epic_resolved_id
                    and (
                        e.get("group_slug") == grp["slug"]
                        or e.get("plan_path") in (frag_path, sep_path)
                    )
                ),
                None,
            )
            route_proj, route_cwd = slug_route[grp["slug"]]
            if existing is not None:
                action = "updated"
                node = existing
                node["group_slug"] = grp["slug"]  # backfill identity on legacy children
                # Preserve a designed child's plan_path; NEVER link an unlinked
                # child here (linking is the inline-fill / fan-out step's job, US2).
                # The one exception is the documented legacy-fragment repoint:
                # a child still on `<doc>#group-<slug>` moves to its separate file
                # (staying linked/ready), never unset.
                if node.get("plan_path") == frag_path:
                    node["plan_path"] = sep_path
                # Re-running with an explicit route reprojects an existing child
                # (e.g. a first pass inherited the epic's repo, a later pass adds
                # per-group routing). No route leaves the child's repo untouched.
                if route_proj is not None:
                    node["project"] = route_proj
                if route_cwd is not None:
                    node["cwd"] = route_cwd
            else:
                action = "created"
                # Born UNLINKED (plan_path=None -> derives `idea`): linking the
                # filled plan is the design-completion signal that flips the child
                # `ready` (x-edf7 US2, Locked Decision 4). group_slug is the durable
                # identity that survives the unlinked window.
                node = _build_backlog_node(
                    title=grp["title"],
                    parent=epic_resolved_id,
                    project=route_proj if route_proj is not None else live_epic.get("project"),
                    cwd=route_cwd if route_cwd is not None else live_epic.get("cwd"),
                    priority=live_epic.get("priority", "p2"),
                    domain=live_epic.get("domain", "code"),
                    plan_path=None,
                )
                node["group_slug"] = grp["slug"]
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

            # Classify the dependency tier against the doc's pin. Stamp the
            # contract fields ONLY on a `contract` dep; pop them on `hard` so a
            # re-decompose downgrade (contract -> hard) cleans up stale stub
            # metadata and the pure-hard path serializes byte-for-byte unchanged
            # (Invariant). The downgrade reason, if any, is surfaced after the lock.
            dep, stub_against, cversion, downgrade = classify_group_dep(
                grp, pinned_versions, base
            )
            if dep == "contract":
                node["dep"] = "contract"
                node["stub_against"] = stub_against
                node["contract_version"] = cversion
                r["dep"] = "contract"
            else:
                node.pop("dep", None)
                node.pop("stub_against", None)
                node.pop("contract_version", None)
                r["dep"] = "hard"
            if downgrade:
                downgrade_box[0].append(downgrade)

        # Surface any unshipped orphans (slug dropped from the spec). They are
        # left in place, not deleted - deleting graph nodes is destructive.
        orphan_box[0] = [o["id"] for o in orphans]
        return graph_entries

    orphan_box: list[list[str]] = [[]]
    base_box: list = [None]
    verbatim_base_box: list = [None]
    epic_cwd_box: list = [None]
    downgrade_box: list[list[str]] = [[]]
    try:
        locked_mutate_graph(_graph_path(), mutator)
    except DecomposeError as e:
        emit_error(ctx, str(e))
        raise typer.Exit(code=e.exit_code)

    epic_resolved_id = epic_id_box[0]
    orphan_ids = orphan_box[0]
    downgrades = downgrade_box[0]

    # Shared post-mutation graph re-read: 3c reads each child's created_at +
    # plan_path from it, and fan-out 4a reuses it. One read, not two. A read
    # failure degrades to an empty map (scaffold falls back to today's date, the
    # fan-out step is a no-op) rather than wedging the already-committed mutation.
    from fno.graph.store import read_graph as _read_graph
    try:
        by_id = {e.get("id"): e for e in _read_graph(_graph_path())}
    except Exception:  # noqa: BLE001 - never wedge the report on a re-read failure
        by_id = {}

    # 3c. Scaffold per-child quick-plan files (--plans separate). Runs OUTSIDE
    #     the graph lock (mirrors the _set_expected_count doc write below), because
    #     it reads settings (plans_content_dir walks .claude/settings) and settings
    #     reads never happen under the lock. Each child is born at its CANONICAL
    #     `fno plan path` name, routed into the CHILD project's plans dir - not the
    #     epic's dir, not the legacy `.group-<slug>.md` name (x-d6a6).
    # US4: transcribe the epic's why (intent + Locked Decisions) once - every child
    # scaffold is born grounded, AND a fan-out seed carries it so the /think worker
    # stays grounded even when its origin transcript is unresolved. A missing
    # Locked-Decisions block degrades to intent-only + a warning; an unreadable doc
    # yields an empty digest (the scaffold then seeds the validator-rejected stub).
    why_digest = ""
    if separate and base_box[0]:
        try:
            why_digest, why_warn = extract_why_digest(
                Path(base_box[0]).read_text(encoding="utf-8")
            )
            if why_warn:
                typer.echo(f"warning: {why_warn}", err=True)
        except (OSError, UnicodeDecodeError):
            pass

    scaffolded: list[str] = []
    if separate and base_box[0]:
        from fno.graph._intake import repo_root
        source_doc = verbatim_base_box[0] or base_box[0]
        id_by_slug = {r["slug"]: r["id"] for r in results}
        for grp in norm:
            slug = grp["slug"]
            child_id = id_by_slug.get(slug)
            if not child_id:
                continue
            child = by_id.get(child_id)
            # Skip 1: already linked - never spawn a stub beside a filled plan
            # (Locked Decision 6; also grandfathers a repointed legacy fragment).
            if child and child.get("plan_path"):
                continue
            # Skip 2: a legacy `.group-<slug>.md` file exists - grandfather it in
            # place, no rename, no canonical duplicate (Locked Decision 4).
            if Path(separate_plan_path(base_box[0], slug)).exists():
                continue
            # Route the stub into the CHILD project's plans dir. The child node's
            # own cwd is the authoritative per-child root: the mutator set it to
            # the routed cwd (or inherited the epic's) at mint, so it already
            # reflects a route made WITHOUT an explicit re-route - a routed child
            # re-decomposed with no route keeps its own repo, not the epic's.
            # created_at sources the filename date, so a later-day re-decompose
            # recomputes the SAME path (idempotent). Fall back to the epic cwd
            # then repo_root() only if the re-read lost the node.
            child_root = (
                (child.get("cwd") if child else None)
                or epic_cwd_box[0]
                or repo_root()
            )
            canonical = Path(
                canonical_child_plan_path(
                    slug, child_id, str(child_root),
                    child.get("created_at") if child else None,
                )
            )
            # Skip 3: canonical already on disk - idempotent re-run.
            if canonical.exists():
                continue
            try:
                canonical.parent.mkdir(parents=True, exist_ok=True)
                canonical.write_text(
                    scaffold_separate_plan(
                        grp, epic_resolved_id, source_doc, why_digest=why_digest
                    ),
                    encoding="utf-8",
                )
                scaffolded.append(str(canonical))
            except OSError as e:
                # Non-fatal: the graph is already the source of truth. Warn loudly
                # so the missing stub is visible, never silently swallowed.
                typer.echo(
                    f"warning: could not scaffold separate plan {canonical}: {e}",
                    err=True,
                )

    # 4a. Per-child design pass (x-edf7 US3) + born-with-why (v2 A1). Runs BEFORE
    #     the report so a flagged fan-out's outcome rides in the --json payload
    #     (a machine caller must see when a child was left an unlinked idea, not a
    #     silent success). Two lanes, one shared RunState bounding the batch's blast
    #     radius (AC1-EDGE):
    #       - `needs_think` group -> FORCE a fan-out /think+/blueprint design pass.
    #         The decompose invocation IS the operator consent (Locked Decision 3),
    #         so the gate + attended-offer are overridden (mirrors the
    #         dispatch_conversational env-forcing); the caps still bound it. A spawn
    #         that does not fire leaves the child `idea` with its stub on disk.
    #       - unflagged group -> today's opt-in born-with-why OFFER (gate-OFF
    #         default => complete no-op).
    #     Only UNLINKED children are candidates (a re-decompose never re-designs a
    #     child that already has a plan). Strictly non-fatal: never wedge decompose.
    fanout: list[dict] = []
    flagged_slugs = {g["slug"] for g in norm if g["needs_think"]}
    slug_by_id = {r["id"]: r["slug"] for r in results}
    created_ids = {r["id"] for r in results if r["action"] == "created"}
    spec_ids = [r["id"] for r in results]
    if spec_ids:
        try:
            from fno.provenance.spawn_think import (
                RunState,
                maybe_spawn_think,
                on_node_born,
            )

            # Reuse the shared post-mutation re-read from 3c (by_id).
            born_rs = RunState()
            # Force the gate + spawn (over the default-OFF / attended-offer) for the
            # flagged fan-out only; reuses the exact env seams dispatch_conversational
            # uses, so no new maybe_spawn branch.
            forced_env = {
                **os.environ,
                "FNO_THINK_SPAWN": "1",
                "FNO_THINK_SPAWN_ATTENDED": "spawn",
            }
            for cid in spec_ids:
                child = by_id.get(cid)
                if child is None or child.get("plan_path"):
                    continue  # already-linked children are done; nothing to design
                if slug_by_id.get(cid) in flagged_slugs:
                    # chain_blueprint: the worker must continue /think -> /blueprint
                    # -> link, else the flagged child stays designless/idea forever
                    # (a bare /think never links plan_path). why_digest keeps it
                    # grounded when the transcript is unresolved; project_root scopes
                    # the /think doc to the CHILD's repo (cross-repo routing).
                    child_root = child.get("_resolved_cwd") or child.get("cwd")
                    res = maybe_spawn_think(
                        child, run_state=born_rs, env=forced_env,
                        quiet=json_mode(ctx), chain_blueprint=True,
                        why_digest=why_digest,
                        project_root=Path(child_root) if child_root else None,
                    )
                    fanout.append({"id": cid, "decision": res.decision,
                                   "reason": res.reason})
                    if res.decision != "spawned" and not json_mode(ctx):
                        typer.echo(
                            f"fan-out /think for {cid} did not spawn "
                            f"({res.reason}); run `/think {cid}` then `/blueprint` "
                            f"to design it (child left idea with its stub)",
                            err=True,
                        )
                elif cid in created_ids:
                    # Already the persisted, slugged node -> skip the re-read.
                    # quiet in --json mode: the offer stderr print would pollute a
                    # captured JSON stream (test_json_output_shape).
                    on_node_born(child, run_state=born_rs, persisted=True,
                                 quiet=json_mode(ctx))
        except Exception:  # noqa: BLE001 - additive; never wedge the decompose
            pass

    # 4b. Report what happened (AC1-UI).
    if json_mode(ctx):
        typer.echo(json.dumps(
            {
                "epic": epic_resolved_id,
                "groups": results,
                "orphaned": orphan_ids,
                "downgrades": downgrades,
                "packaging": plans,
                "scaffolded": scaffolded,
                "fanout": fanout,
            },
            default=str,
        ))
    else:
        typer.echo(f"epic: {epic_resolved_id}")
        typer.echo(
            f"decomposed into {len(results)} group child node(s) "
            f"(packaging: {plans}):"
        )
        for r in results:
            waves = f" waves {r['waves']}" if r["waves"] else ""
            blk = f" blocked_by={r['blocked_by']}" if r["blocked_by"] else ""
            tier = " dep=contract" if r.get("dep") == "contract" else ""
            marker = r["slug"]
            typer.echo(f"  {r['action']}: {r['id']} ({marker}){waves}{blk}{tier}")
        for f in scaffolded:
            typer.echo(f"  scaffolded plan: {f}")
        for fo in fanout:
            if fo["decision"] == "spawned":
                typer.echo(f"  fan-out design pass dispatched: {fo['id']}")
        if orphan_ids:
            typer.echo(
                f"warning: {len(orphan_ids)} group child node(s) no longer in the spec, "
                f"left in place: {', '.join(orphan_ids)}",
                err=True,
            )
        for msg in downgrades:
            typer.echo(f"warning: {msg}", err=True)

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

    # Repaint the epic and every child this decompose CREATED so a decomposed
    # epic's children carry correct blocked_by/parent mirrors from birth (US5).
    # Scoped to created children (not already-linked ones): an existing child's
    # hand-filled plan is left untouched here and its drift rides the sweep.
    _project_plans_from_graph([epic_resolved_id, *created_ids])


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
    args.dry_run = dry_run
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
            roadmap_id=roadmap_id, dry_run=dry_run,
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
        # Mirror nav fields onto the just-linked plan of the CLAIMED node too -
        # this branch returns early, so the append-path projection never runs.
        # Routed through the converger so parent_slug is injected consistently.
        try:
            from fno.plan._project import project_graph_nodes

            project_graph_nodes(read_graph(_graph_path()), [claim_id])
        except Exception as e:  # noqa: BLE001 - additive; never wedge the claim
            sys.stderr.write(f"warning: post-claim plan projection failed: {e}\n")
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

    # Mirror the graph-authoritative navigation fields onto the plan doc the
    # node just linked. Non-fatal: a missing/unreadable plan never fails intake.
    # Routed through the converger so parent_slug is injected consistently.
    if new_id_holder[0]:
        try:
            from fno.plan._project import project_graph_nodes

            project_graph_nodes(read_graph(_graph_path()), [new_id_holder[0]])
        except Exception as e:  # noqa: BLE001 - additive; never wedge the intake
            sys.stderr.write(f"warning: post-intake plan projection failed: {e}\n")

    # Born-with-why (v2 A1): route the intaked node through the shared birth hook
    # for uniformity across birth paths. Independent of the project-warning block
    # above so a warn failure never drops the dispatch. Most intake nodes are
    # built by _build_intake_node (no ambient provenance stamp) and self-skip
    # with 'no-origin'; this keeps every birth path consistent. Non-fatal +
    # opt-in (gate-OFF default => complete no-op).
    if new_id_holder[0]:
        try:
            from fno.graph._intake import _find_node
            from fno.provenance.spawn_think import on_node_born

            born_node = _find_node(read_graph(_graph_path()), new_id_holder[0])
            if born_node is not None:
                # Already the persisted, slugged node -> skip the re-read.
                on_node_born(born_node, persisted=True)
        except Exception:  # noqa: BLE001 - additive; never wedge the intake
            pass


@cli.command("intake", hidden=True)
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
        dry_run=dry_run,
        claims=claims,
    )


# -- update --

@cli.command("note", hidden=True)
def cmd_note(
    task_id: str = typer.Argument(..., help="Node id to append a progress note to."),
    text: str = typer.Argument(..., help="Progress note text (one line)."),
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit the appended note as JSON."
    ),
) -> None:
    """Append a timestamped progress note to a backlog node (append-only).

    Distinct from ``update --details`` (which REPLACES the rationale) and the
    single ``completion_note``: ``note`` accumulates a list of ``{ts, text}``
    entries. The status-fanout backlog-progress adapter stamps one per
    ``task_done``/``run_summary`` (x-2057); it is also hand-runnable.
    """
    from fno.graph.store import append_progress_note

    text = text.strip()
    if not text:
        typer.echo("Error: note text is empty", err=True)
        raise typer.Exit(code=1)

    note = {"ts": datetime.now(timezone.utc).isoformat(), "text": text}
    found, _ = append_progress_note(_graph_path(), task_id, note)
    if not found:
        typer.echo(f"Error: no node resolves to '{task_id}'", err=True)
        raise typer.Exit(code=1)
    if json_output:
        typer.echo(json.dumps({"id": task_id, "note": note}, separators=(",", ":")))
    else:
        typer.echo(f"noted {task_id}: {text}")


@cli.command("update")
def cmd_update(
    task_id: str = typer.Argument(..., help="Feature ID (ab-XXXXXXXX)"),
    completed: bool = typer.Option(False, "--completed", help="Mark as completed"),
    locked_by: Optional[str] = typer.Option(None, "--locked-by", help="Lock owner id ('null' to release)"),
    locked_by_harness: Optional[str] = typer.Option(None, "--locked-by-harness", help="Holder's harness/provider (claude|codex|gemini). 'null' clears."),
    locked_by_harness_session: Optional[str] = typer.Option(None, "--locked-by-harness-session", help="Holder's harness session UUID. 'null' clears."),
    has_brief: Optional[str] = typer.Option(None, "--has-brief", help="Set has_brief flag"),
    plan_path: Optional[str] = typer.Option(None, "--plan-path", help="Plan directory path"),
    pr_number: Optional[str] = typer.Option(None, "--pr-number", help="PR number"),
    pr_url: Optional[str] = typer.Option(None, "--pr-url", help="PR URL"),
    merge_status: Optional[str] = typer.Option(None, "--merge-status", help="Merge status"),
    priority: Optional[str] = typer.Option(None, "--priority", "-p", help="New priority"),
    title: Optional[str] = typer.Option(None, "--title", "-t", help="Update display title"),
    details: Optional[str] = typer.Option(
        None,
        "--details",
        "--description",
        "-d",
        help="Update free-form details/rationale (stored in `details`). Pass 'null' to clear.",
    ),
    domain: Optional[str] = typer.Option(None, "--domain", help="Update domain (e.g. code)"),
    size: Optional[str] = typer.Option(None, "--size", help="Update size estimate: S|M|L"),
    model: Optional[str] = typer.Option(
        None,
        "--model",
        help="Pin the model dispatchers launch this node's worker on (x-571f), e.g. fable|opus|sonnet or a full provider-model id. Single non-whitespace token. Pass 'null' to clear (revert to provider default).",
    ),
    model_tier: Optional[str] = typer.Option(
        None,
        "--model-tier",
        help="Pin a minimum quality tier (high|medium|low) resolved to the cheapest reachable model at dispatch from the benchmark snapshot. Outranked by an exact --model. Pass 'null' to clear.",
    ),
    batch: Optional[str] = typer.Option(
        None,
        "--batch",
        help="Set the batch id this node is a member of (marks node.batch, batch-lane Wave 2). Pass 'null' to clear (requeue for individual ship on abandon).",
    ),
    dispatch_verb: Optional[str] = typer.Option(
        None,
        "--dispatch-verb",
        help="Verb a dispatcher launches this node with (US3), e.g. /think. Validated against config.dispatch.allowed_verbs at dispatch, not here. Pass 'null' to clear (revert to /target no-merge).",
    ),
    dispatch_brief: Optional[str] = typer.Option(
        None,
        "--dispatch-brief",
        help="Free-text brief carried to the worker via TARGET_BRIEF env at cold-start (US3), never the command line. Capped at 8 KB at dispatch. Pass 'null' to clear.",
    ),
    type_: Optional[str] = typer.Option(None, "--type", help="Update node type (feature|epic|bug)"),
    public: Optional[bool] = typer.Option(None, "--public/--no-public", help="Mark node for the public roadmap (fno backlog roadmap)"),
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
    caused_by: Optional[str] = typer.Option(
        None,
        "--caused-by",
        help="Node id this node was created to address (causal link, W4). Pass 'null' to clear.",
    ),
    fixes_pr: Optional[int] = typer.Option(
        None,
        "--fixes-pr",
        help="PR number this node fixes (causal link, W4). Pass 0 to clear.",
    ),
    reverted: Optional[bool] = typer.Option(
        None,
        "--reverted/--no-reverted",
        help="Mark this node's ship as reverted (manual fallback for reconcile's best-effort revert detection).",
    ),
    tag: Optional[List[str]] = typer.Option(
        None, "--tag", hidden=True, help="Add a tag (repeatable, idempotent, lowercase-kebab)."
    ),
    untag: Optional[List[str]] = typer.Option(
        None, "--untag", hidden=True, help="Remove a tag (repeatable, no-op if absent)."
    ),
) -> None:
    from fno.graph._constants import PRIORITY_ORDER, has_node_id_prefix, normalize_tag
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import (
        _parse_blocker_list,
        _validate_blocker_ids,
        _find_node,
        _would_create_cycle,
        _would_exceed_epic_depth,
    )
    from fno.graph._constants import EPIC_NEST_MAX_DEPTH

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

    # Validate size/type the same way priority is validated above, so update
    # can't store garbage (e.g. `--size foo`). 'null' clears size.
    if size is not None and size.lower() != "null" and size.upper() not in {"S", "M", "L"}:
        typer.echo(f"Error: invalid size '{size}'. Must be one of: S, M, L", err=True)
        raise typer.Exit(code=1)
    # Model pin (x-571f): a validated-shape pass-through, not an allowlist. The
    # value must be a single shell-safe token so it survives unquoted use in the
    # dispatchers (dispatch-node.sh $model_arg, the loop-driver MODEL_FLAG
    # word-split) without word-splitting OR globbing. The charset [A-Za-z0-9._:/-]
    # covers every real model id (fable, claude-opus-4-8, openai/gpt-4,
    # us.anthropic.claude-...) while forbidding whitespace and shell/glob
    # metacharacters (* ? [ ] etc.); the CLI (not fno) resolves the alias.
    # 'null' clears.
    if model is not None and model.lower() != "null":
        import re  # module-level `re` is function-local elsewhere; scope it here

        if not re.fullmatch(r"[A-Za-z0-9._:/-]{1,64}", model):
            typer.echo(
                "Error: --model must be a single token of [A-Za-z0-9._:/-], at most 64 chars "
                "(e.g. fable|opus|sonnet or a full provider-model id); no whitespace or "
                "shell/glob metacharacters",
                err=True,
            )
            raise typer.Exit(code=1)
    _VALID_TYPES = {"feature", "epic", "bug", "roadmap"}
    if type_ is not None and type_ not in _VALID_TYPES:
        typer.echo(
            f"Error: invalid type '{type_}'. Must be one of: {', '.join(sorted(_VALID_TYPES))}",
            err=True,
        )
        raise typer.Exit(code=1)

    # Normalize + validate tags OUTSIDE the lock so a malformed tag refuses
    # before any mutation (the node is unchanged on a bad --tag).
    add_tags: list[str] = []
    remove_tags: list[str] = []
    try:
        add_tags = [normalize_tag(t) for t in (tag or [])]
        remove_tags = [normalize_tag(t) for t in (untag or [])]
    except ValueError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=1)
    has_tag_edit = bool(add_tags or remove_tags)

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

    projected_node: list = [None]
    cascade_closed_update: list = []

    # Size flows doc->graph when a plan is (re)linked and the node has no size
    # yet (Wave 2.2). Read the linked plan's frontmatter size best-effort,
    # outside the lock; an explicit --size still wins (applied later in-mutator).
    linked_size: Optional[str] = None
    if plan_path is not None:
        try:
            from fno.graph._intake import normalize_size, repo_root
            from fno.plan._stamp import read_plan_file

            pp = Path(plan_path)
            if not pp.is_absolute():
                pp = Path(repo_root()) / pp
            _, fm, _ = read_plan_file(pp)
            linked_size = normalize_size(fm.get("size"))
        except Exception:
            linked_size = None

    def mutator(entries):
        node = _find_node(entries, task_id)
        if node is None:
            typer.echo(f"Error: graph node {task_id} not found", err=True)
            raise typer.Exit(code=1)
        projected_node[0] = node

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

        if locked_by is not None:
            session = locked_by if locked_by != "null" else None
            # locked_by is canonical; session_id mirror is re-synced at serialize
            # by _normalize_lock_fields. Clearing the lock also clears the US6
            # harness stamp so an unclaim never leaves a stale holder identity.
            node["locked_by"] = session
            node["claimed_at"] = datetime.now(timezone.utc).isoformat() if session else None
            if session is None:
                node["locked_by_harness"] = None
                node["locked_by_harness_session"] = None
        # Harness stamp (US6): the holder's provider + harness-session UUID,
        # settable alongside the claim. 'null' clears; an explicit unclaim above
        # already cleared both.
        if locked_by_harness is not None:
            node["locked_by_harness"] = None if locked_by_harness == "null" else locked_by_harness
        if locked_by_harness_session is not None:
            node["locked_by_harness_session"] = (
                None if locked_by_harness_session == "null" else locked_by_harness_session
            )
        if has_brief is not None:
            node["has_brief"] = has_brief.lower() == "true"
        if plan_path is not None:
            node["plan_path"] = plan_path
            if linked_size and not node.get("size"):
                node["size"] = linked_size
        if pr_number is not None:
            node["pr_number"] = int(pr_number)
        if pr_url is not None:
            node["pr_url"] = pr_url
        if merge_status is not None:
            node["merge_status"] = merge_status
        if batch is not None:
            # 'null' clears the mark (requeue as individual ship on abandon); any
            # other value records the batch id this node is a member of.
            node["batch"] = None if batch.lower() == "null" else batch
        # Dispatch overrides (US3). Stored permissively; the resolver is the trust
        # boundary (allowlist + 8 KB cap at dispatch time, not write time).
        if dispatch_verb is not None:
            node["dispatch_verb"] = None if dispatch_verb.lower() == "null" else dispatch_verb
        if dispatch_brief is not None:
            node["dispatch_brief"] = None if dispatch_brief.lower() == "null" else dispatch_brief
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
        if details is not None:
            node["details"] = None if details.lower() == "null" else details
        if domain is not None:
            node["domain"] = domain
        if size is not None:
            node["size"] = size.upper() if size.lower() != "null" else None
        if model is not None:
            node["model"] = None if model.lower() == "null" else model
        if model_tier is not None:
            if model_tier.lower() == "null":
                node["model_tier"] = None
            else:
                band = model_tier.strip().lower()
                if band not in {"high", "medium", "low"}:
                    typer.echo(
                        f"fno backlog update: invalid --model-tier {model_tier!r}; "
                        "expected high, medium, or low.",
                        err=True,
                    )
                    raise typer.Exit(code=2)
                node["model_tier"] = band
        if type_ is not None:
            node["type"] = type_
        if public is not None:
            node["public"] = public
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
        if caused_by is not None:
            if caused_by.lower() == "null":
                node["caused_by"] = None
            else:
                origin = _find_node(entries, caused_by)
                if origin is None:
                    typer.echo(f"Error: --caused-by node {caused_by} not found", err=True)
                    raise typer.Exit(code=1)
                if origin["id"] == node["id"]:
                    typer.echo("Error: --caused-by cannot reference the node itself", err=True)
                    raise typer.Exit(code=1)
                # Store the resolved id, not the raw input (which may be a
                # prefix/bare-hex form _find_node normalized).
                # ponytail: self-reference check only; add a cycle walk if a
                # consumer ever traverses caused_by chains.
                node["caused_by"] = origin["id"]
        if fixes_pr is not None:
            node["fixes_pr"] = None if fixes_pr == 0 else int(fixes_pr)
        if reverted is not None:
            node["reverted"] = reverted
        if has_tag_edit:
            # Idempotent set semantics, order-preserving: adds skip dupes,
            # removes are no-ops if absent. Normalization happened above.
            current_tags = list(node.get("tags") or [])
            for t in add_tags:
                if t not in current_tags:
                    current_tags.append(t)
            current_tags = [t for t in current_tags if t not in remove_tags]
            node["tags"] = current_tags
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
                if _would_exceed_epic_depth(entries, node, target):
                    typer.echo(
                        f"Error: parenting epic {node['id']} under {target['id']} "
                        f"would exceed the {EPIC_NEST_MAX_DEPTH}-level cap "
                        f"(mission -> epic -> leaf); an epic may nest only under a "
                        f"top-level mission",
                        err=True,
                    )
                    raise typer.Exit(code=1)
                node["parent"] = target["id"]

        # Completion runs LAST so it sees the FINAL parent edge (codex P2): a
        # combined `--completed --parent <epic>` must cascade against the new
        # parent. Use the shared _apply_completion_fields so the close clears
        # session/claim/defer/queue state in lockstep with done/reconcile, then
        # cascade-close now-all-done ancestor epics (x-33b2).
        if completed and not node.get("completed_at"):
            _apply_completion_fields(node)
            cascade_closed_update.extend(
                _cascade_close_parents(entries, node["id"])
            )
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"Updated {task_id}")

    # Project the graph-authoritative fields (nav mirror + forward-only status)
    # onto the plan when a mirrored OR status-affecting field changed. Routed
    # through the fresh-re-read helper (not the pre-recompute `projected_node`)
    # so the node carries its recomputed _status: a `--locked-by` claim reads
    # `claimed` -> plan `in_progress` (AC1-HP; the claim goes through this update
    # path, not the `claim` verb), and a `--completed` close reads `done` ->
    # `done` + `done_at`, including cascade-closed epic parents. Best-effort.
    if projected_node[0] and (
        completed
        or locked_by is not None
        or priority is not None
        or project is not None
        or type_ is not None
        or has_blocker_edit
        or plan_path is not None
        or size is not None
        or parent is not None
        or has_tag_edit
    ):
        _project_plans_from_graph(
            [projected_node[0]["id"], *cascade_closed_update]
        )


# -- unclaim / release --


def _invoking_session_id() -> Optional[str]:
    """Best-effort id of the session running this command, for the unclaim
    "is this lockfile mine?" check. None => treat any live holder as foreign
    (the safe default: never yank a live peer's claim)."""
    try:
        from fno.carveout.core import resolve_session_id
        from fno.graph._intake import repo_root

        # repo_root() returns a str; resolve_session_id() needs a Path (it does
        # `root / ".fno" / ...`). Without the wrap the TypeError is swallowed
        # below and this always returns None, disabling the own-claim release.
        return resolve_session_id(Path(repo_root()))
    except Exception:
        return None


def _invoking_claim_holder() -> Optional[str]:
    """Best-effort full holder recorded by the active target manifest.

    Codex uses a unique per-target ``session_id`` for event deduplication while
    the durable thread id owns its graph/claim lock. Prefer the manifest's
    explicit ``target_claim_holder``; legacy manifests fall back to the target
    session id.
    """
    try:
        from fno.graph._intake import repo_root

        state = Path(repo_root()) / ".fno" / "target-state.md"
        for line in state.read_text(encoding="utf-8").splitlines():
            if line.lstrip().startswith("target_claim_holder:"):
                value = line.split(":", 1)[1].strip().strip("\"'")
                if value and value != "null":
                    return value
    except Exception:
        pass

    sid = _invoking_session_id()
    return f"target-session:{sid}" if sid else None


def _release_node_lockfile(node_id: str) -> str:
    """Best-effort release of the ``node:<id>`` fno-claim lockfile.

    Releases when the holder is stale (PID dead / TTL expired) or matches the
    invoking session; refuses a LIVE foreign holder (warn + point at
    ``force-release``) so we never silently yank a live peer's claim. Returns a
    short human note for the command summary. Never raises - the graph clear is
    the load-bearing part and must not be undone by a lockfile hiccup.
    """
    try:
        from fno.claims.core import (
            claim_status,
            release_claim,
        )
        from fno.claims.io import claims_root_for
    except Exception:
        return "lockfile untouched (claims module unavailable)"

    key = f"node:{node_id}"
    try:
        root = claims_root_for(key)
        status = claim_status(key, root=root)
        state = status.get("state")

        if state == "free":
            return "no lockfile"
        if state == "stale":
            # Holder-verified release, NOT unconditional force-release (codex P1):
            # between this stale snapshot and the unlink, another dispatcher can
            # reclaim the dead lock with a NEW holder. release_claim() only
            # removes the file if its holder still matches the stale holder we
            # saw, so a fresh live holder is left intact rather than yanked.
            release_claim(key, holder=status.get("holder") or "", root=root)
            return "released stale lockfile"
        if state == "corrupted":
            typer.echo(
                f"warning: lockfile {key} is corrupted; graph claim cleared but "
                f"lockfile left intact. Use `fno claim force-release {key} -R <why>` "
                f"to repair.",
                err=True,
            )
            return "lockfile left (corrupted)"

        # state == "live" or "suspect" (x-ba4b): only release if it is ours -
        # a suspect claim (TTL-unexpired, dead pid) is still owned, so a peer's
        # is left intact and only our own is cleared.
        holder = status.get("holder") or ""
        mine = holder == _invoking_claim_holder()
        if mine:
            release_claim(key, holder=holder, root=root)
            return "released own lockfile"

        typer.echo(
            f"warning: lockfile {key} held by LIVE holder {holder!r}; graph claim "
            f"cleared but lockfile left intact. Use "
            f"`fno claim force-release {key} -R <why>` to override.",
            err=True,
        )
        return "lockfile left (live foreign holder)"
    except Exception as exc:  # never let a lockfile error mask the graph clear
        return f"lockfile untouched ({exc})"


def _unclaim_node(task_id: str) -> None:
    """Free a claimed node in one call: clear the graph claim (always) and
    best-effort-release the lockfile (stale or owned). Mirrors the graph-side of
    ``update --locked-by null``, then adds the lockfile release the two-step
    dance forced you to do by hand."""
    from fno.graph._constants import has_node_id_prefix
    from fno.graph.store import locked_mutate_graph
    from fno.graph._intake import _find_node

    if not has_node_id_prefix(task_id):
        typer.echo(
            f"Error: task_id must be a <prefix>-<4..8 hex> node id, got '{task_id}'",
            err=True,
        )
        raise typer.Exit(code=1)

    resolved_id: Optional[str] = None

    def mutator(entries):
        nonlocal resolved_id
        node = _find_node(entries, task_id)
        if node is None:
            typer.echo(f"Error: graph node {task_id} not found", err=True)
            raise typer.Exit(code=1)
        resolved_id = node["id"]
        # Same field clear as `update --locked-by null`; recompute_statuses
        # derives _status back to ready from the now-empty locked_by.
        node["locked_by"] = None
        node["claimed_at"] = None
        return entries

    locked_mutate_graph(_graph_path(), mutator)

    lock_note = _release_node_lockfile(resolved_id or task_id)
    typer.echo(f"Unclaimed {resolved_id or task_id} ({lock_note})")


@cli.command("unclaim", hidden=True)
def cmd_unclaim(
    task_id: str = typer.Argument(
        ..., help="Node id to free (reverts claimed -> ready, releases the lockfile)"
    ),
) -> None:
    """Free a claimed node in one call (graph claim + safe lockfile release)."""
    _unclaim_node(task_id)


@cli.command("release", hidden=True)
def cmd_release(
    task_id: str = typer.Argument(..., help="Alias for `unclaim`"),
) -> None:
    """Alias for `unclaim`: free a claimed node in one call."""
    _unclaim_node(task_id)


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
        # read_graph does not recompute _status, so a node closed out of band
        # (e.g. PR merged via reconcile/done in another process) can carry
        # completed_at while its persisted _status is still "ready". Guard on
        # completed_at so advance / megawalk never dispatch a /target worker for
        # an already-done node.
        candidates = [
            e for e in entries
            if e.get("_status") in allowed and not e.get("completed_at")
        ]
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
        # Containers are never directly buildable (x-33b2): an epic's work lives
        # in its decomposed children, so `next` must never return it - it
        # otherwise ranks first among its siblings (make_selection_sort_key) and
        # is repeatedly re-selected as the head, starving the genuinely-ready leaf
        # below it. Build the leaves, not the box; the epic closes itself via
        # _cascade_close_parents when its last child lands. Computed from the FULL
        # graph so a parent already filtered out of `candidates` still suppresses
        # correctly. Shared with cmd_ready.
        container_ids = _container_ids(entries)
        candidates = [e for e in candidates if e.get("id") not in container_ids]
        # Batch-lane Wave 2: a node already committed to an open batch ships via
        # the batch PR, so drop it from the dispatch pool (else a second worker
        # rebuilds work already on the shared branch). Cleared on abandon so a
        # requeued member resurfaces. Shared with cmd_ready.
        candidates = [e for e in candidates if not _is_batched_member(e)]
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
            # x-571f: the per-node model pin must ride in the next-JSON so the
            # megawalk drain (loop_megawalk.rs) can prefer it over cfg.model.
            # model_tier rides alongside it so the dispatch-time tier resolver
            # sees the annotation (else it silently falls back to the default).
            "model": e.get("model"),
            "model_tier": e.get("model_tier"),
            # x-0676: the per-node dispatch overrides must ride in the next-JSON so
            # `advance`'s resolver routing (US1) actually fires for real graph nodes
            # (which come from this summary), not only for tests that inject them.
            "dispatch_verb": e.get("dispatch_verb"),
            "dispatch_brief": e.get("dispatch_brief"),
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
                winner["locked_by"] = claim
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

@cli.command("ready", hidden=True)
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
    mission: Optional[str] = typer.Option(
        None,
        "--mission",
        help="Restrict to nodes whose mission_id matches (same contract as `next`).",
    ),
    # ponytail: `ready` already always emits JSON; the flag exists only so a
    # caller passing --json (inbox triage) isn't rejected with Typer exit 2.
    # Accepted-and-ignored, never a behavior switch.
    json_output: bool = typer.Option(
        False, "--json", "-J", help="Emit JSON (default; flag accepted for parity)."
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
    # read_graph does not recompute _status, so a node closed out of band can
    # carry completed_at while its persisted _status is still "ready". Guard on
    # completed_at so a done node never lists as actionable work (the same guard
    # is in `next`'s _pick_ready, the dispatch path).
    ready = [
        e for e in entries
        if e.get("_status") in allowed and not e.get("completed_at")
    ]
    ready = filter_by_project(ready, project, all_)
    if roadmap_id:
        ready = [e for e in ready if e.get("roadmap_id") == roadmap_id]
    # Mission scope (same rule as `next`): a mission-scoped caller (the
    # active-backlog daemon's lane-fill, megatron child walks) must never see
    # out-of-mission nodes as actionable (codex P1 on PR #137).
    if mission:
        ready = [e for e in ready if e.get("mission_id") == mission]
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
    # Containers are never actionable work (x-33b2 / codex P2 on PR #69): drop
    # epics so `fno backlog ready` - and the `dispatch-node.sh --all-ready` bulk
    # path that enumerates it - never presents/launches the box instead of its
    # leaves. No all-done exception: the epic auto-closes via
    # _cascade_close_parents when its last child lands, so it is already done
    # rather than a lingering ready container. Shares _container_ids with `next`'s
    # _pick_ready so the surfaces cannot drift.
    container_ids = _container_ids(entries)
    ready = [e for e in ready if e.get("id") not in container_ids]
    # Batch-lane Wave 2: hide open-batch members (they ship via the batch PR, not
    # as individual ready work). Shares _is_batched_member with `next`'s
    # _pick_ready so the surfaces cannot drift.
    ready = [e for e in ready if not _is_batched_member(e)]
    # Epics-first, then flat priority (C3, Locked Decision 7); key built
    # from the full graph so epic parents always resolve.
    ready.sort(key=make_selection_sort_key(entries))

    output = [{
        # slug leads (ab-f82e8083) so a `ready` list / clipboard is readable.
        "slug": e.get("slug"),
        "id": e["id"], "title": e.get("title"), "priority": e.get("priority"),
        "domain": e.get("domain"), "project": e.get("project"),
        "cwd": e.get("cwd"), "parent": e.get("parent"),
        # x-571f: carry the model pin so the lane-fill dispatcher (select_lane_fill
        # -> _ready_nodes -> `fno backlog ready`) can thread it into the spawn.
        # model_tier rides alongside so the tier resolver sees the annotation.
        "model": e.get("model"),
        "model_tier": e.get("model_tier"),
    } for e in ready]

    typer.echo(json.dumps(output, indent=2))


# -- lane-fill --

@cli.command("lane-fill", hidden=True)
def cmd_lane_fill(
    max_lanes: Optional[int] = typer.Option(
        None, "--max", help="Max lanes (default: config.parallel.max_lanes)."
    ),
    project: Optional[str] = typer.Option(
        None, "--project", "-p", help="Filter by project name"
    ),
    mission: Optional[str] = typer.Option(
        None, "--mission", help="Restrict selection to this mission's nodes."
    ),
    claim: bool = typer.Option(
        False,
        "--claim",
        help="Atomically hold a lane slot per selected node (default: preview only).",
    ),
) -> None:
    """Select up to max_lanes ready nodes from DISTINCT domains (parallel mode).

    Prints the JSON list of nodes that would dispatch as concurrent lanes, one
    per distinct domain (epic x-42d5, group 2). Read-only by default; ``--claim``
    atomically holds a dispatch-time lane slot per node - what the dispatcher
    does before spawn (Locked Decision #8). ``max_lanes < 2`` prints ``[]``
    (sequential: use ``fno backlog next``).
    """
    from fno.backlog.advance import select_lane_fill

    if max_lanes is None:
        from fno.config import load_settings
        max_lanes = load_settings().parallel.max_lanes

    selected = select_lane_fill(max_lanes, project, mission=mission, claim=claim)
    typer.echo(json.dumps(selected, indent=2))


# -- dispatch-lanes --

@cli.command("dispatch-lanes", hidden=True)
def cmd_dispatch_lanes(
    max_lanes: Optional[int] = typer.Option(
        None, "--max", help="Max lanes (default: config.parallel.max_lanes)."
    ),
    project: Optional[str] = typer.Option(
        None, "--project", "-p", help="Filter by project name"
    ),
    mission: Optional[str] = typer.Option(
        None, "--mission", help="Restrict dispatch to this mission's nodes."
    ),
    model: Optional[str] = typer.Option(
        None, "--model", "-m",
        help="Pin a model for every lane spawned this run, overriding node annotations.",
    ),
    provider: Optional[str] = typer.Option(
        None, "--provider",
        help="Pin a provider for every lane. (No -p short: it is --project here.)",
    ),
) -> None:
    """Spawn up to max_lanes isolated background lanes (parallel mode, group 3).

    Selects distinct-domain ready nodes (like ``lane-fill``), then for each one
    isolates a worktree off origin/main, seeds its per-lane
    ``.fno/config.local.toml`` (x-cbce: own project.id), and
    spawns a detached ``/target no-merge`` worker rooted there. Prints one JSON
    receipt per lane (``status`` dispatched | skipped). ``max_lanes < 2`` spawns
    nothing (sequential: use ``fno backlog advance`` / ``next``).
    """
    from fno.agents.provider_resolve import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_provider,
    )
    from fno.backlog.advance import dispatch_lanes

    try:
        model = reject_empty_model(model)
        provider = resolve_dispatch_provider(provider)[0] if provider is not None else None
    except DispatchFlagError as exc:
        typer.echo(f"dispatch-lanes: {exc}", err=True)
        raise typer.Exit(code=2)

    if max_lanes is None:
        from fno.config import load_settings
        max_lanes = load_settings().parallel.max_lanes

    receipts = dispatch_lanes(max_lanes, project, mission=mission, model=model, provider=provider)
    typer.echo(json.dumps(receipts, indent=2))


# -- lanes --

@cli.command("lanes", hidden=True)
def cmd_lanes(
    json_output: bool = typer.Option(False, "--json", "-J", help="JSON rollup."),
) -> None:
    """One-read parallel-lane rollup (US5): live lanes vs the cap.

    Joins each live lane-slot claim with its graph node (slug, status, PR) so
    the operator reviews the fleet's shape - which nodes hold lanes, in which
    domains - without stitching ``fno claim list`` to the board by hand. The
    grid's BgRoster tiles show the workers themselves; this is the aggregated
    outcome view. Read-only.
    """
    from fno.claims.core import list_claims
    from fno.claims.lanes import LANE_SLOT_PREFIX

    try:
        from fno.config import load_settings

        max_lanes = load_settings().parallel.max_lanes
    except Exception:  # noqa: BLE001 - a config miss must not hide live lanes
        max_lanes = 1

    nodes: dict = {}
    try:
        from fno.graph.store import read_graph

        nodes = {
            e["id"]: e
            for e in read_graph(_graph_path())
            if isinstance(e, dict) and e.get("id")
        }
    except Exception:  # noqa: BLE001 - rollup degrades to claims-only rows
        pass

    lanes = []
    for s in sorted(list_claims(prefix=LANE_SLOT_PREFIX), key=lambda c: c.get("key", "")):
        meta = s.get("metadata") or {}
        lane_id = meta.get("lane_id") or ""
        node = nodes.get(lane_id) or {}
        lanes.append(
            {
                "slot": s.get("key"),
                "lane_id": lane_id,
                "domain": meta.get("domain"),
                "slug": node.get("slug"),
                "status": node.get("_status"),
                "pr_number": node.get("pr_number"),
                "holder": s.get("holder"),
            }
        )

    if json_output:
        typer.echo(json.dumps({"max_lanes": max_lanes, "active": len(lanes), "lanes": lanes}))
        return
    typer.echo(f"lanes: {len(lanes)}/{max_lanes} active")
    for ln in lanes:
        slug = f"  {ln['slug']}" if ln.get("slug") else ""
        pr = f"  pr#{ln['pr_number']}" if ln.get("pr_number") else ""
        typer.echo(
            f"{ln['slot']}  {ln['lane_id']}{slug}  "
            f"domain={ln.get('domain') or '-'}  {ln.get('status') or '-'}{pr}"
        )


# -- get --

@cli.command("get")
def cmd_get(
    id: str = typer.Argument(
        ...,
        help="Node ab-id, slug, or bare 8-hex (e.g. ab-ff6f96e0 | dashless-spawn | ff6f96e0)",
    ),
    field: Optional[str] = typer.Option(None, help="Print only this field"),
    strict: bool = typer.Option(
        False,
        "--strict",
        help="Exact-only resolution (id/slug/bare-hex); never fuzzy. The stable "
        "surface the /think router seeds a design from - a miss exits 1 so a "
        "typo'd token can never silently seed.",
    ),
) -> None:
    from fno.graph.store import read_graph
    from fno.graph.fuzzy import resolve_node

    entries = read_graph(_graph_path())
    # Deterministic resolution tiers 1-3 (ab-f82e8083): exact ab-id, exact slug,
    # bare-8-hex re-prefix. A slug/bare-hex argument resolves to the same node
    # an ab-id would, so the spawn VALIDATE step (`fno backlog get "$node"`)
    # accepts every exact entry form. `resolve_node` is already exact-only, so
    # --strict pins that contract for the router (x-4af4): should `get` ever gain
    # a describe-it fuzzy default, --strict stays the exact-only seed path.
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

    # Read-through fallback: a node the sweep archived still resolves here
    # (read-only). Mutating verbs stay working-graph-only and error instead.
    from fno.paths import graph_archive_json

    archive_path = graph_archive_json()
    if archive_path.exists():
        archived = read_graph(archive_path)
        amatch = resolve_node(id, archived)
        if amatch.kind == "exact":
            e = dict(amatch.candidates[0])
            e["_archived"] = True
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


# -- project-root (work-map resolution; null-for-unmapped) --

@cli.command("project-root", hidden=True)
def cmd_project_root(
    project: str = typer.Argument(..., help="Project name to resolve against config.work.workspaces."),
) -> None:
    """Print a project's work-map root, or exit 1 (empty stdout) if unmapped.

    The G2 session-project invariant needs to tell "mapped to a root" apart from
    "unmapped" so it can REFUSE an unmapped foreign wave by name rather than
    guess a cwd (AC2-ERR). ``backlog get --field _resolved_cwd`` can't answer
    this: it applies a ``root or cwd`` fallback, so an unmapped project with a
    recorded cwd still prints a (guessed) path. This verb exposes the raw
    ``project_root_from_settings`` lookup - the same pure work-map resolver G1
    uses - with no cwd fallback, so empty/exit-1 means exactly "unmapped".
    """
    from fno.graph._intake import project_root_from_settings

    root = project_root_from_settings(project)
    if not root:
        raise typer.Exit(code=1)
    typer.echo(root)


# -- provenance --

@cli.command("provenance", hidden=True)
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
            # Append-only lifecycle provenance in raw append order (x-b6e4).
            # read_graph's defaults guarantee the key, so no fallback guard.
            "sessions": e["sessions"],
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

    # Lifecycle rows (x-b6e4): raw append order, phase-forward. Distinct from the
    # birth/spawn edges above -- those are single parent pointers; this is the
    # per-phase who-did-what across sessions and harnesses. read_graph's defaults
    # guarantee the key.
    sessions = e["sessions"]
    if sessions:
        lines.append("  lifecycle:")
        for s in sessions:
            lines.append(
                f"    {s.get('phase', '?'):<9} "
                f"{s.get('harness', '?')}:{s.get('session_id', '?')} "
                f"@ {s.get('at', '?')}"
            )

    typer.echo("\n".join(lines))


# -- session add (lifecycle provenance, x-b6e4) --

session_app = typer.Typer(
    name="session",
    help="Append-only lifecycle session provenance (x-b6e4).",
    no_args_is_help=True,
    add_completion=False,
)


@session_app.callback()
def _session_callback() -> None:
    """Keep ``add`` a real subcommand (a single-command Typer app auto-collapses,
    which would parse ``session add <node>`` with ``add`` as the node)."""


@session_app.command("add")
def cmd_session_add(
    node: Optional[str] = typer.Argument(
        None, help="Node id / slug / bare-hex to stamp (mutually exclusive with --pr-number)."
    ),
    phase: str = typer.Option(..., "--phase", help="Lifecycle phase: think|blueprint|do|ship."),
    pr: Optional[int] = typer.Option(
        None, "--pr-number", help="Resolve the UNIQUE node carrying this PR number instead "
                                  "of passing NODE (rejects 0 or multiple matches; never fans out)."
    ),
    repo: Optional[str] = typer.Option(
        None, "--repo", help="Scope --pr-number resolution to an <owner>/<repo> slug "
                             "(pr_number is not unique across repos in a cross-project graph)."
    ),
    harness: Optional[str] = typer.Option(
        None, "--harness", help="Override harness (default: ambient session identity)."
    ),
    session_id: Optional[str] = typer.Option(
        None, "--session-id", help="Override session id (default: ambient session identity)."
    ),
    at: Optional[str] = typer.Option(
        None, "--at", help="ISO-8601 UTC timestamp (default: now); explicit for backfill."
    ),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit the result as JSON."),
) -> None:
    """Stamp a node with a lifecycle phase record (idempotent, append-only).

    Identify the node by NODE (id/slug/hex) or by ``--pr-number <n>`` (the unique
    PR-linked node) -- exactly one of the two. Harness + session id default to the
    ambient session identity; with neither an env marker nor an explicit flag the
    stamp is skipped and a warning names the node/PR and phase (provenance is
    never invented, AC2-ERR). Exit 0 on append or duplicate; exit 2 on missing
    identity, ambiguous/absent node, unknown phase, or bad input.
    """
    from fno.graph.fuzzy import resolve_node
    from fno.graph.store import append_session_record, read_graph, stamp_session_for_pr

    if (node is None) == (pr is None):
        typer.echo("session add: pass exactly one of NODE or --pr-number.", err=True)
        raise typer.Exit(code=2)

    who = node if node is not None else f"pr#{pr}"
    ident = resolve_harness_identity()
    eff_harness = (harness or ident.harness or "").strip()
    eff_session = (session_id or ident.session_id or "").strip()
    if not eff_harness or not eff_session:
        typer.echo(
            f"session add: no ambient identity for {who} phase={phase}; "
            "pass --harness/--session-id or run inside a session. Skipped.",
            err=True,
        )
        raise typer.Exit(code=2)

    try:
        if pr is not None:
            node_id, status = stamp_session_for_pr(
                _graph_path(), pr, phase=phase,
                harness=eff_harness, session_id=eff_session, at=at, repo=repo,
            )
            if status in ("no-node", "ambiguous"):
                typer.echo(
                    f"session add: PR {pr} maps to {status} (phase={phase}); "
                    "resolution is exact and never fans out. Skipped.",
                    err=True,
                )
                raise typer.Exit(code=2)
            added = status == "added"
        else:
            match = resolve_node(node, read_graph(_graph_path()))
            if match.kind != "exact":
                typer.echo(f"session add: no node matches {node!r} (phase={phase}).", err=True)
                raise typer.Exit(code=2)
            node_id = match.candidates[0]["id"]
            found, added = append_session_record(
                _graph_path(), node_id, phase=phase,
                harness=eff_harness, session_id=eff_session, at=at,
            )
            if not found:
                typer.echo(f"session add: node {node_id} not found (phase={phase}).", err=True)
                raise typer.Exit(code=2)
    except ValueError as exc:
        typer.echo(f"session add: {exc} (target={who} phase={phase})", err=True)
        raise typer.Exit(code=2)

    if json_out:
        typer.echo(json.dumps({
            "node_id": node_id, "phase": phase, "harness": eff_harness,
            "session_id": eff_session, "added": added,
        }))
    else:
        state = "recorded" if added else "already recorded"
        typer.echo(f"{state} {phase} {eff_harness}:{eff_session} on {node_id}")


cli.add_typer(session_app, name="session", hidden=True)


# -- backfill-slugs --

@cli.command("backfill-slugs", hidden=True)
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


# -- bases (canonical epic/mission progress Bases) --

@cli.command("bases", hidden=True)
def cmd_bases(
    out: Optional[str] = typer.Option(
        None,
        "--out",
        help="Directory to emit the .base files into (default: internal/fno/backlog/).",
    ),
) -> None:
    """Emit the canonical epic/mission progress Base files (x-6c2b).

    Regenerable: refreshes a file carrying the generated marker, refuses to
    clobber a hand-authored base (one prints `refused:`). Prints one line per
    file: written | unchanged | refused.
    """
    from fno.graph._bases import BASES, write_base
    from fno.graph._intake import repo_root

    out_dir = (
        Path(out) if out else Path(repo_root()) / "internal" / "fno" / "backlog"
    )
    for name, content in BASES.items():
        target = out_dir / name
        action = write_base(target, content)
        typer.echo(f"{action}: {target}")


# -- roadmap (public, curated) --

@cli.command("roadmap", hidden=True)
def cmd_roadmap(
    project: Optional[str] = typer.Option(
        None,
        "--project",
        help="Project to render (defaults to the project mapped to the cwd).",
    ),
    out: Optional[str] = typer.Option(None, "--out", help="Write markdown to this path instead of stdout."),
    html: Optional[str] = typer.Option(None, "--html", help="Also write a standalone HTML file to this path."),
) -> None:
    """Render a public, leak-free roadmap of `public`-flagged nodes.

    Only nodes flagged via `fno backlog update --public` for the given
    project appear, and only their title/priority/size - never IDs, plan
    paths, or cwd. Safe to commit to a public repo or host on a site.
    Grouped into Now / Next / Later / Shipped (reusing the live board's
    column + lane logic, so it can't drift).
    """
    from pathlib import Path

    from fno.graph._intake import detect_project_from_settings, repo_root
    from fno.graph.roadmap_public import (
        render_public_roadmap_html,
        render_public_roadmap_md,
    )
    from fno.graph.store import read_graph

    resolved_project = project or detect_project_from_settings(repo_root())
    if not resolved_project:
        typer.echo(
            "Error: no project given and none mapped to the cwd; pass --project.",
            err=True,
        )
        raise typer.Exit(code=1)

    entries = read_graph(_graph_path())
    md = render_public_roadmap_md(entries, resolved_project)

    if out:
        Path(os.path.expanduser(out)).write_text(md)
        typer.echo(os.path.expanduser(out))
    else:
        typer.echo(md, nl=False)

    if html:
        html_path = os.path.expanduser(html)
        Path(html_path).write_text(render_public_roadmap_html(entries, resolved_project))
        typer.echo(html_path)


# -- tree --

@cli.command("tree", hidden=True)
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

@cli.command("status", hidden=True)
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

@cli.command("briefs", hidden=True)
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

@cli.command("validate", hidden=True)
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

@cli.command("cost", hidden=True)
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

@cli.command("remove", hidden=True)
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

@cli.command(
    "defer",
    epilog="Paired verb: `fno backlog undefer <id>` reverses this (hidden; run its own --help).",
)
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
        node["locked_by"] = None
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
    _project_plans_from_graph([task_id])


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


@cli.command("queue", hidden=True)
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


@cli.command("unqueue", hidden=True)
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


@cli.command("pick", hidden=True)
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


@cli.command("queued", hidden=True)
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


@cli.command("undefer", hidden=True)
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
    _project_plans_from_graph([task_id])


# -- done --

def _project_plans_from_graph(node_ids: list[str]) -> None:
    """Project each named node's mirror fields + forward status onto its plan.

    Re-reads the graph so every node carries its recomputed ``_status`` (a claim
    reads ``claimed`` -> ``in_progress``; a close reads ``done`` -> ``done`` +
    ``done_at``), then delegates to the shared converger. Covers cascade-closed
    epic parents that ``_stamp_and_graduate_plan`` never stamps. Best-effort per
    node: a missing or unreadable plan never fails the mutation.
    """
    ids = [i for i in dict.fromkeys(node_ids) if i]
    if not ids:
        return
    try:
        from fno.graph.store import read_graph
        from fno.plan._project import project_graph_nodes

        entries = read_graph(_graph_path())
    except Exception as e:  # noqa: BLE001 - additive; never wedge the mutation
        sys.stderr.write(f"warning: plan projection setup failed: {e}\n")
        return
    project_graph_nodes(entries, ids)


def _apply_completion_fields(node: dict) -> None:
    """Set the fields that mark a node done.

    Shared by ``done`` and ``reconcile`` so both close paths stay in
    lockstep. The caller owns the idempotency check (skip when
    ``completed_at`` is already set). ``recompute_statuses`` derives
    ``_status: done`` from ``completed_at`` and unblocks dependents.
    """
    node["locked_by"] = None
    node["claimed_at"] = None
    # Done dominates deferred per the cascade. Clear any deferred/queued state
    # so the row presents as cleanly done with no ghost fields.
    node["deferred_at"] = None
    node["deferred_reason"] = None
    node["queued_at"] = None
    node["queued_reason"] = None
    node["completed_at"] = datetime.now(timezone.utc).isoformat()


def _cascade_close_parents(entries: list[dict], node_id: str) -> list[str]:
    """Close ancestor epics whose children are now all complete (x-33b2).

    Called inside the close mutator right after a node's completion fields are
    set. An epic is a container with no PR of its own - its work IS its
    decomposed children - so it is "done" exactly when all of them are. Walking
    UP the ``parent`` chain, each ancestor whose children all carry
    ``completed_at`` is closed too (and tagged with a completion_note so the
    PR-less close is self-explaining), continuing to the grandparent.

    This is the closure path that lets epics be excluded from build-SELECTION
    everywhere (`next`/`ready`/advance_dependents never dispatch the box): the
    box closes itself off the merge event that finishes its last child. It fires
    on every close path (done + reconcile + update --completed) since each calls
    this after ``_apply_completion_fields``, and it is uniform across projects
    because it follows the parent EDGE, not a project filter - so a cross-project
    parent closes on the same merge that completes its last child.

    Idempotent: an already-done or missing ancestor stops that branch. The walk
    is depth-capped against a malformed parent cycle.
    """
    id_to_entry = {
        e["id"]: e for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    children_by_parent: dict[str, list[dict]] = {}
    for e in entries:
        if isinstance(e, dict) and isinstance(e.get("parent"), str):
            children_by_parent.setdefault(e["parent"], []).append(e)

    closed: list[str] = []
    cur = id_to_entry.get(node_id)
    for _ in range(64):  # depth cap: guards against a malformed parent cycle
        pid = cur.get("parent") if isinstance(cur, dict) else None
        if not isinstance(pid, str):
            break
        parent = id_to_entry.get(pid)
        if parent is None or parent.get("completed_at"):
            break  # missing or already-closed ancestor -> stop this branch
        kids = children_by_parent.get(pid) or []
        if not kids or any(not k.get("completed_at") for k in kids):
            break  # at least one child still open -> the epic is not done yet
        _apply_completion_fields(parent)
        if not parent.get("completion_note"):
            parent["completion_note"] = "auto-closed: all children complete"
        closed.append(pid)
        cur = parent  # cascade up to the grandparent
    return closed


def _strandable_epic_ids(entries: list[dict]) -> set[str]:
    """Open epics (parents) whose children are ALL done - closeable right now.

    Read-only. The cascade (_cascade_close_parents) only fires on a child-CLOSE
    event, so an epic whose children were all completed BEFORE this code shipped
    (or whose last child closed via a path that did not cascade) is stranded:
    open, all children done, and - now that containers are hidden from
    next/ready - unreachable for closure. This identifies them so reconcile can
    self-heal (codex P2 on PR #69).
    """
    children_by_parent: dict[str, list[dict]] = {}
    for e in entries:
        if isinstance(e, dict) and isinstance(e.get("parent"), str):
            children_by_parent.setdefault(e["parent"], []).append(e)
    id_to_entry = {
        e["id"]: e for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    out: set[str] = set()
    for pid, kids in children_by_parent.items():
        parent = id_to_entry.get(pid)
        if (
            parent is not None
            and not parent.get("completed_at")
            and all(k.get("completed_at") for k in kids)
        ):
            out.add(pid)
    return out


def _sweep_close_done_epics(entries: list[dict]) -> list[str]:
    """Close every open epic whose children are all done (self-heal/migration).

    Idempotent, mutating, run inside a close mutator. Repeats to a fixpoint so a
    freshly-closed epic heals ITS parent too (grandparent chains). Returns the
    ids it closed so the caller can auto-continue their dependents. Reconcile
    runs this so pre-existing stranded all-done epics (codex P2 on PR #69) heal
    on the next reconcile pass - going forward the cascade prevents new ones, so
    this is a no-op once migrated.
    """
    id_to_entry = {
        e["id"]: e for e in entries
        if isinstance(e, dict) and isinstance(e.get("id"), str)
    }
    closed: list[str] = []
    for _ in range(64):  # fixpoint, depth-capped against a malformed cycle
        ready = _strandable_epic_ids(entries)
        if not ready:
            break
        for pid in ready:
            parent = id_to_entry.get(pid)
            if parent is None or parent.get("completed_at"):
                continue
            _apply_completion_fields(parent)
            if not parent.get("completion_note"):
                parent["completion_note"] = "auto-closed: all children complete"
            closed.append(pid)
    return closed


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


@cli.command(
    "done",
    epilog="Related: `fno backlog reconcile` closes nodes whose PR merged outside the gate (hidden).",
)
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
    is MERGED (x-aba7: graph done = merged, uniformly). An OPEN PR is NOT
    closing evidence - the node is awaiting merge and closes on the actual
    merge via reconcile / merge-triggered advance. CI state is irrelevant to
    the close decision.

    Exit codes:
        0  success (node closed)
        1  validation error (bad id, node not found)
        2  usage error (--force without --reason)
        3  gh cross-check refused: CLOSED-unmerged / UNKNOWN, no merge evidence
           (retryable when the PR merges; walker treats this as Parked)
        4  gh outage: subprocess failure / timeout / parse error; retryable
        5  awaiting merge: PR OPEN, not merged; node stays in_review
           (success-shaped; close lands via reconcile/advance at merge)
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

        # Try each ref in order; the first MERGED ref is closing evidence
        # (x-aba7: graph done = merged). An OPEN ref means the node is awaiting
        # merge - success-shaped (exit 5), never closing evidence. CI state is
        # NOT consulted: whether CI is green is the session's finish-line concern
        # (loop-check), not the graph-close decision.
        evidence_found = False
        refusal_reason: Optional[str] = None
        outage_error: Optional[str] = None
        open_pr_number: Optional[int] = None  # first OPEN ref -> awaiting merge

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
                if open_pr_number is None:
                    open_pr_number = pr_number
            else:
                # CLOSED (not merged) or UNKNOWN
                refusal_reason = (
                    f"PR #{pr_number} state={pr_state.state} (not merged)"
                )

        if not evidence_found:
            # A definitive OPEN ref wins over an outage: we KNOW the node has a
            # live PR awaiting merge, so exit 5 (success-shaped, retryable on
            # merge) rather than the conservative outage retry.
            if open_pr_number is not None:
                typer.echo(
                    f"awaiting merge: PR #{open_pr_number} is OPEN, not merged. "
                    f"{task_id} stays in_review and closes on merge "
                    f"(reconcile / merge-triggered advance). "
                    f"Use --force --reason TEXT for an early close.",
                    err=True,
                )
                raise typer.Exit(code=5)

            # No MERGED, no OPEN. An outage on any ref is a retryable outage
            # (covers pure outage AND the partial CLOSED+outage conservatism:
            # never refuse when a ref we could not query might be evidence).
            if outage_error:
                typer.echo(
                    f"Error: gh cross-check failed for {task_id}: {outage_error}\n"
                    f"The check is retryable once gh is available again. Node stays open.",
                    err=True,
                )
                raise typer.Exit(code=4)

            # Pure policy refusal - CLOSED-unmerged / UNKNOWN only.
            msg = refusal_reason or f"PR #{first_pr_number}: no merged evidence"
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
    cascade_closed_out: list = []

    # Cost stamp (Wave 2.2): the ledger has per-plan cost the node never captured
    # (2-3 fills). Aggregate it outside the lock; ledger absent/rowless -> null,
    # never blocks the close. Reuses the same rollup `fno done` uses.
    cost_rollup: dict = {}
    try:
        from fno.done.cli import _rollup_from_ledger

        cost_rollup = _rollup_from_ledger(node.get("plan_path"))
    except Exception:
        cost_rollup = {}

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
        # Fill-only: never overwrite a cost a richer path (e.g. `fno done`)
        # already stamped, and don't drop rows appended during the run.
        if cost_rollup.get("cost_usd") is not None and not n.get("cost_usd"):
            n["cost_usd"] = cost_rollup["cost_usd"]
        if cost_rollup.get("cost_sessions") and not n.get("cost_sessions"):
            n["cost_sessions"] = cost_rollup["cost_sessions"]
        # Close any now-all-done ancestor epic (x-33b2): the box is done when its
        # children are, and it carries no PR of its own to close it explicitly.
        cascade_closed_out.extend(_cascade_close_parents(entries, task_id))
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

    # Project the closed node + any cascade-closed epic parents onto their plans
    # (forward-only, stamps done_at) AFTER the stamp above, so the primary plan's
    # shipped_at is written before its done_at (never done-before-shipped). The
    # primary is already `done` here, so this is a no-op on it and its real job
    # is the cascade-closed epic parents that _stamp_and_graduate_plan skips.
    # --skip-stamp suppresses ALL plan writes, projection included.
    if not skip_stamp:
        _project_plans_from_graph([task_id, *cascade_closed_out])

    # A2 (x-122a): retro-at-done lifecycle trigger. Dispatch a `retro` context
    # /think while the closed node's session context is still resolvable. Gated
    # by config.think_spawn.on_retro (default OFF) and strictly non-fatal: a
    # dispatch failure never unwinds the close it rode in on.
    try:
        from fno.provenance.spawn_think import on_node_retro

        on_node_retro(node)
    except Exception:  # noqa: BLE001 - additive; never wedge `done`
        pass


# -- reconcile (close merged-PR drift) --

@cli.command("advance", hidden=True)
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
    model: Optional[str] = typer.Option(
        None, "--model", "-m",
        help="Pin a model for the dispatched worker(s), overriding node annotations.",
    ),
    provider: Optional[str] = typer.Option(
        None, "--provider",
        help="Pin a provider for the dispatched worker(s). (No -p short: it is --project here.)",
    ),
) -> None:
    """Dispatch a fresh /target no-merge worker for the next now-unblocked node.

    Merge-triggered auto-continue (ab-3cd195b6). Opt-in and non-fatal: when
    auto-continue is disabled it emits advance_skipped{disabled} and dispatches
    nothing. Driven by the merge event (reconcile / post-merge), so megawalk,
    /target, and /megatron all inherit it without driver-specific code. Always
    exits 0 (a dispatch decision is never an error to the host op).
    """
    from fno.agents.provider_resolve import (
        DispatchFlagError,
        reject_empty_model,
        resolve_dispatch_provider,
    )
    from fno.backlog.advance import advance as _advance
    from fno.backlog.advance import advance_dependents as _advance_deps

    # Validate the dispatch pins before any spawn; provider is resolved only when
    # given so an absent pin lets the spawn path keep its per-node/default choice.
    try:
        model = reject_empty_model(model)
        provider = resolve_dispatch_provider(provider)[0] if provider is not None else None
    except DispatchFlagError as exc:
        typer.echo(f"advance: {exc}", err=True)
        raise typer.Exit(code=2)

    # RC2 (x-33b2): closed_project is the CLOSED NODE's own project, read from the
    # graph - NEVER the --project next-selection flag. --project restricts which
    # project advance() picks `next` from; it is normally OMITTED on a manual
    # `advance --closed A`, which left closed_project=None and defeated
    # advance_dependents' same-project guard, misrouting a same-project dependent
    # through the cross-project --cwd path onto a protected branch where the bg
    # worker dies. Mirror the reconcile path (cli.py reads the node's .project).
    closed_project: Optional[str] = None
    if closed:
        try:
            from fno.graph._intake import _find_node
            from fno.graph.store import read_graph

            _cn = _find_node(read_graph(_graph_path()), closed)
            closed_project = _cn.get("project") if _cn else None
        except Exception:  # noqa: BLE001 - non-fatal; advance_deps fails closed on None
            closed_project = None

    try:
        result = _advance(
            closed_node_id=closed, project=project, verbose=verbose,
            model=model, provider=provider,
        )
        # G1 (AC5-FR): follow this node's blocked_by edges into OTHER projects.
        # Only meaningful with --closed (an edge source); the project-scoped
        # next selection above never reaches a foreign dependent. Shares the
        # dispatch:<id> dedup with reconcile's call so a node seen by both the
        # reconcile sweep and this explicit verb dispatches at most once.
        if closed:
            _advance_deps(
                closed_node_id=closed, closed_project=closed_project, verbose=verbose,
                model=model, provider=provider,
            )
            # G4: route the closed node's contract dependents to a reconcile pass
            # (or a pending sentinel). Shares the dispatch:<id> dedup with the two
            # advance paths so a node seen by all three dispatches at most once.
            from fno.backlog.reconcile_dispatch import dispatch_reconcile_for_blocker
            dispatch_reconcile_for_blocker(closed_node_id=closed, verbose=verbose)
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


@cli.command("reconcile", hidden=True)
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
        _effective_reconcile_cwd,
        emit_gate_escape_for_record,
        emit_human_touch_for_record,
        emit_session_satisfied_for_record,
        scan_merge_drift,
        write_retro_sentinel,
    )
    from fno.paths import retro_pending_dir

    entries = read_graph(_graph_path())
    records = scan_merge_drift(entries, node_id=node)

    closeable = [r for r in records if r.closeable]
    failures = [r for r in records if r.error is not None]

    # Pre-existing stranded all-done epics to self-heal this sweep (codex P2 on
    # PR #69). Read-only check so a reconcile with no drift AND nothing to heal
    # still skips the lock entirely. ONLY on a full reconcile: a node-scoped
    # `reconcile --node <id>` must not close/dispatch unrelated epics (codex P2),
    # so the global sweep is suppressed there (the targeted node's own cascade
    # still fires).
    strandable = _strandable_epic_ids(entries) if node is None else set()

    closed: list[dict] = []
    healed_epics: list[str] = []

    if not dry_run and (closeable or strandable):
        # Apply every close in ONE locked mutation rather than locking once
        # per node: locked_mutate_graph acquires a file lock and rewrites the
        # whole graph, so a per-node loop is O(N) lock+rewrite cycles. The
        # mutator collects the records it actually closed (idempotency: a node
        # closed or removed out-of-band between the read-only scan and the lock
        # is skipped) so the post-lock work only touches genuinely-closed nodes.
        actually_closed: list = []
        # Ancestor epics the cascade closed this sweep (x-33b2). Their OWN
        # dependents (a node blocked_by the epic) must be auto-continued too, so
        # we accumulate the ids here and run the same dispatch path for them
        # after the lock - else an epic-level dependent stalls.
        cascade_closed_acc: list = []

        def mutator(entries):
            actually_closed.clear()
            cascade_closed_acc.clear()
            for record in closeable:
                node_obj = _find_node(entries, record.node_id)
                if node_obj and not node_obj.get("completed_at"):
                    _apply_completion_fields(node_obj)
                    # Backfill the PR ref for a reverse-mapped node (dead before
                    # the node<->PR stamp): the recovered number/url live only on
                    # the record, so without this the closed node stays
                    # pr_number: null - the board loses the shipped-PR link and
                    # detect_reverted_nodes() (which reads node_pr_refs) can never
                    # match a later revert. Only fill when absent so the forward
                    # path (node already stamped) is untouched.
                    if record.pr_number and not node_obj.get("pr_number"):
                        node_obj["pr_number"] = record.pr_number
                        node_obj["pr_url"] = record.pr_url
                    # Cascade-close now-all-done ancestor epics (x-33b2), uniform
                    # across projects (follows the parent edge, not a filter).
                    cascade_closed_acc.extend(
                        _cascade_close_parents(entries, record.node_id)
                    )
                    actually_closed.append(record)
            # Self-heal pre-existing stranded all-done epics (codex P2): close any
            # open epic whose children are already all done, even with no drift
            # this sweep. Full reconcile only - a node-scoped run must not touch
            # unrelated epics. Going forward the cascade prevents new ones, so
            # this is a no-op once migrated. Their dependents auto-continue via
            # the same cascade_closed_acc dispatch loop.
            if node is None:
                cascade_closed_acc.extend(_sweep_close_done_epics(entries))
            return entries

        # Capture the POST-lock graph (codex P2): resolving a closed node's
        # project/cwd from the pre-lock `entries` snapshot risks stale routing if
        # a concurrent reparent/reproject landed between scan and lock, and a
        # cascade parent only reachable in the locked graph would resolve to None.
        post_entries = locked_mutate_graph(_graph_path(), mutator)

        def _auto_continue_after_close(node_id, project, root):
            """Merge-triggered auto-continue dispatch for one just-closed node:
            same-project `next`, cross-project dependents, and contract de-stub.
            Shared by directly-closed records AND cascade-closed ancestor epics
            (x-33b2) so an epic-level dependent is dispatched, not stranded."""
            from fno.backlog.advance import advance as _advance
            from fno.backlog.advance import advance_dependents as _advance_deps
            from fno.backlog.reconcile_dispatch import dispatch_reconcile_for_blocker
            _advance(closed_node_id=node_id, project=project, project_root=root)
            _advance_deps(closed_node_id=node_id, closed_project=project, project_root=root)
            dispatch_reconcile_for_blocker(closed_node_id=node_id, project_root=root)

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

            # W4 touch telemetry: an out-of-band merge is a human steering
            # action no loop performed. Once per node - reconcile only ever
            # closes a node once (AC4-EDGE).
            emit_human_touch_for_record(record)

            # Tier-1 gate_escape (x-f894): a STRICTER subset of the touch above -
            # only when a required review bot never reviewed the oob-merged PR
            # (the #222 boundary). Resolve the repo's required bots (empty on
            # most repos -> no-op) and let the helper apply the boundary + emit.
            # Fully fail-open: never abort the close.
            _required_bots: list = []
            if record.cwd:
                try:
                    from fno.config import load_settings_for_repo
                    _settings = load_settings_for_repo(Path(record.cwd))
                    # The review block lives under `config:` (SettingsModel ->
                    # config.review), NOT at the top level - reading
                    # `_settings.review` always missed and returned [], so the
                    # emit short-circuited and this telemetry never fired in the
                    # real CLI path (codex P2 on PR #232). github_apps is the bot
                    # half of the required gate; a local peer reviewer that never
                    # reviewed is NOT counted here. That under-reports (fail-safe
                    # direction) and is acceptable for a Tier-1 metric - dead-bot
                    # is the recurring escape this catches.
                    _required_bots = list(
                        _settings.review.github_apps or []
                    )
                except Exception:
                    _required_bots = []  # fail open: unresolvable config -> no emit
            emit_gate_escape_for_record(record, required_bots=_required_bots)

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
                _adv_node = _find_node(post_entries, record.node_id)
                _adv_project = _adv_node.get("project") if _adv_node else None
                # Resolve auto-continue state against the CLOSED NODE's project
                # context, not the reconcile's cwd (codex P2): a full-graph
                # reconcile run from project A can close a node belonging to
                # project B, and B's campaign-arm marker lives under B's root.
                _adv_cwd = _adv_node.get("cwd") if _adv_node else None
                # If the closed node's recorded cwd is an archived worktree,
                # route from its project root instead (x-3dd0): otherwise advance
                # probes the campaign-arm marker under a missing dir and strands
                # the next node. Re-resolved from the POST-lock node so a
                # concurrent reproject is still honored.
                if _adv_cwd:
                    _adv_cwd = _effective_reconcile_cwd(_adv_cwd, _adv_project)
                _adv_root = Path(_adv_cwd) if _adv_cwd else None
                # next (same-project) + cross-project dependents (G1) + contract
                # de-stub (G4), in one shared helper.
                _auto_continue_after_close(record.node_id, _adv_project, _adv_root)
            except Exception as _adv_exc:  # noqa: BLE001 - never abort the sweep
                typer.echo(
                    f"warning: auto-continue advance after closing "
                    f"{record.node_id} failed: {_adv_exc}",
                    err=True,
                )

            # Post-merge-ritual auto-dispatch (x-47be / task 2.1): when
            # config.post_merge.auto_run is armed for the closed node's repo,
            # spawn ONE bg /fno:pr merged worker for the merged PR (it runs
            # reconcile + retro + parking-lot + canonical-sync). Opt-in, deduped
            # per merge SHA, strictly non-fatal, and the posture is never silent.
            try:
                _pm_auto = False
                _pm_cfg_err: Optional[str] = None
                if record.cwd:
                    try:
                        from fno.config import load_settings_for_repo
                        _pm_auto = bool(
                            load_settings_for_repo(Path(record.cwd)).post_merge.auto_run
                        )
                    except Exception as _cfg_exc:  # noqa: BLE001
                        # Fail open (no dispatch) but do NOT misreport a real
                        # config-load error as "auto_run off" - an opted-in
                        # operator would be pointed at the wrong cause.
                        _pm_cfg_err = str(_cfg_exc)[:160]
                # Echoes are guarded on `not json_out`: CliRunner (and the real
                # --json consumer) folds stderr into stdout, so any prose here
                # would corrupt the JSON payload (x-4d9d). The dispatch itself
                # still fires in JSON mode - only the human line is suppressed.
                if _pm_auto:
                    from fno.graph._reconcile import dispatch_post_merge_ritual
                    try:
                        _pm_node = _find_node(post_entries, record.node_id)
                    except Exception:  # noqa: BLE001 - warm route is best-effort
                        _pm_node = None
                    _pm = dispatch_post_merge_ritual(
                        record.pr_number,
                        dedup_key=record.merge_sha,
                        node_cwd=record.cwd,
                        auto_run=True,
                        source_session_id=(
                            (_pm_node or {}).get("source_session_id")
                        ),
                        source_harness=((_pm_node or {}).get("source_harness")),
                    )
                    if not json_out:
                        if _pm.outcome == "routed-warm":
                            typer.echo(
                                f"post-merge: routed warm /fno:pr merged "
                                f"{record.pr_number} to session {_pm.short_id}"
                            )
                        elif _pm.outcome == "dispatched":
                            typer.echo(
                                f"post-merge: dispatched /fno:pr merged "
                                f"{record.pr_number} (short_id={_pm.short_id}, "
                                f"{_pm.detail or 'cold'})"
                            )
                        elif _pm.outcome == "already-dispatched":
                            typer.echo(
                                f"post-merge: ritual already dispatched for PR "
                                f"#{record.pr_number}; skipping"
                            )
                        elif _pm.outcome == "spawn-failed":
                            # Loud + recoverable: the node is already closed so a
                            # later reconcile will NOT re-dispatch. Name the manual
                            # recovery so the canonical sync is not silently skipped.
                            typer.echo(
                                f"warning: post-merge ritual dispatch for PR "
                                f"#{record.pr_number} failed: {_pm.detail}. "
                                f"Recover with `fno pr sync-canonical --pr-number "
                                f"{record.pr_number}` or `/fno:pr merged "
                                f"{record.pr_number}`.",
                                err=True,
                            )
                elif not json_out:
                    if _pm_cfg_err is not None:
                        typer.echo(
                            f"post-merge: could not resolve config for "
                            f"{record.cwd} ({_pm_cfg_err}); not dispatching ritual "
                            f"for PR #{record.pr_number}",
                            err=True,
                        )
                    else:
                        typer.echo(
                            f"post-merge: auto_run off; not dispatching ritual for PR "
                            f"#{record.pr_number}",
                            err=True,
                        )
            except Exception as _pm_exc:  # noqa: BLE001 - never abort the sweep
                if not json_out:
                    typer.echo(
                        f"warning: post-merge ritual dispatch after closing "
                        f"{record.node_id} failed: {_pm_exc}",
                        err=True,
                    )

        # Project every closed node (records + cascade-closed epic parents) onto
        # its plan (forward-only, stamps done_at). The per-record stamp above
        # only touches directly-closed records; the epic parents need this.
        _project_plans_from_graph(
            [r.node_id for r in actually_closed] + list(cascade_closed_acc)
        )

        # x-33b2: a cascade-closed parent epic unblocks its OWN dependents (a node
        # blocked_by the epic). The per-record loop above only dispatched the
        # directly-closed children, so run the same auto-continue for each
        # cascade-closed ancestor too - else an epic-level dependent stalls.
        # Deduped; project/cwd read from the (close-stable) graph.
        _seen_parents: set = set()
        for _pid in cascade_closed_acc:
            if _pid in _seen_parents:
                continue
            _seen_parents.add(_pid)
            try:
                _pn = _find_node(post_entries, _pid)
                _pproj = _pn.get("project") if _pn else None
                _proot = Path(_pn["cwd"]) if _pn and _pn.get("cwd") else None
                _auto_continue_after_close(_pid, _pproj, _proot)
            except Exception as _exc:  # noqa: BLE001 - never abort the sweep
                typer.echo(
                    f"warning: auto-continue after cascade-closing {_pid} failed: {_exc}",
                    err=True,
                )
        # Epics the cascade/sweep auto-closed this run, for user-visible accounting
        # (codex P3): the close summaries below otherwise only describe PR-drift
        # records and would report "in sync" even after healing epics.
        healed_epics = sorted(_seen_parents)
    elif dry_run and (closeable or strandable):
        # Accurate --dry-run preview (codex P2): the heal set is NOT just the
        # pre-close `strandable` epics - closing a closeable last child cascade-
        # closes its parent, and the sweep fixpoint reaches ancestors. Simulate
        # the exact close + cascade + sweep on a THROWAWAY deep copy so the
        # preview matches a real run, mutating nothing real.
        import copy as _copy

        _sim = _copy.deepcopy(entries)
        _sim_acc: list = []
        for record in closeable:
            _sn = _find_node(_sim, record.node_id)
            if _sn and not _sn.get("completed_at"):
                _apply_completion_fields(_sn)
                _sim_acc.extend(_cascade_close_parents(_sim, record.node_id))
        if node is None:
            _sim_acc.extend(_sweep_close_done_epics(_sim))
        healed_epics = sorted(set(_sim_acc))

    # W4 causal links: best-effort revert stamp, full sweep only. A merged
    # "Revert ..." PR referencing a PR carried by a graph node flips that
    # node's `reverted` flag so survival math stops counting it. Strictly
    # non-fatal; misses fall back to `fno backlog update --reverted`.
    reverted_stamped: list[dict] = []
    if node is None:
        try:
            from fno.graph._reconcile import (
                ReconcileError,
                detect_reverted_nodes,
                fetch_recent_merged_prs,
            )

            try:
                merged_prs = fetch_recent_merged_prs()
            except ReconcileError:
                # gh unauthed/offline: reconcile auto-fires on SessionStart,
                # so a degraded gh must stay quiet (manual --reverted remains).
                merged_prs = []
            pairs = detect_reverted_nodes(merged_prs, entries)
            if pairs and not dry_run:
                def _stamp_reverts(entries2):
                    for nid, _rpr in pairs:
                        n = _find_node(entries2, nid)
                        if n is not None and not n.get("reverted"):
                            n["reverted"] = True
                    return entries2

                locked_mutate_graph(_graph_path(), _stamp_reverts)
            reverted_stamped = [
                {"node_id": nid, "revert_pr": rpr} for nid, rpr in pairs
            ]
        except Exception as exc:  # noqa: BLE001 - never abort the sweep
            typer.echo(f"warning: revert detection skipped: {exc}", err=True)

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
            # Auto-closed container epics (cascade + self-heal sweep); on --dry-run
            # this is the simulated preview of what a real run would heal (codex P3).
            "healed_epics": healed_epics,
            # Nodes whose ship a merged revert PR names (stamped unless --dry-run).
            "reverted": reverted_stamped,
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

    if (
        not closeable
        and not failures
        and not strandable
        and not healed_epics
        and not reverted_stamped
    ):
        typer.echo("No merged-PR drift found. Backlog is in sync.")
        return

    if dry_run:
        typer.echo(f"Would close {len(closeable)} node(s) (dry-run, nothing mutated):")
        for r in closeable:
            typer.echo(f"  {r.node_id}  PR #{r.pr_number} MERGED  {r.pr_url or ''}".rstrip())
        if healed_epics:
            typer.echo(
                f"Would self-heal {len(healed_epics)} container epic(s): "
                + ", ".join(healed_epics)
            )
    else:
        typer.echo(f"Closed {len(closed)} node(s):")
        for c in closed:
            stamp_note = " (plan stamped)" if c["plan_stamped"] else ""
            typer.echo(f"  {c['node_id']}  PR #{c['pr_number']}{stamp_note}")
        if closed:
            typer.echo(f"Retro sentinels written under {retro_pending_dir()}")
        if healed_epics:
            typer.echo(
                f"Auto-closed {len(healed_epics)} container epic(s) "
                f"(all children complete): " + ", ".join(healed_epics)
            )

    if reverted_stamped:
        verb = "Would stamp" if dry_run else "Stamped"
        typer.echo(f"{verb} {len(reverted_stamped)} node(s) reverted:")
        for r in reverted_stamped:
            typer.echo(f"  {r['node_id']}  revert PR #{r['revert_pr']}")

    if failures:
        typer.echo(f"{len(failures)} node(s) could not be resolved:", err=True)
        for r in failures:
            typer.echo(f"  {r.node_id}  PR #{r.pr_number}: {r.error}", err=True)
        # Partial reconcile: non-zero exit so callers can detect it.
        raise typer.Exit(code=4)


# -- maintain (recurring backlog + kanban hygiene sweep) --

def _validity_rg_search(symbol: str) -> Optional[int]:
    """Bounded git-grep file count for a named symbol under the repo root.

    Returns the number of tracked files mentioning ``symbol`` (a validity signal:
    0 files -> the symbol likely no longer exists), or ``None`` when the source
    is unavailable (rg/git missing, timeout, not a repo) so the sweep records it
    unavailable rather than reading a spurious zero. 5 s cap (Locked Decision #7).
    """
    import subprocess

    from fno.paths import resolve_repo_root

    try:
        root = str(resolve_repo_root())
    except Exception:
        return None
    try:
        proc = subprocess.run(
            ["git", "-C", root, "grep", "-l", "--fixed-strings", "-e", symbol],
            capture_output=True, text=True, timeout=_maintain_source_timeout(),
        )
    except (OSError, subprocess.SubprocessError):
        return None
    # git grep exits 1 with no output when there are no matches (not an error).
    if proc.returncode not in (0, 1):
        return None
    return sum(1 for line in proc.stdout.splitlines() if line.strip())


def _maintain_source_timeout() -> float:
    from fno.graph import maintain as _m

    return _m.EVIDENCE_SOURCE_TIMEOUT_S


@cli.command("maintain", hidden=True)
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
    recheck: bool = typer.Option(
        False,
        "--recheck",
        help="Validity sweep: re-review watermarked ideas (ignore prior decks).",
    ),
    no_validity: bool = typer.Option(
        False,
        "--no-validity",
        help="Skip the validity sweep (the leg that calls the analyzer).",
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

        _maintain_cfg = load_settings().backlog.maintain
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
                    n["locked_by"] = None
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

    # --- leg 8: validity sweep (proposal-only, ALWAYS - never mutates) ---
    # Runs even under --apply as proposal-only; a single analyzer call reviews the
    # oldest stale ideas and writes an immutable evidence deck. Self-limiting:
    # once the pile is watermarked, later runs find 0 eligible and skip the call.
    validity_result = None
    if not no_validity:
        try:
            from fno.config import load_settings

            _vcfg = load_settings().backlog.maintain
            v_days, v_batch = _vcfg.validity_days, _vcfg.validity_batch_size
        except Exception:
            v_days, v_batch = _maintain.VALIDITY_DAYS_DEFAULT, _maintain.VALIDITY_BATCH_DEFAULT

        from fno import paths as _paths

        try:
            _deck_dir = _paths.state_dir() / "validity-decks"
        except Exception:
            _deck_dir = None

        if _deck_dir is not None:
            def _exists_factory(node):
                root = node.get("cwd")
                if not isinstance(root, str) or not os.path.isdir(root):
                    return None  # repo unavailable -> path evidence recorded unavailable
                root_p = os.path.abspath(os.path.expanduser(root))
                # `rel` is extracted from untrusted node text; contained_path_exists
                # rejects an absolute or `../` escape from the repo root (CWE-22).
                return lambda rel: _maintain.contained_path_exists(root_p, rel)

            # Re-read seam: the sweep calls this AFTER the analyzer returns, so a
            # node that raced to claimed/done/deferred DURING analysis voids its
            # recommendation (AC4-EDGE).
            def _reread():
                return recompute_statuses(read_graph(_graph_path()))

            validity_result = _maintain.run_validity_sweep(
                entries,
                validity_days=v_days,
                batch_size=v_batch,
                out_dir=_deck_dir,
                claimed_ids=frozenset(claimed | _live_claimed_node_ids()),
                recheck=recheck,
                exists_factory=_exists_factory,
                search=_validity_rg_search,
                reread=_reread,
            )
            if validity_result.error and not json_out:
                typer.echo(f"validity: {validity_result.error}", err=True)
                raise typer.Exit(code=1)

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
        if validity_result is not None:
            payload["validity"] = {
                "eligible": validity_result.eligible,
                "counts": validity_result.counts,
                "deck": validity_result.deck_md,
                "degraded": validity_result.degraded,
                "stale": validity_result.stale,
                "error": validity_result.error,
            }
        typer.echo(json.dumps(payload, indent=2))
        if validity_result is not None and validity_result.error:
            raise typer.Exit(code=1)
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

    if validity_result is not None:
        for w in validity_result.warnings:
            typer.echo(f"  validity config: {w}", err=True)
        if validity_result.eligible == 0:
            typer.echo("validity: 0 eligible ideas")
        else:
            c = validity_result.counts
            tag = " (DEGRADED: analyzer unavailable)" if validity_result.degraded else ""
            stale = f", {validity_result.stale} stale" if validity_result.stale else ""
            typer.echo(
                f"validity: reviewed {validity_result.eligible} ideas{tag} -> "
                f"promote {c.get('promote', 0)} | keep {c.get('keep', 0)} | "
                f"supersede {c.get('supersede', 0)} | needs-human "
                f"{c.get('needs-human', 0)}{stale}"
            )
            typer.echo(f"  deck: {validity_result.deck_md}")


# -- reprioritize --

@cli.command("reprioritize", hidden=True)
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
    _project_plans_from_graph([result["id"]])


# -- archive --

@cli.command("archive", hidden=True)
def cmd_archive(
    apply: bool = typer.Option(
        False, "--apply", help="Move the entries (default: dry-run, report only)."
    ),
    older_than_days: int = typer.Option(
        30, "--older-than-days", help="Only archive terminal nodes older than N days."
    ),
    roadmap_id: Optional[str] = typer.Option(
        None, "--roadmap-id", help="Restrict the sweep to this roadmap group."
    ),
) -> None:
    """Sweep old terminal (done/superseded) nodes into graph-archive.json.

    Dry-run by default: prints how many would move and why some are held back.
    ``--apply`` mutates under the graph lock (archive written first, then the
    working graph, so a crash duplicates rather than loses). Never archives a
    node an OPEN node still references (blocker, parent, or supersede target).
    """
    from datetime import datetime, timezone

    from fno.graph.store import (
        _apply_graph_defaults,
        _read_json,
        _write_json,
        read_graph,
        locked_mutate_graph,
        GraphCorruptError,
    )
    from fno.graph.archive import partition_for_archive, merge_into_archive

    now = datetime.now(timezone.utc)

    def _split(entries):
        # Guard against the FULL graph so an open node in another roadmap that
        # references one of these terminal nodes (blocker/parent/supersede) is
        # still protected; only the archive SET is roadmap-restricted.
        to_archive, _remaining_pool, skipped = partition_for_archive(
            entries, older_than_days, now
        )
        if roadmap_id:
            to_archive = [e for e in to_archive if e.get("roadmap_id") == roadmap_id]
        arch_ids = {e["id"] for e in to_archive if isinstance(e, dict) and e.get("id")}
        remaining = [e for e in entries if e.get("id") not in arch_ids]
        return to_archive, remaining, skipped

    if not apply:
        to_archive, _rem, skipped = _split(read_graph(_graph_path()))
        typer.echo(
            f"[dry-run] would archive {len(to_archive)} terminal node(s) "
            f"older than {older_than_days}d to {_archive_path()}"
        )
        held = {}
        for s in skipped:
            held[s["_skip"]] = held.get(s["_skip"], 0) + 1
        for reason, n in sorted(held.items()):
            typer.echo(f"  held back ({reason}): {n}")
        typer.echo("Re-run with --apply to move them.")
        return

    archived_count: list = [0]

    def mutator(entries):
        to_archive, remaining, _skipped = _split(entries)
        if not to_archive:
            return entries
        archived_count[0] = len(to_archive)

        # Archive-first: append (deduped) and write the archive BEFORE returning
        # `remaining` for the graph write, so a crash leaves a duplicate (healed
        # on the next sweep) rather than a lost node.
        archive_path = _archive_path()
        try:
            existing = _apply_graph_defaults(_read_json(archive_path))
        except GraphCorruptError:
            typer.echo(f"Warning: {archive_path} corrupt, starting fresh archive", err=True)
            existing = []
        archive_path.parent.mkdir(parents=True, exist_ok=True)
        _write_json(merge_into_archive(existing, to_archive), archive_path)
        return remaining

    locked_mutate_graph(_graph_path(), mutator)
    if archived_count[0]:
        typer.echo(f"Archived {archived_count[0]} terminal node(s) to {_archive_path()}")
    else:
        typer.echo("No terminal nodes eligible to archive.")


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


def _do_intake_multi(args, all_paths: list[str], *, roadmap_id, dry_run) -> None:
    """Multi-path intake flow delegating to intake helpers."""
    from fno.graph._constants import PRIORITY_ORDER
    from fno.graph.store import read_graph, locked_mutate_graph
    from fno.graph._intake import (
        _prepare_intake, _build_intake_node, _validate_cli_deps,
    )
    from fno.graph.depends import _derive_title

    cli_deps: list[str] = (
        [d.strip() for d in args.deps.split(",") if d.strip()] if args.deps else []
    )
    _validate_cli_deps(cli_deps, read_graph(_graph_path()))

    resolved: list[dict] = []
    for raw in all_paths:
        if not os.path.exists(raw):
            resolved.append({"path": raw, "files": [], "status": "missing"})
            continue
        resolved.append({"path": raw, "files": [raw], "status": "ready"})

    concrete_files = [f for r in resolved if r["status"] == "ready" for f in r["files"]]
    if not concrete_files:
        for r in resolved:
            if r["status"] == "missing":
                typer.echo(f"warning: not found, skipped: {r['path']}", err=True)
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
                typer.echo(f"  warning: not found, skipped: {r['path']}")
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

    missing = sum(1 for r in resolved if r["status"] == "missing")
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

    def _resolve_against(pool: list[dict]) -> list[dict]:
        # Exact resolution first (id / slug / bare-hex). Trying resolve_node
        # BEFORE the ab- prefix branch is deliberate: a title can slugify to an
        # `ab-`-led slug (e.g. "AB test cleanup" -> `ab-test-cleanup`), which
        # resolve_id would reject as a malformed id; the exact-slug tier catches
        # it so `find` and `get` resolve the same slug (codex P2).
        node = resolve_node(query, pool)
        if node.kind == "exact":
            return list(node.candidates)
        if q.startswith("ab-"):
            # Canonical id / id-prefix path - unchanged (resolve_id owns it).
            match = resolve_id(query, pool)
            if match.kind == "ambiguous":
                return list(match.candidates)
            if match.kind in {"exact", "fuzzy", "branch_derived"}:
                return [e for e in pool if e.get("id") == match.id]
            return []
        # High-recall describe-it search over title+slug+details.
        return search_entries(query, pool, fields=("title", "slug", "details"))

    def _passes_filters(e: dict) -> bool:
        if domain is not None and e.get("domain") != domain:
            return False
        if project is not None and e.get("project") != project:
            return False
        if status is not None and e.get("_status") != status:
            return False
        return True

    matched = [e for e in _resolve_against(entries) if _passes_filters(e)]

    # Read-through fallback to the archive: a node the sweep drained out of the
    # working graph must still surface here, or archiving done nodes silently
    # destroys the dedup recall `/think` + `/blueprint` depend on. Mirrors
    # `backlog get`'s fallback: working graph first, archive read lazily only on
    # a miss, results stamped `_archived`. A corrupt/absent archive is a miss,
    # never a crash (design "Errors").
    if not matched:
        from fno.paths import graph_archive_json

        archive_path = graph_archive_json()
        if archive_path.exists():
            # Guard the whole read + resolve + filter: a corrupt archive OR a
            # malformed archived entry must degrade to a miss, never propagate a
            # crash to the caller (design "Errors").
            try:
                archived = read_graph(archive_path)
                hits = [
                    {**e, "_archived": True}
                    for e in _resolve_against(archived)
                    if _passes_filters(e)
                ]
            except Exception:
                hits = []
            matched.extend(hits)

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


@cli.command("new", hidden=True)
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

@cli.command("rehash", hidden=True)
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


cli.add_typer(collisions_app, name="collisions", hidden=True)


# ---------------------------------------------------------------------------
# supersede: mark old node as replaced by a new node; auto-defer the old one
# ---------------------------------------------------------------------------


@cli.command("supersede", hidden=True)
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
        old_node["locked_by"] = None
        old_node["claimed_at"] = None
        old_node["deferred_at"] = datetime.now(timezone.utc).isoformat()
        old_node["deferred_reason"] = f"superseded by {new_id}: {cleaned_reason}"
        return entries

    locked_mutate_graph(_graph_path(), mutator)
    typer.echo(f"superseded {replaces} with {new_id}")
    _project_plans_from_graph([replaces, new_id])

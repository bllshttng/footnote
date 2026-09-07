"""How a backlog node dict is built, and the plan-less ``new`` verb.

``_build_backlog_node`` is the single builder every creation verb routes
through (add, idea, new, decompose, capture promote, retro land), and
``_session_provenance`` is the ambient origin it stamps. ``cmd_new`` lives
beside them: it was the last writer that built its dict inline, which made
``source_kind`` a two-writer field. cli.py re-exports the private names so
the lazy ``from fno.graph.cli import _build_backlog_node`` imports in
capture.py and retro/land.py keep resolving.
"""

from __future__ import annotations

import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

import typer

from fno.graph._constants import SOURCE_KIND_DEFAULT, validate_source_kind

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


def _session_provenance(
    running_cwd: Optional[str] = None,
    *,
    source_node: Optional[str] = None,
    known_ids: Optional[set] = None,
) -> dict:
    """Parent-edge provenance for a node born inside a live session.

    Reads the running session's env + ``.fno/target-state.md`` and returns
    ``source_session_id`` / ``source_harness`` / ``source_cwd`` /
    ``source_node_id`` / ``source_plan_path``. Every key degrades to ``None``
    and the function NEVER raises (AC-EDGE).

    The origin resolves through three branches in strict precedence:

    1. ``source_node`` - an explicit ``--source-node``, already resolved and
       validated by the CLI verb. Taken as given: re-judging it would need a
       raise this function has promised not to make.
    2. The owned manifest (below, claude-only and ownership-proven).
    3. ``FNO_NODE`` - written at spawn time by the spawner. NOT gated on
       harness: it is the only origin signal a codex/opencode worker has.

    ``known_ids`` is the caller's live snapshot. When supplied, an ambiently
    resolved id absent from it degrades to ``None`` rather than stamping an edge
    that dangles, and the dropped token comes back in ``source_node_dropped``.
    Every filing path resolves it inside its locked mutator, so the check costs
    no extra read; omitting it skips the check.

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

    from fno.claims.self_identity import resolve_self_identity

    identity = resolve_self_identity()
    session = getattr(identity, "session_id", None)
    harness = getattr(identity, "harness", None)

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
        except (OSError, ValueError):
            pass

    if source_node_id is None:
        source_node_id = (os.environ.get("FNO_NODE") or "").strip() or None

    dropped: Optional[str] = None
    if source_node_id is not None and known_ids is not None and source_node_id not in known_ids:
        dropped, source_node_id = source_node_id, None

    if source_node:
        # The explicit flag wins, so nothing was dropped: an ambient candidate
        # that lost a precedence contest is not a capture failure to report.
        source_node_id, dropped = source_node, None

    return {
        "source_session_id": session,
        "source_harness": harness,
        # session cwd is the transcript-resolver key; only meaningful with a session.
        "source_cwd": cwd if session else None,
        "source_node_id": source_node_id,
        "source_plan_path": source_plan_path,
        # Not a node field. An ambient signal naming a node the graph no longer
        # has is the one case worth telling the operator about: capture silently
        # regressing to nothing is what this feature exists to catch.
        "source_node_dropped": dropped,
    }


def _build_backlog_node(
    *,
    title: str,
    type_: str = "feature",
    parent: Optional[str] = None,
    project: Optional[str] = None,
    cwd: Optional[str] = None,
    priority: str = "p2",
    blocks_everything: bool = False,
    difficulty: Optional[str],
    difficulty_source: str = "filed",
    domain: str = "code",
    blocked_by: Optional[list[str]] = None,
    roadmap_id: Optional[str] = None,
    vision_path: Optional[str] = None,
    details: Optional[str] = None,
    size: Optional[str] = None,
    batch: Optional[str] = None,
    plan_path: Optional[str] = None,
    tags: Optional[list[str]] = None,
    source_node: Optional[str] = None,
    known_ids: Optional[set] = None,
    out: Optional[dict] = None,
    source_kind: str = SOURCE_KIND_DEFAULT,
    source: Optional[str] = None,
    source_project: Optional[str] = None,
    source_inbox_msg: Optional[str] = None,
    artifact_url: Optional[str] = None,
    completion_note: Optional[str] = None,
    source_session_id: Optional[str] = None,
) -> _NodeFields:
    """Build a backlog node dict shared by ``cmd_add`` and ``cmd_idea``.

    ``out``, when given, receives metadata ABOUT the capture that is not itself
    a node field (currently ``source_node_dropped``). A separate channel rather
    than a transient key on the returned dict, so a caller that does not know to
    strip it cannot persist it into the graph.

    Centralizes the field set so a schema addition (e.g. a new graph
    field) shows up in every entry-creating verb at once. ``difficulty`` is
    required so every caller makes the routing decision explicitly instead of
    silently minting an unroutable node. ``difficulty_source`` names the writer
    in the history entry. The returned dict has no ``id`` - the caller assigns
    one inside its locked mutator so duplicate-ID checks happen against the
    live snapshot.
    """
    from fno.graph._constants import ID_PREFIX  # noqa: F401

    # Chokepoint validation: the vocabulary is enforced where the field is
    # written, so a new writer cannot mint an out-of-vocabulary value even if
    # it skips its own CLI-level check.
    validate_source_kind(source_kind)

    # Parent-edge provenance (x-30f6): stamped from the running session's env +
    # manifest, or from an explicit --source-node. Centralized here so
    # every creator verb (add/idea/decompose) self-describes its origin.
    prov = _session_provenance(source_node=source_node, known_ids=known_ids)
    if out is not None:
        out["source_node_dropped"] = prov["source_node_dropped"]
    return {
        "id": None,  # caller fills inside locked mutator
        "parent": parent,
        "tags": list(tags or []),
        "title": title,
        "type": type_,
        "project": project,
        "cwd": cwd,
        "priority": priority,
        "blocks_everything": blocks_everything,
        "difficulty": difficulty,
        "difficulty_history": (
            [{"value": difficulty, "source": difficulty_source, "ts": datetime.now(timezone.utc).isoformat()}]
            if difficulty is not None
            else []
        ),
        "domain": domain,
        "blocked_by": list(blocked_by or []),
        "session_id": None,
        "locked_at": None,
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
        "source": source,
        "source_kind": source_kind,
        "source_project": source_project,
        "source_inbox_msg": source_inbox_msg,
        "artifact_url": artifact_url,
        "completion_note": completion_note,
        "source_session_id": source_session_id or prov["source_session_id"],
        "source_harness": prov["source_harness"],
        "source_cwd": prov["source_cwd"],
        "source_node_id": prov["source_node_id"],
        "source_plan_path": prov["source_plan_path"],
    }


def cmd_new(
    title: str = typer.Argument(..., help="Title of the new entry"),
    domain: str = typer.Option("code", "--domain", help="Domain (fuzzy-suggested against history)"),
    project: Optional[str] = typer.Option(
        None,
        "--project",
        help="Project name. Defaults to current git repo's basename; pass --unscoped to skip auto-scope.",
    ),
    priority: str = typer.Option("p2", "--priority", help="p0|p1|p2|p3"),
    difficulty: str = typer.Option(
        "medium", "--difficulty", help="Intrinsic work difficulty: low|medium|high."
    ),
    blocks_everything: bool = typer.Option(
        False, "--blocks-everything", help="Acknowledge that p0 blocks all downstream work."
    ),
    unscoped: bool = typer.Option(
        False,
        "--unscoped",
        help="Create with project=null and cwd=null. Default auto-scopes to current git repo.",
    ),
    force_domain: bool = typer.Option(
        False,
        "--force-domain",
        help="Skip the fuzzy domain suggestion and use --domain verbatim.",
    ),
    source_kind: str = typer.Option(
        "organic",
        "--source-kind",
        help="organic|from_inbox|from_observation|from_supervisor|operator_request",
    ),
    source_project: Optional[str] = typer.Option(
        None, "--source-project", help="Source project name"
    ),
    source_session_id: Optional[str] = typer.Option(
        None, "--source-session-id", help="Source session ID"
    ),
    source_inbox_msg: Optional[str] = typer.Option(
        None, "--source-inbox-msg", help="Source inbox message ID"
    ),
) -> None:
    """Create a new graph entry without a plan file.

    Auto-scopes project and cwd from the current git repo by default. Pass
    --unscoped to opt out (e.g. for cross-project ideas with no clear home).
    --project always overrides the auto-detected name when both are present.
    """
    from fno.graph.cli import (  # noqa: F401 - cycle-safe: call-time only
        _graph_path,
        _refuse_create_on_external_backend,
        _safe_stderr_warn,
        _validate_priority_or_exit,
    )
    _refuse_create_on_external_backend()
    from fno.graph._constants import (
        mint_node_id,
        normalize_difficulty,
    )
    from fno.graph.fuzzy import suggest_domain
    from fno.graph.store import read_graph, locked_mutate_graph

    try:
        validate_source_kind(source_kind)
    except ValueError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=1)

    _validate_priority_or_exit(priority, blocks_everything=blocks_everything)
    try:
        normalized_difficulty = normalize_difficulty(difficulty)
        if normalized_difficulty is None:
            raise ValueError("difficulty must not be empty")
        difficulty = normalized_difficulty
    except ValueError as exc:
        typer.echo(f"Error: {exc}", err=True)
        raise typer.Exit(code=2)

    entries = read_graph(_graph_path())

    if not force_domain:
        sugg = suggest_domain(domain, entries)
        if sugg.confidence == "fuzzy" and sugg.match != domain:
            typer.echo(
                f"fno backlog new: did you mean --domain {sugg.match}? "
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
        live_ids = {e.get("id") for e in es}
        new_id = mint_node_id(live_ids)
        new_id_holder[0] = new_id
        node = _build_backlog_node(
            title=title,
            project=resolved_project,
            cwd=resolved_cwd,
            priority=priority,
            blocks_everything=blocks_everything,
            difficulty=difficulty,
            domain=domain,
            source_kind=source_kind,
            source="fno-new",
            source_project=source_project,
            source_inbox_msg=source_inbox_msg,
            source_session_id=source_session_id,
            known_ids=live_ids,
        )
        node["id"] = new_id
        es.append(node)
        return es

    locked_mutate_graph(_graph_path(), mutator)

    # Filing-time dedup net (plan x-6ac7): `fno backlog new` is a reachable plan-less
    # birth path with its own mutator, so it gets the same post-write warn as
    # idea/add/intake (codex P2). Non-fatal; this verb's stdout is the bare id,
    # not JSON, so the stderr receipt cannot corrupt a machine-readable payload.
    if new_id_holder[0] is not None:
        try:
            from fno.graph._intake import _find_node, _warn_similar_nodes

            post_entries = read_graph(_graph_path())
            node = _find_node(post_entries, new_id_holder[0] or "")
            if node is not None:
                _warn_similar_nodes(node, post_entries, intake_hint=False)
        except Exception as e:  # noqa: BLE001 - dedup never breaks a filing
            _safe_stderr_warn(f"warning: post-file dedup check skipped: {e}\n")

    typer.echo(new_id_holder[0])


def register(app: "typer.Typer") -> None:
    """Mount ``cmd_new`` on the backlog app (single command, no sub-app)."""
    app.command(
        "new",
        hidden=True,
        epilog="Paired verb: `fno backlog remove <id>` deletes it.",
    )(cmd_new)

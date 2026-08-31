"""`fno whoami ledger <node|pr|session>` - the ledger's three-direction reader.

The join the operator wants (which harness session shipped which PR for which
node) has been recorded in ledger.json for a long time, but reading it lived as
hand-written Python against raw JSON and nobody ran it. One argument resolves
in three directions with no flag: all digits (or ``#``-prefixed, or a github
PR url) reads as a PR number, the node-id shape reads as a graph node, and
anything else reads as a session id.
"""

from __future__ import annotations

import re
from pathlib import Path

import typer

from fno import paths as _paths
from fno.cost._register import LEDGER_SESSION_UNRESOLVED
from fno.ledger_join import _entry_owns_pr
from fno.scoreboard.fold import load_ledger_rows, read_graph_nodes

_NODE_ID_SHAPE = re.compile(r"^[a-z][a-z0-9]{0,7}-[0-9a-f]{4,8}$")
_PR_URL_SHAPE = re.compile(r"github\.com/[^/]+/[^/]+/pull/(\d+)")


def _resume_command(harness: object, session_id: str) -> str | None:
    """The harness-DECLARED interactive resume form, or None.

    The capability table is the single source; ``codex resume {session_id}``
    and ``claude --resume {session_id}`` both come from there, never from a
    hardcoded form here.
    """
    if not isinstance(harness, str) or not harness:
        return None
    from fno.agents.harness_map import _HARNESS_CAPS

    strategy = (_HARNESS_CAPS.get(harness) or {}).get("resume_strategy") or {}
    form = (strategy.get("forms") or {}).get("interactive_resume") or {}
    tokens = form.get("tokens") or []
    if not tokens:
        return None
    return " ".join(t.replace("{session_id}", session_id) for t in tokens)


def _print_row(row: dict, gnode: dict | None) -> None:
    node = row.get("graph_node_id")
    pr = row.get("pr_number")
    typer.echo(f"node:     {node or '-'}")
    typer.echo(f"pr:       {'#' + str(pr) if pr else '-'}  {row.get('pr_url') or ''}".rstrip())
    # A backstop row never learned the plan path; the graph node did.
    plan_path = row.get("plan_path") or (gnode or {}).get("plan_path")
    if plan_path:
        typer.echo(f"plan:     {plan_path}")
    if row.get("root_path"):
        typer.echo(f"worktree: {row['root_path']}")
    typer.echo(f"status:   {row.get('status') or '-'}  ({row.get('completed') or '-'})")

    harnesses = {
        s["session_id"]: s["harness"]
        for s in (gnode or {}).get("sessions", []) or []
        if isinstance(s, dict) and s.get("session_id") and s.get("harness")
    }
    sessions = row.get("sessions")
    if not isinstance(sessions, list) or not sessions:
        # A legacy row predating the field. Never a blank: an absent key and a
        # recorded "no session" are different facts.
        typer.echo("session:  (absent - this row predates session recording)")
        return
    for sid in sessions:
        typer.echo(f"session:  {sid}")
        if sid == LEDGER_SESSION_UNRESOLVED:
            typer.echo("          no resume handle was recorded for this run")
            continue
        harness = harnesses.get(sid)
        cmd = _resume_command(harness, sid)
        if harness:
            typer.echo(f"          harness: {harness}")
        else:
            typer.echo("          harness unrecorded; no resume command inferred")
        if cmd:
            typer.echo(f"resume:   {cmd}")
        elif harness:
            typer.echo(f"resume:   (none - {harness} declares no interactive resume form)")


def ledger_show_command(arg: str) -> None:
    """Resolve one node id, PR number, or session id to its ledger row(s)."""
    try:
        rows = load_ledger_rows(_paths.ledger_json())
    except Exception as exc:  # noqa: BLE001 - a broken ledger is the headline
        typer.echo(f"ledger unreadable: {exc}", err=True)
        raise typer.Exit(code=1)

    digits = arg[1:] if arg.startswith("#") else arg
    url_pr = _PR_URL_SHAPE.search(arg)
    tried: list[str] = []
    slug: str | None = None
    if url_pr or digits.isdigit():
        pr = int(url_pr.group(1)) if url_pr else int(digits)
        tried.append(f"PR #{pr}")
        # The ledger is GLOBAL and PR numbers collide across repos, so a bare
        # number attributes nothing without this repo's slug.
        from fno.graph._reconcile import resolve_current_repo_slug

        slug = resolve_current_repo_slug(str(Path.cwd()))
        matches = [
            r
            for r in rows
            if r.get("pr_number") == pr
            and _entry_owns_pr(r, pr, (slug or "").lower())
        ]
    elif _NODE_ID_SHAPE.match(arg):
        tried.append(f"node {arg}")
        matches = [r for r in rows if r.get("graph_node_id") == arg]
    else:
        tried.append(f"session {arg}")
        matches = [
            r
            for r in rows
            if arg in (r.get("sessions") or [])
            or arg in {r.get("session_id"), r.get("fno_id")}
        ]

    if not matches:
        suffix = (
            " (repo slug unresolved; PR numbers collide across repos)"
            if slug is None and tried[0].startswith("PR")
            else ""
        )
        typer.echo(
            f"no ledger row for {arg!r}; tried {', '.join(tried)}{suffix}",
            err=True,
        )
        raise typer.Exit(code=1)

    # One graph read for the whole result, not one per matched row.
    nodes = {
        n.get("id"): n
        for n in read_graph_nodes(_paths.graph_json())
        if isinstance(n, dict) and n.get("id")
    }
    for i, row in enumerate(matches):
        if i:
            typer.echo("---")
        _print_row(row, nodes.get(row.get("graph_node_id")))

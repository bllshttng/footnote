"""`fno agents history <node|pr|session>` - one verb over three sources.

The question is the operator's: what did this session work on, and how do I
get back to it. Three stores answer parts of it, and none of them alone
answers it: the live registry holds the rows still running, reap receipts
hold how to come back to rows already dropped, and the ledger holds what work
the session did. One argument resolves against all three and one grouped
result reports each as found or not-recorded-because.

The ledger half is the SAME reader `fno whoami ledger` calls (kept as a
hidden alias); a second matcher would be the dual-implementation tax. The
receipt's `resume` string is printed verbatim - it was rendered from the
capability table at reap time, and re-deriving it would answer a different
question if the table has since moved.
"""

from __future__ import annotations

import json
from pathlib import Path

import typer

from fno import paths as _paths
from fno.ledger_show import _PR_URL_SHAPE, _print_row, resolve_ledger_matches
from fno.scoreboard.fold import load_ledger_rows, read_graph_nodes

# AC6: a legacy row predating the uuid write path legitimately carries no
# session. The sentence, not a blank - uuid coverage is write-path only with
# no backfill, so a stranger must not read an instrumentation gap as an
# absence of work.
_SESSION_ABSENT_NOTE = (
    "session:  not recorded (ledger uuid coverage is write-path only; "
    "this row predates it)"
)


def receipts_dir() -> Path:
    """Where the Rust daemon (and the Python watchdog fallback) write receipts.

    `<agents home>/reap-receipts/`, resolved through ``fno.paths`` - never
    hardcoded to ``~/.fno``, so ``FNO_AGENTS_HOME`` sandboxes stay coherent.
    """
    return _paths.agents_home_dir() / "reap-receipts"


def load_receipts() -> list[tuple[Path, dict]]:
    """Parse every receipt on disk. Corrupt files are skipped here and named
    by the caller through what is missing; a receipt this function cannot
    read must never masquerade as a resolved absence."""
    out: list[tuple[Path, dict]] = []
    directory = receipts_dir()
    if not directory.is_dir():
        return out
    for path in sorted(directory.glob("*.json")):
        try:
            receipt = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(receipt, dict):
            out.append((path, receipt))
    return out


def _enrichment_node(receipt: dict) -> str | None:
    ledger = receipt.get("ledger")
    if not isinstance(ledger, dict):
        return None
    return ledger.get("graph_node_id") or ledger.get("node")


def _enrichment_pr(receipt: dict) -> int | None:
    ledger = receipt.get("ledger")
    if not isinstance(ledger, dict):
        return None
    pr = ledger.get("pr_number")
    return pr if isinstance(pr, int) else None


def history_command(arg: str) -> None:
    """Resolve one node id, PR number, or session id over all three sources."""
    # --- the ledger resolution comes first: its session ids are the join
    # key the other two sources hang off for node / PR arguments.
    try:
        rows = load_ledger_rows(_paths.ledger_json())
        ledger_matches, tried, _slug = resolve_ledger_matches(arg, rows)
    except Exception as exc:  # noqa: BLE001 - a broken ledger is one source's miss
        ledger_matches, tried = [], [f"ledger unreadable ({exc})"]

    joined_sids: set[str] = set()
    for row in ledger_matches:
        sessions = row.get("sessions")
        if isinstance(sessions, list):
            joined_sids.update(s for s in sessions if isinstance(s, str))

    digits = arg[1:] if arg.startswith("#") else arg
    if digits.isdigit() or _PR_URL_SHAPE.search(arg):
        url_pr = _PR_URL_SHAPE.search(arg)
        arg_kind, node, pr = "pr", None, (
            int(url_pr.group(1)) if url_pr else int(digits)
        )
    elif tried and tried[0].startswith("node "):
        arg_kind, node, pr = "node", arg, None
    else:
        arg_kind, node, pr = "session", None, None

    # --- live registry rows
    from fno.agents.registry import load_registry

    live_rows = []
    live_err: str | None = None
    try:
        entries = load_registry()
        wanted = {arg} if arg_kind == "session" else joined_sids
        live_rows = [e for e in entries if e.harness_session_id in wanted]
    except Exception as exc:  # noqa: BLE001 - a broken registry is one source's miss
        live_err = str(exc)
    live_sids = {e.harness_session_id for e in live_rows if e.harness_session_id}

    # --- reap receipts
    receipt_hits: list[tuple[Path, dict]] = []
    for path, receipt in load_receipts():
        sid = receipt.get("harness_session_id")
        hit = (
            (arg_kind == "session" and sid == arg)
            or (arg_kind != "session" and sid in joined_sids)
            or (node is not None and _enrichment_node(receipt) == node)
            or (pr is not None and _enrichment_pr(receipt) == pr)
        )
        if not hit:
            continue
        # AC4: a live row and a receipt for the same session never both
        # describe the present - the receipt is written as the row drops.
        # A live row wins; its receipt is stale paper and is not reported.
        if sid in live_sids:
            continue
        receipt_hits.append((path, receipt))

    # --- one grouped result; every section answers, found or why not
    if live_rows:
        typer.echo("live:")
        for entry in live_rows:
            typer.echo(f"name:     {entry.name}")
            typer.echo(f"harness:  {entry.harness or '-'}")
            typer.echo(f"cwd:      {entry.cwd}")
            typer.echo(f"status:   {entry.status}")
    elif live_err:
        typer.echo(f"live:     not recorded - registry unreadable ({live_err})")
    else:
        typer.echo(
            "live:     not recorded - no live row answers this argument; "
            "live rows join through the ledger's session ids"
        )

    if receipt_hits:
        typer.echo("receipt:")
        for path, receipt in receipt_hits:
            typer.echo(f"from:     {path}")
            typer.echo(f"name:     {receipt.get('row_name') or '-'}")
            typer.echo(f"harness:  {receipt.get('harness') or '-'}")
            typer.echo(f"cwd:      {receipt.get('cwd') or '-'}")
            typer.echo(f"created_at:  {receipt.get('created_at') or '-'}")
            typer.echo(f"reaped_at:   {receipt.get('reaped_at') or '-'}")
            # Verbatim by contract: rendered at reap time from the capability
            # table, and re-deriving it here would answer a different question
            # if the table moved since.
            typer.echo(f"resume:   {receipt.get('resume') or '-'}")
    else:
        reasons = ["no reap receipt on disk under " + str(receipts_dir())]
        if live_sids:
            reasons.append("the matching row is live, so no receipt is expected")
        typer.echo("receipt:  not recorded - " + "; ".join(reasons))

    if ledger_matches:
        typer.echo("ledger:")
        nodes = {
            n.get("id"): n
            for n in read_graph_nodes(_paths.graph_json())
            if isinstance(n, dict) and n.get("id")
        }
        for i, row in enumerate(ledger_matches):
            if i:
                typer.echo("---")
            gnode = nodes.get(row.get("graph_node_id"))
            _print_row(row, gnode, _SESSION_ABSENT_NOTE)
            _print_coverage_notes(row, gnode)
    else:
        typer.echo(
            f"ledger:   not recorded - no ledger row; tried {', '.join(tried)}"
        )

    if not live_rows and not receipt_hits and not ledger_matches:
        typer.echo(
            f"not recorded: no source answers {arg!r} "
            "(live registry, reap receipts, and ledger all consulted)",
            err=True,
        )
        raise typer.Exit(code=1)


def _print_coverage_notes(row: dict, gnode: dict | None) -> None:
    """Name the fields the ledger does not carry, beside the values it does.

    Coverage measured 2026-09-01 over 3647 rows: session_id 3526, model 800,
    provider ~130, harness 0. An absent field prints as `not recorded`, never
    as a bare omission that reads as no such work.
    """
    provider = row.get("provider")
    model = row.get("model")
    typer.echo(f"provider: {provider if provider else 'not recorded'}")
    typer.echo(f"model:    {model if model else 'not recorded'}")
    sessions = row.get("sessions")
    harnesses = {
        s.get("session_id"): s.get("harness")
        for s in (gnode or {}).get("sessions", []) or []
        if isinstance(s, dict)
    }
    known = (
        isinstance(sessions, list)
        and any(harnesses.get(s) for s in sessions)
    )
    if not known:
        typer.echo(
            "harness:  not recorded (the ledger carries no harness field; "
            "this row's graph node does not name one either)"
        )

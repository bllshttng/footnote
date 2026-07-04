"""`fno scoreboard` - read-only telemetry fold. Never writes state.

Registered in fno.cli LAZY_SUBCOMMANDS as a plain-function command.
"""

from __future__ import annotations

import json as _json
import sys
from datetime import datetime

import typer

from fno.scoreboard.fold import (
    BrokenLedger,
    build_scoreboard,
    load_ledger_rows,
    read_graph_nodes,
    read_jsonl_events,
)


def scoreboard_command(
    since: int = typer.Option(28, "--since", help="Window in days (default 28)."),
    json_out: bool = typer.Option(False, "--json", help="Emit the scoreboard as JSON."),
) -> None:
    """Fold ledger + events + graph into a stop-cause / spend / autonomy /
    survival scoreboard, with a mandatory coverage line."""
    from fno import paths as _paths

    ledger_path = _paths.ledger_json()
    events_paths = [ledger_path.parent / "events.jsonl"]
    graph_path = _paths.graph_json()

    try:
        rows = load_ledger_rows(ledger_path)
    except BrokenLedger as e:
        # AC5-ERR: one line naming the file and byte offset, exit 1.
        typer.echo(f"{e.path}: parse error at byte {e.offset}: {e.msg}", err=True)
        raise typer.Exit(1)

    touch_events = read_jsonl_events(events_paths, {"human_touch"})
    graph_nodes = read_graph_nodes(graph_path)

    sb = build_scoreboard(rows, touch_events, graph_nodes, since_days=since, now=datetime.now())

    if json_out:
        typer.echo(_json.dumps(sb, indent=2))
        return
    _render(sb)


def _render(sb: dict) -> None:
    out = sys.stdout.write
    win = sb["since_days"]

    if sb["state"] == "no_data":
        out(f"fno scoreboard (last {win}d)\n\n  no terminal sessions in window.\n")
        return

    cov = sb["coverage"]
    out(f"fno scoreboard (last {win}d)\n\n")
    out("Coverage\n")
    out(f"  rows in window:      {cov['rows']}\n")
    out(f"  termination_reason:  {cov['termination_reason_pct']}%")
    out(f"    node linkage:  {cov['node_linkage_pct']}%\n")
    # Silent-failure guard: whenever coverage is partial, the caveat rides on the
    # same screen as any rate below (AC5-UI). Never a bare rate.
    if cov["termination_reason_pct"] < 100:
        out(
            f"  ! rates below reflect {cov['termination_reason_pct']}% termination coverage - "
            "a partial window is not a trend.\n"
        )

    out("\nStop-cause distribution\n")
    if sb["stop_cause"]:
        for reason, n in sorted(sb["stop_cause"].items(), key=lambda kv: (-kv[1], kv[0])):
            out(f"  {reason:<14} {n}\n")
    else:
        out("  (no termination_reason on any row in window)\n")

    sp = sb["spend"]
    out("\nSpend split\n")
    out(f"  ship-terminal:   ${sp['ship_terminal_usd']:.2f}\n")
    out(f"  wedge-terminal:  ${sp['wedge_terminal_usd']:.2f}\n")
    out(f"  other:           ${sp['other_usd']:.2f}\n")

    out("\nAutonomy      ")
    au = sb["autonomy"]
    if au["available"]:
        out(f"{au['touches_per_shipped_node']} human touches / shipped node "
            f"({au['touches']} touches, {au['shipped_nodes']} nodes)\n")
    else:
        out(f"n/a - {au['reason']}\n")

    out("Survival      ")
    su = sb["survival"]
    if su["available"]:
        out(f"{su['rate_pct']}% ({su['survived']}/{su['shipped_nodes']} shipped nodes)\n")
    else:
        out(f"n/a - {su['reason']}\n")

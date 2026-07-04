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
    build_calibration,
    build_scoreboard,
    build_skill_scoreboard,
    load_ledger_rows,
    read_graph_nodes,
    read_jsonl_events,
)


def scoreboard_command(
    since: int = typer.Option(28, "--since", help="Window in days (default 28)."),
    json_out: bool = typer.Option(False, "--json", "-J", help="Emit the scoreboard as JSON."),
    calibration: bool = typer.Option(
        False,
        "--calibration",
        help=(
            "Verifier calibration: join verifier_verdict events to per-node "
            "outcomes (merged_clean/bounced/reverted) and print the confusion "
            "table. All-time (ignores --since); gated on >=10 verdicts."
        ),
    ),
    by_skill: bool = typer.Option(
        False,
        "--by-skill",
        help=(
            "Skill-outcome attribution: which skill (+version) ran in which "
            "session, joined to runs/ship-rate/revert-rate/touches/cost per "
            "skill+version, with a coverage line for how many runs attributed."
        ),
    ),
) -> None:
    """Fold ledger + events + graph into a stop-cause / spend / autonomy /
    survival scoreboard, with a mandatory coverage line."""
    if since < 1:
        raise typer.BadParameter("--since must be at least 1 (days).")
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

    if calibration:
        cal = build_calibration(
            read_jsonl_events(events_paths, {"verifier_verdict"}),
            rows,
            read_graph_nodes(graph_path),
        )
        if json_out:
            typer.echo(_json.dumps(cal, indent=2))
            return
        _render_calibration(cal)
        return

    if by_skill:
        sb = build_skill_scoreboard(
            rows,
            read_graph_nodes(graph_path),
            read_jsonl_events(events_paths, {"human_touch"}),
            since_days=since,
            now=datetime.now(),
        )
        if json_out:
            typer.echo(_json.dumps(sb, indent=2))
            return
        _render_by_skill(sb)
        return

    touch_events = read_jsonl_events(events_paths, {"human_touch"})
    graph_nodes = read_graph_nodes(graph_path)

    # Naive LOCAL throughout: the ledger's `completed` is written naive-local, so
    # `now` matches it; aware event timestamps are converted to local in
    # fold._parse_ts. One timeline, no local/UTC boundary skew.
    sb = build_scoreboard(rows, touch_events, graph_nodes, since_days=since, now=datetime.now())

    if json_out:
        typer.echo(_json.dumps(sb, indent=2))
        return
    _render(sb)


def _render_calibration(cal: dict) -> None:
    out = sys.stdout.write
    out("fno scoreboard --calibration\n\n")
    excluded = cal.get("excluded") or {}
    excl_bits = [f"{n} {k}" for k, n in sorted(excluded.items())]
    if cal.get("unattributed"):
        excl_bits.append(f"{cal['unattributed']} unattributed")
    excl_line = f" (excluded: {', '.join(excl_bits)})" if excl_bits else ""

    if cal["state"] == "insufficient":
        out(
            f"  {cal['n']} verdicts so far, need >={cal['need']} for "
            f"calibration.{excl_line}\n"
        )
        return

    out(f"  N={cal['n']} verdicts{excl_line}\n")
    if cal.get("untimed_outcomes"):
        out(
            f"  ! {cal['untimed_outcomes']} node(s) lack a timestamped ship row; "
            f"their outcomes are conservative (any caused_by fix counts as bounced).\n"
        )
    out("\n")
    outcomes = ("merged_clean", "bounced", "reverted")
    out(f"  {'':<10}" + "".join(f"{o:>14}" for o in outcomes) + "\n")
    for verdict in ("pass", "concerns", "fail"):
        row = cal["table"][verdict]
        out(f"  {verdict:<10}" + "".join(f"{row[o]:>14}" for o in outcomes) + "\n")
    fp = cal["false_positive"]
    out(
        f"\n  false-positive (pass -> bounced/reverted): "
        f"{fp['count']}/{fp['of_pass']} ({fp['rate_pct']}%)\n"
    )


def _render_by_skill(sb: dict) -> None:
    out = sys.stdout.write
    win = sb["since_days"]
    if sb["state"] == "no_data":
        out(f"fno scoreboard --by-skill (last {win}d)\n\n  no terminal sessions in window.\n")
        return

    cov = sb["coverage"]
    out(f"fno scoreboard --by-skill (last {win}d)\n\n")
    out("Coverage\n")
    out(f"  rows in window:      {cov['rows']}\n")
    out(f"  attributed:          {cov['attributed_pct']}%\n")
    if cov["attributed_pct"] < 100:
        out(
            f"  ! rows below reflect {cov['attributed_pct']}% attribution coverage - "
            "unattributed rows are listed, never dropped.\n"
        )
    out("\n")
    out(f"  {'skill':<32}{'version':<10}{'runs':>6}{'ship%':>7}{'revert%':>9}{'touch/run':>11}{'cost/run':>10}  method\n")
    for row in sb["rows"]:
        out(
            f"  {row['skill']:<32}{row['version']:<10}{row['runs']:>6}"
            f"{row['ship_rate_pct']:>6}%{row['revert_rate_pct']:>8}%"
            f"{row['touches_per_run']:>11}{row['cost_per_run']:>10.2f}  {row['method']}\n"
        )


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
    # Silent-failure guard: whenever coverage is partial on EITHER axis, the
    # caveat rides on the same screen as any rate below (AC5-UI). Stop-cause/spend
    # lean on termination_reason; autonomy/survival lean on node linkage - a gap in
    # either can bias a rate, so both gate the caveat. Never a bare rate.
    if cov["termination_reason_pct"] < 100 or cov["node_linkage_pct"] < 100:
        out(
            f"  ! rates below reflect {cov['termination_reason_pct']}% termination / "
            f"{cov['node_linkage_pct']}% node-linkage coverage - a partial window is not a trend.\n"
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

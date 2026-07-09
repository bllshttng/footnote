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
    build_efficiency,
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
    efficiency: bool = typer.Option(
        False,
        "--efficiency",
        help=(
            "Session-efficiency graders: per-row loop_check fires and CI-red "
            "episodes joined from recorded telemetry, aggregated into "
            "per-outcome-class costs and median/p90 distributions. Grades the "
            "process, not just the terminal state."
        ),
    ),
) -> None:
    """Fold ledger + events + graph into a stop-cause / spend / autonomy /
    survival scoreboard, with a mandatory coverage line."""
    if since < 1:
        raise typer.BadParameter("--since must be at least 1 (days).")
    # The view flags are mutually exclusive: each renders a different fold.
    _views = [f for f, on in (("--calibration", calibration), ("--by-skill", by_skill), ("--efficiency", efficiency)) if on]
    if len(_views) > 1:
        raise typer.BadParameter(f"{' and '.join(_views)} are mutually exclusive views; pick one.")
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

    if efficiency:
        eff = build_efficiency(
            rows,
            read_jsonl_events(events_paths, {"loop_check"}),
            read_graph_nodes(graph_path),
            since_days=since,
            now=datetime.now(),
        )
        if json_out:
            typer.echo(_json.dumps(eff, indent=2))
            return
        _render_efficiency(eff)
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


def _fmt(v) -> str:
    """Render a None-able metric: 'n/a' when unmeasurable (never a fake 0), a
    plain integer when whole (no sci-notation for million-scale token counts),
    else one decimal."""
    if v is None:
        return "n/a"
    if isinstance(v, float) and v.is_integer():
        v = int(v)
    return str(v) if isinstance(v, int) else f"{v:.1f}"


def _render_efficiency(eff: dict) -> None:
    out = sys.stdout.write
    win = eff["since_days"]
    if eff["state"] == "no_data":
        out(f"fno scoreboard --efficiency (last {win}d)\n\n  no terminal sessions in window.\n")
        return

    cov = eff["coverage"]
    out(f"fno scoreboard --efficiency (last {win}d)\n\n")
    out("Coverage\n")
    out(f"  rows in window:      {cov['rows']}\n")
    out(f"  loop-check join:     {cov['loop_join_pct']}%")
    out(f"    transcript:  {cov['transcript_pct']}%")
    out(f"    node linkage:  {cov['node_linkage_pct']}%\n")
    out(f"  outcome tracked:     {cov['outcome_tracked_pct']}% of shipped rows\n")
    if cov["loop_join_pct"] < 100 or cov["node_linkage_pct"] < 100:
        out(
            f"  ! metrics below reflect {cov['loop_join_pct']}% loop-join / "
            f"{cov['node_linkage_pct']}% node-linkage coverage - a partial window is not a trend.\n"
        )
    if cov["ci_unparsed"]:
        out(
            f"  ! {cov['ci_unparsed']} loop_check fire(s) carried an unrecognized ci shape "
            "(emitter drift); their sessions' ci_reds are n/a, not counted as green.\n"
        )

    out("\nPer-outcome-class cost\n")
    out(f"  {'class':<20}{'n':>4}{'spend$':>10}{'med tok':>10}{'med fires':>11}{'med min':>9}\n")
    for cls, b in sorted(eff["per_outcome_class"].items(), key=lambda kv: (-kv[1]["n"], kv[0])):
        out(
            f"  {cls:<20}{b['n']:>4}{b['spend_usd']:>10.2f}"
            f"{_fmt(b['median_tokens']):>10}{_fmt(b['median_fires']):>11}{_fmt(b['median_duration_min']):>9}\n"
        )

    out("\nDistribution (rows with >=1 loop_check fire)\n")
    out(f"  {'metric':<18}{'median':>10}{'p90':>10}{'n':>6}\n")
    for metric in ("loop_fires", "ci_reds", "tokens_total", "duration_minutes"):
        d = eff["distribution"][metric]
        out(f"  {metric:<18}{_fmt(d['median']):>10}{_fmt(d['p90']):>10}{d['n']:>6}\n")


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
        revert = f"{row['revert_rate_pct']}%" if row["revert_rate_pct"] is not None else "n/a"
        out(
            f"  {row['skill']:<32}{row['version']:<10}{row['runs']:>6}"
            f"{row['ship_rate_pct']:>6}%{revert:>9}"
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

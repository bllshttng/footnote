"""`fno whoami scoreboard` - read-only telemetry fold. Never writes state.

Registered in fno.cli LAZY_SUBCOMMANDS as a plain-function command.
"""

from __future__ import annotations

import json as _json
import sys
from dataclasses import asdict
from datetime import datetime
from pathlib import Path

import typer

from fno.scoreboard.fold import (
    BrokenLedger,
    CONTEXT_TRACE_EVENT_KINDS,
    build_calibration,
    build_efficiency,
    build_lanes,
    build_plan_fidelity,
    build_provider_scoreboard,
    build_scoreboard,
    build_skill_scoreboard,
    classify_deliveries,
    emission_failures_snapshot,
    load_ledger_rows,
    read_graph_nodes,
    read_jsonl_events_with_coverage,
)
from fno import paths as _paths


def _delivery_event_paths(rows: list[dict], canonical_root: Path) -> list[Path]:
    paths: list[Path] = []
    canonical_roots = {canonical_root.resolve()}
    for row in rows:
        root_path = row.get("root_path")
        if isinstance(root_path, str) and root_path and Path(root_path).exists():
            paths.append(Path(root_path) / ".fno" / "events.jsonl")
        row_canonical = row.get("canonical_root_path")
        if isinstance(row_canonical, str) and row_canonical:
            canonical_roots.add(Path(row_canonical).expanduser().resolve())
    for root in sorted(canonical_roots):
        salvage_root = root / ".fno" / "salvage"
        if salvage_root.is_dir():
            paths.extend(sorted(salvage_root.glob("*/events.jsonl")))
    return paths


def _event_node_id(e: dict) -> str | None:
    raw = e.get("data")
    d = raw if isinstance(raw, dict) else {}
    nid = e.get("graph_node_id") or d.get("graph_node_id")
    return nid if isinstance(nid, str) and nid else None


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
    plan_fidelity: bool = typer.Option(
        False,
        "--plan-fidelity",
        help=(
            "Plan-fidelity graders: join each planning thread's plan doc to its "
            "delivery (PR diff + SUMMARY.md) and score AC-coverage, scope-drift, "
            "and data-model-surprise. Grades PLANNING quality; an unimplemented "
            "plan is reported `unjoined`, never scored 0%."
        ),
    ),
    by_provider: bool = typer.Option(
        False,
        "--by-provider",
        help=(
            "Provider-outcome attribution: shipped runs and delivered nodes "
            "per provider/model (wedge spend included, nodes counted once, "
            "shared credit shown), post-ship bounce rate, median iterations, "
            "and re-dispatch counts, with an unattributed bucket and a "
            "coverage line. Feeds quota-aware dispatch."
        ),
    ),
    lanes: bool = typer.Option(
        False,
        "--lanes",
        help="Lane truth: retrospective provider/model/effort cells plus live occupancy and headroom.",
    ),
    project: str = typer.Option(
        None,
        "--project",
        help=(
            "Scope every denominator to one project. Rows with no project stay "
            "unattributed - counted in the scope line, never copied in."
        ),
    ),
) -> None:
    """Fold ledger + events + graph into a stop-cause / spend / autonomy /
    survival scoreboard, with a mandatory coverage line."""
    if since < 1:
        raise typer.BadParameter("--since must be at least 1 (days).")
    # The view flags are mutually exclusive: each renders a different fold.
    _views = [
        f for f, on in (("--calibration", calibration), ("--by-skill", by_skill),
                        ("--efficiency", efficiency), ("--plan-fidelity", plan_fidelity),
                        ("--by-provider", by_provider), ("--lanes", lanes))
        if on
    ]
    if len(_views) > 1:
        raise typer.BadParameter(f"{' and '.join(_views)} are mutually exclusive views; pick one.")
    ledger_path = _paths.ledger_json()
    from fno.events import EPHEMERAL_SUFFIX  # lazy: keeps schema load off the help path

    events_paths = [  # ephemeral rows (human_touch) live in the sibling journal (x-add3)
        ledger_path.parent / "events.jsonl",
        ledger_path.parent / ("events.jsonl" + EPHEMERAL_SUFFIX),
    ]
    graph_path = _paths.graph_json()

    try:
        rows = load_ledger_rows(ledger_path)
    except BrokenLedger as e:
        # AC5-ERR: one line naming the file and byte offset, exit 1.
        typer.echo(f"{e.path}: parse error at byte {e.offset}: {e.msg}", err=True)
        raise typer.Exit(1)

    scope = None
    pnodes: set[str] = set()
    classified = None
    if project:
        classified = classify_deliveries(read_graph_nodes(graph_path), rows, project)
        if "scoped" not in classified:
            raise RuntimeError(f"classifier returned no scope; keys={sorted(classified)}")
        scoped = classified["scoped"]
        pnodes = set(scoped.get("node_ids") or [])
        rows = scoped.get("rows") or rows
        scope = (classified.get("coverage") or {}).get("project_scope")

        def _nodes():
            return scoped.get("entries") or []

        def _events(kinds):
            read = read_jsonl_events_with_coverage(events_paths, set(kinds))
            read["events"] = [e for e in read["events"] if _event_node_id(e) in pnodes]
            return read

    else:

        def _nodes():
            return read_graph_nodes(graph_path)

        def _events(kinds):
            return read_jsonl_events_with_coverage(events_paths, set(kinds))

    def _finish(view: dict, render) -> None:
        if scope:
            view["project_scope"] = scope
        if json_out:
            typer.echo(_json.dumps(view, indent=2))
        else:
            render(view)

    if calibration:
        verdict_read = _events({"verifier_verdict"})
        cal = build_calibration(
            verdict_read["events"],
            rows,
            _nodes(),
        )
        cal["event_coverage"] = verdict_read["coverage"]
        return _finish(cal, _render_calibration)

    if by_skill:
        touch_read = _events({"human_touch"})
        sb = build_skill_scoreboard(
            rows,
            _nodes(),
            touch_read["events"],
            since_days=since,
            now=datetime.now(),
        )
        sb["event_coverage"] = touch_read["coverage"]
        return _finish(sb, _render_by_skill)

    if efficiency:
        loop_read = _events({"loop_check"})
        eff = build_efficiency(
            rows,
            loop_read["events"],
            _nodes(),
            since_days=since,
            now=datetime.now(),
        )
        eff["event_coverage"] = loop_read["coverage"]
        return _finish(eff, _render_efficiency)

    if by_provider:
        pb = build_provider_scoreboard(
            rows,
            _nodes(),
            since_days=since,
            now=datetime.now(),
        )
        return _finish(pb, _render_by_provider)

    if lanes:
        from fno.agents.registry import load_registry
        from fno.config import load_settings, provider_limits_table

        settings = load_settings()
        rate_read = _events({"provider_rate_limited"})
        lane_view = build_lanes(
            rows,
            _nodes(),
            [asdict(row) for row in load_registry(path=_paths.agents_registry_path())],
            rate_read["events"],
            dict(provider_limits_table(settings.agents)),
            since_days=since,
            now=datetime.now(),
        )
        lane_view["event_coverage"] = rate_read["coverage"]
        return _finish(lane_view, _render_lanes)

    if plan_fidelity:
        trace_paths = [*events_paths, _paths.project_log("events.jsonl")]
        project_root = _paths.resolve_repo_root()
        canonical_root = _paths.resolve_canonical_worktree(project_root, timeout=2) or project_root
        trace_paths.extend(_delivery_event_paths(rows, canonical_root))
        trace_read = read_jsonl_events_with_coverage(
            trace_paths, CONTEXT_TRACE_EVENT_KINDS
        )
        trace_events = trace_read["events"]
        if project:
            trace_events = [e for e in trace_events if _event_node_id(e) in pnodes]
        pf = build_plan_fidelity(
            rows,
            _nodes(),
            since_days=since,
            now=datetime.now(),
            loop_check_events=[
                event for event in trace_events if event.get("type") == "loop_check"
            ],
            trace_events=trace_events,
            event_coverage=trace_read["coverage"],
        )
        return _finish(pf, _render_plan_fidelity)

    touch_read = _events({"human_touch"})
    graph_nodes = _nodes()

    # Naive LOCAL throughout: the ledger's `completed` is written naive-local, so
    # `now` matches it; aware event timestamps are converted to local in
    # fold._parse_ts. One timeline, no local/UTC boundary skew.
    sb = build_scoreboard(
        rows,
        touch_read["events"],
        graph_nodes,
        since_days=since,
        now=datetime.now(),
        classified=classified,
    )

    sb["event_coverage"] = touch_read["coverage"]
    sb["emission_failures"] = emission_failures_snapshot()
    return _finish(sb, _render)


def _render_calibration(cal: dict) -> None:
    out = sys.stdout.write
    out("fno whoami scoreboard --calibration\n\n")
    excluded = cal.get("excluded") or {}
    excl_bits = [f"{n} {k}" for k, n in sorted(excluded.items())]
    if cal.get("unattributed"):
        excl_bits.append(f"{cal['unattributed']} unattributed")
    excl_line = f" (excluded: {', '.join(excl_bits)})" if excl_bits else ""

    if cal["state"] == "insufficient":
        out(f"  {cal['n']} verdicts so far, need >={cal['need']} for calibration.{excl_line}\n")
        return

    out(f"  N={cal['n']} verdicts{excl_line}\n")
    if cal.get("untimed_outcomes"):
        out(f"  ! {cal['untimed_outcomes']} node(s) lack a timestamped ship row; their"
            " outcomes are conservative (any caused_by fix counts as bounced).\n")
    out("\n")
    outcomes = ("merged_clean", "bounced", "reverted")
    out(f"  {'':<10}" + "".join(f"{o:>14}" for o in outcomes) + "\n")
    for verdict in ("pass", "concerns", "fail"):
        row = cal["table"][verdict]
        out(f"  {verdict:<10}" + "".join(f"{row[o]:>14}" for o in outcomes) + "\n")
    fp = cal["false_positive"]
    out(f"\n  false-positive (pass -> bounced/reverted): {fp['count']}/{fp['of_pass']}"
        f" ({fp['rate_pct']}%)\n")


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
        out(f"fno whoami scoreboard --efficiency (last {win}d)\n\n  no terminal sessions in window.\n")
        return

    cov = eff["coverage"]
    out(f"fno whoami scoreboard --efficiency (last {win}d)\n\n")
    out("Coverage\n")
    out(f"  rows in window:      {cov['rows']}\n")
    out(f"  loop-check join:     {cov['loop_join_pct']}%")
    out(f"    transcript:  {cov['transcript_pct']}%")
    out(f"    node linkage:  {cov['node_linkage_pct']}%\n")
    out(f"  outcome tracked:     {cov['outcome_tracked_pct']}% of shipped rows\n")
    if cov["loop_join_pct"] < 100 or cov["node_linkage_pct"] < 100:
        out(f"  ! metrics below reflect {cov['loop_join_pct']}% loop-join /"
            f" {cov['node_linkage_pct']}% node-linkage: a partial window is not a trend.\n")
    if cov["ci_unparsed"]:
        out(f"  ! {cov['ci_unparsed']} loop_check fire(s) carried an unrecognized ci shape"
            " (emitter drift); their sessions' ci_reds are n/a, not counted as green.\n")

    out("\nPer-outcome-class cost\n")
    out(f"  {'class':<20}{'n':>4}{'spend$':>10}{'med tok':>10}{'med fires':>11}{'med min':>9}\n")
    for cls, b in sorted(eff["per_outcome_class"].items(), key=lambda kv: (-kv[1]["n"], kv[0])):
        out(
            f"  {cls:<20}{b['n']:>4}{b['spend_usd']:>10.2f}"
            f"{_fmt(b['median_tokens']):>10}{_fmt(b['median_fires']):>11}{_fmt(b['median_duration_min']):>9}\n"
        )

    pvb = eff.get("plan_vs_build_cost") or {}
    if pvb:
        out("\nPlan vs build cost per node\n")
        out(f"  {'node':<14}{'plan$':>10}{'build$':>10}\n")
        for nid, c in sorted(pvb.items(), key=lambda kv: -kv[1]["plan_usd"]):
            out(f"  {nid:<14}{c['plan_usd']:>10.2f}{c['build_usd']:>10.2f}\n")

    out("\nDistribution (rows with >=1 loop_check fire)\n")
    out(f"  {'metric':<18}{'median':>10}{'p90':>10}{'n':>6}\n")
    for metric in ("loop_fires", "ci_reds", "tokens_total", "duration_minutes"):
        d = eff["distribution"][metric]
        out(f"  {metric:<18}{_fmt(d['median']):>10}{_fmt(d['p90']):>10}{d['n']:>6}\n")


def _render_plan_fidelity(pf: dict) -> None:
    out = sys.stdout.write
    win = pf["since_days"]
    if pf["state"] == "no_data":
        out(f"fno whoami scoreboard --plan-fidelity (last {win}d)\n\n  no terminal sessions in window.\n")
        return
    cov = pf["coverage"]
    out(f"fno whoami scoreboard --plan-fidelity (last {win}d)\n\n")
    out(f"  {cov['planned_rows']} planned row(s), {cov['joined_pct']}% joined to a delivery.\n\n")
    for r in pf["results"]:
        if r["status"] == "unjoined":
            out(f"  {r.get('session_id') or '?':<24} unjoined (plan not yet delivered)\n")
            continue
        ac = r["ac_coverage"]
        ac_s = f"{ac['verified']}/{ac['total']} ({ac['pct']}%)" if ac else "n/a"
        dm = _fmt(r["data_model_surprise"])
        drift = len(r["scope_drift"]["unplanned"]) if r["scope_drift"] else "n/a"
        pr = r.get("probes")
        probes_s = f"{pr['passed']}/{pr['declared']}" if pr else "n/a"
        context = (r.get("context_outcome_trace") or {}).get("context")
        ctx_s = f"{context['bytes']}B" if context and context.get("bytes") is not None else "n/a"
        out(f"  {r.get('session_id') or '?':<24} PR#{r.get('pr_number') or '?'} "
            f"AC {ac_s} | drift {drift} | data-model-surprise {dm} | "
            f"deviations {_fmt(r['deviation_load'])} | probes {probes_s} | "
            f"context {ctx_s} | outcome {r.get('outcome') or 'n/a'}\n")
    comparison = pf.get("context_comparison") or {}
    out(f"\n  context comparison: {comparison.get('label', 'rejected')}"
        f" ({comparison.get('reason', comparison.get('claim', 'no contract'))})\n")


def _render_by_skill(sb: dict) -> None:
    out = sys.stdout.write
    win = sb["since_days"]
    if sb["state"] == "no_data":
        out(f"fno whoami scoreboard --by-skill (last {win}d)\n\n  no terminal sessions in window.\n")
        return

    cov = sb["coverage"]
    out(f"fno whoami scoreboard --by-skill (last {win}d)\n\n")
    out("Coverage\n")
    out(f"  rows in window:      {cov['rows']}\n")
    out(f"  attributed:          {cov['attributed_pct']}%\n")
    if cov["attributed_pct"] < 100:
        out(f"  ! rows below reflect {cov['attributed_pct']}% attribution coverage -"
            " unattributed rows are listed, never dropped.\n")
    out("\n")
    out(f"  {'skill':<32}{'version':<10}{'runs':>6}{'ship%':>7}{'revert%':>9}{'touch/run':>11}{'cost/run':>10}  method\n")
    for row in sb["rows"]:
        revert = f"{row['revert_rate_pct']}%" if row["revert_rate_pct"] is not None else "n/a"
        out(
            f"  {row['skill']:<32}{row['version']:<10}{row['runs']:>6}"
            f"{row['ship_rate_pct']:>6}%{revert:>9}"
            f"{row['touches_per_run']:>11}{row['cost_per_run']:>10.2f}  {row['method']}\n"
        )


def _render_by_provider(pb: dict) -> None:
    out = sys.stdout.write
    win = pb["since_days"]
    if pb["state"] == "no_data":
        out(f"fno whoami scoreboard --by-provider (last {win}d)\n\n  no terminal sessions in window.\n")
        return

    cov = pb["coverage"]
    out(f"fno whoami scoreboard --by-provider (last {win}d)\n\n")
    out("Coverage\n")
    out(f"  rows in window:      {cov['rows']} execution rows\n")
    out(f"  attributed:          {cov['attributed_pct']}%\n")
    if cov["attributed_pct"] < 100:
        out(f"  ! rows below reflect {cov['attributed_pct']}% provider attribution -"
            " unattributed rows are a visible bucket, never dropped.\n")
    out("\n")
    out(f"  {'provider':<16}{'model':<22}{'runs':>6}{'ships':>7}{'nodes':>7}{'shared':>8}{'spend$':>10}{'$/ship':>9}{'bounce%':>13}{'med iter':>10}{'retries':>9}\n")
    prev = None
    for row in pb["rows"]:
        provider = row["provider"] if row["provider"] != prev else ""
        prev = row["provider"]
        cps = f"{row['cost_per_shipped_usd']:.2f}" if row["cost_per_shipped_usd"] is not None else "n/a"
        # bounce rides with its denominator: "50% of 4" never a bare rate
        bounce = f"{row['bounce_rate_pct']}% of {row['shipped_linked']}" if row["bounce_rate_pct"] is not None else "n/a"
        out(
            f"  {provider:<16}{row['model']:<22}{row['runs']:>6}{row['shipped']:>7}"
            f"{row.get('delivered_nodes', 0):>7}{row.get('shared_nodes', 0):>8}"
            f"{row['spend_usd']:>10.2f}{cps:>9}{bounce:>13}{_fmt(row['median_iterations']):>10}{row['retry_rows']:>9}\n"
        )


def _render_lanes(view: dict) -> None:
    out = sys.stdout.write
    win = view["since_days"]
    out(f"fno whoami scoreboard --lanes (last {win}d)\n\n")
    cov = view["coverage"]
    out("Coverage\n")
    out(f"  rows in window:  {cov['rows']}\n")
    out(f"  provider:        {cov['provider']}/{cov['rows']}\n")
    out(f"  model:           {cov['model']}/{cov['rows']}\n")
    out(f"  effort:          {cov['effort']}/{cov['rows']}\n")
    if any(cov[axis] < cov["rows"] for axis in ("provider", "model", "effort")):
        out("  ! axis coverage is partial; missing values are not a model verdict.\n")

    out("\nRetrospective\n")
    if not view["retrospective"]:
        out("  no execution rows in window.\n")
    else:
        out("  provider          model                    effort size runs ok% wall-min carveouts sample\n")
        for row in view["retrospective"]:
            out(f"  {row['provider']:<16} {row['model']:<24} {row['effort']:<6} "
                f"{row['size']:<4} {row['runs']:>4} {row['ok_pct']:>3}% "
                f"{row['wall_minutes']:>8.1f} {row['carveouts_filed']:>9} {row['sample_state']}\n")

    out("\nLive\n")
    if not view["live"]:
        out("  no active lanes.\n")
    else:
        out("  provider          model                    effort occupancy cap headroom\n")
        for row in view["live"]:
            cap = row["cap"] if row["cap"] is not None else "n/a"
            headroom = row["headroom"] if row["headroom"] is not None else "n/a"
            out(f"  {row['provider']:<16} {row['model']:<24} {row['effort']:<6} "
                f"{row['occupancy']:>9} {cap:>3} {headroom:>8}\n")
    out(f"\n  provider_rate_limited events: {view['rate_limited']}\n")


def _render(sb: dict) -> None:
    out = sys.stdout.write
    win = sb["since_days"]

    if sb["state"] == "no_data":
        out(f"fno whoami scoreboard (last {win}d)\n\n  no terminal sessions in window.\n")
        return

    cov = sb["coverage"]
    out(f"fno whoami scoreboard (last {win}d)\n\n")
    out("Coverage\n")
    out(f"  rows in window:      {cov['rows']}\n")
    out(f"  termination_reason:  {cov['termination_reason_pct']}%")
    out(f"    node linkage:  {cov['node_linkage_pct']}%\n")
    # Silent-failure guard: whenever coverage is partial on EITHER axis, the caveat
    # rides on the same screen as any rate below (AC5-UI). Never a bare rate.
    if cov["termination_reason_pct"] < 100 or cov["node_linkage_pct"] < 100:
        out(f"  ! rates below reflect {cov['termination_reason_pct']}% termination /"
            f" {cov['node_linkage_pct']}% node-linkage: a partial window is not a trend.\n")

    # Journal integrity rides the same screen as any rate it could bias.
    ec = sb.get("event_coverage")
    if ec and not ec.get("complete", True):
        out(f"  ! event journals incomplete: {ec.get('malformed_lines', 0)} malformed line(s),"
            f" {ec.get('unreadable_paths', 0)} unreadable file(s); omissions, not zeros.\n")
    emit = sb.get("emission_failures")
    if emit and emit.get("available"):
        out(f"  ! touch emission failures: {emit.get('count')} since {emit.get('measured_since')}"
            f" (measured {emit.get('measured_at')}; server instance lifetime).\n")
    elif emit:
        out(f"  ! touch emission failures: unknown ({emit.get('reason') or 'server unreachable'}).\n")

    # x-b6bd: shipped is the merge; the terminal count rides beside it for one release.
    shipped = sb.get("shipped_nodes")
    if shipped is not None:
        by_term = sb.get("shipped_by_terminal", 0)
        classes = sb.get("delivery_classes") or {}
        bits = " ".join(f"{n} {name}" for name, n in sorted(classes.items()))
        no_row = sb.get("merged_nodes_without_ledger_row") or 0
        out(f"\nShipped       {shipped} nodes (confirmed merge, doc or delivery evidence);"
            f" by session terminal alone: {by_term}\n")
        if bits:
            out(f"              by evidence: {bits}\n")
        if no_row:
            out(f"              merged nodes with no ledger row: {no_row}\n")
        if shipped and by_term < 0.9 * shipped:
            out("  ! terminal-only undercounts nodes whose PR merged after the session stopped;"
                " the merge is the count.\n")
    scope = sb.get("project_scope")
    if scope:
        out(f"\nProject scope {scope['project']}: {scope['nodes']} nodes; "
            f"{scope['unattributed_rows']} unattributed, "
            f"{scope['other_project_rows']} other-project row(s) kept out.\n")

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
        pending = su.get("pending")
        suffix = f", {pending} pending observation" if pending else ""
        out(f"{su['rate_pct']}% ({su['survived']}/{su['shipped_nodes']} shipped nodes{suffix})\n")
    else:
        out(f"n/a - {su['reason']}\n")

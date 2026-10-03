//! `fno backlog reconcile`: close open backlog nodes whose PR has merged
//! outside the ship gate. The port of cli.py's cmd_reconcile/_reconcile_once
//! over the native library layer (drift_scan, binding, supersession,
//! closures, drift_emit). The external-backend shape and the auto-continue
//! dispatch still ride the Python wheel from inside this arm (the same
//! split the next/undispatched arms make); the advance port (PR9) deletes
//! the auto-continue round-trip.

use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::binding::bind_pr_rows;
use super::closures::{
    cascade_close_contained, strandable_contained_ids, strandable_epic_ids, strandable_orphan_ids,
    sweep_close_done_epics, sweep_close_stranded_contained, sweep_reparent_stranded_orphans,
    sweep_stamp_carried_sessions,
};
use super::drift_emit::{
    emit_gate_escape_for_record, emit_human_touch_for_record, emit_session_satisfied_for_record,
    write_retro_sentinel,
};
use super::drift_scan::{
    collect_open_binding_heals, detect_reverted_nodes, effective_reconcile_cwd, query_seam,
    scan_merge_drift, ListingCache, MergeDriftRecord, OpenBindingHeal,
};
use super::merge_state::fetch_pr_closure_context;
use super::promise::{resolve_promise_evidence, PromiseOutcome};
use super::supersession::{
    apply_edge_settlement, successors_owing_verification, summarize_edge_settlement,
    verify_pending_supersessions,
};
use crate::backlog::pr_link::{repo_slug_from_url, resolve_current_repo_slug};
use crate::backlog::workflows::{
    apply_completion_fields, cascade_close_parents, reopen_outranks_merge, reparent_live_children,
    rollup_from_ledger, stamp_and_graduate_plan,
};
use crate::backlog_ready::node_is_open;
use crate::graph_store::{self, read_rows, settle_blocked_by_edges};

const USAGE: &str = "usage: fno-agents backlog reconcile [--dry-run|-N] [--node ID] [--json|-J] [--pr-number N] [--repo owner/repo]";

/// The door owns the help now: the Python parser that rendered the option
/// surface is deleted with its verb. The epilog's reopen pointer survives
/// verbatim.
const HELP: &str = "\
fno-agents backlog reconcile
Close open backlog nodes whose PR has merged outside the ship gate.

The completion ritual (stamp plan -> mark node done -> capture follow-ups)
runs automatically only through /target's ship gate or
scripts/lib/pr-merge.sh. A PR merged any other way (manual GitHub merge,
bare gh pr merge) leaves the node open. This verb detects that drift and
closes it mechanically: mark done, best-effort stamp the plan, and drop a
retro sentinel so a later session captures follow-ups. It never
auto-creates inbox lines or backlog nodes, never auto-resumes work, and
never clobbers a node that is already done.

Side effect: also runs claim GC (reap_dead_claims), archiving dead
lockfiles under the claims store's .expired/. --dry-run propagates to it
(no archiving). This fires on every throttled auto-reconcile, including
the SessionStart hook - not just a manual invocation.

Options:
  --dry-run, -N    Report candidates only; mutate nothing (graph stays byte-identical).
  --node ID        Restrict the scan to a single node id (ab-XXXXXXXX).
  --json, -J       Emit structured JSON instead of a human summary.
  --pr-number N    Bind every node named in this merged PR's exact closure line (Fixes, or the retired Backlog-Closure: spelling) to the PR (filling an absent primary or appending to additional_prs) BEFORE the drift scan below runs, so a PR naming several nodes closes all of them in this one invocation rather than only the one node stamped at creation. All-or-nothing: an unknown, malformed, or cross-repo claim binds nothing.
  --repo OWNER/REPO  owner/repo scoping --pr-number's gh query and cross-repo claim check. Resolved from the checkout's origin remote when omitted.
  -h, --help       Show this message and exit.

Paired verb: `fno backlog reopen <id> --reason ...` reverses a close this
made. It refuses on a merged PR, which is what closed the node here, so an
intentional correction of an auto-close needs --force. Correction is
reopen, a verb the native binary serves after the python leg retired.
";

struct Args {
    dry_run: bool,
    node: Option<String>,
    json_out: bool,
    pr_number: Option<i64>,
    repo: Option<String>,
}

fn parse_args(tail: &[String]) -> Option<Args> {
    let mut a = Args {
        dry_run: false,
        node: None,
        json_out: false,
        pr_number: None,
        repo: None,
    };
    let mut index = 0;
    while index < tail.len() {
        match tail[index].as_str() {
            "--dry-run" | "-N" => a.dry_run = true,
            "--json" | "-J" => a.json_out = true,
            "--node" => {
                index += 1;
                a.node = tail.get(index)?.to_string().into();
            }
            "--pr-number" => {
                index += 1;
                let raw = tail.get(index)?;
                a.pr_number = Some(raw.parse().ok()?);
            }
            "--repo" => {
                index += 1;
                a.repo = tail.get(index)?.to_string().into();
            }
            "-h" | "--help" => return None,
            _ => return None,
        }
        index += 1;
    }
    Some(a)
}

/// The door entry: resolve the graph path, arm the single-flight gate, run
/// one pass.
pub(crate) fn run(tail: &[String]) -> i32 {
    if tail.iter().any(|a| a == "-h" || a == "--help") {
        println!("{HELP}");
        return 0;
    }
    let Some(args) = parse_args(tail) else {
        eprintln!("{USAGE}");
        return 2;
    };
    let repo_resolved = match args.repo.clone() {
        Some(repo) => Some(repo),
        None if args.pr_number.is_some() => {
            Some(resolve_current_repo_slug(None).unwrap_or_else(|| "unresolved".into()))
        }
        None => None,
    };
    gate(&args, repo_resolved, || once(&args))
}

fn once(args: &Args) -> i32 {
    let graph_path = super::settings::graph_path();
    once_at(args, &graph_path)
}

/// The reconcile gate: the mutual-exclusion refusal (a bad invocation is
/// refused even while the scope is held), the dry-run bypass (--dry-run
/// mutates nothing and stays readable mid-sweep), then the single-flight
/// claim. A held tick prints the held receipt and exits 0: it is not an
/// error. The claim is released the moment the work returns - no record
/// replay, so the next sweep inside the TTL still runs.
fn gate(args: &Args, repo_resolved: Option<String>, run: impl FnOnce() -> i32) -> i32 {
    if args.node.is_some() && args.pr_number.is_some() {
        eprintln!(
            "--node and --pr-number are mutually exclusive: --pr-number already scopes the scan to every node its own trailer claims, which --node cannot narrow without silently stranding the other claimed nodes stamped-but-unclosed. Run them separately."
        );
        return 2;
    }
    if args.dry_run {
        return run();
    }
    let repo = args
        .repo
        .clone()
        .or(repo_resolved)
        .unwrap_or_else(|| "unresolved".into());
    let key = match (&args.node, args.pr_number) {
        (Some(node), _) => format!("flight:backlog-reconcile:node:{node}"),
        (None, Some(pr)) => format!("flight:backlog-reconcile:pr:{repo}:{pr}"),
        (None, None) => "flight:backlog-reconcile".into(),
    };
    let holder = format!("reconcile-{}", std::process::id());
    let opts = crate::claims::AcquireOpts {
        ttl_ms: Some(30 * 60 * 1000),
        reason: Some("single-flight".into()),
        ..Default::default()
    };
    match crate::claims::acquire(&key, &holder, opts) {
        crate::claims::AcquireOutcome::Acquired(_record) => {
            let code = run();
            let _ = crate::claims::release(&key, &holder, None, None);
            code
        }
        outcome => {
            // Held, or the lock itself unavailable: the in-flight run owns
            // the work (held), or the gate is fail-open the way the Python
            // acquire_flight is (error: the caller owns its action and the
            // sweep proceeds ungated).
            if let crate::claims::AcquireOutcome::HeldByOther {
                holder: held_by, ..
            } = outcome
            {
                report_held(&key, &held_by, args.json_out);
            }
            0
        }
    }
}

/// The held receipt: the live holder's own record names the held-for
/// seconds the Python gate printed. `requests` is a counter only the
/// Python flight writer kept, so the native record omits it rather than
/// printing a zero that reads as the first try.
fn report_held(key: &str, held_by: &str, json_out: bool) {
    let record = crate::claims::claim_path(key, None)
        .ok()
        .and_then(|path| crate::claims::read_claim_file(&path).ok());
    let (holder, held_for_s) = record
        .map(|rec| {
            let held = (crate::claims::now_ms() - rec.acquired_at).max(0) / 1000;
            (rec.holder, held)
        })
        .unwrap_or_else(|| (held_by.to_string(), 0));
    if json_out {
        println!(
            "{{\"held\": true, \"holder\": {}, \"held_for_s\": {}}}",
            serde_json::to_string(&holder).unwrap_or_default(),
            held_for_s
        );
    } else {
        println!(
            "backlog reconcile: held, a run for this scope is already in flight (holder={holder}, held_for_s={held_for_s}s); the in-flight run owns the work, this one stood down"
        );
    }
}

/// The sweep body, gate-free: one reconcile pass. Legs in _reconcile_once's
/// order; every leg degrades to a warning, never aborts the sweep.
fn once_at(args: &Args, graph_path: &Path) -> i32 {
    let mut entries = match read_rows(graph_path) {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("reconcile: the graph read failed: {e}");
            return 1;
        }
    };
    let full_sweep = args.node.is_none() && args.pr_number.is_none();
    let mut stderr_log: Vec<String> = Vec::new();

    // --pr-number: bind every exact trailer claim BEFORE the scan, so a PR
    // naming several nodes closes all of them in this one invocation.
    let mut supersession_files: HashMap<i64, Vec<String>> = HashMap::new();
    let mut closure_claims: Vec<String> = Vec::new();
    let mut closure_bound: Vec<String> = Vec::new();
    let mut closure_refused: Option<String> = None;
    let mut pr_ctx_url: Option<String> = None;
    if let Some(pr_number) = args.pr_number {
        let (claims, bound, refusal, files, url) = closure_claims_leg(
            &mut entries,
            graph_path,
            pr_number,
            args.repo.as_deref(),
            args.dry_run,
            args.json_out,
        );
        closure_claims = claims;
        closure_bound = bound;
        closure_refused = refusal;
        if !files.is_empty() {
            supersession_files.insert(pr_number, files);
        }
        pr_ctx_url = url;
    }

    // Open-PR binding heals: one bounded listing per repo BEFORE the scan
    // feeds the forward scan. Full and explicit-node runs only.
    let listings = ListingCache::new();
    let mut open_bound: Vec<Value> = Vec::new();
    let mut open_advisories: Vec<String> = Vec::new();
    if full_sweep || args.node.is_some() {
        let (heals, advisories) = collect_open_binding_heals(
            &entries,
            single_scope(args.node.as_deref()).as_ref(),
            Some(&listings),
            query_seam,
        );
        open_advisories = advisories;
        open_bound = heals_leg_persist(
            &mut entries,
            graph_path,
            heals,
            args.node.as_deref(),
            args.dry_run,
        );
        if !args.json_out {
            for b in &open_bound {
                eprintln!(
                    "open_pr_bound: {} -> PR #{} ({})",
                    b["node"].as_str().unwrap_or("?"),
                    b["pr"].as_i64().unwrap_or(0),
                    b["url"].as_str().unwrap_or(""),
                );
            }
            for advisory in &open_advisories {
                eprintln!("warning: {advisory}");
            }
        }
    }

    // The scan scope: --node wins; --pr-number sweeps only what THIS PR
    // could touch; a bare run sweeps the graph.
    let scope = scan_scope(
        &entries,
        args.node.as_deref(),
        args.pr_number,
        pr_ctx_url.as_deref(),
        &closure_claims,
        args.repo.as_deref(),
        args.json_out,
        &mut stderr_log,
    );
    let mut records = scan_merge_drift(&entries, scope.as_ref(), Some(&listings), query_seam);

    // Auto-discovery (full sweep): merge-closure claims on OTHER discovered
    // PRs bind too, capped and budgeted like the Python leg.
    if full_sweep {
        let snapshot = records_closeable_snapshot(&records);
        auto_discover_leg(
            &mut entries,
            graph_path,
            &snapshot,
            args.pr_number,
            &mut supersession_files,
            &mut closure_claims,
            &mut closure_bound,
            args.dry_run,
            args.json_out,
            &mut stderr_log,
        );
        if !closure_bound.is_empty() {
            // Binds just landed (persisted, or in memory on a dry run): the
            // scan reruns on the healed rows.
            records = scan_merge_drift(&entries, scope.as_ref(), Some(&listings), query_seam);
        }
    }

    let mut sweep = partition_and_probe(records, &entries, full_sweep, args.dry_run);

    // The promise gate + reopen guard partition the closeable set.
    let (gated, mut promise_held, promise_warnings, reopen_expired) =
        promise_gate_leg(&sweep.closeable, &entries, args.json_out, &mut stderr_log);
    sweep.closeable = gated;

    // The close: ONE locked mutation, then the post-lock legs.
    let close = if args.dry_run {
        None
    } else {
        close_leg(
            graph_path,
            &entries,
            &sweep.closeable,
            &supersession_files,
            &sweep,
            &reopen_expired,
            full_sweep,
            &mut stderr_log,
        )
    };
    promise_held.extend(
        close
            .as_ref()
            .map(|c| c.held_recheck.clone())
            .unwrap_or_default(),
    );
    let post_entries = match read_rows(graph_path) {
        Ok(rows) => rows,
        Err(_) => entries.clone(),
    };
    let closed_rows = close
        .as_ref()
        .map(|c| post_close_leg(c, &post_entries, args.json_out, &mut stderr_log))
        .unwrap_or_default();

    // The dry-run preview simulates the exact close on a throwaway copy.
    let preview = args.dry_run.then(|| preview_leg(&entries, &sweep));

    // Full sweep only: stamp `reverted` on nodes a merged revert PR names.
    let reverted = revert_leg(
        &entries,
        graph_path,
        full_sweep,
        args.dry_run,
        &mut stderr_log,
    );

    // Canonical-sync catch-up: when the pr-watch daemon is dead the next
    // interactive session catches the canonical up. --dry-run skips.
    let sync_catchup = sync_catchup_leg(args.dry_run, args.json_out);

    // Claim GC: the unattended reaper reaches every path that fires
    // reconcile. --dry-run propagates as a would-archive count.
    let claim_reap = claim_reap_leg(args.dry_run, args.json_out);

    // Parent epics of this run's closure claims that are still open, each
    // naming its still-open sibling children. Read-only, never mutates.
    let epics_waiting = epics_waiting_leg(
        &closure_claims,
        &entries,
        preview.as_ref().and_then(|p| p.sim.as_deref()),
        args.dry_run,
        graph_path,
        args.json_out,
    );

    // Ledger session harvest: fill every execution row's ABSENT `sessions`
    // from its graph node and mark the rest explicitly, before the report.
    let ledger_harvest = harvest_leg(&post_entries, args.dry_run, args.json_out);

    // Persisted-vs-derived status drift, full sweep only (the `reclaimed`
    // roll the goldens pin).
    let reclaimed = if full_sweep {
        status_drift(&entries)
    } else {
        Vec::new()
    };

    // The legs' warnings print on stderr in both modes: the SessionStart
    // hook reads --json and discards stderr, so a warning that stayed
    // buffered would be invisible exactly where it matters least and
    // silent where it matters most.
    for line in &stderr_log {
        eprintln!("{line}");
    }

    let supersession_unverified = close
        .as_ref()
        .map(|c| c.supersession_unverified.clone())
        .unwrap_or_default();
    let exit = report_leg(
        args,
        &sweep,
        close.as_ref(),
        &closed_rows,
        preview.as_ref(),
        &promise_held,
        &promise_warnings,
        &closure_claims,
        &closure_bound,
        &closure_refused,
        &open_bound,
        &open_advisories,
        &supersession_unverified,
        &LegOutcomes {
            reverted,
            sync_catchup,
            claim_reap,
            ledger_harvest,
            epics_waiting,
            reclaimed,
        },
        full_sweep,
    );
    exit
}

fn single_scope(node: Option<&str>) -> Option<BTreeSet<String>> {
    node.map(|n| {
        let mut set = BTreeSet::new();
        set.insert(n.to_string());
        set
    })
}

/// Persist (or preview) the open-binding heals: only a node id healed by
/// exactly ONE candidate PR binds (a count above one reads ambiguous),
/// the node must still be open, and a node that already carries refs only
/// rebinds when its heal says so. Returns the {node, pr, url} rows.
fn heals_leg_persist(
    entries: &mut Vec<Value>,
    graph_path: &Path,
    heals: Vec<OpenBindingHeal>,
    _node: Option<&str>,
    dry_run: bool,
) -> Vec<Value> {
    if heals.is_empty() {
        return Vec::new();
    }
    let apply = |rows: &mut Vec<Value>| -> Vec<Value> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for h in &heals {
            *counts.entry(h.node_id.clone()).or_default() += 1;
        }
        let mut kept: Vec<Value> = Vec::new();
        for h in &heals {
            if counts.get(&h.node_id).copied() != Some(1) {
                continue;
            }
            let Some(index) = rows
                .iter()
                .position(|e| e.get("id").and_then(Value::as_str) == Some(h.node_id.as_str()))
            else {
                continue;
            };
            let current = &rows[index];
            if !node_is_open(current) {
                continue;
            }
            let has_refs = !super::merge_evidence::node_pr_refs(current).is_empty();
            if has_refs && !h.rebind {
                continue;
            }
            let result = bind_pr_rows(
                rows,
                std::slice::from_ref(&h.node_id),
                h.pr_number,
                h.pr_url.as_deref(),
                None,
                h.rebind,
            );
            if result.outcome == "bound" && !result.bound_ids().is_empty() {
                kept.push(json!({
                    "node": h.node_id,
                    "pr": h.pr_number,
                    "url": h.pr_url,
                }));
            }
        }
        kept
    };
    if dry_run {
        let mut rows = entries.clone();
        let mut kept = apply(&mut rows);
        for row in kept.iter_mut() {
            row["would"] = Value::Bool(true);
        }
        // In-memory only (the Python dry-run contract): the forward-scan
        // preview below sees the healed rows, disk never moves.
        *entries = rows;
        return kept;
    }
    let kept: std::cell::RefCell<Vec<Value>> = std::cell::RefCell::new(Vec::new());
    let published = graph_store::mutate_rows(
        graph_path,
        std::time::Duration::from_secs(30),
        None,
        None,
        |working| {
            // A conflict retry reruns the closure: the last attempt's rows win.
            kept.borrow_mut().clear();
            let filled = apply(working);
            kept.borrow_mut().extend(filled);
            Ok(true)
        },
    );
    if published.is_err() {
        return Vec::new();
    }
    // Real fills just persisted: the caller's scan must consume the healed
    // rows, not the pre-heal snapshot.
    if let Ok(rows) = read_rows(graph_path) {
        *entries = rows;
    }
    kept.into_inner()
}

/// The scan scope: --node wins; a --pr-number run sweeps only what THIS PR
/// could touch (its stamped ref plus every exact trailer claim plus the
/// open ref-less nodes of this checkout); a bare run sweeps the graph.
#[allow(clippy::too_many_arguments)]
fn scan_scope(
    entries: &[Value],
    node: Option<&str>,
    pr_number: Option<i64>,
    pr_url: Option<&str>,
    claims: &[String],
    repo: Option<&str>,
    json_out: bool,
    stderr_log: &mut Vec<String>,
) -> Option<BTreeSet<String>> {
    if let Some(node) = node {
        let mut set = BTreeSet::new();
        set.insert(node.to_string());
        return Some(set);
    }
    let Some(pr_number) = pr_number else {
        return None;
    };
    let our_repo = repo
        .map(str::to_string)
        .or_else(|| repo_slug_from_url(pr_url));
    if our_repo.is_none() && !json_out {
        stderr_log.push(format!(
            "warning: reconcile --pr-number {pr_number}: could not resolve this repo's slug; scoping to exact trailer claims and open ref-less nodes only, skipping any ref-stamped bare-number match"
        ));
    }
    let mut ids: BTreeSet<String> = claims.iter().cloned().collect();
    // Open ref-less nodes whose cwd sits inside this checkout could be
    // closed by this PR's merge.
    if let Some(root) = checkout_root() {
        for e in entries {
            let Some(nid) = e.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !node_is_open(e) || !super::merge_evidence::node_pr_refs(e).is_empty() {
                continue;
            }
            if super::binding::node_cwd_in_repo(e, &root) {
                ids.insert(nid.to_string());
            }
        }
    }
    let Some(our_repo) = our_repo else {
        return Some(ids);
    };
    for e in entries {
        let Some(nid) = e.get("id").and_then(Value::as_str) else {
            continue;
        };
        for (num, url) in super::merge_evidence::node_pr_refs(e) {
            if num != pr_number {
                continue;
            }
            match repo_slug_from_url(url.as_deref()) {
                None => ids.insert(nid.to_string()),
                Some(slug) => {
                    if slug.eq_ignore_ascii_case(&our_repo) {
                        ids.insert(nid.to_string())
                    } else {
                        false
                    }
                }
            };
            break;
        }
    }
    Some(ids)
}

/// The checkout the sweep runs in (git toplevel of the cwd), for the
/// cwd-inside-repo gate.
fn checkout_root() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(root)
    }
}

/// Bind ``claims`` to ``pr_number`` against ``entries`` - the Python
/// _bind_and_report twin. Persists via one locked mutation unless
/// ``dry_run``, in which case it mutates ``entries`` in place ONLY so the
/// caller's own forward-scan preview reflects the bind without ever
/// touching disk. Returns (refusal, bound_ids).
fn bind_claims(
    entries: &mut Vec<Value>,
    graph_path: &Path,
    claims: &[String],
    pr_number: i64,
    pr_url: Option<&str>,
    repo: Option<&str>,
    dry_run: bool,
) -> (Option<String>, Vec<String>) {
    if dry_run {
        let mut rows = entries.clone();
        let result = bind_pr_rows(&mut rows, claims, pr_number, pr_url, repo, false);
        let out = (result.refusal.clone(), result.bound_ids());
        *entries = rows;
        return out;
    }
    let mut outcome: (Option<String>, Vec<String>) = (None, Vec::new());
    let published = graph_store::mutate_rows(
        graph_path,
        std::time::Duration::from_secs(30),
        None,
        None,
        |working| {
            let result = bind_pr_rows(working, claims, pr_number, pr_url, repo, false);
            outcome = (result.refusal.clone(), result.bound_ids());
            if result.refusal.is_some() {
                return Ok(false);
            }
            Ok(true)
        },
    );
    match published {
        Ok(_) => outcome,
        Err(e) => (Some(format!("the graph write failed: {e}")), Vec::new()),
    }
}

/// The --pr-number closure-claims leg: fetch the PR's closure context once,
/// refuse an unmerged PR that carries a closure line, bind every exact
/// trailer claim all-or-nothing, and report. Returns (claims, bound,
/// refusal, changed_files, pr_url). On a real bind `entries` is re-read by
/// the caller (the persisted graph outranks the pre-bind snapshot).
fn closure_claims_leg(
    entries: &mut Vec<Value>,
    graph_path: &Path,
    pr_number: i64,
    repo: Option<&str>,
    dry_run: bool,
    json_out: bool,
) -> (
    Vec<String>,
    Vec<String>,
    Option<String>,
    Vec<String>,
    Option<String>,
) {
    let refused = |message: String| -> (
        Vec<String>,
        Vec<String>,
        Option<String>,
        Vec<String>,
        Option<String>,
    ) {
        if !json_out {
            eprintln!("warning: reconcile --pr-number: {message}");
        }
        (Vec::new(), Vec::new(), Some(message), Vec::new(), None)
    };
    let ctx = match fetch_pr_closure_context(pr_number, repo) {
        Ok(ctx) => ctx,
        Err(e) => return refused(format!("could not query PR #{pr_number}: {}", e.message)),
    };
    if ctx.state != "MERGED" {
        let claims = crate::king_board::pr_closure::parse(&ctx.body);
        if !claims.is_empty() {
            return refused(format!(
                "PR #{pr_number} is not merged (state={})",
                ctx.state
            ));
        }
        return (Vec::new(), Vec::new(), None, Vec::new(), None);
    }
    let claims = crate::king_board::pr_closure::parse(&ctx.body);
    if claims.is_empty() {
        return (Vec::new(), Vec::new(), None, ctx.changed_files, ctx.url);
    }
    let pr_url = ctx.url.clone();
    let (refusal, bound) = bind_claims(
        entries,
        graph_path,
        &claims,
        pr_number,
        pr_url.as_deref(),
        repo,
        dry_run,
    );
    if !dry_run {
        // Real bind just persisted: the caller's scan consumes the
        // persisted rows, not the pre-bind snapshot.
        match read_rows(graph_path) {
            Ok(rows) => *entries = rows,
            Err(e) => return refused(format!("the graph re-read failed: {e}")),
        }
    }
    if let Some(message) = &refusal {
        if !json_out {
            eprintln!("warning: reconcile --pr-number {pr_number}: refused binding: {message}");
        }
    } else if !bound.is_empty() && !json_out {
        eprintln!(
            "reconcile --pr-number {pr_number}: bound {}",
            bound.join(", ")
        );
    }
    (claims, bound, refusal, ctx.changed_files, pr_url)
}

/// The closeable records, snapshotted before the discovery leg moves the
/// scan: one candidate per PR (the discovery dedup key), in scan order.
fn records_closeable_snapshot(records: &[MergeDriftRecord]) -> Vec<MergeDriftRecord> {
    records.iter().filter(|r| r.closeable()).cloned().collect()
}

/// Auto-discovery (full sweep): every closeable record's PR other than the
/// explicitly-scoped one gets its closure body read once; exact claims bind
/// through the same all-or-nothing door. Capped at 20 PRs and a 60s budget.
/// A record's OWN url or cwd resolves the repo - the graph is CROSS-PROJECT,
/// so a --repo passed for the --pr-number leg never scopes an unrelated
/// discovered PR.
#[allow(clippy::too_many_arguments)]
fn auto_discover_leg(
    entries: &mut Vec<Value>,
    graph_path: &Path,
    snapshot: &[MergeDriftRecord],
    skip_pr: Option<i64>,
    supersession_files: &mut HashMap<i64, Vec<String>>,
    closure_claims: &mut Vec<String>,
    closure_bound: &mut Vec<String>,
    dry_run: bool,
    json_out: bool,
    _stderr_log: &mut Vec<String>,
) {
    const MAX_AUTO_DISCOVER: usize = 20;
    const AUTO_DISCOVER_BUDGET_S: u64 = 60;
    let mut discovered: Vec<&MergeDriftRecord> = Vec::new();
    let mut seen: BTreeSet<i64> = BTreeSet::new();
    for r in snapshot {
        if Some(r.pr_number) == skip_pr || !seen.insert(r.pr_number) {
            continue;
        }
        discovered.push(r);
    }
    let dropped = discovered.len().saturating_sub(MAX_AUTO_DISCOVER);
    if dropped > 0 && !json_out {
        let names: Vec<String> = discovered[MAX_AUTO_DISCOVER..]
            .iter()
            .map(|r| r.pr_number.to_string())
            .collect();
        eprintln!(
            "reconcile: auto-discovery capped at {MAX_AUTO_DISCOVER}; deferred {dropped} PR(s) to a later sweep: {}",
            names.join(", ")
        );
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(AUTO_DISCOVER_BUDGET_S);
    let mut auto_bound_any = false;
    for record in discovered.into_iter().take(MAX_AUTO_DISCOVER) {
        if std::time::Instant::now() >= deadline {
            if !json_out {
                eprintln!(
                    "reconcile: auto-discovery stopped at its {AUTO_DISCOVER_BUDGET_S}s budget (gh is slow or degraded); the rest defers to a later sweep"
                );
            }
            break;
        }
        // Neither the record's url nor its cwd resolves a repo: skip rather
        // than guess, since a same-numbered PR can exist in the wrong repo
        // and a fresh (no-existing-ref) node would bind to it blind.
        let repo_for_pr = record
            .pr_url
            .as_deref()
            .and_then(|u| repo_slug_from_url(Some(u)))
            .or_else(|| {
                record
                    .cwd
                    .as_deref()
                    .and_then(|c| resolve_current_repo_slug(Some(c)))
            });
        let Some(repo_for_pr) = repo_for_pr else {
            continue;
        };
        // Best-effort discovery: the forward scan already has this PR's
        // state, so a failed read just ends this record's turn.
        let Ok(ctx) = fetch_pr_closure_context(record.pr_number, Some(&repo_for_pr)) else {
            continue;
        };
        supersession_files.insert(record.pr_number, ctx.changed_files.clone());
        let claims = crate::king_board::pr_closure::parse(&ctx.body);
        if claims.is_empty() {
            continue;
        }
        let mut merged: BTreeSet<String> = closure_claims.iter().cloned().collect();
        merged.extend(claims.iter().cloned());
        *closure_claims = merged.into_iter().collect();
        let (refusal, bound) = bind_claims(
            entries,
            graph_path,
            &claims,
            record.pr_number,
            ctx.url.as_deref(),
            Some(&repo_for_pr),
            dry_run,
        );
        if !bound.is_empty() {
            closure_bound.extend(bound);
            auto_bound_any = true;
        } else if let Some(refusal) = refusal {
            if !json_out {
                eprintln!(
                    "warning: reconcile: auto-discovered PR #{} refused binding: {refusal}",
                    record.pr_number
                );
            }
        }
    }
    if auto_bound_any && !dry_run {
        // Real binds just persisted: rescan from the persisted rows.
        if let Ok(rows) = read_rows(graph_path) {
            *entries = rows;
        }
    }
}

/// Partition the scan records and run the sweep's self-heal detectors +
/// evidence gathers (full sweep only): strandable epics/contained/orphans,
/// owed supersession evidence, and the edge-settlement probe. All
/// fail-open.
struct Sweep {
    closeable: Vec<MergeDriftRecord>,
    failures: Vec<MergeDriftRecord>,
    strandable_epics: Vec<String>,
    strandable_contained: Vec<String>,
    strandable_orphans: Vec<String>,
    owed_evidence: Vec<(String, Value)>,
    owed_failures: Vec<Value>,
    settlement: Value,
}

fn partition_and_probe(
    records: Vec<MergeDriftRecord>,
    entries: &[Value],
    full_sweep: bool,
    dry_run: bool,
) -> Sweep {
    let mut closeable: Vec<MergeDriftRecord> = Vec::new();
    let mut failures: Vec<MergeDriftRecord> = Vec::new();
    for record in records {
        if record.closeable() {
            closeable.push(record);
        } else if record.error.is_some() {
            failures.push(record);
        }
    }
    let empty = Value::Object(Default::default());
    if !full_sweep {
        return Sweep {
            closeable,
            failures,
            strandable_epics: Vec::new(),
            strandable_contained: Vec::new(),
            strandable_orphans: Vec::new(),
            owed_evidence: Vec::new(),
            owed_failures: Vec::new(),
            settlement: empty,
        };
    }
    let to_ids = |set: BTreeSet<String>| set.into_iter().collect::<Vec<String>>();
    let strandable_epics = to_ids(strandable_epic_ids(entries));
    let strandable_contained = to_ids(strandable_contained_ids(entries));
    let strandable_orphans = to_ids(strandable_orphan_ids(entries));

    // Pending supersessions whose successor closed outside this sweep are
    // owed a verdict. The gh round trips stay OUTSIDE the graph lock, and a
    // dry run gathers nothing (a preview mutates nothing and owes nothing).
    let mut owed_evidence: Vec<(String, Value)> = Vec::new();
    let mut owed_failures: Vec<Value> = Vec::new();
    if !dry_run {
        for (successor_id, successor) in successors_owing_verification(entries) {
            let Some(pr) = successor.get("pr_number").and_then(Value::as_i64) else {
                continue;
            };
            let repo = repo_slug_from_url(successor.get("pr_url").and_then(Value::as_str));
            let cwd = if repo.is_none() {
                successor.get("cwd").and_then(Value::as_str)
            } else {
                None
            };
            match super::merge_state::query_pr_merge_state(pr, repo.as_deref(), cwd, true) {
                Err(e) => owed_failures.push(json!({
                    "successor": successor_id,
                    "pr_number": pr.to_string(),
                    "error": e.message,
                    "kind": e.kind,
                    "remedy": e.remedy_for(pr, repo.as_deref()),
                })),
                Ok(state) if state.state == "MERGED" => owed_evidence.push((
                    successor_id,
                    json!({
                        "changed_files": state.changed_files,
                        "files_truncated": state.files_truncated,
                        "pr_number": pr,
                        "merged_at": state.merged_at,
                    }),
                )),
                Ok(_) => {}
            }
        }
    }

    // Edge settlement: computes the plan once; the mutator applies its
    // change map under the lock and reports the receipts. The Rust probe
    // is a pure fold (no store subprocess), so it cannot fail the way the
    // Python keeper round-trip could.
    let settlement = if dry_run {
        Value::Object(Default::default())
    } else {
        let (_settled, receipts, changes) = settle_blocked_by_edges(entries.to_vec());
        json!({
            "blocked_by": serde_json::to_value(&changes).unwrap_or_default(),
            "receipts": receipts,
        })
    };
    Sweep {
        closeable,
        failures,
        strandable_epics,
        strandable_contained,
        strandable_orphans,
        owed_evidence,
        owed_failures,
        settlement,
    }
}

/// Expiry half of the reopen guard: true when a ref OTHER than `skip_pr`
/// merged after the node's reopen. A read outage must not close on a
/// guess, so any query failure reads false.
fn merge_postdates_reopen(node: &Value, skip_pr: i64, cwd: Option<&str>) -> bool {
    use crate::sync_canonical::parse_iso;
    let reopened = node
        .get("reopened_at")
        .and_then(Value::as_str)
        .and_then(|raw| parse_iso(raw));
    let Some(reopened) = reopened else {
        return false;
    };
    for (number, url) in super::merge_evidence::node_pr_refs(node) {
        if number == skip_pr {
            continue;
        }
        let Ok(state) = super::merge_state::query_pr_merge_state(
            number,
            repo_slug_from_url(url.as_deref()).as_deref(),
            cwd,
            false,
        ) else {
            return false;
        };
        if state.state != "MERGED" {
            continue;
        }
        let Some(merged_at) = state.merged_at.as_deref().and_then(parse_iso) else {
            continue;
        };
        if merged_at > reopened {
            return true;
        }
    }
    false
}

/// The reopen-vs-merge hold reason: the reopener's own words.
fn reopen_held_reason(node: &Value, pr_number: i64) -> String {
    let why = node
        .get("reopened_reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    format!(
        "reopened after PR #{pr_number} merged: {}",
        if why.is_empty() {
            "no reopen reason recorded"
        } else {
            why
        }
    )
}

/// One held-open row: (node_id, first reason line, outcome).
type HeldRow = (String, String, String);

/// The promise gate over the closeable set: reopen guard first, then the
/// promise verdict. Returns the gated records, the held roll, the gate
/// warnings, and the reopen-expired ids (a reopen a LATER merged ref
/// already outlived skips the locked recheck).
fn promise_gate_leg(
    closeable: &[MergeDriftRecord],
    entries: &[Value],
    json_out: bool,
    stderr_log: &mut Vec<String>,
) -> (Vec<MergeDriftRecord>, Vec<HeldRow>, Vec<Value>, Vec<String>) {
    let mut gated: Vec<MergeDriftRecord> = Vec::new();
    let mut held: Vec<HeldRow> = Vec::new();
    let mut warnings: Vec<Value> = Vec::new();
    let mut reopen_expired: Vec<String> = Vec::new();
    for record in closeable {
        let node = entries
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(record.node_id.as_str()))
            .cloned()
            .unwrap_or_else(|| {
                json!({
                    "id": record.node_id,
                    "plan_path": record.plan_path,
                    "pr_number": record.pr_number,
                    "pr_url": record.pr_url,
                })
            });
        let resolved = effective_reconcile_cwd(
            node.get("cwd").and_then(Value::as_str).unwrap_or(""),
            node.get("project").and_then(Value::as_str),
        );
        let gate_cwd = if Path::new(&resolved).is_dir() {
            Some(resolved)
        } else {
            None
        };
        // Reopen guard BEFORE the promise gate: a deliberate reopen
        // postdating the merge holds, unless a later ref merged after it.
        if reopen_outranks_merge(&node, record.merged_at.as_deref().unwrap_or("")) {
            if !merge_postdates_reopen(&node, record.pr_number, gate_cwd.as_deref()) {
                held.push((
                    record.node_id.clone(),
                    reopen_held_reason(&node, record.pr_number),
                    "reopen_held".into(),
                ));
                continue;
            }
            // Expired: the locked recheck covers only a NEWER reopen.
            reopen_expired.push(record.node_id.clone());
        }
        let verdict = resolve_promise_evidence(&node, gate_cwd.as_deref(), &[]);
        if let Some(warning) = &verdict.warning {
            if !json_out {
                stderr_log.push(format!("warning: {warning}"));
            }
            warnings.push(json!({"node_id": record.node_id, "warning": warning}));
        }
        if verdict.satisfied() {
            gated.push(record.clone());
        } else {
            let reason = verdict.reason.unwrap_or_default();
            let first_line = reason.lines().next().unwrap_or("").to_string();
            let outcome = match verdict.outcome {
                PromiseOutcome::Unmet => "promise_unmet",
                _ => "promise_unknown",
            };
            held.push((record.node_id.clone(), first_line, outcome.into()));
        }
    }
    (gated, held, warnings, reopen_expired)
}

/// The held-open roll, split by WHY the node stayed open.
fn summarize_promise_held(held: &[HeldRow], dry_run: bool) -> String {
    let verb = if dry_run { "Holding" } else { "Held" };
    let headlines = [
        ("promise_unmet", "merged PR, unmet plan promise"),
        (
            "promise_unknown",
            "ship count unconfirmed, retryable read failure",
        ),
        ("reopen_held", "deliberate reopen postdates the merge"),
    ];
    let mut order: Vec<&str> = headlines.iter().map(|(k, _)| *k).collect();
    for outcome in held.iter().map(|(_, _, o)| o.as_str()) {
        if !order.contains(&outcome) {
            order.push(outcome);
        }
    }
    let mut lines: Vec<String> = Vec::new();
    for outcome in &order {
        let rows: Vec<&HeldRow> = held.iter().filter(|(_, _, o)| o == outcome).collect();
        if rows.is_empty() {
            continue;
        }
        let headline = headlines
            .iter()
            .find(|(k, _)| k == outcome)
            .map(|(_, v)| *v)
            .unwrap_or(outcome);
        lines.push(format!("{verb} {} node(s) open ({headline}):", rows.len()));
        for (nid, reason, _) in rows {
            lines.push(format!("  {nid}: {reason}"));
        }
    }
    lines.join("\n")
}

/// The close mutator's collected output, run as ONE locked mutation:
/// complete the closeable records (with the locked reopen recheck),
/// re-parent their live children, verify supersessions, roll up the
/// ledger, backfill reverse-mapped PR refs, close their contained
/// children, cascade-close now-all-done ancestor epics, then the
/// full-sweep self-heals. Everything the post-lock legs and the report
/// consume.
struct CloseResult {
    actually_closed: Vec<MergeDriftRecord>,
    cascade_closed: Vec<String>,
    contained_closed: Vec<String>,
    carried_stamped: Vec<String>,
    reparented: Vec<(String, Option<String>)>,
    supersession_unverified: Vec<Value>,
    settlement_receipts: Vec<Value>,
    contained_errors: Vec<Value>,
    /// Holds found by the locked reopen recheck, after the scan.
    held_recheck: Vec<HeldRow>,
}

#[allow(clippy::too_many_arguments)]
fn close_leg(
    graph_path: &Path,
    entries: &[Value],
    closeable: &[MergeDriftRecord],
    supersession_files: &HashMap<i64, Vec<String>>,
    sweep: &Sweep,
    reopen_expired: &[String],
    full_sweep: bool,
    stderr_log: &mut Vec<String>,
) -> Option<CloseResult> {
    let settlement_pending = sweep.settlement["receipts"]
        .as_array()
        .map(|r| !r.is_empty())
        .unwrap_or(false);
    let any_work = !closeable.is_empty()
        || !sweep.strandable_epics.is_empty()
        || !sweep.strandable_contained.is_empty()
        || !sweep.owed_evidence.is_empty()
        || settlement_pending;
    if !any_work {
        return None;
    }
    // Ledger rollup, precomputed outside the lock (ledger I/O must not
    // block other graph mutations). Reconcile is the MAINSTREAM close: a
    // session lands its PR open, `done` exits 5 awaiting merge, and
    // reconcile closes it at the merge - so without the rollup here,
    // session_id / points are never recorded on the normal path at all.
    let rollups: HashMap<String, crate::backlog::workflows::Rollup> = closeable
        .iter()
        .filter_map(|record| {
            entries
                .iter()
                .find(|e| e.get("id").and_then(Value::as_str) == Some(record.node_id.as_str()))
                .map(|node| (record.node_id.clone(), rollup_from_ledger(node)))
        })
        .collect();
    let result: std::cell::RefCell<CloseResult> = std::cell::RefCell::new(CloseResult {
        actually_closed: Vec::new(),
        cascade_closed: Vec::new(),
        contained_closed: Vec::new(),
        carried_stamped: Vec::new(),
        reparented: Vec::new(),
        supersession_unverified: Vec::new(),
        settlement_receipts: Vec::new(),
        contained_errors: Vec::new(),
        held_recheck: Vec::new(),
    });
    let published = graph_store::mutate_rows(
        graph_path,
        std::time::Duration::from_secs(60),
        None,
        None,
        |entries| {
            // A conflict retry reruns the closure from a fresh read: the
            // last attempt's collections win.
            let mut r = result.borrow_mut();
            r.actually_closed.clear();
            r.cascade_closed.clear();
            r.contained_closed.clear();
            r.carried_stamped.clear();
            r.reparented.clear();
            r.supersession_unverified.clear();
            r.settlement_receipts.clear();
            r.contained_errors.clear();
            r.held_recheck.clear();
            drop(r);
            close_mutator(
                entries,
                closeable,
                supersession_files,
                sweep,
                reopen_expired,
                full_sweep,
                &rollups,
                &result,
                stderr_log,
            );
            Ok(true)
        },
    );
    if let Err(e) = published {
        stderr_log.push(format!(
            "warning: the close mutation failed: {e}; nothing closed this run"
        ));
        return None;
    }
    Some(result.into_inner())
}

/// The inside-the-lock half of the close: the per-record completion plus
/// the full-sweep self-heals, all mutating `entries` in place. Guarded
/// like the Python mutator: a failing heal never aborts the close.
#[allow(clippy::too_many_arguments)]
fn close_mutator(
    entries: &mut [Value],
    closeable: &[MergeDriftRecord],
    supersession_files: &HashMap<i64, Vec<String>>,
    sweep: &Sweep,
    reopen_expired: &[String],
    full_sweep: bool,
    rollups: &HashMap<String, crate::backlog::workflows::Rollup>,
    result: &std::cell::RefCell<CloseResult>,
    stderr_log: &mut Vec<String>,
) {
    for record in closeable {
        let Some(index) = entries
            .iter()
            .position(|e| e.get("id").and_then(Value::as_str) == Some(record.node_id.as_str()))
        else {
            continue;
        };
        // Idempotency: a node closed out-of-band between the scan and the
        // lock is skipped.
        if entries[index]
            .get("completed_at")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        {
            continue;
        }
        // Locked recheck of the reopen guard: a reopen landing between the
        // scan and this transaction would re-close here.
        if !reopen_expired.contains(&record.node_id)
            && reopen_outranks_merge(&entries[index], record.merged_at.as_deref().unwrap_or(""))
        {
            result.borrow_mut().held_recheck.push((
                record.node_id.clone(),
                reopen_held_reason(&entries[index], record.pr_number),
                "reopen_held".into(),
            ));
            continue;
        }
        apply_completion_fields(&mut entries[index], true);
        let mut moved = reparent_live_children(entries, &record.node_id);
        result.borrow_mut().reparented.append(&mut moved);
        let files = if record.changed_files.is_empty() {
            supersession_files
                .get(&record.pr_number)
                .cloned()
                .unwrap_or_default()
        } else {
            record.changed_files.clone()
        };
        let receipts = verify_pending_supersessions(
            entries,
            &record.node_id,
            &files,
            record.pr_number,
            record.merged_at.as_deref(),
            !record.files_truncated,
        );
        result.borrow_mut().supersession_unverified.extend(receipts);
        apply_ledger_rollup(&mut entries[index], rollups.get(&record.node_id));

        // Backfill the PR ref for a reverse-mapped node: the recovered
        // number/url live only on the record.
        let has_pr = entries[index]
            .get("pr_number")
            .and_then(Value::as_i64)
            .is_some();
        if record.pr_number > 0 && !has_pr {
            if let Some(obj) = entries[index].as_object_mut() {
                obj.insert("pr_number".into(), json!(record.pr_number));
                obj.insert("pr_url".into(), record.pr_url.clone().into());
            }
        }
        let mut contained =
            cascade_close_contained(entries, &record.node_id, record.merged_at.as_deref());
        result.borrow_mut().contained_closed.append(&mut contained);
        let mut cascaded = cascade_close_parents(entries, &record.node_id);
        result.borrow_mut().cascade_closed.append(&mut cascaded);
        result.borrow_mut().actually_closed.push(record.clone());
    }
    if !full_sweep {
        return;
    }
    let mut contained = sweep_close_stranded_contained(entries);
    result.borrow_mut().contained_closed.append(&mut contained);
    let mut epics = sweep_close_done_epics(entries);
    result.borrow_mut().cascade_closed.append(&mut epics);
    let mut reparented = sweep_reparent_stranded_orphans(entries);
    result.borrow_mut().reparented.append(&mut reparented);
    let mut stamped = sweep_stamp_carried_sessions(entries);
    result.borrow_mut().carried_stamped.append(&mut stamped);
    for (successor_id, evidence) in &sweep.owed_evidence {
        let files: Vec<String> = evidence["changed_files"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let receipts = verify_pending_supersessions(
            entries,
            successor_id,
            &files,
            evidence["pr_number"].as_i64().unwrap_or(0),
            evidence["merged_at"].as_str(),
            !evidence["files_truncated"].as_bool().unwrap_or(false),
        );
        result.borrow_mut().supersession_unverified.extend(receipts);
    }
    let receipts = apply_edge_settlement(entries, &sweep.settlement);
    result.borrow_mut().settlement_receipts = receipts;
    let _ = stderr_log;
}

/// The ledger rollup applied to one closing node: session_id / points /
/// cost fill-only (a prior stamp already owns its rows; re-applying would
/// double-count the same run). No env session: reconcile is the detached
/// SessionStart sweep, so CLAUDECODE_SESSION_ID names whoever started it,
/// NOT the closed node's work - the ledger's attribution wins.
fn apply_ledger_rollup(node: &mut Value, rollup: Option<&crate::backlog::workflows::Rollup>) {
    let Some(rollup) = rollup else { return };
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    let prior_cost = obj.get("cost_usd").cloned().filter(|v| !v.is_null());
    let prior_sessions = obj.get("cost_sessions").cloned();
    if obj.get("session_id").and_then(Value::as_str).is_none() {
        if let Some(session_id) = &rollup.session_id {
            obj.insert("session_id".into(), json!(session_id));
        }
    }
    if obj.get("points").map(Value::is_null).unwrap_or(true) {
        if let Some(points) = &rollup.points {
            obj.insert("points".into(), points.clone());
        }
    }
    if obj.get("cost_usd").map(Value::is_null).unwrap_or(true) {
        if let Some(cost_usd) = rollup.cost_usd {
            obj.insert("cost_usd".into(), json!(cost_usd));
        }
    }
    let sessions_empty = obj
        .get("cost_sessions")
        .and_then(Value::as_array)
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if sessions_empty && !rollup.cost_sessions.is_empty() {
        obj.insert(
            "cost_sessions".into(),
            Value::Array(rollup.cost_sessions.clone()),
        );
    }
    // Cost is fill-only, matching cmd_done: a prior stamp (fno backlog
    // cost / a loop writer) timestamps its rows at recording time while
    // the ledger row carries the completion time, so re-applying
    // double-counts the same run. The rollup's job here is
    // session_id / points.
    if let Some(cost) = prior_cost {
        obj.insert("cost_usd".into(), cost);
        obj.insert(
            "cost_sessions".into(),
            prior_sessions.unwrap_or(Value::Null),
        );
    }
}

/// The configured required review bots (config.review.github_apps), for
/// the dead-bot escape. Unresolvable config reads empty: fail open, no
/// emit.
fn required_bots_for(cwd: Option<&str>) -> Vec<String> {
    let anchor = Path::new(cwd.unwrap_or("."));
    let Some(value) = crate::agents_config::config_value_deep(anchor, &["review", "github_apps"])
    else {
        return Vec::new();
    };
    value
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The retro-pending sentinel dir: the configured override, else the state
/// root's `retro-pending`.
fn retro_pending_dir() -> Option<PathBuf> {
    let anchor = std::env::current_dir().ok()?;
    let state = crate::agents_config::state_dir(&anchor)?;
    Some(state.join("retro-pending"))
}

/// The post-lock leg per actually-closed record: plan stamp, retro
/// sentinel, session-satisfied, human touch, ledger backstop, the
/// dead-bot escape, and merge-triggered auto-continue (still the Python
/// advance until its port). Every step best-effort.
fn post_close_leg(
    close: &CloseResult,
    post_entries: &[Value],
    _json_out: bool,
    stderr_log: &mut Vec<String>,
) -> Vec<Value> {
    let mut closed_rows: Vec<Value> = Vec::new();
    let sentinel_dir = retro_pending_dir();
    let ledger = ledger_path_for_leg();
    for record in &close.actually_closed {
        // The stamp twin is fire-and-forget (best-effort inside), so the
        // receipt reports whether a stamp RAN, not whether it landed.
        let stamped = record
            .plan_path
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|plan| {
                stamp_and_graduate_plan(
                    plan,
                    record.pr_url.as_deref(),
                    record.session_id.as_deref(),
                );
                record.pr_url.is_some()
            })
            .unwrap_or(false);
        let sentinel: Option<String> = match sentinel_dir.as_deref() {
            Some(dir) => match write_retro_sentinel(record, dir) {
                Ok(path) => Some(path.to_string_lossy().into_owned()),
                Err(e) => {
                    stderr_log.push(format!(
                        "warning: closed {} but failed to write its retro sentinel: {e}",
                        record.node_id
                    ));
                    None
                }
            },
            None => {
                stderr_log.push(format!(
                    "warning: closed {} but failed to write its retro sentinel: no state root",
                    record.node_id
                ));
                None
            }
        };
        emit_session_satisfied_for_record(record, "reconcile_detected_merge");
        emit_human_touch_for_record(record);
        ledger_backstop_leg(record, post_entries, ledger.as_deref(), stderr_log);
        let bots = required_bots_for(record.cwd.as_deref());
        emit_gate_escape_for_record(record, &bots, None);
        closed_rows.push(json!({
            "node_id": record.node_id,
            "pr_number": record.pr_number,
            "pr_url": record.pr_url,
            "plan_stamped": stamped,
            "sentinel": sentinel,
        }));
        auto_continue(record, post_entries, stderr_log);
    }
    closed_rows
}

/// The ledger backstop (US3): the ledger's only writer is the origin's own
/// finalize, so a killed/reaped origin leaks its row. Stamp a minimal row
/// for the transcript-gone tail; the direct-finalize rung's full row
/// supersedes it via the collapse rule. The graph node already carries the
/// durable per-phase provenance (sessions[] with harness + session_id), so
/// a created row never lands session-less. Best-effort: never aborts the
/// close (AC1-ERR).
fn ledger_backstop_leg(
    record: &MergeDriftRecord,
    post_entries: &[Value],
    ledger: Option<&Path>,
    stderr_log: &mut Vec<String>,
) {
    let Some(ledger) = ledger else { return };
    let node = post_entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(record.node_id.as_str()));
    let project = resolve_current_repo_slug(None).or_else(|| {
        node.and_then(|n| n.get("project"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let sessions: Vec<String> = node
        .and_then(|n| n.get("sessions"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|s| s.get("session_id"))
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let params = json!({
        "ledger_path": ledger.to_string_lossy(),
        "node_id": record.node_id,
        "pr_number": record.pr_number,
        "pr_url": record.pr_url,
        "project": project,
        "merged_at": record.merged_at,
        "plan_path": record.plan_path,
        "node_sessions": sessions,
    });
    if let Err(e) = crate::ledger_axes::ledger_backstop_core(post_entries, &params) {
        stderr_log.push(format!(
            "warning: ledger upsert for {} (PR #{}) failed: {e}; node close unaffected",
            record.node_id, record.pr_number
        ));
    }
}

/// The ledger path for the shelled legs: the checkout's ledger on its
/// space (the Python upsert wrote the same file through paths.ledger_json).
fn ledger_path_for_leg() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Some(crate::paths::ledger_path(&cwd))
}

/// Merge-triggered auto-continue for one closed node: still the Python
/// advance (same-project next, cross-project dependents, contract
/// de-stub), shelled through the same wheel the door forwards to. The
/// advance port deletes this round-trip. Strictly non-fatal.
fn auto_continue(record: &MergeDriftRecord, post_entries: &[Value], stderr_log: &mut Vec<String>) {
    let node = post_entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(record.node_id.as_str()));
    let project = node
        .and_then(|n| n.get("project"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let cwd = node
        .and_then(|n| n.get("cwd"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .map(|raw| effective_reconcile_cwd(&raw, project.as_deref()));
    let py = crate::scrape::fno_py();
    let mut cmd = std::process::Command::new(py);
    cmd.args(["backlog", "advance"]);
    if let Some(project) = &project {
        cmd.args(["--project", project]);
    }
    if let Some(cwd) = &cwd {
        cmd.current_dir(cwd);
    }
    let outcome = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut child = match outcome {
        Ok(child) => child,
        Err(e) => {
            stderr_log.push(format!(
                "warning: auto-continue advance after closing {} failed: {e}",
                record.node_id
            ));
            return;
        }
    };
    // A bounded child: the leg is best-effort and one wedged advance (a gh
    // outage) must not stall the sweep's post-close loop behind it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    stderr_log.push(format!(
                        "warning: auto-continue advance after closing {} exceeded 300s; killed",
                        record.node_id
                    ));
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                stderr_log.push(format!(
                    "warning: auto-continue advance after closing {} failed: {e}",
                    record.node_id
                ));
                return;
            }
        }
    }
}

/// The late legs' results, gathered before the report.
struct LegOutcomes {
    reverted: Vec<Value>,
    sync_catchup: Value,
    claim_reap: Value,
    ledger_harvest: Value,
    epics_waiting: Vec<Value>,
    reclaimed: Vec<(String, String, String)>,
}

/// W4 causal links: a merged "Revert ..." PR referencing a PR carried by a
/// graph node flips that node's `reverted` flag so survival math stops
/// counting it. Full sweep only; strictly non-fatal; misses fall back to
/// `fno backlog update --reverted`.
fn revert_leg(
    entries: &[Value],
    graph_path: &Path,
    full_sweep: bool,
    dry_run: bool,
    stderr_log: &mut Vec<String>,
) -> Vec<Value> {
    if !full_sweep {
        return Vec::new();
    }
    // gh unauthed/offline: reconcile auto-fires on SessionStart, so a
    // degraded gh must stay quiet (manual --reverted remains).
    let merged_prs = super::drift_scan::list_merged_pr_branches(".", 30).unwrap_or_default();
    let pairs = detect_reverted_nodes(&merged_prs, entries);
    if pairs.is_empty() {
        return Vec::new();
    }
    if !dry_run {
        let stamp = pairs.clone();
        let published = graph_store::mutate_rows(
            graph_path,
            std::time::Duration::from_secs(30),
            None,
            None,
            |rows| {
                for (nid, _) in &stamp {
                    let Some(index) = rows
                        .iter()
                        .position(|e| e.get("id").and_then(Value::as_str) == Some(nid.as_str()))
                    else {
                        continue;
                    };
                    if rows[index]
                        .get("reverted")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    if let Some(obj) = rows[index].as_object_mut() {
                        obj.insert("reverted".into(), Value::Bool(true));
                    }
                }
                Ok(true)
            },
        );
        if let Err(e) = published {
            stderr_log.push(format!("warning: revert detection skipped: {e}"));
        }
    }
    pairs
        .into_iter()
        .map(|(nid, pr)| json!({"node_id": nid, "revert_pr": pr}))
        .collect()
}

/// Canonical-sync catch-up: reconcile auto-fires on SessionStart, so when
/// the pr-watch daemon is dead the next interactive session catches the
/// canonical up instead of the outage waiting for a human. Native: the
/// Python shim was one verb_call over this same crate's catchup sweep.
/// --dry-run skips (a preview mutates nothing); a failure never fails the
/// sweep.
fn sync_catchup_leg(dry_run: bool, json_out: bool) -> Value {
    if dry_run {
        return json!({"outcome": "not-run"});
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let out = crate::sync_canonical::run_catchup(&crate::sync_canonical::Deps::real(), &cwd);
    if !json_out {
        for line in &out.stdout {
            println!("{line}");
        }
        for line in &out.stderr {
            eprintln!("{line}");
        }
        if out.outcome != "disabled" && out.outcome != "fresh" {
            eprintln!("sync catch-up: {}", out.outcome);
        }
    }
    json!({
        "outcome": out.outcome,
        "stale": out.stale,
        "pr_number": out.pr_number,
        "swept": out.swept,
        "detail": out.detail,
    })
}

/// Claim GC. This leg reaches every path that fires reconcile (the
/// SessionStart hook and a manual invocation), so a reaper on any one
/// caller would be a guard on one of N reachable paths. --dry-run
/// propagates (one mode contract); best-effort like sync_catchup: a reap
/// error never fails the sweep.
fn claim_reap_leg(dry_run: bool, json_out: bool) -> Value {
    let mut cmd = std::process::Command::new(crate::scrape::fno_py());
    cmd.args(["claims", "reap", "--json"]);
    if !dry_run {
        cmd.arg("--apply");
    }
    let outcome = cmd.output();
    let parsed: Option<Value> = match outcome {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            serde_json::from_str(stdout.trim()).ok()
        }
        Err(_) => None,
    };
    let Some(summary) = parsed else {
        let detail = "the claims reap subprocess did not return JSON".to_string();
        if !json_out {
            eprintln!("warning: claim reap skipped: {detail}");
        }
        return json!({"outcome": "error", "detail": detail});
    };
    if !json_out {
        let count = if dry_run {
            summary
                .get("would_reap")
                .and_then(Value::as_i64)
                .unwrap_or(0)
        } else {
            summary.get("reaped").and_then(Value::as_i64).unwrap_or(0)
        };
        if count > 0 {
            let verb = if dry_run { "would archive" } else { "archived" };
            eprintln!("claim reap: {verb} {count} dead claim(s)");
        }
        let failed = summary
            .get("reap_failed")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !failed.is_empty() {
            // A provably-dead claim whose archive move never completed is
            // not the same as zero dead claims found - the reaped=0 count
            // above stays silent about it, so this must not be gated on
            // count (AC5's "positive marker" rule applies to output too).
            let paths: Vec<String> = failed
                .iter()
                .take(3)
                .filter_map(|f| {
                    f.as_array()
                        .and_then(|pair| pair.first())
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect();
            let more = if failed.len() > 3 { "..." } else { "" };
            eprintln!(
                "warning: claim reap left {} dead claim(s) un-archived (move did not complete): {paths}{more}",
                failed.len(),
                paths = paths.join(", ")
            );
        }
    }
    let mut merged = serde_json::Map::new();
    merged.insert("outcome".into(), json!("ok"));
    if let Value::Object(map) = summary {
        merged.extend(map);
    }
    Value::Object(merged)
}

/// Parent epics of this run's closure claims that are still open, each
/// naming its still-open sibling children exactly - else a PR that ships
/// one of two required nodes reads as silent. Read-only: never mutates.
/// A dry run reads the SIMULATED close when the preview built one.
#[allow(clippy::too_many_arguments)]
fn epics_waiting_leg(
    closure_claims: &[String],
    entries: &[Value],
    sim: Option<&[Value]>,
    dry_run: bool,
    graph_path: &Path,
    json_out: bool,
) -> Vec<Value> {
    if closure_claims.is_empty() {
        return Vec::new();
    }
    let ew_entries: Vec<Value> = if dry_run {
        sim.map(|rows| rows.to_vec())
            .unwrap_or_else(|| entries.to_vec())
    } else {
        read_rows(graph_path).unwrap_or_else(|_| entries.to_vec())
    };
    let mut ew_epics: BTreeSet<String> = BTreeSet::new();
    for cid in closure_claims {
        let parent = ew_entries
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(cid.as_str()))
            .and_then(|e| e.get("parent"))
            .and_then(Value::as_str);
        if let Some(pid) = parent {
            ew_epics.insert(pid.to_string());
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for eid in &ew_epics {
        let epic = ew_entries
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(eid.as_str()));
        let Some(epic) = epic else { continue };
        if !node_is_open(epic) {
            continue; // unknown, or already closed/superseded - nothing outstanding
        }
        let outstanding: Vec<String> = ew_entries
            .iter()
            .filter(|e| {
                e.get("parent").and_then(Value::as_str) == Some(eid.as_str())
                    && node_is_open(e)
                    && e.get("id").and_then(Value::as_str).is_some()
            })
            .filter_map(|e| e.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if outstanding.is_empty() {
            continue;
        }
        if !json_out {
            eprintln!("{eid} still open, waiting on: {}", outstanding.join(", "));
        }
        out.push(json!({"epic": eid, "outstanding": outstanding}));
    }
    out
}

/// Ledger session harvest: fill every execution row's ABSENT `sessions`
/// from its graph node and mark the rest explicitly, before the sweep
/// reports. Reconcile auto-fires on SessionStart, so this lands before any
/// reaping work can consult a row. The counts ride the JSON payload on
/// --json (a stray stdout line breaks json.loads consumers) and a plain
/// line on the human path.
fn harvest_leg(post_entries: &[Value], dry_run: bool, json_out: bool) -> Value {
    let nodes: serde_json::Map<String, Value> = post_entries
        .iter()
        .filter_map(|e| {
            let id = e.get("id").and_then(Value::as_str)?;
            Some((id.to_string(), e.clone()))
        })
        .collect();
    let ledger = ledger_path_for_leg();
    let (filled, marked) = match ledger.as_deref() {
        Some(path) => {
            crate::ledger_axes::harvest_sessions(path, &nodes, dry_run).unwrap_or_else(|e| {
                eprintln!("warning: ledger harvest skipped: {e}");
                (0, 0)
            })
        }
        None => (0, 0),
    };
    if !json_out {
        println!("ledger harvest: filled {filled}, marked {marked}");
    }
    json!({"filled": filled, "marked": marked})
}

/// The dry-run preview: the exact close + cascade + sweep simulated on a
/// throwaway copy so the preview matches a real run, mutating nothing
/// real. `rows` feed the report (kind/id/parent), `carried_stamped` the
/// carried-session count, `sim` the simulated graph the epics-waiting leg
/// prefers over the un-simulated entries.
struct Preview {
    rows: Vec<Value>,
    carried_stamped: Vec<String>,
    sim: Option<Vec<Value>>,
}

fn preview_leg(entries: &[Value], sweep: &Sweep) -> Preview {
    let nothing_pending = sweep.closeable.is_empty()
        && sweep.strandable_epics.is_empty()
        && sweep.strandable_contained.is_empty()
        && sweep.strandable_orphans.is_empty();
    if nothing_pending {
        return Preview {
            rows: Vec::new(),
            carried_stamped: Vec::new(),
            sim: None,
        };
    }
    let mut sim = entries.to_vec();
    let mut acc: Vec<String> = Vec::new();
    let mut contained: Vec<String> = Vec::new();
    let mut reparented: Vec<(String, Option<String>)> = Vec::new();
    for record in &sweep.closeable {
        let Some(index) = sim
            .iter()
            .position(|e| e.get("id").and_then(Value::as_str) == Some(record.node_id.as_str()))
        else {
            continue;
        };
        if sim[index]
            .get("completed_at")
            .map(|v| !v.is_null())
            .unwrap_or(false)
        {
            continue;
        }
        apply_completion_fields(&mut sim[index], false);
        reparented.extend(reparent_live_children(&mut sim, &record.node_id));
        contained.extend(cascade_close_contained(
            &mut sim,
            &record.node_id,
            record.merged_at.as_deref(),
        ));
        acc.extend(cascade_close_parents(&mut sim, &record.node_id));
    }
    if sweep.strandable_epics.is_empty()
        && sweep.strandable_contained.is_empty()
        && sweep.strandable_orphans.is_empty()
    {
        let carried = sweep_stamp_carried_sessions(&mut sim);
        return Preview {
            rows: preview_rows(&acc, &contained, &reparented),
            carried_stamped: carried,
            sim: Some(sim),
        };
    }
    contained.extend(sweep_close_stranded_contained(&mut sim));
    acc.extend(sweep_close_done_epics(&mut sim));
    reparented.extend(sweep_reparent_stranded_orphans(&mut sim));
    let carried = sweep_stamp_carried_sessions(&mut sim);
    Preview {
        rows: preview_rows(&acc, &contained, &reparented),
        carried_stamped: carried,
        sim: Some(sim),
    }
}

/// The report rows, deduped and sorted the way the Python preview's
/// `sorted(set(...))` accs are.
fn preview_rows(
    epics: &[String],
    contained: &[String],
    reparented: &[(String, Option<String>)],
) -> Vec<Value> {
    let epic_ids: BTreeSet<String> = epics.iter().cloned().collect();
    let contained_ids: BTreeSet<String> = contained.iter().cloned().collect();
    let reparent_pairs: BTreeSet<(String, Option<String>)> = reparented.iter().cloned().collect();
    let mut rows: Vec<Value> = Vec::new();
    for id in epic_ids {
        rows.push(json!({"kind": "epic", "id": id}));
    }
    for id in contained_ids {
        rows.push(json!({"kind": "contained", "id": id}));
    }
    for (id, parent) in reparent_pairs {
        rows.push(json!({"kind": "reparent", "id": id, "parent": parent}));
    }
    rows
}

/// The receipt: --json payload or the human summary, with the exit code
/// (4 when any PR read failed: a partial failure, not a clean sweep).
#[allow(clippy::too_many_arguments)]
fn report_leg(
    args: &Args,
    sweep: &Sweep,
    close: Option<&CloseResult>,
    closed_rows: &[Value],
    preview: Option<&Preview>,
    promise_held: &[HeldRow],
    promise_warnings: &[Value],
    closure_claims: &[String],
    closure_bound: &[String],
    closure_refused: &Option<String>,
    open_bound: &[Value],
    open_advisories: &[String],
    supersession_unverified: &[Value],
    outcomes: &LegOutcomes,
    _full_sweep: bool,
) -> i32 {
    let healed_epics: Vec<String> = close
        .map(|c| c.cascade_closed.clone())
        .unwrap_or_default()
        .into_iter()
        .chain(
            preview
                .map(|p| {
                    p.rows
                        .iter()
                        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("epic"))
                        .filter_map(|r| r.get("id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default(),
        )
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    let contained_closed: Vec<String> = close
        .map(|c| c.contained_closed.clone())
        .unwrap_or_else(|| {
            preview
                .map(|p| {
                    p.rows
                        .iter()
                        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("contained"))
                        .filter_map(|r| r.get("id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        })
        .into_iter()
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    let reparented: Vec<(String, Option<String>)> = close
        .map(|c| c.reparented.clone())
        .unwrap_or_else(|| {
            preview
                .map(|p| {
                    p.rows
                        .iter()
                        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("reparent"))
                        .filter_map(|r| {
                            let nid = r.get("id").and_then(Value::as_str)?.to_string();
                            let parent =
                                r.get("parent").and_then(Value::as_str).map(str::to_string);
                            Some((nid, parent))
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .into_iter()
        .collect::<BTreeSet<(String, Option<String>)>>()
        .into_iter()
        .collect();
    let carried_stamped: Vec<String> = close
        .map(|c| c.carried_stamped.clone())
        .unwrap_or_else(|| {
            preview
                .map(|p| p.carried_stamped.clone())
                .unwrap_or_default()
        })
        .into_iter()
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    let blocked_by_settlement: Vec<Value> = close
        .map(|c| c.settlement_receipts.clone())
        .unwrap_or_default();
    if args.json_out {
        let payload = json!({
            "dry_run": args.dry_run,
            "closure_claims": closure_claims,
            "closure_bound": closure_bound,
            "closure_refused": closure_refused,
            "open_pr_bound": open_bound,
            "open_binding_advisories": open_advisories,
            "supersession_unverified": supersession_unverified,
            "blocked_by_settlement": blocked_by_settlement,
            "epics_waiting": outcomes.epics_waiting,
            "candidates": sweep.closeable.iter().map(|r| json!({
                "node_id": r.node_id, "pr_number": r.pr_number,
                "pr_url": r.pr_url, "plan_path": r.plan_path,
                "merge_sha": r.merge_sha,
            })).collect::<Vec<_>>(),
            "closed": closed_rows,
            "healed_epics": healed_epics,
            "reparented": reparented.iter().map(|(nid, p)| json!({
                "node_id": nid, "parent": p,
            })).collect::<Vec<_>>(),
            "reclaimed": outcomes.reclaimed.iter().map(|(nid, before, after)| json!({
                "node_id": nid, "from": before, "to": after,
            })).collect::<Vec<_>>(),
            "contained_closed": contained_closed,
            "carried_stamped": carried_stamped,
            "contained_errors": close.map(|c| c.contained_errors.clone()).unwrap_or_default(),
            "reverted": outcomes.reverted,
            "sync_catchup": outcomes.sync_catchup,
            "claim_reap": outcomes.claim_reap,
            "ledger_harvest": outcomes.ledger_harvest,
            "failures": sweep.failures.iter().map(|r| json!({
                "node_id": r.node_id, "pr_number": r.pr_number,
                "error": r.error, "kind": r.error_kind, "remedy": r.remedy,
            })).collect::<Vec<_>>(),
            "promise_unmet": promise_held.iter().filter(|(_, _, o)| o == "promise_unmet").map(|(n, r, _)| json!({
                "node_id": n, "reason": r,
            })).collect::<Vec<_>>(),
            "promise_unknown": promise_held.iter().filter(|(_, _, o)| o == "promise_unknown").map(|(n, r, _)| json!({
                "node_id": n, "reason": r,
            })).collect::<Vec<_>>(),
            "reopen_held": promise_held.iter().filter(|(_, _, o)| o == "reopen_held").map(|(n, r, _)| json!({
                "node_id": n, "reason": r,
            })).collect::<Vec<_>>(),
            "promise_warnings": promise_warnings,
            "supersession_evidence_failures": sweep.owed_failures,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        emit_auto_continue_tick(closed_rows.len(), healed_epics.len(), sweep.failures.len());
        if !sweep.failures.is_empty() || !sweep.owed_failures.is_empty() {
            return 4;
        }
        return 0;
    }
    let in_sync = sweep.closeable.is_empty()
        && sweep.failures.is_empty()
        && sweep.strandable_epics.is_empty()
        && sweep.strandable_contained.is_empty()
        && healed_epics.is_empty()
        && contained_closed.is_empty()
        && carried_stamped.is_empty()
        && outcomes.reverted.is_empty()
        && promise_held.is_empty()
        && promise_warnings.is_empty()
        && sweep.owed_failures.is_empty()
        && closure_claims.is_empty()
        && outcomes.reclaimed.is_empty()
        && blocked_by_settlement.is_empty();
    if in_sync {
        println!("No merged-PR drift found. Backlog is in sync.");
        return 0;
    }
    let retro_dir = retro_pending_dir();
    let (out_lines, err_lines) = human_lines(
        args,
        sweep,
        closed_rows,
        &healed_epics,
        &contained_closed,
        &reparented,
        &carried_stamped,
        promise_held,
        promise_warnings,
        outcomes,
        retro_dir.as_deref(),
        &blocked_by_settlement,
    );
    for line in out_lines {
        println!("{line}");
    }
    for line in err_lines {
        eprintln!("{line}");
    }
    if !sweep.failures.is_empty() || !sweep.owed_failures.is_empty() {
        return 4;
    }
    0
}

/// Auto-continue heartbeat, only from a scheduled context: a SessionStart
/// reconcile must not mask an unloaded agent with a fresh row.
fn emit_auto_continue_tick(closed: usize, healed: usize, failures: usize) {
    let scheduler = std::env::var("FNO_CONTROL_PLANE_SCHEDULER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let Some(scheduler) = scheduler else { return };
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Some(state) = crate::agents_config::state_dir(&cwd) else {
        return;
    };
    let events = state.join("events.jsonl");
    let journal = crate::loop_runtime::Journal::new_raw(events.clone(), events);
    crate::tick_ledger::emit_tick(
        &journal,
        "auto_continue",
        &scheduler,
        closed as u64,
        if closed == 0 {
            Some("no-web-merges")
        } else {
            None
        },
        Some(&format!(
            "closed={closed} healed={healed} failures={failures}"
        )),
        1800,
    );
}

/// The human summary, split (stdout lines, stderr lines) the way
/// typer.echo's err flag split it: the close receipts print to stdout, the
/// warnings and the held roll to stderr.
#[allow(clippy::too_many_arguments)]
fn human_lines(
    args: &Args,
    sweep: &Sweep,
    closed_rows: &[Value],
    healed_epics: &[String],
    contained_closed: &[String],
    reparented: &[(String, Option<String>)],
    carried_stamped: &[String],
    promise_held: &[HeldRow],
    promise_warnings: &[Value],
    outcomes: &LegOutcomes,
    retro_dir: Option<&Path>,
    blocked_by_settlement: &[Value],
) -> (Vec<String>, Vec<String>) {
    let mut out: Vec<String> = Vec::new();
    let mut err: Vec<String> = Vec::new();
    if args.dry_run {
        if !sweep.closeable.is_empty() {
            out.push(format!(
                "Would close {} node(s) (dry-run, nothing mutated):",
                sweep.closeable.len()
            ));
        }
        for r in &sweep.closeable {
            out.push(
                format!(
                    "  {}  PR #{} MERGED  {}",
                    r.node_id,
                    r.pr_number,
                    r.pr_url.clone().unwrap_or_default()
                )
                .trim_end()
                .to_string(),
            );
        }
        if !contained_closed.is_empty() {
            // "those PRs" only reads correctly when a PR actually drifted
            // this run. On a heal-only sweep `closeable` is empty, so the
            // 0-header above says nothing-to-do directly over a line saying
            // otherwise.
            let whose = if sweep.closeable.is_empty() {
                "already-merged delivery units"
            } else {
                "those PRs"
            };
            out.push(format!(
                "Would close {} contained node(s) shipped inside {whose}: {}",
                contained_closed.len(),
                contained_closed.join(", ")
            ));
        }
        if !healed_epics.is_empty() {
            out.push(format!(
                "Would self-heal {} container epic(s): {}",
                healed_epics.len(),
                healed_epics.join(", ")
            ));
        }
        if !reparented.is_empty() {
            out.push(reparent_receipt_line(reparented, "Would "));
        }
        for (node_id, before, after) in &outcomes.reclaimed {
            out.push(format!("Would reclaim {node_id}: {before} -> {after}"));
        }
    } else {
        // Suppressed ONLY on a heal-only sweep (no drift candidates at
        // all), where a bare "Closed 0 node(s):" sits above a line saying
        // nodes were closed. With candidates present, "Closed 0" is real
        // signal - it says every one of them was already closed between
        // the scan and the lock.
        if !closed_rows.is_empty() || !sweep.closeable.is_empty() {
            out.push(format!("Closed {} node(s):", closed_rows.len()));
        }
        for c in closed_rows {
            let stamp_note = if c["plan_stamped"].as_bool().unwrap_or(false) {
                " (plan stamped)"
            } else {
                ""
            };
            out.push(format!(
                "  {}  PR #{}{}",
                c["node_id"].as_str().unwrap_or("?"),
                c["pr_number"].as_i64().unwrap_or(0),
                stamp_note
            ));
        }
        if !closed_rows.is_empty() {
            let retro_note = retro_dir
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "(state root)".to_string());
            out.push(format!("Retro sentinels written under {retro_note}"));
        }
        if !contained_closed.is_empty() {
            // Same wording rule as the dry-run branch: with no drift this
            // sweep there are no "those PRs" to point at, and "Also"
            // implies a preceding close that did not happen.
            let lead = if closed_rows.is_empty() {
                "Closed"
            } else {
                "Also closed"
            };
            let whose = if closed_rows.is_empty() {
                "already-merged delivery units"
            } else {
                "those PRs"
            };
            out.push(format!(
                "{lead} {} contained node(s) shipped inside {whose} (cost stays on the delivery unit): {}",
                contained_closed.len(),
                contained_closed.join(", ")
            ));
        }
        if !carried_stamped.is_empty() {
            out.push(format!(
                "Recorded the shipping session on {} node(s) carried in another node's PR: {}",
                carried_stamped.len(),
                carried_stamped.join(", ")
            ));
        }
        if !healed_epics.is_empty() {
            out.push(format!(
                "Auto-closed {} container epic(s) (all children complete): {}",
                healed_epics.len(),
                healed_epics.join(", ")
            ));
        }
        if !reparented.is_empty() {
            // Bare-lead shape: groom's _reconcile_leg_outcome parses this line.
            out.push(reparent_receipt_line(reparented, ""));
        }
        for (node_id, before, after) in &outcomes.reclaimed {
            out.push(format!("reclaimed {node_id}: {before} -> {after}"));
        }
    }
    if !blocked_by_settlement.is_empty() {
        out.push(summarize_edge_settlement(blocked_by_settlement));
    }
    if !promise_held.is_empty() {
        // Held open, not failed: either the plan promised more than merged,
        // or the ship count could not be read. summarize_promise_held
        // splits them.
        err.push(summarize_promise_held(promise_held, args.dry_run));
    }
    if !promise_warnings.is_empty() {
        err.push("Promise ship-count warnings:".into());
        for warning in promise_warnings {
            err.push(format!(
                "  {}: {}",
                warning["node_id"].as_str().unwrap_or("?"),
                warning["warning"].as_str().unwrap_or("")
            ));
        }
    }
    if !sweep.owed_failures.is_empty() {
        err.push("Supersession evidence reads failed:".into());
        for failure in &sweep.owed_failures {
            err.push(format!(
                "  {} PR #{}: {}\n    {}",
                failure["successor"].as_str().unwrap_or("?"),
                failure["pr_number"].as_str().unwrap_or("?"),
                failure["error"].as_str().unwrap_or(""),
                failure["remedy"].as_str().unwrap_or("")
            ));
        }
    }
    if !outcomes.reverted.is_empty() {
        let verb = if args.dry_run {
            "Would stamp"
        } else {
            "Stamped"
        };
        out.push(format!(
            "{verb} {} node(s) reverted:",
            outcomes.reverted.len()
        ));
        for rev in &outcomes.reverted {
            out.push(format!(
                "  {}  revert PR #{}",
                rev["node_id"].as_str().unwrap_or("?"),
                rev["revert_pr"].as_i64().unwrap_or(0)
            ));
        }
    }
    if !sweep.failures.is_empty() {
        err.push(format!(
            "{} node(s) could not be resolved:",
            sweep.failures.len()
        ));
        for r in &sweep.failures {
            err.push(format!(
                "  {}  PR #{}: {}",
                r.node_id,
                r.pr_number,
                r.error.clone().unwrap_or_default()
            ));
            if let Some(remedy) = &r.remedy {
                err.push(format!("    {remedy}"));
            }
        }
    }
    (out, err)
}

/// One line per batch. Bare lead is the past tense whose line start groom
/// parses (`^re-parented N stranded child`); "Would " previews instead.
fn reparent_receipt_line(pairs: &[(String, Option<String>)], lead: &str) -> String {
    let verb = if lead.is_empty() {
        "re-parented"
    } else {
        "re-parent"
    };
    let listed: Vec<String> = pairs
        .iter()
        .map(|(cid, p)| format!("{cid} -> {}", p.clone().unwrap_or_else(|| "(none)".into())))
        .collect();
    format!(
        "{lead}{verb} {} stranded child(ren) under terminal parents: {}",
        pairs.len(),
        listed.join(", ")
    )
}

/// The persisted-vs-derived status drift (the _status_drift twin): the
/// full sweep's `reclaimed` roll. A blocked row whose blockers are gone
/// derives unblocked, and the persisted copy only says blocked - that
/// gap is what this leg names. Read-only.
fn status_drift(entries: &[Value]) -> Vec<(String, String, String)> {
    let blocked_truthy = |e: &Value| {
        e.get("blocked_by")
            .map(|v| {
                v.as_array().map(|a| !a.is_empty()).unwrap_or(false)
                    || v.as_str().map(|s| !s.is_empty()).unwrap_or(false)
            })
            .unwrap_or(false)
    };
    let mut persisted: HashMap<String, String> = HashMap::new();
    for e in entries {
        let (Some(id), Some(status)) = (
            e.get("id").and_then(Value::as_str),
            e.get("status").and_then(Value::as_str),
        ) else {
            continue;
        };
        if status == "blocked" && blocked_truthy(e) {
            continue;
        }
        persisted.insert(id.to_string(), status.to_string());
    }
    let mut derived_rows = entries.to_vec();
    // The store keeper supplies each row's plan rung on every write; the
    // reclaim derivation needs the same supply, or a plan-less node keeps
    // its stored status instead of deriving the ladder's idea rung.
    let rungs: std::collections::BTreeMap<String, String> = entries
        .iter()
        .filter_map(|e| {
            crate::graph_store::entry_id(e).map(|id| {
                (
                    id.to_string(),
                    crate::backlog_ready::plan_rung(e).to_string(),
                )
            })
        })
        .collect();
    crate::graph_store::recompute_statuses_with_plan_rungs(&mut derived_rows, Some(&rungs));
    let mut out: Vec<(String, String, String)> = Vec::new();
    for e in &derived_rows {
        let (Some(id), Some(status)) = (
            e.get("id").and_then(Value::as_str),
            e.get("status").and_then(Value::as_str),
        ) else {
            continue;
        };
        if let Some(before) = persisted.get(id) {
            if before != status {
                out.push((id.to_string(), before.clone(), status.to_string()));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_reads_the_documented_surface_and_refuses_strangers() {
        let a = parse_args(&tail(&["--dry-run", "--node", "ab-1234abcd", "-J"])).expect("parses");
        assert!(a.dry_run && a.json_out);
        assert_eq!(a.node.as_deref(), Some("ab-1234abcd"));
        let a = parse_args(&tail(&["--pr-number", "77", "--repo", "o/r"])).expect("parses");
        assert_eq!(a.pr_number, Some(77));
        assert_eq!(a.repo.as_deref(), Some("o/r"));
        assert!(parse_args(&tail(&["--bogus"])).is_none());
        assert!(parse_args(&tail(&["--pr-number", "notanint"])).is_none());
        assert!(
            parse_args(&["--help".to_string()]).is_none(),
            "help is the door's arm"
        );
    }

    #[test]
    fn scan_scope_node_wins_and_a_bare_run_sweeps_the_graph() {
        let entries = Vec::new();
        let scope = scan_scope(
            &entries,
            Some("ab-1234abcd"),
            Some(7),
            None,
            &[],
            None,
            true,
            &mut Vec::new(),
        )
        .expect("node scope");
        assert_eq!(scope.len(), 1);
        assert!(scope.contains("ab-1234abcd"));
        assert!(scan_scope(&entries, None, None, None, &[], None, true, &mut Vec::new()).is_none());
    }

    #[test]
    fn scan_scope_pr_number_scopes_to_claims_and_same_repo_refs() {
        let entries = vec![
            json!({"id": "ab-claim01", "status": "in_review"}),
            json!({"id": "ab-samerep", "status": "in_review", "pr_number": 7,
                   "pr_url": "https://github.com/o/r/pull/7"}),
            json!({"id": "ab-otherre", "status": "in_review", "pr_number": 7,
                   "pr_url": "https://github.com/other/repo/pull/7"}),
        ];
        let scope = scan_scope(
            &entries,
            None,
            Some(7),
            Some("https://github.com/o/r/pull/7"),
            &["ab-claim01".to_string()],
            Some("o/r"),
            true,
            &mut Vec::new(),
        )
        .expect("pr scope");
        assert!(scope.contains("ab-claim01"));
        assert!(scope.contains("ab-samerep"));
        assert!(
            !scope.contains("ab-otherre"),
            "a cross-repo same-number ref never joins the scope"
        );
    }

    #[test]
    fn status_drift_names_only_the_rows_where_persisted_and_derived_diverge() {
        let entries = vec![
            // A stale in_progress row whose PR makes the ladder say in_review.
            json!({"id": "ab-drift01", "status": "in_progress", "pr_number": 9,
                   "pr_url": "https://github.com/o/r/pull/9"}),
            // Already consistent: no row.
            json!({"id": "ab-fine01", "status": "idea"}),
            // Blocked with live blockers reads blocked both ways: no row.
            json!({"id": "ab-blk001", "status": "blocked", "blocked_by": ["ab-fine01"]}),
        ];
        let drift = status_drift(&entries);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert_eq!(drift[0].0, "ab-drift01");
        assert_eq!(drift[0].1, "in_progress");
        assert_eq!(drift[0].2, "in_review");
    }

    #[test]
    fn summarize_promise_held_splits_the_roll_by_outcome() {
        let held = vec![
            (
                "ab-a".to_string(),
                "short one ship".to_string(),
                "promise_unmet".to_string(),
            ),
            (
                "ab-b".to_string(),
                "gh timed out".to_string(),
                "promise_unknown".to_string(),
            ),
            (
                "ab-c".to_string(),
                "reopened after PR #1 merged: fix forward".to_string(),
                "reopen_held".to_string(),
            ),
        ];
        let text = summarize_promise_held(&held, false);
        assert!(text.contains("Held 1 node(s) open (merged PR, unmet plan promise):"));
        assert!(
            text.contains("Held 1 node(s) open (ship count unconfirmed, retryable read failure):")
        );
        assert!(text.contains("Held 1 node(s) open (deliberate reopen postdates the merge):"));
        assert!(text.contains("  ab-a: short one ship"));
        let dry = summarize_promise_held(&held, true);
        assert!(dry.starts_with("Holding 1 node(s) open"));
    }

    #[test]
    fn reparent_receipt_line_leads_read_would_and_past() {
        let pairs = vec![("x-kid".to_string(), None)];
        assert_eq!(
            reparent_receipt_line(&pairs, "Would "),
            "Would re-parent 1 stranded child(ren) under terminal parents: x-kid -> (none)"
        );
        assert_eq!(
            reparent_receipt_line(&pairs, ""),
            "re-parented 1 stranded child(ren) under terminal parents: x-kid -> (none)"
        );
    }
}

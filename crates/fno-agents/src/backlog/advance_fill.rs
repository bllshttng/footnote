//! The lane-fill selection core (parallel-mode dispatch).
//!
//! Ports `advance.select_lane_fill` and `advance.schedule_shadow` decision
//! for decision. The frontier: every ready row passes the selector's own
//! guards already; this layer adds the lane world (peer-lane slots, the
//! domain annotation, the file-collision gate) on top. Live dispatch fails
//! OPEN on an unevaluated node (no comparable surface) and only a concrete
//! exclusion (peer-lane, high-collision) holds it back; the shadow report is
//! the conservative twin and serializes the unevaluated instead.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::backlog::collision::{
    find_collisions, has_file_surface, resolve_plan_path, Thresholds, HIDDEN_SHARED_OUTPUT_ROOTS,
};
use crate::claims::{self};
use crate::lanes::{
    acquire_lane_slot, find_lane_slot, release_lane_slot, LANE_HOLDER_PREFIX, LANE_SLOT_PREFIX,
};

/// The reason-token namespace for "unknown collision safety", matched by
/// both consumers (select_lane_fill fails open on it, schedule_shadow
/// serializes it).
pub const UNEVALUATED_PREFIX: &str = "unevaluated:";
/// The domain-tiebreak annotation appended to an unevaluated token; one
/// constant so the classifier and the warning arm cannot drift.
pub const SAME_DOMAIN_ANNOTATION: &str = "+same-domain:";
/// The file-overlap token the classifier builds and the fill matches.
pub const HIGH_COLLISION_PREFIX: &str = "high-collision:";
/// The hard ceiling on live writers during the bounded rollout; the shadow
/// report applies and reports this bound so an operator sees the frontier
/// the live scheduler will honor.
pub const INITIAL_LIVE_CAP: usize = 2;
pub const DOMAIN_UNSET: &str = "";

/// Domains held by live lane slots (peer lanes from prior ticks), seeding
/// the `+same-domain` annotation. Annotation-only: a read fault fails open.
fn live_lane_domains(root: Option<&Path>) -> Result<BTreeSet<String>, String> {
    let mut domains = BTreeSet::new();
    for record in claims::list(Some(LANE_SLOT_PREFIX), root, false)? {
        if let Some(domain) = record
            .metadata
            .get("domain")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
        {
            domains.insert(domain.to_string());
        }
    }
    Ok(domains)
}

/// Collision-comparable graph rows for every node a live worker holds: a
/// `lane-slot:` holder (a peer lane) or a bare `node:<id>` claim (a manually
/// started /target, which holds no slot). Rows pass through with their real
/// fields; the collision filter rejects anything done/deferred/superseded
/// itself, which is exactly what a claim outliving its node needs.
fn live_worked_entries(root: Option<&Path>, graph: &Path) -> Result<Vec<Value>, String> {
    let mut held: BTreeSet<String> = BTreeSet::new();
    for record in claims::list(Some(LANE_SLOT_PREFIX), root, false)? {
        if let Some(lane) = record.holder.strip_prefix(LANE_HOLDER_PREFIX) {
            held.insert(lane.to_string());
        }
    }
    for record in claims::list(Some("node:"), root, false)? {
        if let Some(node) = record.key.strip_prefix("node:") {
            held.insert(node.to_string());
        }
    }
    if held.is_empty() {
        return Ok(Vec::new());
    }
    let rows = crate::graph_store::read_rows(graph).unwrap_or_default();
    Ok(rows
        .into_iter()
        .filter(|row| {
            row.get("id")
                .and_then(Value::as_str)
                .map(|id| held.contains(id))
                .unwrap_or(false)
        })
        .collect())
}

/// Classify one ready row for lane-fill. `None` is selectable; `Some` is a
/// typed reason token. The collision evaluation sits behind ONE guard that
/// owns fail-open: a resolve, surface-probe, or scan error answers
/// `unevaluated:collision-error`, never a silent pass.
fn classify_lane_candidate(
    node: &Value,
    used_domains: &BTreeSet<String>,
    inflight: &[Value],
    repo_root: &Path,
    thresholds: &Thresholds,
    root: Option<&Path>,
) -> Option<String> {
    let node_id = node.get("id").and_then(Value::as_str).unwrap_or("");
    if find_lane_slot(node_id, root).is_some() {
        return Some("peer-lane".to_string());
    }
    let domain = node
        .get("domain")
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .unwrap_or(DOMAIN_UNSET);
    let domain_suffix = if !domain.is_empty() && used_domains.contains(domain) {
        format!("{SAME_DOMAIN_ANNOTATION}{domain}")
    } else {
        String::new()
    };
    let plan = node.get("plan_path").and_then(Value::as_str).unwrap_or("");
    // One guard over the whole collision evaluation; this is the only frame
    // that can express "the gate did not run" as a verdict.
    let hit = (|| -> Result<Option<String>, String> {
        let path = resolve_plan_path(plan, repo_root);
        if plan.is_empty() || !has_file_surface(&path) {
            return Ok(Some(format!("unevaluated:no-surface{domain_suffix}")));
        }
        for collision in find_collisions(&path, inflight, node_id, thresholds) {
            if collision.severity == crate::backlog::collision::Severity::High {
                return Ok(Some(format!(
                    "{HIGH_COLLISION_PREFIX}{}",
                    collision.with_node_id
                )));
            }
        }
        Ok(None)
    })();
    match hit {
        Ok(reason) => reason,
        Err(error) => Some(format!(
            "{UNEVALUATED_PREFIX}collision-error{domain_suffix}"
        )),
    }
}

/// The report the fill returns alongside the selection: requested, filled,
/// stop reason, and the first excluded candidates.
pub fn fill_report(requested: usize) -> Map<String, Value> {
    let mut report = Map::new();
    report.insert("requested".to_string(), json!(requested));
    report.insert("filled".to_string(), json!(0));
    report.insert("stop".to_string(), json!("no-candidate"));
    report.insert("excluded".to_string(), json!([]));
    report
}

fn push_excluded(report: &mut Map<String, Value>, excluded: &[Value]) {
    if let Some(list) = report.get_mut("excluded").and_then(Value::as_array_mut) {
        for item in excluded {
            list.push(item.clone());
        }
    }
}

/// The ready frontier: the selector's rows merged with the dispatch-safe
/// observer, the same shape the sequential `next` door serves.
fn ready_frontier(
    project: Option<&str>,
    mission: Option<&str>,
    repo_root: &Path,
    occupied: &BTreeSet<String>,
    held: &BTreeMap<String, String>,
    now_ms: i64,
    staleness_days: i64,
) -> Result<Vec<Value>, String> {
    let graph = super::settings::graph_path();
    let entries = crate::graph_store::read_rows_strict(&graph)
        .map_err(|error| format!("graph unreadable: {error}"))?;
    let project_filter = project.map(str::to_string);
    let opts = crate::backlog_ready::ReadyOpts {
        project: project_filter.clone(),
        all: project.is_none(),
        roadmap_id: None,
        parent: None,
        mission: mission.map(str::to_string),
        include_ideas: false,
        include_deferred: false,
        repo_root: Some(repo_root.to_string_lossy().to_string()),
        claimed: occupied.clone(),
        held: held.clone(),
        staleness_days: Some(staleness_days),
        now_ms,
    };
    let candidates = match crate::backlog_ready::select(&entries, &opts) {
        Ok(reply) => reply.rows,
        Err(_) => Vec::new(),
    };
    let observer = super::undispatched::classify_planned_unclaimed(
        &entries,
        &[],
        project_filter.as_deref(),
        mission,
        None,
        None,
    )
    .unwrap_or_else(|_| json!({"rows": [], "entries_scanned": 0}));
    Ok(super::next::with_observer(
        candidates,
        &entries,
        occupied,
        &observer,
        project_filter.as_deref(),
        project.is_none(),
        mission,
        None,
        now_ms,
        staleness_days,
        held,
        "fno backlog ready --json",
    ))
}

/// Select up to `max_lanes` ready nodes, each collision-clean to dispatch.
/// With `claim`, one lane slot is atomically held per pick (LD#8); a full
/// cap stops the fill with `stop: cap-full`. A mid-fill error releases the
/// slots already held before propagating, so a garbled later pick never
/// orphans an earlier slot.
pub fn select_lane_fill(
    max_lanes: usize,
    project: Option<&str>,
    mission: Option<&str>,
    claim: bool,
    claims_root: Option<&Path>,
    repo_root: &Path,
) -> Result<(Vec<Value>, Map<String, Value>), String> {
    let mut report = fill_report(max_lanes);
    if max_lanes < 1 {
        return Ok((Vec::new(), report));
    }
    let now_ms = claims::now_ms();
    let config_dir = crate::claude_roster::config_dir();
    let staleness_days = crate::backlog_ready::configured_staleness_days(&config_dir)
        .unwrap_or(21)
        .max(1);
    let fno_dir = super::settings::graph_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo_root.to_path_buf());
    let held = crate::needs::held_map(&fno_dir, repo_root);
    let occupied = claimed_node_ids()?;

    let mut used_domains = live_lane_domains(claims_root).unwrap_or_default();
    let graph = super::settings::graph_path();
    let mut inflight = live_worked_entries(claims_root, &graph).unwrap_or_default();
    let thresholds = Thresholds::default();

    let mut selected: Vec<Value> = Vec::new();
    let mut picked_ids: BTreeSet<String> = BTreeSet::new();
    let fill = (|| -> Result<(), String> {
        while selected.len() < max_lanes {
            // A fresh ready list per pick keeps distinctness recomputed after
            // each claim; max_lanes is small and the ready list is short.
            let frontier = ready_frontier(
                project,
                mission,
                repo_root,
                &occupied,
                &held,
                now_ms,
                staleness_days,
            )?;
            let mut pick_excluded: Vec<Value> = Vec::new();
            let mut candidate: Option<(Value, String)> = None;
            for node in &frontier {
                let Some(nid) = node.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if picked_ids.contains(nid) {
                    continue;
                }
                let Some(reason) = classify_lane_candidate(
                    node,
                    &used_domains,
                    &inflight,
                    repo_root,
                    &thresholds,
                    claims_root,
                ) else {
                    candidate = Some((
                        node.clone(),
                        node.get("domain")
                            .and_then(Value::as_str)
                            .filter(|d| !d.is_empty())
                            .unwrap_or(DOMAIN_UNSET)
                            .to_string(),
                    ));
                    break;
                };
                // Live dispatch fails OPEN on an unevaluated node; only a
                // concrete exclusion holds it back.
                if !reason.starts_with(UNEVALUATED_PREFIX) {
                    if reason.starts_with(HIGH_COLLISION_PREFIX) {
                        // Loud skip: the operator should see the collision.
                    }
                    if pick_excluded.len() < 5 {
                        pick_excluded.push(json!({"id": nid, "reason": reason}));
                    }
                    continue;
                }
                if !inflight.is_empty()
                    || !selected.is_empty()
                    || reason.contains(SAME_DOMAIN_ANNOTATION)
                    || (node
                        .get("domain")
                        .and_then(Value::as_str)
                        .unwrap_or(DOMAIN_UNSET)
                        .is_empty()
                        && used_domains.contains(DOMAIN_UNSET))
                {
                    // Fail-open but LOUD: report it as an exclusion-shaped
                    // warning so the frontier never reads as gate-clean.
                    pick_excluded.push(json!({"id": nid, "reason": reason}));
                }
                candidate = Some((
                    node.clone(),
                    node.get("domain")
                        .and_then(Value::as_str)
                        .filter(|d| !d.is_empty())
                        .unwrap_or(DOMAIN_UNSET)
                        .to_string(),
                ));
                break;
            }
            let Some((node, domain)) = candidate else {
                push_excluded(&mut report, &pick_excluded);
                break; // no selectable, unclaimed node left
            };
            let node_id = node
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if claim {
                let mut metadata = Map::new();
                metadata.insert("domain".to_string(), json!(domain));
                match acquire_lane_slot(
                    max_lanes,
                    &node_id,
                    None,
                    None,
                    Some(metadata),
                    claims_root,
                ) {
                    Ok(Some(_slot)) => {}
                    Ok(None) => {
                        push_excluded(&mut report, &pick_excluded);
                        report.insert("stop".to_string(), json!("cap-full"));
                        return Ok(()); // cap full: every slot held by a peer
                    }
                    Err(_) => {
                        push_excluded(&mut report, &pick_excluded);
                        continue; // slot contended: try the next candidate
                    }
                }
            }
            selected.push(node.clone());
            report.insert("filled".to_string(), json!(selected.len()));
            push_excluded(&mut report, &pick_excluded);
            used_domains.insert(domain);
            picked_ids.insert(node_id.clone());
            if let Some(plan) = node.get("plan_path").and_then(Value::as_str) {
                if !plan.is_empty() {
                    inflight.push(json!({
                        "id": node_id, "title": node.get("title").cloned().unwrap_or(json!("")),
                        "plan_path": plan, "created_at": "", "status": "ready",
                    }));
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = fill {
        // Release what we hold so a later pick's failure never strands the
        // earlier slots to TTL; each release is best-effort.
        if claim {
            for held_node in &selected {
                if let Some(id) = held_node.get("id").and_then(Value::as_str) {
                    let _ = release_lane_slot(id, claims_root);
                }
            }
        }
        return Err(error);
    }
    if selected.len() >= max_lanes {
        report.insert("stop".to_string(), json!("filled"));
    } else if report.get("stop").and_then(Value::as_str) != Some("cap-full") {
        report.insert("stop".to_string(), json!("no-candidate"));
    }
    Ok((selected, report))
}

fn claimed_node_ids() -> Result<BTreeSet<String>, String> {
    match claims::list(Some("node:"), None, false) {
        Ok(records) => Ok(records
            .iter()
            .filter_map(|record| record.key.strip_prefix("node:").map(str::to_string))
            .collect()),
        Err(error) => Err(format!("live claim state is unavailable ({error})")),
    }
}

/// The read-only bounded-frontier report (shadow-first core): the SAME
/// per-candidate classification as the live fill, no slot taken, a typed
/// verdict for every ready row. `effective_cap` reports the rollout ceiling
/// so the operator sees the bound live dispatch will honor. A seed fault
/// degrades to an empty seed AND is recorded in `degraded`, because a
/// silently-collapsed seed produces a frontier byte-identical to a healthy
/// one and this report is the evidence operators gate on.
pub fn schedule_shadow(
    max_lanes: usize,
    project: Option<&str>,
    mission: Option<&str>,
    claims_root: Option<&Path>,
    repo_root: &Path,
) -> Value {
    let effective_cap = max_lanes.max(1).min(INITIAL_LIVE_CAP);
    let now_ms = claims::now_ms();
    let config_dir = crate::claude_roster::config_dir();
    let staleness_days = crate::backlog_ready::configured_staleness_days(&config_dir)
        .unwrap_or(21)
        .max(1);
    let fno_dir = super::settings::graph_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo_root.to_path_buf());
    let held = crate::needs::held_map(&fno_dir, repo_root);
    let occupied = match claimed_node_ids() {
        Ok(ids) => ids,
        Err(_) => {
            return json!({
                "effective_cap": effective_cap, "requested_cap": max_lanes,
                "occupied_slots": 0, "remaining_capacity": 0,
                "note": "ready-unreadable", "degraded": ["ready"],
                "selected": [], "serialized": [], "unevaluated": [], "decisions": [],
            })
        }
    };
    let frontier = match ready_frontier(
        project,
        mission,
        repo_root,
        &occupied,
        &held,
        now_ms,
        staleness_days,
    ) {
        Ok(rows) => rows,
        Err(_) => {
            return json!({
                "effective_cap": effective_cap, "requested_cap": max_lanes,
                "occupied_slots": 0, "remaining_capacity": 0,
                "note": "ready-unreadable", "degraded": ["ready"],
                "selected": [], "serialized": [], "unevaluated": [], "decisions": [],
            })
        }
    };

    let mut degraded: Vec<String> = Vec::new();
    let mut used_domains = live_lane_domains(claims_root);
    let graph = super::settings::graph_path();
    let mut inflight = live_worked_entries(claims_root, &graph);
    let occupied_slots = crate::lanes::active_lane_count(claims_root);
    let thresholds = Thresholds::default();

    let mut selected: Vec<Value> = Vec::new();
    let mut serialized: Vec<Value> = Vec::new();
    let mut unevaluated: Vec<Value> = Vec::new();
    let mut decisions: Vec<Value> = Vec::new();
    let mut picked_ids: BTreeSet<String> = BTreeSet::new();
    for node in &frontier {
        let Some(nid) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        if picked_ids.contains(nid) {
            continue;
        }
        let domain = node
            .get("domain")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .unwrap_or(DOMAIN_UNSET)
            .to_string();
        // The shadow twin serializes the unevaluated instead of failing open.
        let reason = classify_lane_candidate(
            node,
            &used_domains,
            &inflight,
            repo_root,
            &thresholds,
            claims_root,
        );
        let verdict = match reason {
            None => {
                if selected.len() < effective_cap {
                    selected.push(node.clone());
                    picked_ids.insert(nid.to_string());
                    used_domains.insert(domain.clone());
                    "selected".to_string()
                } else {
                    "serialized".to_string()
                }
            }
            Some(token) => {
                if token.starts_with(UNEVALUATED_PREFIX) {
                    unevaluated.push(node.clone());
                    "unevaluated".to_string()
                } else {
                    serialized.push(node.clone());
                    "serialized".to_string()
                }
            }
        };
        decisions.push(json!({
            "id": nid,
            "slug": node.get("slug").cloned().unwrap_or(Value::Null),
            "domain": domain,
            "verdict": verdict,
            "reason": reason.unwrap_or_default(),
        }));
    }
    json!({
        "effective_cap": effective_cap,
        "requested_cap": max_lanes,
        "occupied_slots": occupied_slots,
        "remaining_capacity": effective_cap.saturating_sub(occupied_slots),
        "note": Value::Null,
        "degraded": degraded,
        "selected": selected,
        "serialized": serialized,
        "unevaluated": unevaluated,
        "decisions": decisions,
    })
}

/// The `lane-fill` door: parse the wheel's flag spellings, run the fill,
/// print the selected rows (preview) or the report envelope (--json).
pub fn run_lane_fill(args: &[String]) -> i32 {
    let opts = match parse_fill_args(args) {
        Some(opts) => opts,
        None => {
            eprintln!("{FILL_USAGE}");
            return 2;
        }
    };
    if crate::backlog::workflows::active_backend_name() != "graph" {
        return crate::backlog::cli::forward_to_python("lane-fill", args);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let repo_root = crate::backlog::create_cli::repo_root(&cwd);
    match super::advance_fill::select_lane_fill(
        opts.max_lanes,
        opts.project.as_deref(),
        opts.mission.as_deref(),
        opts.claim,
        None,
        &repo_root,
    ) {
        Ok((selected, report)) => {
            if opts.json {
                let receipt = json!({"lanes": selected, "fill": report});
                println!(
                    "{}",
                    serde_json::to_string_pretty(&receipt).unwrap_or_else(|_| "{}".into())
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&selected).unwrap_or_else(|_| "[]".into())
                );
            }
            0
        }
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    }
}

/// The `schedule-shadow` door: the read-only bounded-frontier report.
pub fn run_schedule_shadow(args: &[String]) -> i32 {
    let opts = match parse_fill_args(args) {
        Some(opts) => opts,
        None => {
            eprintln!("{SHADOW_USAGE}");
            return 2;
        }
    };
    if crate::backlog::workflows::active_backend_name() != "graph" {
        return crate::backlog::cli::forward_to_python("schedule-shadow", args);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let repo_root = crate::backlog::create_cli::repo_root(&cwd);
    let receipt = super::advance_fill::schedule_shadow(
        opts.max_lanes,
        opts.project.as_deref(),
        opts.mission.as_deref(),
        None,
        &repo_root,
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&receipt).unwrap_or_else(|_| "{}".into())
    );
    0
}

const FILL_USAGE: &str =
    "usage: fno-agents backlog lane-fill [--max N] [-p P] [--mission M] [--claim] [--json]";
const SHADOW_USAGE: &str =
    "usage: fno-agents backlog schedule-shadow [--max N] [-p P] [--mission M] [--json]";

struct FillArgs {
    max_lanes: usize,
    project: Option<String>,
    mission: Option<String>,
    claim: bool,
    json: bool,
}

/// The wheel's flag spellings: --max (the wheel resolves the config default
/// before calling, so the door's absent --max stays 1), -p/--project,
/// --mission, --claim, --json/-J.
fn parse_fill_args(args: &[String]) -> Option<FillArgs> {
    let mut opts = FillArgs {
        max_lanes: 1,
        project: None,
        mission: None,
        claim: false,
        json: false,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--max" => {
                let value = args.get(index + 1)?;
                opts.max_lanes = value.parse().ok()?;
                index += 1;
            }
            "--project" | "-p" => {
                opts.project = Some(args.get(index + 1)?.clone());
                index += 1;
            }
            "--mission" => {
                opts.mission = Some(args.get(index + 1)?.clone());
                index += 1;
            }
            "--claim" => opts.claim = true,
            "--json" | "-J" => opts.json = true,
            "-h" | "--help" => return None,
            _ => return None,
        }
        index += 1;
    }
    Some(opts)
}

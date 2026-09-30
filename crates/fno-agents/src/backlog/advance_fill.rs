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
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::backlog::collision::{find_collisions, has_file_surface, resolve_plan_path, Thresholds};
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
        Err(_) => Some(format!(
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
    let mut used_domains = match live_lane_domains(claims_root) {
        Ok(domains) => domains,
        Err(_) => {
            degraded.push("lanes".to_string());
            BTreeSet::new()
        }
    };
    let graph = super::settings::graph_path();
    let inflight = match live_worked_entries(claims_root, &graph) {
        Ok(rows) => rows,
        Err(_) => {
            degraded.push("collision".to_string());
            Vec::new()
        }
    };
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
        let verdict = match &reason {
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
    match select_lane_fill(
        opts.max_lanes,
        opts.project.as_deref(),
        opts.mission.as_deref(),
        opts.claim,
        None,
        Path::new(&repo_root),
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
    let receipt = schedule_shadow(
        opts.max_lanes,
        opts.project.as_deref(),
        opts.mission.as_deref(),
        None,
        Path::new(&repo_root),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Isolated claims root: the dir that CONTAINS `.fno/claims`.
    fn sandbox(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fno-fill-test-{name}-{}-{}",
            std::process::id(),
            crate::claims::now_ms()
        ));
        fs::create_dir_all(dir.join(".fno")).unwrap();
        dir
    }

    /// Declare the claims root for the guard: claim_dirs consults the
    /// global side even when the test passes an explicit root, and the
    /// undeclared $HOME fallback panics under test (the finalize.rs
    /// save/set/restore pattern).
    ///
    /// cargo runs a module's tests on parallel threads and the pin rides the
    /// process ENV, so unpinned siblings race the reader: one test's list()
    /// resolved while a sibling had re-pinned the global arm and its
    /// first-wins merge displaced a seeded slot (CI 2026-09-30: a two-slot
    /// seeding read back as one domain). The lock serializes every
    /// pin-to-restore span; the guard rides the return value so Rust ties
    /// release to the same scope that ends the test.
    static CLAIMS_ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn declare_claims_root(
        dir: &Path,
    ) -> (
        std::sync::MutexGuard<'static, ()>,
        Option<std::ffi::OsString>,
    ) {
        let guard = CLAIMS_ROOT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prior = std::env::var_os("FNO_CLAIMS_ROOT");
        std::env::set_var("FNO_CLAIMS_ROOT", dir);
        (guard, prior)
    }

    fn restore_claims_root(
        guard: std::sync::MutexGuard<'static, ()>,
        prior: Option<std::ffi::OsString>,
    ) {
        match prior {
            Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
            None => std::env::remove_var("FNO_CLAIMS_ROOT"),
        }
        drop(guard);
    }

    fn plan_with_files(dir: &Path, name: &str, files: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let rows: String = files
            .iter()
            .map(|f| format!("| `{f}` | modify |\n"))
            .collect();
        fs::write(
            &path,
            format!("# P\n\n## Files to Modify\n\n| File | Action |\n|---|---|\n{rows}"),
        )
        .unwrap();
        path
    }

    fn node(id: &str, plan: &str, domain: &str) -> Value {
        let mut node = json!({"id": id});
        if !plan.is_empty() {
            node["plan_path"] = json!(plan);
        }
        if !domain.is_empty() {
            node["domain"] = json!(domain);
        }
        node
    }

    #[test]
    fn peer_lane_holds_back_its_node() {
        let dir = sandbox("peer-lane");
        let (lock, prior) = declare_claims_root(&dir);
        let root = Some(dir.as_path());
        acquire_lane_slot(2, "ab-held0001", None, None, None, root)
            .unwrap()
            .unwrap();
        let verdict = classify_lane_candidate(
            &node("ab-held0001", "", "code"),
            &BTreeSet::new(),
            &[],
            &dir,
            &Thresholds::default(),
            root,
        );
        assert_eq!(verdict.as_deref(), Some("peer-lane"));
        restore_claims_root(lock, prior);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_surface_answers_the_unevaluated_fail_open_token() {
        let dir = sandbox("no-surface");
        let (lock, prior) = declare_claims_root(&dir);
        let root = Some(dir.as_path());
        // An empty plan and a plan_path that resolves to nothing are both
        // "the gate did not run", never a silent pass: the token is loud.
        let missing = dir.join("missing.md").to_string_lossy().to_string();
        for plan in ["", missing.as_str()] {
            let verdict = classify_lane_candidate(
                &node("ab-open0001", plan, ""),
                &BTreeSet::new(),
                &[],
                &dir,
                &Thresholds::default(),
                root,
            );
            assert_eq!(
                verdict.as_deref(),
                Some("unevaluated:no-surface"),
                "plan {plan:?}"
            );
        }
        restore_claims_root(lock, prior);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn same_domain_annotation_joins_the_token() {
        let dir = sandbox("same-domain");
        let (lock, prior) = declare_claims_root(&dir);
        let root = Some(dir.as_path());
        let mut used = BTreeSet::new();
        used.insert("code".to_string());
        let verdict = classify_lane_candidate(
            &node("ab-anno0001", "", "code"),
            &used,
            &[],
            &dir,
            &Thresholds::default(),
            root,
        );
        assert_eq!(
            verdict.as_deref(),
            Some("unevaluated:no-surface+same-domain:code")
        );
        restore_claims_root(lock, prior);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn high_collision_holds_back_the_candidate() {
        let dir = sandbox("high-collision");
        let (lock, prior) = declare_claims_root(&dir);
        let root = Some(dir.as_path());
        let candidate = plan_with_files(&dir, "cand.md", &["a.py", "b.py", "c.py", "d.py"]);
        let other = plan_with_files(&dir, "other.md", &["a.py", "b.py", "c.py", "z.py"]);
        let inflight = vec![json!({
            "id": "ab-other001", "title": "Other", "status": "ready",
            "plan_path": other.to_string_lossy(), "created_at": "2026-01-02",
        })];
        let verdict = classify_lane_candidate(
            &node("ab-cand0001", &candidate.to_string_lossy(), ""),
            &BTreeSet::new(),
            &inflight,
            &dir,
            &Thresholds::default(),
            root,
        );
        assert_eq!(verdict.as_deref(), Some("high-collision:ab-other001"));
        restore_claims_root(lock, prior);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clean_candidate_is_selectable() {
        let dir = sandbox("clean");
        let (lock, prior) = declare_claims_root(&dir);
        let root = Some(dir.as_path());
        // A selectable candidate has a REAL file surface with nothing in
        // flight against it; an empty plan answers the unevaluated token,
        // which the live fill fails open on but the classifier must report.
        let plan = plan_with_files(&dir, "clean.md", &["clean-only.py"]);
        let verdict = classify_lane_candidate(
            &node("ab-clean001", &plan.to_string_lossy(), "code"),
            &BTreeSet::new(),
            &[],
            &dir,
            &Thresholds::default(),
            root,
        );
        assert!(verdict.is_none(), "surface with no collision is selectable");
        restore_claims_root(lock, prior);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_lane_domains_reads_slot_domains() {
        let dir = sandbox("domains");
        let (lock, prior) = declare_claims_root(&dir);
        let root = Some(dir.as_path());
        for (lane, domain) in [("ab-dom0001", "code"), ("ab-dom0002", "docs")] {
            let mut metadata = serde_json::Map::new();
            metadata.insert("domain".to_string(), json!(domain));
            acquire_lane_slot(4, lane, None, None, Some(metadata), root)
                .unwrap()
                .unwrap();
        }
        let domains = live_lane_domains(root).unwrap();
        assert_eq!(
            domains,
            BTreeSet::from(["code".to_string(), "docs".to_string()])
        );
        restore_claims_root(lock, prior);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn live_worked_entries_joins_slots_and_node_claims() {
        let dir = sandbox("worked");
        let (lock, prior) = declare_claims_root(&dir);
        let root = Some(dir.as_path());
        acquire_lane_slot(2, "ab-lane0001", None, None, None, root)
            .unwrap()
            .unwrap();
        let outcome = crate::claims::acquire(
            "node:ab-node0002",
            "manual-target",
            crate::claims::AcquireOpts {
                root: Some(dir.clone()),
                events_dir: Some(dir.clone()),
                ..Default::default()
            },
        );
        assert!(matches!(
            outcome,
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        // The .json anchor: seed_rows and read_rows resolve the physical
        // store from the anchor, so both sides must name the same file.
        let graph = dir.join("graph.json");
        crate::graph_store::seed_rows(
            &graph,
            &[
                json!({"id": "ab-lane0001", "title": "Lane", "status": "in_progress"}),
                json!({"id": "ab-node0002", "title": "Node", "status": "in_progress"}),
                json!({"id": "ab-free0003", "title": "Free", "status": "ready"}),
            ],
        )
        .unwrap();
        let rows = live_worked_entries(root, &graph).unwrap();
        let mut ids: Vec<&str> = rows
            .iter()
            .filter_map(|row| row.get("id").and_then(Value::as_str))
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["ab-lane0001", "ab-node0002"]);
        restore_claims_root(lock, prior);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fill_report_starts_shaped_and_empty() {
        let report = fill_report(3);
        assert_eq!(report.get("requested"), Some(&json!(3)));
        assert_eq!(report.get("filled"), Some(&json!(0)));
        assert_eq!(report.get("stop"), Some(&json!("no-candidate")));
        assert_eq!(report.get("excluded"), Some(&json!([])));
    }

    #[test]
    fn fill_args_parse_the_wheel_spellings() {
        let parsed = parse_fill_args(&[
            "--max".to_string(),
            "3".to_string(),
            "-p".to_string(),
            "proj".to_string(),
            "--claim".to_string(),
            "-J".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.max_lanes, 3);
        assert_eq!(parsed.project.as_deref(), Some("proj"));
        assert!(parsed.claim);
        assert!(parsed.json);
        assert!(parse_fill_args(&["--bogus".to_string()]).is_none());
    }
}

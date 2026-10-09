//! `fno agents top`: every live worker process with tree RSS, in one process.
//!
//! The rows are the display union the spawn gate counts: the fno registry's
//! live rows plus the claude daemon roster, deduped on the claude short id.
//! Every slow read the Python view paid per row is one batched read here:
//! transcript truth comes from the daemon's in-memory cursors
//! ([`crate::truth_probe::family1_truth_probe_many_measured`]), the bg session
//! pid from the roster's own `replPid` (never an lsof scan), and RSS from one
//! process-table walk. The lead check-in reads [`payload`] in process.

use crate::state::RegistryEntry;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The registry statuses that mean "this run holds a process", the census's
/// `LIVE_STATUSES`. Every other status is terminal.
const LIVE_STATUSES: &[&str] = &["spawning", "ready", "idle", "busy", "live", "restarting"];

/// A stored `spawning` older than this, with live process evidence, is a
/// token that stopped being a measurement (the list projection's rule).
const STALE_SPAWNING_S: i64 = 600;

/// Twelve-minute reconcile runs are measured (`FLIGHT_TTL_MS` in
/// flight_gate.rs); a single-flight hold older than this shows here.
const LONG_HOLD_S: i64 = 12 * 60;

/// Sidechain transcripts older than this are not scanned.
const SUBAGENT_SCAN_WINDOW_S: u64 = 2 * 3600;

/// How much of each journal the pane counter read takes: several 30s
/// snapshots of a busy day, never a multi-MB slurp.
const PANE_COUNTERS_TAIL_BYTES: u64 = 512 * 1024;

const PANE_COUNTER_FIELDS: [&str; 5] = [
    "bytes_in",
    "grid_updates",
    "frames_composited",
    "frames_emitted",
    "cpu_ns",
];

const PREDICATE: &str = "rows are RUNS holding a process (census LIVE_STATUSES); \
per-session liveness is fno agents truth <handle>";

/// One row of the display union, before the truth and cost joins.
#[derive(Debug, Clone, PartialEq)]
struct Worker {
    source: &'static str,
    name: String,
    harness: String,
    substrate: String,
    /// The recorded pid; for a claude bg row this is the PTY host.
    pid: Option<u32>,
    /// The process that IS the session; cost reads this one.
    session_pid: Option<u32>,
    stored_status: String,
    status_basis: Option<&'static str>,
    session_id: Option<String>,
    spawned_by: Option<String>,
    /// Index into the registry rows this worker joins, when it joins one.
    entry: Option<usize>,
}

#[derive(Debug, Default)]
struct Census {
    workers: Vec<Worker>,
    warnings: Vec<String>,
    live_registry_names: HashSet<String>,
}

fn status_word(entry: &RegistryEntry) -> String {
    serde_json::to_value(entry.status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn now_epoch_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn older_than(created_at: &str, seconds: i64, now: i64) -> bool {
    chrono::DateTime::parse_from_rfc3339(created_at)
        .map(|t| now - t.timestamp() > seconds)
        .unwrap_or(false)
}

/// The display union. `pid_alive` and `claim_live` are injected so the
/// liveness rules are testable without real processes or claim files.
fn census(
    roster: &[crate::claude_roster::RosterWorker],
    rows: &[RegistryEntry],
    pid_alive: &dyn Fn(u32, Option<u64>) -> bool,
    claim_live: &dyn Fn(&str) -> bool,
    now: i64,
) -> Census {
    let mut out = Census::default();
    let mut counted: HashSet<String> = HashSet::new();
    let mut seen_sessions: HashSet<&str> = HashSet::new();
    for w in roster {
        if w.session_id.is_empty() || !seen_sessions.insert(&w.session_id) {
            continue;
        }
        let Some(pid) = w.pid.filter(|p| pid_alive(*p, None)) else {
            continue;
        };
        let short = w.short_id().to_string();
        counted.insert(short.clone());
        out.workers.push(Worker {
            source: "claude",
            name: short,
            harness: "claude".into(),
            substrate: "(foreign)".into(),
            pid: Some(pid),
            // The roster names the session's own process; the recorded pid
            // is the PTY host that launched it.
            session_pid: w.repl_pid.or(Some(pid)),
            stored_status: "live".into(),
            status_basis: None,
            session_id: Some(w.session_id.clone()),
            spawned_by: None,
            entry: None,
        });
    }
    let roster_live = counted.clone();

    for (index, row) in rows.iter().enumerate() {
        let stored = status_word(row);
        if !LIVE_STATUSES.contains(&stored.as_str()) {
            continue;
        }
        let harness = row.harness.clone().unwrap_or_default();
        let pid_alive_now = row.pid.is_some_and(|p| pid_alive(p, row.pid_start_time));
        let bg_alive = row.pid.is_none()
            && harness == "claude"
            && !row.short_id.is_empty()
            && roster_live.contains(&row.short_id);
        // A codex thread lane has no local process and no roster row; its
        // live `worker:<name>` slot claim is the liveness oracle.
        let claim_alive = !pid_alive_now && !bg_alive && claim_live(&row.name);
        if !(pid_alive_now || bg_alive || claim_alive) {
            continue;
        }
        out.live_registry_names.insert(row.name.clone());
        if !row.short_id.is_empty() && counted.contains(&row.short_id) {
            // Already shown as its roster row: the lead this fno row
            // attributes the cost to rides onto that row.
            if let Some(shown) = out
                .workers
                .iter_mut()
                .find(|w| w.source == "claude" && w.name == row.short_id)
            {
                shown.spawned_by = row.spawned_by_session.clone();
                shown.entry = Some(index);
            }
            continue;
        }
        if !row.short_id.is_empty() {
            counted.insert(row.short_id.clone());
        }
        let substrate = if row.mux.is_some() {
            "pane"
        } else if bg_alive {
            "bg"
        } else {
            "worker"
        };
        let stale_spawning = stored == "spawning"
            && (pid_alive_now || claim_alive)
            && older_than(&row.created_at, STALE_SPAWNING_S, now);
        out.workers.push(Worker {
            source: "fno",
            name: row.name.clone(),
            harness,
            substrate: substrate.into(),
            pid: row.pid,
            session_pid: row.pid,
            stored_status: if stale_spawning {
                "quiet".into()
            } else {
                stored
            },
            status_basis: stale_spawning.then_some("stale-spawning-live-pid"),
            session_id: row.harness_session_id.clone(),
            spawned_by: row.spawned_by_session.clone(),
            entry: Some(index),
        });
    }
    out
}

fn role_label(entry: &RegistryEntry) -> Option<String> {
    entry
        .role_level
        .map(|level| format!("L{level} {}", entry.role_scope.as_deref().unwrap_or("?")))
}

fn opt_str(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |s| json!(s))
}

/// The worker rows: the census joined to truth, the session-to-node map,
/// and the process-table RSS. Heaviest first.
fn worker_rows(
    census: &Census,
    rows: &[RegistryEntry],
    truth: &HashMap<String, crate::truth_probe::TruthProbe>,
    outcome: crate::truth_probe::BatchOutcome,
    sessions: &Map<String, Value>,
    rss: &BTreeMap<u32, u64>,
) -> Vec<Value> {
    let by_session: HashMap<&str, &RegistryEntry> = rows
        .iter()
        .filter_map(|e| e.harness_session_id.as_deref().map(|sid| (sid, e)))
        .collect();
    let mut out: Vec<Value> = census
        .workers
        .iter()
        .map(|w| {
            let entry = w
                .entry
                .map(|i| &rows[i])
                .or_else(|| w.session_id.as_deref().and_then(|s| by_session.get(s).copied()));
            let probe = truth.get(&truth_handle(w, entry));
            let progress = entry.map(|e| {
                crate::daemon::list_rows::progress_from_truth(
                    probe,
                    outcome,
                    &w.harness,
                    e.route_settings_path.as_deref(),
                )
                .0
            });
            let handle = entry.map(|e| e.name.as_str()).filter(|n| *n != w.name);
            let joined = w
                .session_id
                .as_deref()
                .and_then(|s| sessions.get(&s.trim().to_ascii_lowercase()));
            let pid = w.session_pid.or(w.pid);
            json!({
                "source": w.source,
                "name": w.name,
                "handle": handle,
                "harness": w.harness,
                "substrate": w.substrate,
                "lead": w.spawned_by.as_deref().map(|s| s.chars().take(8).collect::<String>()).filter(|s| !s.is_empty()),
                "pid": pid,
                "rss_mb": pid.and_then(|p| rss.get(&p)),
                "status": crate::daemon::list_rows::rendered_status_from_truth(probe),
                "status_age_s": probe.and_then(|p| p.last_activity_age_s),
                "stored_status": w.stored_status,
                "status_basis": w.status_basis,
                "progress": progress,
                "reach": opt_str(probe.and_then(|p| p.reachability.as_deref())),
                "reach_basis": opt_str(probe.and_then(|p| p.basis.as_deref())),
                "node": joined.and_then(|j| j.get("node")).cloned().unwrap_or(Value::Null),
                "node_basis": joined.and_then(|j| j.get("basis")).cloned().unwrap_or(Value::Null),
                "pr": joined.and_then(|j| j.get("pr")).cloned().unwrap_or(Value::Null),
                "pr_basis": joined.and_then(|j| j.get("pr_basis")).cloned().unwrap_or(Value::Null),
                "role": entry.and_then(role_label),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        let rss = |v: &Value| v.get("rss_mb").and_then(Value::as_u64).unwrap_or(0);
        rss(b).cmp(&rss(a))
    });
    out
}

fn truth_handle(w: &Worker, entry: Option<&RegistryEntry>) -> String {
    match entry {
        Some(e) => crate::daemon::list_rows::registry_truth_handle(e),
        None => w.session_id.clone().unwrap_or_else(|| w.name.clone()),
    }
}

/// Per-provider lane occupancy from the gate's own lane reading, never a
/// second registry walk. An unreadable lane renders unreadable, never 0.
fn lane_rows(cwd: &Path, registry: &Path, warnings: &mut Vec<String>) -> Vec<Value> {
    let lanes = match crate::spawn_gate_verb::lanes_answer(cwd, registry, None, warnings) {
        Ok(lanes) => lanes,
        Err(fault) => {
            return vec![json!({
                "provider": null,
                "unreadable": format!("gate probe unreadable: {}: {}", fault.provider, fault.error),
            })]
        }
    };
    let Some(lanes) = lanes.as_object() else {
        return vec![json!({"provider": null, "unreadable": "gate probe unreadable"})];
    };
    let mut providers: Vec<&String> = lanes.keys().collect();
    providers.sort();
    providers
        .into_iter()
        .filter_map(|provider| {
            let lane = lanes.get(provider)?.as_object()?;
            let cap = lane.get("cap").cloned().unwrap_or(Value::Null);
            let Some(live) = lane.get("live").and_then(Value::as_u64) else {
                return Some(json!({
                    "provider": provider, "cap": cap, "holders": [], "count": null,
                    "unreadable": "the gate could not read this lane",
                }));
            };
            let mut holders: Vec<String> = lane
                .get("counted")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|h| h.as_str().map(str::to_string))
                .collect();
            holders.sort();
            let full = cap.as_u64().is_some_and(|c| live >= c);
            Some(json!({
                "provider": provider, "cap": cap, "holders": holders,
                "count": live, "full": full,
            }))
        })
        .collect()
}

fn long_holds(cwd: &Path) -> Result<Value, String> {
    let mut dirs: Vec<PathBuf> = crate::claims_root::global_claims_dir()
        .into_iter()
        .collect();
    for dir in crate::stuck_work::claims_dirs(cwd) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    crate::claims::long_holds(&dirs, LONG_HOLD_S)
}

/// Read-only sidechain rows (claude only): `<projects>/<cwd>/<session>/
/// subagents/agent-*.jsonl` within the scan window, newest first, each
/// verdict `active` inside the live threshold. An absent store is zero rows;
/// an unreadable one is zero rows plus a warning. Display only: a subagent
/// has no pid and no mail transport, so it never enters the census.
fn subagent_section(projects: &Path, live_threshold: u64, now: SystemTime) -> Value {
    let mut warnings: Vec<String> = Vec::new();
    let mut recent: Vec<(SystemTime, PathBuf)> = Vec::new();
    match std::fs::read_dir(projects) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warnings.push(format!(
            "subagents: projects store unreadable ({e}); no rows"
        )),
        Ok(cwd_dirs) => {
            for session_dir in cwd_dirs
                .flatten()
                .filter_map(|d| std::fs::read_dir(d.path()).ok())
                .flat_map(|r| r.flatten())
            {
                let Ok(files) = std::fs::read_dir(session_dir.path().join("subagents")) else {
                    continue;
                };
                for file in files.flatten() {
                    let name = file.file_name();
                    let name = name.to_string_lossy();
                    if !name.starts_with("agent-") || !name.ends_with(".jsonl") {
                        continue;
                    }
                    let Ok(mtime) = file.metadata().and_then(|m| m.modified()) else {
                        continue;
                    };
                    let age = now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0);
                    if age <= SUBAGENT_SCAN_WINDOW_S {
                        recent.push((mtime, file.path()));
                    }
                }
            }
        }
    }
    recent.sort_by(|a, b| b.0.cmp(&a.0));
    let mut seen: HashSet<String> = HashSet::new();
    let mut rows: Vec<Value> = Vec::new();
    for (mtime, path) in recent {
        let Some(rec) = first_record(&path) else {
            continue;
        };
        let field = |k: &str| rec.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let mut agent_id = field("agentId");
        if agent_id.is_empty() {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            agent_id = stem.strip_prefix("agent-").unwrap_or("").to_string();
        }
        if agent_id.is_empty() || !seen.insert(agent_id.clone()) {
            continue;
        }
        let mut parent = field("sessionId");
        if parent.is_empty() {
            parent = path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
        }
        let age = now.duration_since(mtime).map(|d| d.as_secs()).unwrap_or(0);
        rows.push(json!({
            "agent_id": agent_id,
            "parent": parent.chars().take(8).collect::<String>(),
            "branch": field("gitBranch"),
            "age": crate::claims::fmt_age_s(age as i64),
            "verdict": if age <= live_threshold { "active" } else { "idle" },
            "cwd": field("cwd"),
        }));
    }
    json!({
        "rows": rows,
        "warnings": warnings,
        "live_threshold": live_threshold,
        "scan_window_h": SUBAGENT_SCAN_WINDOW_S / 3600,
    })
}

fn first_record(path: &Path) -> Option<Value> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file).read_line(&mut line).ok()?;
    serde_json::from_str::<Value>(&line)
        .ok()
        .filter(Value::is_object)
}

/// Difference the last two `mux_pane_counters` snapshots of the
/// journal-latest mux session. The mux emits monotonic totals; a decrease
/// means the server restarted on the same socket, reported born-and-gone,
/// never negative. A broken journal reads `unreadable`, never "no cost".
fn pane_counter_rows(path: &Path) -> Value {
    let empty = |status: &str| json!({"status": status, "rows": [], "born": [], "gone": [], "session": null, "window_s": null});
    let sibling = PathBuf::from(format!(
        "{}{}",
        path.display(),
        crate::events::EPHEMERAL_SUFFIX
    ));
    let rotated = PathBuf::from(format!("{}.1", sibling.display()));
    let mut samples: Vec<Value> = Vec::new();
    for candidate in [path, &rotated, &sibling] {
        let tail = match read_tail(candidate, PANE_COUNTERS_TAIL_BYTES) {
            Ok(Some(tail)) => tail,
            Ok(None) => continue,
            Err(e) => {
                let mut out = empty("unreadable");
                out["error"] = json!(e.to_string());
                return out;
            }
        };
        samples.extend(
            tail.lines()
                .filter(|l| l.contains("\"mux_pane_counters\""))
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|ev| ev.get("type").and_then(Value::as_str) == Some("mux_pane_counters")),
        );
    }
    let mut by_session: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for ev in &samples {
        let session = ev
            .pointer("/data/session")
            .map(Value::to_string)
            .unwrap_or_default();
        by_session.entry(session).or_default().push(ev);
    }
    let ts = |ev: &Value| {
        ev.get("ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let Some(events) = by_session
        .values()
        .max_by_key(|evs| evs.last().map(|e| ts(e)).unwrap_or_default())
        .filter(|evs| evs.len() >= 2)
    else {
        return empty("insufficient-samples");
    };
    let (older, newer) = (events[events.len() - 2], events[events.len() - 1]);
    let panes = |ev: &Value| -> BTreeMap<i64, Value> {
        ev.pointer("/data/panes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p.get("pane_id")?.as_i64()?, p.clone())))
            .collect()
    };
    let (older_panes, newer_panes) = (panes(older), panes(newer));
    let counter = |p: &Value, f: &str| p.get(f).and_then(Value::as_i64).unwrap_or(0);
    let (mut rows, mut born, mut gone) = (Vec::new(), Vec::new(), Vec::new());
    for (id, p) in &newer_panes {
        let Some(q) = older_panes.get(id) else {
            born.push(*id);
            continue;
        };
        if PANE_COUNTER_FIELDS
            .iter()
            .any(|f| counter(p, f) < counter(q, f))
        {
            born.push(*id);
            gone.push(*id);
            continue;
        }
        let mut row = json!({"pane_id": id, "node": p.get("node"), "name": p.get("name"), "cmd": p.get("cmd")});
        for f in PANE_COUNTER_FIELDS {
            row[f] = json!(counter(p, f) - counter(q, f));
        }
        rows.push(row);
    }
    gone.extend(
        older_panes
            .keys()
            .filter(|id| !newer_panes.contains_key(id)),
    );
    born.sort_unstable();
    gone.sort_unstable();
    let secs = |ev: &Value| {
        chrono::DateTime::parse_from_rfc3339(&ts(ev))
            .ok()
            .map(|t| t.timestamp_millis())
    };
    let window_s = match (secs(older), secs(newer)) {
        (Some(a), Some(b)) => json!(((b - a) as f64 / 100.0).round() / 10.0),
        _ => Value::Null,
    };
    json!({
        "status": "ok", "rows": rows, "born": born, "gone": gone,
        "session": newer.pointer("/data/session").cloned().unwrap_or(Value::Null),
        "window_s": window_s,
    })
}

fn read_tail(path: &Path, max: u64) -> std::io::Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let size = file.metadata()?.len();
    if size > max {
        file.seek(SeekFrom::Start(size - max))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if size > max {
        // Drop the partial line the seek landed in.
        text = text
            .split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default();
    }
    Ok(Some(text))
}

/// The `top --json` payload: worker rows, lanes, slot claims, long holds,
/// and the optional subagent and pane-counter sections.
pub fn payload(include_subagents: bool, include_pane_stats: bool) -> Value {
    let home = crate::paths::AgentsHome::from_env();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut warnings: Vec<String> = Vec::new();
    let roster = match crate::claude_roster::ClaudeRoster::load(
        &crate::claude_roster::default_roster_path(),
    ) {
        Ok(r) => r.workers.into_values().collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            warnings.push(format!(
                "top: claude roster unreadable ({e}); counting fno registry only"
            ));
            Vec::new()
        }
    };
    let rows = match crate::state::load_registry(&home.registry_json()) {
        Ok(r) => r.entries,
        Err(e) => {
            warnings.push(format!(
                "top: fno registry unreadable ({e}); registry rows omitted"
            ));
            Vec::new()
        }
    };
    let claims_root = crate::claims_root::global_claims_root();
    let claim_live = |name: &str| {
        matches!(
            crate::claims::status(&format!("worker:{name}"), claims_root.as_deref()).0,
            crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
        )
    };
    let mut c = census(
        &roster,
        &rows,
        &crate::daemon::pid_is_ours,
        &claim_live,
        now_epoch_s(),
    );
    warnings.append(&mut c.warnings);

    let handles: Vec<String> = c
        .workers
        .iter()
        .map(|w| truth_handle(w, w.entry.map(|i| &rows[i])))
        .collect();
    let (truth, outcome) = crate::truth_probe::family1_truth_probe_many_measured(&handles);
    let sessions = crate::session_join::sessions_map();
    let roots: Vec<u32> = c
        .workers
        .iter()
        .filter_map(|w| w.session_pid.or(w.pid))
        .collect();
    let (table, _) = crate::census::process_table_ps();
    let rss = crate::session_cost::tree_rss(&table, &roots);
    let workers = worker_rows(&c, &rows, &truth, outcome, &sessions, &rss);

    let slot_claims = crate::spawn_gate::live_worker_slot_claims(&mut warnings)
        .into_iter()
        .filter(|r| !c.live_registry_names.contains(&r.name))
        .count();
    let lanes = lane_rows(&cwd, &home.registry_json(), &mut warnings);
    let mut out = json!({
        "workers": workers,
        "predicate": PREDICATE,
        "lanes": lanes,
        "slot_claims": slot_claims,
    });
    match long_holds(&cwd) {
        Ok(holds) => {
            out["long_holds"] = holds.get("rows").cloned().unwrap_or(json!([]));
            out["long_hold_lines"] = holds.get("lines").cloned().unwrap_or(json!([]));
        }
        Err(e) => {
            warnings.push(format!("long holds read failed: {e}"));
            out["long_holds_error"] = json!(e);
        }
    }
    if include_subagents {
        let section = subagent_section(
            &crate::claude_drive::claude_projects_dir(),
            crate::subagent_hold::live_threshold(),
            SystemTime::now(),
        );
        warnings.extend(
            section["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|w| w.as_str().map(str::to_string)),
        );
        out["subagents"] = section["rows"].clone();
        out["subagent_scope"] = json!({
            "live_threshold": section["live_threshold"],
            "scan_window_h": section["scan_window_h"],
        });
    }
    if include_pane_stats {
        out["pane_stats"] =
            pane_counter_rows(&crate::decision_index::default_state_path("events.jsonl"));
    }
    out["warnings"] = json!(warnings);
    out
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) if s.is_empty() => "-".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The human table: lanes first (a full lane refuses spawns the table calls
/// healthy), then long holds, the rows, and the optional sections.
pub fn render(p: &Value) -> String {
    let mut out: Vec<String> = Vec::new();
    let list = |key: &str| {
        p.get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    out.extend(list("warnings").iter().map(cell));
    let lanes = list("lanes");
    for r in &lanes {
        if r["provider"].is_null() {
            out.push(format!("LANES  {}", cell(&r["unreadable"])));
            continue;
        }
        let cap = cell(&r["cap"]);
        let (occupancy, verdict) = if r["count"].is_null() {
            (
                format!("?/{cap}"),
                format!("unreadable: {}", cell(&r["unreadable"])),
            )
        } else if r["full"] == json!(true) {
            (format!("{}/{cap}", r["count"]), "FULL".into())
        } else {
            let word = if r["cap"].is_null() { "uncapped" } else { "ok" };
            (format!("{}/{cap}", r["count"]), word.into())
        };
        let mut line = format!(
            "LANES  {:<9} {occupancy:>7}  {verdict}",
            cell(&r["provider"])
        );
        let holders: Vec<String> = r["holders"]
            .as_array()
            .into_iter()
            .flatten()
            .map(cell)
            .collect();
        if !holders.is_empty() {
            line.push_str(&format!("  holders: {}", holders.join(", ")));
        }
        out.push(line);
    }
    if !lanes.is_empty() {
        out.push(String::new());
    }
    let hold_lines = list("long_hold_lines");
    if !hold_lines.is_empty() {
        out.extend(hold_lines.iter().map(cell));
        out.push(String::new());
    }
    out.push(format!(
        "{:<7} {:<24} {:<9} {:<10} {:<9} {:>7} {:>7} {:<8} {:<17} {:<11} STATUS",
        "SOURCE",
        "NAME",
        "HARNESS",
        "SUBSTRATE",
        "LEAD",
        "PID",
        "RSS_MB",
        "NODE",
        "PROGRESS",
        "REACH"
    ));
    let workers = list("workers");
    if workers.is_empty() {
        out.push("no live workers (runs holding a process)".into());
    }
    for r in &workers {
        let mut name = cell(&r["name"]);
        if let Some(role) = r["role"].as_str() {
            name.push_str(&format!(" [{role}]"));
        }
        if let Some(handle) = r["handle"].as_str() {
            name.push_str(&format!(" ={handle}"));
        }
        let mut activity = cell(&r["status"]);
        if let Some(age) = r["status_age_s"].as_f64() {
            activity.push_str(&format!(" {}", crate::claims::fmt_age_s(age as i64)));
        }
        if let Some(basis) = r["status_basis"].as_str() {
            activity.push_str(&format!(" ({basis})"));
        }
        out.push(format!(
            "{:<7} {:<24} {:<9} {:<10} {:<9} {:>7} {:>7} {:<8} {:<17} {:<11} {activity}",
            cell(&r["source"]),
            name,
            cell(&r["harness"]),
            cell(&r["substrate"]),
            cell(&r["lead"]),
            cell(&r["pid"]),
            cell(&r["rss_mb"]),
            cell(&r["node"]),
            cell(&r["progress"]),
            cell(&r["reach"]),
        ));
    }
    if let Some(n) = p["slot_claims"].as_u64().filter(|n| *n > 0) {
        out.push(format!("(+{n} queued headless slot claim(s))"));
    }
    out.push(format!(
        "census: {PREDICATE}. PID/RSS are the process at scan time; REACH reads the \
         transcript (fno agents truth for the full evidence); NODE reads the graph"
    ));
    if let Some(rows) = p["subagents"].as_array() {
        out.push(String::new());
        out.push(format!(
            "subagents (claude only; active = mtime within {}s; older rows age out after {}h)",
            p["subagent_scope"]["live_threshold"], p["subagent_scope"]["scan_window_h"]
        ));
        out.push(format!(
            "{:<16} {:<9} {:<12} {:>5} {:<8} CWD",
            "AGENT", "PARENT", "BRANCH", "AGE", "VERDICT"
        ));
        if rows.is_empty() {
            out.push("none in the scan window (claude only; codex/opencode/agy task layouts not measured)".into());
        }
        for r in rows {
            out.push(format!(
                "{:<16} {:<9} {:<12} {:>5} {:<8} {}",
                cell(&r["agent_id"]),
                cell(&r["parent"]),
                cell(&r["branch"]),
                cell(&r["age"]),
                cell(&r["verdict"]),
                cell(&r["cwd"])
            ));
        }
    }
    if let Some(section) = p.get("pane_stats") {
        out.push(String::new());
        out.extend(render_pane_stats(section));
    }
    out.join("\n")
}

fn render_pane_stats(section: &Value) -> Vec<String> {
    let mut out = vec![
        "pane counters (mux server; monotonic totals differenced over the window)".to_string(),
    ];
    match section["status"].as_str() {
        Some("ok") => {}
        Some("unreadable") => {
            out.push(format!("journal unreadable: {}", cell(&section["error"])));
            return out;
        }
        _ => {
            out.push(
                "insufficient samples: need two mux_pane_counters events in the journal \
                 (the mux emits one per 30s while panes live)"
                    .into(),
            );
            return out;
        }
    }
    if !section["window_s"].is_null() {
        out.push(format!(
            "session {} | window {}s",
            cell(&section["session"]),
            section["window_s"]
        ));
    }
    out.push(format!(
        "{:>5} {:<11} {:<16} {:>10} {:>8} {:>10} {:>8} {:>9}",
        "PANE", "NODE", "NAME", "BYTES_IN", "GRIDS", "COMPOSITED", "EMITTED", "CPU_MS"
    ));
    let rows = section["rows"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        out.push("no pane appeared in both samples".into());
    }
    for r in &rows {
        out.push(format!(
            "{:>5} {:<11} {:<16} {:>10} {:>8} {:>10} {:>8} {:>9.1}",
            cell(&r["pane_id"]),
            cell(&r["node"]),
            cell(&r["name"]),
            cell(&r["bytes_in"]),
            cell(&r["grid_updates"]),
            cell(&r["frames_composited"]),
            cell(&r["frames_emitted"]),
            r["cpu_ns"].as_f64().unwrap_or(0.0) / 1_000_000.0
        ));
    }
    for key in ["born", "gone"] {
        let ids: Vec<String> = section[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(cell)
            .collect();
        if !ids.is_empty() {
            out.push(format!("{key} this window: {}", ids.join(", ")));
        }
    }
    out
}

/// `fno-agents top [--json|-J] [--subagents] [--pane-stats]`.
pub fn run(args: &[String]) -> i32 {
    let mut as_json = false;
    let (mut subagents, mut pane_stats) = (false, false);
    for arg in args {
        match arg.as_str() {
            "--json" | "-J" => as_json = true,
            "--subagents" => subagents = true,
            "--pane-stats" => pane_stats = true,
            "-h" | "--help" => {
                println!(
                    "fno agents top [--json] [--subagents] [--pane-stats]\n\n\
                     Every live worker process (fno registry and the claude roster) with pid, \
                     tree RSS (MB), served status and node. --subagents adds claude sidechains; \
                     --pane-stats adds per-pane mux counter deltas."
                );
                return 0;
            }
            other => {
                eprintln!("fno agents top: unknown flag {other}");
                return 2;
            }
        }
    }
    let p = payload(subagents, pane_stats);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&p).unwrap_or_default());
    } else {
        println!("{}", render(&p));
    }
    0
}

#[cfg(test)]
#[path = "agents_top_tests.rs"]
mod tests;

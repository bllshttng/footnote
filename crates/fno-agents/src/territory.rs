//! The territory fact set : one module owns territory identity,
//! membership, and the drain-target facts.
//!
//! Ownership follows the seam rule (rust-python-seam.md): the active-backlog
//! supervisor is a Rust loop that must not stop, and it owns the drain
//! decision. The whole fact set therefore lives beside it - graph membership
//! (moved here from `king_board::scope`, which calls this home), the live
//! crown list from the registry cache, the workspace project map, the
//! mission's root project from the graph, and the `active_backlog` config
//! block. Python reads the territory receipt only
//! through `fno config active-backlog*`, which prints this module's output.
//!
//! Fail-closed shape: an unreadable source is `TerritoryUnknown` naming the
//! source, never an empty territory list and never a silently-drained scope.

use crate::agents_config::config_lookup;
use crate::king_board::{graph_json_path, project_map};
use crate::paths::AgentsHome;
use crate::state::{load_registry, Registry, RegistryEntry};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Scope compilation (moved from king_board::scope - the board and the drain
// call the same home)
// ---------------------------------------------------------------------------

/// Compile a canonical crown scope into the graph node ids it contains
/// (board.compile_scope_ids).
pub(crate) fn compile_scope_ids(
    scope: &str,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
) -> Result<HashSet<String>, String> {
    compile_territory(scope, entries, projects).map(|(_, ids)| ids)
}

/// The territory-key + membership read (AC1): the canonical scope key
/// alongside the ids it contains. `Err` is the explicit unknown - a scope
/// that is neither a configured project nor a graph epic - never an empty
/// territory, so a capacity/board reader that gets `Err` stops instead of
/// reading the scope as drained or unbounded.
pub(crate) fn compile_territory(
    scope: &str,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
) -> Result<(String, HashSet<String>), String> {
    use crate::king_board::s_str;
    let canonical_scope = |scopes: &[String]| {
        let mut sorted: Vec<String> = scopes.to_vec();
        sorted.sort();
        sorted.dedup();
        sorted.join(",")
    };
    let members: Vec<String> = scope
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if members.is_empty() {
        return Err("a crown needs a scope: name an epic or a project".to_string());
    }
    let projects = projects.clone()?;

    let entry_by_id = |id: &str| {
        entries
            .iter()
            .find(|e| s_str(e, "id").map(|i| i == id).unwrap_or(false))
    };

    // resolve_crown: the (level, canonical) pair, derived together. A
    // multi-member scope is a project portfolio OR a rung-2 SET of epics -
    // the Python twin (king/scope.py) rules both; mixed is a refusal, and an
    // epic-set member must be a live epic exactly as a single-epic scope must.
    let (level_two, canonical): (bool, String) = if members.len() > 1 {
        let mut project_members: Vec<String> = Vec::new();
        let mut epic_members: Vec<String> = Vec::new();
        for m in &members {
            match projects.get(m.as_str()) {
                Some(canon) => project_members.push(canon.clone()),
                None => epic_members.push(m.clone()),
            }
        }
        if !project_members.is_empty() && !epic_members.is_empty() {
            return Err(format!(
                "a multi-scope crown rules PROJECTS or EPICS, never both at once: {}. \
                 Name projects only (a portfolio) or epics only (a set).",
                members
                    .iter()
                    .map(|m| match projects.get(m.as_str()) {
                        Some(canon) => format!("{canon} (a project)"),
                        None => match entry_by_id(m) {
                            Some(e) if s_str(e, "type") == Some("epic") => {
                                format!("{m} (an epic)")
                            }
                            Some(e) => format!(
                                "{m} (a {}, not an epic)",
                                s_str(e, "type").unwrap_or("node")
                            ),
                            None => format!("{m} (not a configured project or a known node)"),
                        },
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !epic_members.is_empty() {
            for m in &epic_members {
                match entry_by_id(m) {
                    None => {
                        return Err(format!(
                            "{m:?} is neither a configured project nor a backlog node; \
                             nothing to reign over (check for a typo)"
                        ))
                    }
                    Some(entry) => {
                        if s_str(entry, "type") != Some("epic") {
                            return Err(format!(
                                "{m:?} is a {}, not an epic. Implementers get no crowns - \
                                 a single node is work, not a territory. Crown the epic \
                                 above it, or its project.",
                                s_str(entry, "type").unwrap_or("node")
                            ));
                        }
                    }
                }
            }
            (true, canonical_scope(&epic_members))
        } else {
            (false, canonical_scope(&project_members))
        }
    } else {
        let raw = members[0].as_str();
        if let Some(canon) = projects.get(raw) {
            (false, canon.clone())
        } else {
            match entry_by_id(raw) {
                None => {
                    return Err(format!(
                        "{raw:?} is neither a configured project nor a backlog node; \
                         nothing to reign over (check for a typo)"
                    ))
                }
                Some(entry) => {
                    if s_str(entry, "type") != Some("epic") {
                        return Err(format!("crown scope {raw:?} is not an epic in the graph"));
                    }
                    (true, raw.to_string())
                }
            }
        }
    };

    if level_two {
        let mut ids: HashSet<String> = HashSet::new();
        // descendants_of: BFS over parent links, cycle-safe. A rung-2 scope
        // is a SET: the board sees the nodes under EVERY member, so the walk
        // starts from each root, not just the first.
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in entries {
            if let (Some(id), Some(parent)) = (s_str(e, "id"), s_str(e, "parent")) {
                children.entry(parent).or_default().push(id);
            }
        }
        let mut frontier: Vec<&str> = Vec::new();
        for root_id in canonical
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match entry_by_id(root_id) {
                None => {
                    return Err(format!(
                        "crown scope {root_id:?} is not an epic in the graph"
                    ))
                }
                Some(entry) if s_str(entry, "type") != Some("epic") => {
                    return Err(format!(
                        "crown scope {root_id:?} is not an epic in the graph"
                    ));
                }
                _ => {}
            }
            ids.insert(root_id.to_string());
            if let Some(next) = children.get(root_id) {
                frontier.extend(next.iter().copied());
            }
        }
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(id) = frontier.pop() {
            if !seen.insert(id) {
                continue;
            }
            ids.insert(id.to_string());
            if let Some(next) = children.get(id) {
                frontier.extend(next.iter().copied());
            }
        }
        return Ok((canonical, ids));
    }

    let project_set: HashSet<String> = canonical
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut ids = HashSet::new();
    for e in entries {
        let Some(id) = s_str(e, "id") else {
            continue;
        };
        let project = s_str(e, "project").unwrap_or("").to_string();
        let canonical_project = projects.get(project.as_str()).unwrap_or(&project);
        if project_set.contains(canonical_project) {
            ids.insert(id.to_string());
        }
    }
    Ok((canonical, ids))
}

/// The canonical comma-joined form of a raw scope spelling: members split on
/// commas, trimmed, sorted, deduped. The same normalization the registry
/// cache readers apply before dedup.
pub(crate) fn canonical_scope(scope: &str) -> String {
    let mut members: Vec<String> = scope
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    members.sort();
    members.dedup();
    members.join(",")
}

// ---------------------------------------------------------------------------
// Territory resolution
// ---------------------------------------------------------------------------

/// Why a territory read failed. Never read as an empty list.
#[derive(Debug, Clone)]
pub struct TerritoryUnknown(pub String);

impl std::fmt::Display for TerritoryUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One territory: the canonical scope key, what it contains, and where its
/// drain roots. `project`/`cwd` are "" when the territory cannot be rooted
/// (the drain assembly skips an unrootable territory; the readout still
/// renders it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Territory {
    /// The canonical comma-joined scope string.
    pub key: String,
    /// The crown rung of the scope's holder: 2 for an epic set, 1 for a
    /// project (portfolio or loose), 0 when the row carried no level.
    pub rung: u8,
    /// No live crown holds the scope; the drain continues regardless.
    pub kingless: bool,
    /// Epic ids at rung 2, project names at rungs 0/1.
    pub members: Vec<String>,
    /// The root project the drain journal roots at: the first member epic's
    /// own project at rung 2, the project itself at rungs 0/1.
    pub project: String,
    /// The root project's workspace path, "" when unmapped.
    pub cwd: String,
}

/// One live crown row: the canonical scope, its rung, its holder's name and
/// the holder's harness session id (the crown-name store binds its record by
/// session id, law d-e952ed19 - never by the mutable row name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crown {
    pub scope: String,
    pub level: u8,
    pub holder: String,
    pub holder_session: Option<String>,
}

/// The `active_backlog` config block, coerced with the Python validator's
/// fail-safe truth table (`fno.config.ActiveBacklogConfig`): a bad scalar
/// drops to its default, an invalid interval disables, an ambiguous map value
/// disables that project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveBacklogFacts {
    /// True, or the per-project map (only clear affirmatives kept).
    pub enabled: Result<bool, HashMap<String, bool>>,
    /// Parsed poll floor; `None` disables the whole feature.
    pub interval_seconds: Option<i64>,
    pub failure_limit: u32,
    pub max_concurrent: u32,
}

fn coerce_affirmative(v: &toml::Value) -> bool {
    match v {
        toml::Value::Boolean(b) => *b,
        toml::Value::String(s) => matches!(s.trim(), "true" | "yes" | "on" | "1"),
        toml::Value::Integer(i) => *i != 0,
        _ => false,
    }
}

fn coerce_positive_int(v: &toml::Value, default: u32) -> u32 {
    match v.as_integer() {
        Some(i) if i > 0 => i as u32,
        _ => default,
    }
}

/// Mirror of the Python `_parse_duration_to_seconds`: `"5m"`/`"30s"`/`"2h"`/
/// `"1d"`, a bare digit string, or an integer; `None` for zero, negative, or
/// unparseable.
pub(crate) fn parse_duration_to_seconds(v: &toml::Value) -> Option<i64> {
    match v {
        toml::Value::Integer(i) => {
            let secs = *i;
            if secs > 0 {
                Some(secs)
            } else {
                None
            }
        }
        toml::Value::String(s) => {
            let s = s.trim();
            if let Ok(secs) = s.parse::<i64>() {
                return if secs > 0 { Some(secs) } else { None };
            }
            // Byte-slice only on a known ASCII unit suffix - never on a bare
            // trailing-byte count, which can land mid multi-byte UTF-8
            // character (e.g. an operator typo like "5é") and panic.
            let (mult, digits) = if let Some(d) = s.strip_suffix('s') {
                (1, d)
            } else if let Some(d) = s.strip_suffix('m') {
                (60, d)
            } else if let Some(d) = s.strip_suffix('h') {
                (3600, d)
            } else if let Some(d) = s.strip_suffix('d') {
                (86400, d)
            } else {
                return None;
            };
            if digits.is_empty() {
                return None;
            }
            let n: i64 = digits.parse().ok()?;
            let secs = n.checked_mul(mult)?;
            if secs > 0 {
                Some(secs)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Read `active_backlog` from the layered config the same way every other
/// Rust getter does (`config_lookup`), coercing with the Python truth table.
pub fn active_backlog_facts(cwd: &Path) -> ActiveBacklogFacts {
    let mut facts = ActiveBacklogFacts {
        enabled: Ok(false),
        interval_seconds: Some(300),
        failure_limit: 3,
        max_concurrent: 1,
    };
    let Some(block) = config_lookup(cwd, &["active_backlog"]) else {
        return facts;
    };
    let table = block.as_table().cloned().unwrap_or_default();
    match table.get("enabled") {
        None => {}
        Some(toml::Value::Boolean(b)) => facts.enabled = Ok(*b),
        Some(toml::Value::Table(map)) => {
            let mut out = HashMap::new();
            for (k, v) in map {
                out.insert(k.clone(), coerce_affirmative(v));
            }
            facts.enabled = Err(out);
        }
        Some(other) => facts.enabled = Ok(coerce_affirmative(other)),
    }
    if let Some(v) = table.get("interval") {
        facts.interval_seconds = parse_duration_to_seconds(v);
    }
    if let Some(v) = table.get("failure_limit") {
        facts.failure_limit = coerce_positive_int(v, 3);
    }
    if let Some(v) = table.get("max_concurrent") {
        facts.max_concurrent = coerce_positive_int(v, 1);
    }
    facts
}

impl ActiveBacklogFacts {
    /// Fail-closed: an invalid interval disables the feature entirely.
    pub fn any_enabled(&self) -> bool {
        if self.interval_seconds.is_none() {
            return false;
        }
        match &self.enabled {
            Ok(b) => *b,
            Err(map) => map.values().any(|v| *v),
        }
    }

    pub fn is_enabled_for(&self, project: Option<&str>) -> bool {
        if self.interval_seconds.is_none() {
            return false;
        }
        match &self.enabled {
            Ok(b) => *b,
            Err(map) => match project {
                None => false,
                Some(p) => map.get(p).copied().unwrap_or(false),
            },
        }
    }
}

/// Live crown scopes from the registry cache, one per DISTINCT canonical
/// scope, in scope order - the read the court and the drain share. A registry
/// read fault is `TerritoryUnknown`, never an empty crown list (the daemon
/// must not read "no kings" out of an unreadable registry).
pub fn live_crowns(registry_path: &Path) -> Result<Vec<Crown>, TerritoryUnknown> {
    let rows: Vec<RegistryEntry> = match load_registry(registry_path) {
        Ok(Registry { entries, .. }) => entries,
        Err(e) => {
            return Err(TerritoryUnknown(format!(
                "territory: registry unreadable ({e})"
            )))
        }
    };
    let mut out: Vec<Crown> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for row in rows {
        let raw = row.crown_scope.as_deref().unwrap_or("").trim();
        if raw.is_empty() || !crate::spawn_gate::status_is_liveish(&row.status) {
            continue;
        }
        let canon = canonical_scope(raw);
        if canon.is_empty() || !seen.insert(canon.clone()) {
            continue;
        }
        let level = row.crown_level.unwrap_or(0).clamp(0, 255) as u8;
        out.push(Crown {
            scope: canon,
            level,
            holder: row.name.clone(),
            holder_session: row.harness_session_id.clone(),
        });
    }
    out.sort_by(|a, b| a.scope.cmp(&b.scope));
    Ok(out)
}

/// Which live crown answers for each node: the deepest crown level whose
/// scope holds the node, then the lowest canonical scope on a tie. A crown
/// whose scope does not compile owns nothing; it comes back in the second
/// list with its reason, and each caller decides whether that blind spot
/// refuses.
pub(crate) fn node_owners(
    crowns: &[Crown],
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
) -> (HashMap<String, String>, Vec<(String, String)>) {
    let mut sorted: Vec<&Crown> = crowns.iter().collect();
    sorted.sort_by(|a, b| b.level.cmp(&a.level).then_with(|| a.scope.cmp(&b.scope)));
    let mut owners: HashMap<String, String> = HashMap::new();
    let mut failures: Vec<(String, String)> = Vec::new();
    for crown in sorted {
        match compile_territory(&crown.scope, entries, projects) {
            Ok((_, ids)) => {
                for id in ids {
                    owners.entry(id).or_insert_with(|| crown.scope.clone());
                }
            }
            Err(e) => failures.push((crown.scope.clone(), e)),
        }
    }
    (owners, failures)
}

/// The workspace project map: canonical project name -> normalized absolute
/// path (mirrors `fno.graph.maintain.load_workspaces`). Best-effort like the
/// Python read: a missing map contributes nothing, and the drain assembly
/// treats an unmapped territory as unrootable.
pub fn workspace_paths(cwd: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(work) = config_lookup(cwd, &["work"]).and_then(|v| v.as_table().cloned()) else {
        return out;
    };
    // Multi-workspace shape: work.workspaces.<ws>.projects[] (an array of
    // tables, the only form the Python iterator accepts).
    if let Some(workspaces) = work.get("workspaces").and_then(|v| v.as_table().cloned()) {
        for (_ws, ws_data) in &workspaces {
            let Some(projects) = ws_data.get("projects").and_then(|p| p.as_array()) else {
                continue;
            };
            for project in projects {
                let Some(table) = project.as_table() else {
                    continue;
                };
                let Some(name) = table.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                if let Some(path) = table.get("path").and_then(|p| p.as_str()) {
                    if !path.is_empty() {
                        out.entry(name.to_string()).or_insert(normalize_path(path));
                    }
                }
            }
        }
    }
    // Legacy flat shape: work.projects.<name>.path.
    if let Some(flat) = work.get("projects").and_then(|v| v.as_table().cloned()) {
        for (name, cfg) in &flat {
            let Some(path) = cfg.get("path").and_then(|p| p.as_str()) else {
                continue;
            };
            if !name.is_empty() && !path.is_empty() {
                out.entry(name.clone())
                    .or_insert_with(|| normalize_path(path));
            }
        }
    }
    out
}

/// `os.path.normpath(expanduser(raw))` for the path shapes settings carry:
/// expand a leading `~`, then collapse `.`/`..`/duplicate separators
/// lexically (no filesystem access, matching normpath).
pub(crate) fn normalize_path(raw: &str) -> String {
    // A bare "~" expands like "~/" with an empty rest, matching Python's
    // os.path.expanduser (which treats both the same), not just the
    // slash-prefixed form.
    let expanded: String = if raw == "~" || raw.starts_with("~/") {
        let rest = raw.strip_prefix('~').unwrap().trim_start_matches('/');
        match std::env::var_os("HOME") {
            Some(home) if rest.is_empty() => home.to_string_lossy().into_owned(),
            Some(home) => format!("{}/{}", home.to_string_lossy(), rest),
            None => raw.to_string(),
        }
    } else {
        raw.to_string()
    };
    let absolute = expanded.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in expanded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                // `..` above the root collapses, matching normpath.
                if !parts.is_empty() && *parts.last().unwrap() != ".." {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// Graph entries at the resolved graph store, or the unknown naming the
/// read. Through the backend switch (`graph_store::read_rows_strict`):
/// under sqlite the file is a frozen mirror, and a boundary answered from
/// it asserts territory the store does not recognize. Strict, because an
/// unreadable graph is unknown, never an empty list.
pub(crate) fn graph_entries(config_cwd: &Path) -> Result<Vec<Value>, TerritoryUnknown> {
    let path = graph_json_path(config_cwd);
    crate::graph_store::read_rows_strict(&path).map_err(|e| {
        TerritoryUnknown(format!(
            "territory: graph unreadable ({}): {e}",
            path.display()
        ))
    })
}

/// One territory per live crown scope, plus one kingless rung-1 territory per
/// workspace project no live project-rung crown rules - the crown list seeds
/// the mission list, machinery never needs a king to dispatch. Territories
/// come back in canonical scope order. A rung-2 territory whose first member
/// epic has no graph entry, or whose root project is unmapped in the
/// workspace, still resolves - with `project`/`cwd` empty - so the readout
/// can name it; the drain assembly skips what it cannot root.
pub fn resolve_territories(
    config_cwd: &Path,
    registry_path: &Path,
) -> Result<Vec<Territory>, TerritoryUnknown> {
    let crowns = live_crowns(registry_path)?;
    let entries = graph_entries(config_cwd)?;
    let paths = workspace_paths(config_cwd);

    let epic_project = |epic_id: &str| -> Option<String> {
        entries
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(epic_id))
            .and_then(|e| e.get("project").and_then(Value::as_str))
            .filter(|p| !p.is_empty())
            .map(str::to_string)
    };

    let mut territories: Vec<Territory> = Vec::new();
    let mut ruled_projects: HashSet<String> = HashSet::new();
    for crown in &crowns {
        let members: Vec<String> = crown
            .scope
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if crown.level != 2 {
            ruled_projects.extend(members.iter().cloned());
        }
        // Root at the first member epic's own project; the converge core
        // fans out across projects at dispatch time.
        let (project, cwd) = if crown.level == 2 {
            match members.first().and_then(|m| epic_project(m)) {
                Some(p) => (p.clone(), paths.get(&p).cloned().unwrap_or_default()),
                None => (String::new(), String::new()),
            }
        } else {
            let p = members.first().cloned().unwrap_or_default();
            let cwd = paths.get(&p).cloned().unwrap_or_default();
            (p, cwd)
        };
        territories.push(Territory {
            key: crown.scope.clone(),
            rung: crown.level,
            kingless: false,
            members,
            project,
            cwd,
        });
    }
    let mut workspace_projects: Vec<&String> = paths.keys().collect();
    workspace_projects.sort();
    for name in workspace_projects {
        if ruled_projects.contains(name) {
            continue;
        }
        let cwd = paths.get(name).cloned().unwrap_or_default();
        territories.push(Territory {
            key: name.clone(),
            rung: 1,
            kingless: true,
            members: vec![name.clone()],
            project: name.clone(),
            cwd,
        });
    }
    // Python-order contract: crowned territories in scope order first, then
    // the kingless loose territories in project order (the readout renders
    // rows in exactly this order).
    Ok(territories)
}

/// `fno-agents territory-rows`: the AC7 projection as JSON on stdout.
/// Daemon-free, invoked by the config passthroughs and the status fold.
pub fn run_territory_rows(args: &[String]) -> i32 {
    let _ = args;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = AgentsHome::from_env().registry_json();
    let rows = territory_rows(&cwd, &registry);
    println!(
        "{}",
        serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
    );
    0
}

/// `fno-agents active-backlog-receipt`: the drain reading as JSON on stdout,
/// the object shape (`targets`/`missions`/`skip_reason`). Exit 1 with
/// the reason on stderr when a source is unreadable - `unknown` must never
/// print as an empty list. Invoked by the `fno config active-backlog`
/// passthrough and rank's dispatcher note.
pub fn run_active_backlog_receipt(args: &[String]) -> i32 {
    let _ = args;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = AgentsHome::from_env().registry_json();
    let report = crate::active_backlog::resolve_targets_report(&cwd, &registry);
    if let Some(failure) = &report.failure {
        eprintln!("active-backlog: {failure}");
        return 1;
    }
    let reading = serde_json::json!({
        "targets": report.targets,
        "missions": report.missions,
        "skip_reason": report.skip_reason,
    });
    println!(
        "{}",
        serde_json::to_string(&reading)
            .unwrap_or_else(|_| "{\"targets\":[],\"missions\":0}".to_string())
    );
    0
}

// ---------------------------------------------------------------------------
// The readout projection (AC7)
// ---------------------------------------------------------------------------

/// The per-territory cap: `agents.max_live_per_territory`, default 4, coerced
/// like every other positive-int knob.
pub fn territory_cap(config_cwd: &Path) -> u32 {
    config_lookup(config_cwd, &["agents", "max_live_per_territory"])
        .map(|v| coerce_positive_int(&v, 4))
        .unwrap_or(4)
}

/// One readout row per territory: scope, membership state, rung, kingless
/// state, crown holder, mission, and live count against the cap. The
/// projection the status payload, the hidden config
/// verb, the king check-in, and the operational probe share, so none of them
/// can disagree. Fail-safe per row: an unreadable source shrinks that row's
/// answer (membership "unknown", live null), never the whole
/// projection.
pub fn territory_rows(config_cwd: &Path, registry_path: &Path) -> Vec<Value> {
    let cap = territory_cap(config_cwd);
    let territories = match resolve_territories(config_cwd, registry_path) {
        Ok(t) => t,
        Err(TerritoryUnknown(reason)) => {
            return vec![json!({"membership": "unknown", "reason": reason, "cap": cap})]
        }
    };
    // One registry parse feeds holders and the owner rule alike.
    let crowns = live_crowns(registry_path).unwrap_or_default();
    let holders: HashMap<String, String> = crowns
        .iter()
        .map(|c| (c.scope.clone(), c.holder.clone()))
        .collect();
    let entries = graph_entries(config_cwd).unwrap_or_default();
    let live = crate::spawn_gate::live_rows(registry_path, &mut Vec::new());
    let live_nodes: Vec<Option<&str>> = live.iter().map(|r| r.node.as_deref()).collect();

    // Exclusive membership: a node counts for the one live crown that owns
    // it - the deepest crown holding it, the same rule `node_owners` gives
    // the spawn gate and the court - so a worker can never cost two
    // territories at once. An unowned node stays loose.
    let (owners, _) = node_owners(&crowns, &entries, &project_map(config_cwd));

    let memberships: Vec<(&Territory, Result<(String, HashSet<String>), String>)> = territories
        .iter()
        .map(|territory| {
            (
                territory,
                compile_territory(&territory.key, &entries, &project_map(config_cwd)),
            )
        })
        .collect();

    memberships
        .into_iter()
        .map(|(territory, compiled)| {
            let (membership, ids) = match compiled {
                Ok((_, ids)) => ("ok", ids),
                Err(_) => ("unknown", HashSet::new()),
            };
            let mut ids = ids;
            if membership == "ok" {
                if territory.kingless {
                    ids.retain(|id| !owners.contains_key(id));
                } else {
                    ids.retain(|id| owners.get(id) == Some(&territory.key));
                }
            }
            let live_count = if membership == "ok" {
                Some(
                    live_nodes
                        .iter()
                        .filter_map(|n| *n)
                        .filter(|n| ids.contains(*n))
                        .count(),
                )
            } else {
                None
            };
            json!({
                "scope": territory.key,
                "membership": membership,
                "rung": territory.rung,
                "kingless": territory.kingless,
                "holder": holders.get(&territory.key),
                "mission": if territory.rung == 2 { territory.members.first() } else { None },
                "live": live_count,
                "cap": cap,
            })
        })
        .collect()
}

#[cfg(test)]
mod moved_scope_tests {
    use super::*;
    use serde_json::json;
    fn node(id: &str, status: &str, priority: &str) -> Value {
        json!({"id": id, "slug": id, "status": status, "priority": priority, "type": "feature"})
    }

    #[test]
    fn scope_compiles_an_epic_to_itself_plus_descendants() {
        let entries = vec![
            json!({"id": "x-epic", "type": "epic", "status": "ready", "priority": "p1"}),
            json!({"id": "x-ch1d", "parent": "x-epic", "status": "ready", "priority": "p1"}),
            json!({"id": "x-gr2d", "parent": "x-ch1d", "status": "ready", "priority": "p1"}),
            json!({"id": "x-outs", "status": "ready", "priority": "p1"}),
        ];
        let projects = Ok(HashMap::new());
        let ids = compile_scope_ids("x-epic", &entries, &projects).unwrap();
        assert!(ids.contains("x-epic"));
        assert!(ids.contains("x-ch1d"));
        assert!(ids.contains("x-gr2d"));
        assert!(!ids.contains("x-outs"));
    }

    #[test]
    fn scope_compiles_projects_by_the_project_field() {
        let entries = vec![
            json!({"id": "x-aaaa", "project": "fno", "status": "ready", "priority": "p1"}),
            json!({"id": "x-bbbb", "project": "other", "status": "ready", "priority": "p1"}),
        ];
        let mut map = HashMap::new();
        map.insert("fno".to_string(), "fno".to_string());
        let ids = compile_scope_ids("fno", &entries, &Ok(map)).unwrap();
        assert!(ids.contains("x-aaaa"));
        assert!(!ids.contains("x-bbbb"));
    }

    #[test]
    fn a_non_epic_single_scope_is_refused() {
        let entries = vec![node("x-aaaa", "ready", "p1")];
        let projects = Ok(HashMap::new());
        let err = compile_scope_ids("x-aaaa", &entries, &projects).unwrap_err();
        assert!(err.contains("not an epic"), "{err}");
    }

    #[test]
    fn a_multi_scope_crown_of_epics_compiles_as_the_union() {
        // The Python twin (king/scope.py) rules a rung-2 SET of epics: the
        // board sees the nodes under EVERY member, not just the first. This
        // twin refused any multi-scope that was not all projects, so a set
        // crown never resolved its board.
        let entries = vec![
            json!({"id": "e-1", "type": "epic", "status": "ready", "priority": "p1"}),
            json!({"id": "e-1a", "parent": "e-1", "status": "ready", "priority": "p1"}),
            json!({"id": "e-2", "type": "epic", "status": "ready", "priority": "p1"}),
            json!({"id": "e-2a", "parent": "e-2", "status": "ready", "priority": "p1"}),
            json!({"id": "x-outs", "status": "ready", "priority": "p1"}),
        ];
        let projects = Ok(HashMap::new());
        let ids = compile_scope_ids("e-2,e-1", &entries, &projects).unwrap();
        for expected in ["e-1", "e-1a", "e-2", "e-2a"] {
            assert!(ids.contains(expected), "missing {expected}: {ids:?}");
        }
        assert!(!ids.contains("x-outs"));
    }

    #[test]
    fn a_mixed_project_and_epic_multi_scope_is_refused() {
        let entries =
            vec![json!({"id": "e-1", "type": "epic", "status": "ready", "priority": "p1"})];
        let mut map = HashMap::new();
        map.insert("alpha".to_string(), "alpha".to_string());
        let err = compile_scope_ids("alpha,e-1", &entries, &Ok(map)).unwrap_err();
        assert!(err.contains("never both at once"), "{err}");
    }

    #[test]
    fn a_multi_scope_member_that_is_not_an_epic_is_refused() {
        let entries = vec![
            json!({"id": "e-1", "type": "epic", "status": "ready", "priority": "p1"}),
            json!({"id": "n-1", "type": "feature", "status": "ready", "priority": "p1"}),
        ];
        let projects = Ok(HashMap::new());
        let err = compile_scope_ids("e-1,n-1", &entries, &projects).unwrap_err();
        assert!(err.contains("not an epic"), "{err}");
    }

    #[test]
    fn territory_membership_returns_the_canonical_key_with_the_ids() {
        let entries = vec![
            json!({"id": "x-epic", "type": "epic", "status": "ready", "priority": "p1"}),
            json!({"id": "x-ch1d", "parent": "x-epic", "status": "ready", "priority": "p1"}),
        ];
        let projects = Ok(HashMap::new());
        let (key, ids) = compile_territory("x-epic", &entries, &projects).unwrap();
        assert_eq!(key, "x-epic");
        assert!(ids.contains("x-epic") && ids.contains("x-ch1d"));
    }

    #[test]
    fn territory_membership_err_is_the_explicit_unknown_not_an_empty_set() {
        let entries = vec![node("x-aaaa", "ready", "p1")];
        let projects = Ok(HashMap::new());
        let err = compile_territory("x-aaaa", &entries, &projects).unwrap_err();
        assert!(err.contains("not an epic"), "{err}");
    }
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use serde_json::json;

    /// Interlocked, restoring env scope: holds the shared env lock and
    /// restores the variables these tests pin on drop, so an assert-panic
    /// mid-test cannot leak a pin toward a deleted tempdir into every later
    /// test in the process.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn take() -> Self {
            let lock = crate::claims::test_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let saved = ["FNO_CONFIG", "FNO_HOME"]
                .iter()
                .map(|var| (*var, std::env::var_os(var)))
                .collect();
            Self { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (var, saved) in &self.saved {
                match saved {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }

    fn env_guard() -> EnvGuard {
        EnvGuard::take()
    }

    fn write_fixture(
        dir: &Path,
        config: &str,
        graph: Value,
        registry_rows: Value,
    ) -> (PathBuf, PathBuf) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("config.toml"), config).unwrap();
        crate::graph_store::seed_rows(
            &dir.join("graph.json"),
            graph["entries"].as_array().unwrap(),
        )
        .unwrap();
        let registry_path = dir.join("registry.json");
        std::fs::write(&registry_path, registry_rows.to_string()).unwrap();
        (dir.to_path_buf(), registry_path)
    }

    const BASE_CONFIG: &str = "\
[active_backlog]
enabled = true
interval = \"5m\"
failure_limit = 3
max_concurrent = 2
[[work.workspaces.main.projects]]
name = \"alpha\"
path = \"/repo/alpha\"
";

    fn graph_fixture() -> Value {
        json!({"entries": [
            {"id": "e-1", "type": "epic", "project": "alpha", "status": "in_progress", "priority": "p1"},
            {"id": "e-1a", "parent": "e-1", "project": "alpha", "status": "ready", "priority": "p1"},
            {"id": "e-loose", "project": "alpha", "status": "ready", "priority": "p1"},
            {"id": "e-outs", "project": "beta", "status": "ready", "priority": "p1"}
        ]})
    }

    fn registry_fixture() -> Value {
        let mut v = json!({"agents": [
            {"name": "king-a", "status": "live", "crown_scope": "e-1", "crown_level": 2,
             "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"},
            {"name": "w-1", "status": "live", "node": "e-1a", "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z",
             "pid": std::process::id()},
            {"name": "w-2", "status": "exited", "node": "e-1a", "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"}
        ]});
        v["schema_version"] = json!(crate::state::REGISTRY_SCHEMA_VERSION);
        v
    }

    fn fixture_env() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_CONFIG", tmp.path().join("config.toml"));
        std::env::set_var("FNO_HOME", tmp.path());
        let (cwd, registry) =
            write_fixture(tmp.path(), BASE_CONFIG, graph_fixture(), registry_fixture());
        (tmp, cwd, registry)
    }

    #[test]
    fn resolve_returns_crowned_and_kingless_territories_in_scope_order() {
        let _env = env_guard();
        let (_tmp, cwd, registry) = fixture_env();
        let ts = resolve_territories(&cwd, &registry).unwrap();
        let scopes: Vec<&str> = ts.iter().map(|t| t.key.as_str()).collect();
        // The rung-2 crown over e-1 (rooted in alpha) plus alpha's loose
        // rung-1 territory: the crown rules its descendants, not the
        // project's parentless nodes. Crowns come first in scope order, then
        // the kingless loose territories sorted - the recorded Python order.
        assert_eq!(scopes, ["e-1", "alpha"]);
        assert_eq!(ts[0].rung, 2);
        assert!(!ts[0].kingless);
        assert_eq!(ts[0].members, ["e-1"]);
        assert_eq!(ts[0].project, "alpha");
        assert_eq!(ts[1].rung, 1);
        assert!(ts[1].kingless);
        assert_eq!(ts[1].project, "alpha");
        assert_eq!(ts[1].cwd, "/repo/alpha");
    }

    #[test]
    fn an_unreadable_graph_is_unknown_never_an_empty_list() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_CONFIG", tmp.path().join("config.toml"));
        std::env::set_var("FNO_HOME", tmp.path());
        let (_cwd, registry) = write_fixture(
            tmp.path(),
            BASE_CONFIG,
            json!({"entries": []}),
            registry_fixture(),
        );
        std::fs::remove_file(tmp.path().join("graph.json")).unwrap();
        let err = resolve_territories(&tmp.path().to_path_buf(), &registry).unwrap_err();
        assert!(err.0.contains("graph unreadable"), "{err}");
    }

    #[test]
    fn an_unreadable_registry_is_unknown_never_no_kings() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_CONFIG", tmp.path().join("config.toml"));
        std::env::set_var("FNO_HOME", tmp.path());
        let (_cwd, _registry) = write_fixture(
            tmp.path(),
            BASE_CONFIG,
            graph_fixture(),
            json!({"agents": []}),
        );
        // A MISSING registry reads as an empty one (fail-open, the Python
        // loader's contract); a CORRUPT one is the unknown case.
        std::fs::write(&tmp.path().join("registry.json"), "{not json").unwrap();
        let registry = tmp.path().join("registry.json");
        let err = resolve_territories(&tmp.path().to_path_buf(), &registry).unwrap_err();
        assert!(err.0.contains("registry unreadable"), "{err}");
    }

    #[test]
    fn a_levelless_crown_row_still_resolves_at_rung_zero() {
        let _env = env_guard();
        let (_tmp, cwd, registry) = fixture_env();
        std::fs::write(
            &registry,
            json!({"schema_version": crate::state::REGISTRY_SCHEMA_VERSION, "agents": [
                {"name": "k", "status": "live", "crown_scope": "alpha", "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"}
            ]})
            .to_string(),
        )
        .unwrap();
        let ts = resolve_territories(&cwd, &registry).unwrap();
        assert_eq!(ts.len(), 1, "the rung-0 crown rules alpha: no loose copy");
        assert_eq!(ts[0].rung, 0);
        assert!(!ts[0].kingless);
    }

    #[test]
    fn facts_coerce_with_the_python_truth_table() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_CONFIG", tmp.path().join("config.toml"));
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"[active_backlog]
enabled = "banana"
interval = "0s"
failure_limit = "x"
max_concurrent = 0
"#,
        )
        .unwrap();
        let facts = active_backlog_facts(tmp.path());
        assert_eq!(
            facts.enabled,
            Ok(false),
            "an ambiguous scalar fails safe to off"
        );
        assert_eq!(facts.interval_seconds, None, "an invalid interval disables");
        assert_eq!(facts.failure_limit, 3, "a bad scalar drops to the default");
        assert_eq!(
            facts.max_concurrent, 1,
            "a non-positive cap drops to the default"
        );
    }

    #[test]
    fn per_project_map_enables_only_clear_affirmatives() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_CONFIG", tmp.path().join("config.toml"));
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"[active_backlog]
enabled = { alpha = true, beta = "nope" }
interval = "30s"
"#,
        )
        .unwrap();
        let facts = active_backlog_facts(tmp.path());
        let map = facts.enabled.clone().unwrap_err();
        assert_eq!(map.get("alpha"), Some(&true));
        assert_eq!(map.get("beta"), Some(&false));
        assert!(facts.any_enabled());
        assert!(facts.is_enabled_for(Some("alpha")));
        assert!(!facts.is_enabled_for(Some("beta")));
    }

    #[test]
    fn workspace_paths_mirror_the_python_shape() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_CONFIG", tmp.path().join("config.toml"));
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"[[work.workspaces.main.projects]]
name = "alpha"
path = "/repo/alpha"
"#,
        )
        .unwrap();
        let paths = workspace_paths(tmp.path());
        assert_eq!(paths.get("alpha").map(String::as_str), Some("/repo/alpha"));
    }

    #[test]
    fn normalize_path_expands_a_bare_tilde_like_expanduser() {
        let _env = env_guard();
        std::env::set_var("HOME", "/Users/tester");
        assert_eq!(normalize_path("~"), "/Users/tester");
        assert_eq!(normalize_path("~/repo"), "/Users/tester/repo");
    }

    #[test]
    fn parse_duration_to_seconds_never_panics_on_a_non_ascii_suffix() {
        // A malformed operator value ending mid multi-byte UTF-8 character
        // must fall back to None, never panic the process.
        let v = toml::Value::String("5\u{e9}".to_string());
        assert_eq!(parse_duration_to_seconds(&v), None);
        let v = toml::Value::String("5m".to_string());
        assert_eq!(parse_duration_to_seconds(&v), Some(300));
    }

    #[test]
    fn territory_rows_project_the_ac7_row_shape() {
        let _env = env_guard();
        let (_tmp, cwd, registry) = fixture_env();
        let rows = territory_rows(&cwd, &registry);
        assert_eq!(rows.len(), 2);
        let loose = rows.iter().find(|r| r["scope"] == "alpha").unwrap();
        let crowned = rows.iter().find(|r| r["scope"] == "e-1").unwrap();
        assert_eq!(loose["kingless"], true);
        assert_eq!(loose["rung"], 1);
        assert_eq!(loose["live"], 0);
        assert_eq!(loose["cap"], 4);
        assert_eq!(crowned["holder"], "king-a");
        assert_eq!(crowned["mission"], "e-1");
        assert_eq!(crowned["live"], 1, "w-1 works e-1a inside the crown scope");
    }

    fn owners_from_registry(
        cwd: &Path,
        registry: &Path,
    ) -> (HashMap<String, String>, Vec<(String, String)>) {
        let crowns = live_crowns(registry).unwrap();
        let entries: Vec<Value> = graph_fixture()["entries"].as_array().unwrap().clone();
        node_owners(&crowns, &entries, &project_map(cwd))
    }

    #[test]
    fn node_owners_maps_each_node_to_its_deepest_live_crown() {
        let _env = env_guard();
        let (_tmp, cwd, registry) = fixture_env();
        std::fs::write(
            &registry,
            json!({"schema_version": crate::state::REGISTRY_SCHEMA_VERSION, "agents": [
                {"name": "king-p", "status": "live", "crown_scope": "alpha", "crown_level": 1,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"},
                {"name": "king-a", "status": "live", "crown_scope": "e-1", "crown_level": 2,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"}
            ]})
            .to_string(),
        )
        .unwrap();
        let (owners, failures) = owners_from_registry(&cwd, &registry);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(owners.get("e-1"), Some(&"e-1".to_string()));
        assert_eq!(owners.get("e-1a"), Some(&"e-1".to_string()));
        assert_eq!(owners.get("e-loose"), Some(&"alpha".to_string()));
        assert_eq!(owners.get("e-outs"), None, "beta has no crown");
    }

    #[test]
    fn a_scope_tie_goes_to_the_lowest_canonical_scope() {
        let _env = env_guard();
        let (_tmp, cwd, registry) = fixture_env();
        std::fs::write(
            &registry,
            json!({"schema_version": crate::state::REGISTRY_SCHEMA_VERSION, "agents": [
                {"name": "king-01", "status": "live", "crown_scope": "e-0,e-1", "crown_level": 2,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"},
                {"name": "king-1", "status": "live", "crown_scope": "e-1", "crown_level": 2,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"}
            ]})
            .to_string(),
        )
        .unwrap();
        let entries: Vec<Value> = vec![
            json!({"id": "e-0", "type": "epic", "project": "alpha", "status": "in_progress", "priority": "p1"}),
            json!({"id": "e-1", "type": "epic", "project": "alpha", "status": "in_progress", "priority": "p1"}),
            json!({"id": "e-1a", "parent": "e-1", "project": "alpha", "status": "ready", "priority": "p1"}),
            json!({"id": "e-loose", "project": "alpha", "status": "ready", "priority": "p1"}),
        ];
        let (owners, failures) = node_owners(
            &live_crowns(&registry).unwrap(),
            &entries,
            &project_map(&cwd),
        );
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(owners.get("e-1a"), Some(&"e-0,e-1".to_string()));
        assert_eq!(owners.get("e-0"), Some(&"e-0,e-1".to_string()));
    }

    #[test]
    fn an_exited_crown_row_owns_nothing() {
        let _env = env_guard();
        let (_tmp, cwd, registry) = fixture_env();
        std::fs::write(
            &registry,
            json!({"schema_version": crate::state::REGISTRY_SCHEMA_VERSION, "agents": [
                {"name": "king-p", "status": "live", "crown_scope": "alpha", "crown_level": 1,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"},
                {"name": "king-a", "status": "exited", "crown_scope": "e-1", "crown_level": 2,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"}
            ]})
            .to_string(),
        )
        .unwrap();
        let (owners, failures) = owners_from_registry(&cwd, &registry);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(owners.get("e-1"), Some(&"alpha".to_string()));
        assert_eq!(owners.get("e-1a"), Some(&"alpha".to_string()));
        assert_eq!(owners.get("e-loose"), Some(&"alpha".to_string()));
    }

    #[test]
    fn territory_rows_count_each_worker_in_exactly_one_row() {
        let _env = env_guard();
        let (_tmp, cwd, registry) = fixture_env();
        std::fs::write(
            &registry,
            json!({"schema_version": crate::state::REGISTRY_SCHEMA_VERSION, "agents": [
                {"name": "king-p", "status": "live", "crown_scope": "alpha", "crown_level": 1,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"},
                {"name": "king-a", "status": "live", "crown_scope": "e-1", "crown_level": 2,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"},
                {"name": "w-1", "status": "live", "node": "e-1a", "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z",
                 "pid": std::process::id()},
                {"name": "w-loose", "status": "live", "node": "e-loose", "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z",
                 "pid": std::process::id()}
            ]})
            .to_string(),
        )
        .unwrap();
        let rows = territory_rows(&cwd, &registry);
        let loose = rows.iter().find(|r| r["scope"] == "alpha").unwrap();
        let crowned = rows.iter().find(|r| r["scope"] == "e-1").unwrap();
        assert_eq!(
            crowned["live"], 1,
            "e-1a's worker counts for e-1 only: {rows:?}"
        );
        assert_eq!(
            loose["live"], 1,
            "e-loose's worker counts for alpha only: {rows:?}"
        );
    }

    #[test]
    fn a_non_epic_crown_scope_reads_unknown_while_others_stay_ok() {
        let _env = env_guard();
        let (_tmp, cwd, registry) = fixture_env();
        std::fs::write(
            &registry,
            json!({"schema_version": crate::state::REGISTRY_SCHEMA_VERSION, "agents": [
                {"name": "king-a", "status": "live", "crown_scope": "e-1", "crown_level": 2,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"},
                {"name": "king-bad", "status": "live", "crown_scope": "e-loose", "crown_level": 2,
                 "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z"},
                {"name": "w-1", "status": "live", "node": "e-1a", "cwd": "/repo/alpha", "harness": "claude", "created_at": "2026-09-07T00:00:00Z",
                 "pid": std::process::id()}
            ]})
            .to_string(),
        )
        .unwrap();
        let rows = territory_rows(&cwd, &registry);
        let bad = rows.iter().find(|r| r["scope"] == "e-loose").unwrap();
        let good = rows.iter().find(|r| r["scope"] == "e-1").unwrap();
        assert_eq!(bad["membership"], "unknown");
        assert!(bad["live"].is_null());
        assert_eq!(good["membership"], "ok");
        assert_eq!(good["live"], 1, "{rows:?}");
    }
}

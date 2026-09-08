//! The territory fact set (x-e221): one module owns territory identity,
//! membership, the drain-target facts, and the standing blueprinter's record.
//!
//! Ownership follows the seam rule (rust-python-seam.md): the active-backlog
//! supervisor is a Rust loop that must not stop, and it owns the drain
//! decision. The whole fact set therefore lives beside it - graph membership
//! (moved here from `king_board::scope`, which calls this home), the live
//! crown list from the registry cache, the workspace project map, the
//! mission's root project from the graph, the `active_backlog` config block,
//! and the blueprinter record store. Python reads the territory receipt only
//! through `fno config active-backlog*`, which prints this module's output.
//!
//! Fail-closed shape: an unreadable source is `TerritoryUnknown` naming the
//! source, never an empty territory list and never a silently-drained scope.

use crate::agents_config::config_lookup;
use crate::king_board::{graph_json_path, home_dot_fno, project_map};
use crate::paths::AgentsHome;
use crate::state::{load_registry, Registry, RegistryEntry};
use chrono::{DateTime, Utc};
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

/// The territory-key + membership read (x-e221 AC1): the canonical scope key
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

/// One live crown row: the canonical scope, its rung, its holder's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crown {
    pub scope: String,
    pub level: u8,
    pub holder: String,
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
fn parse_duration_to_seconds(v: &toml::Value) -> Option<i64> {
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
            let (digits, unit) = s.split_at(s.len().saturating_sub(1));
            if digits.is_empty() {
                return None;
            }
            let n: i64 = digits.parse().ok()?;
            let mult = match unit {
                "s" => 1,
                "m" => 60,
                "h" => 3600,
                "d" => 86400,
                _ => return None,
            };
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
        });
    }
    out.sort_by(|a, b| a.scope.cmp(&b.scope));
    Ok(out)
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
fn normalize_path(raw: &str) -> String {
    let expanded: String = if let Some(rest) = raw.strip_prefix("~/") {
        match std::env::var_os("HOME") {
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

/// Graph entries at the resolved `graph.json`, or the unknown naming the read.
fn graph_entries(config_cwd: &Path) -> Result<Vec<Value>, TerritoryUnknown> {
    let path = graph_json_path(config_cwd);
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        TerritoryUnknown(format!(
            "territory: graph unreadable ({}): {e}",
            path.display()
        ))
    })?;
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|e| TerritoryUnknown(format!("territory: graph unparseable: {e}")))?;
    if let Some(list) = parsed.get("entries").and_then(Value::as_array) {
        return Ok(list.clone());
    }
    if let Some(list) = parsed.as_array() {
        return Ok(list.clone());
    }
    Err(TerritoryUnknown(
        "territory: graph is neither an entries object nor a list".to_string(),
    ))
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
    let projects = project_map(config_cwd);
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

// ---------------------------------------------------------------------------
// The blueprinter record store
// ---------------------------------------------------------------------------

/// `urllib.parse.quote(scope, safe="")`: unreserved ASCII stays, every other
/// byte becomes %XX uppercase. The record files on disk were named by the
/// Python verb, so the Rust store must produce byte-identical paths.
fn py_quote(scope: &str) -> String {
    let mut out = String::with_capacity(scope.len());
    for b in scope.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' | b'~' => {
                out.push(*b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The durable scope-keyed record file (`<state>/blueprinters/`).
pub fn record_path(config_cwd: &Path, scope: &str) -> PathBuf {
    state_dir(config_cwd)
        .join("blueprinters")
        .join(format!("{}.json", py_quote(scope)))
}

/// The state dir: a configured `state_dir` wins, else `$FNO_HOME`, else
/// `~/.fno` - the same resolution the nudge sentinel and the graph default
/// use.
fn state_dir(config_cwd: &Path) -> PathBuf {
    if let Some(v) =
        config_lookup(config_cwd, &["state_dir"]).and_then(|v| v.as_str().map(str::to_string))
    {
        let expanded = crate::king_board::expand_home(&v);
        if expanded.is_absolute() {
            return expanded;
        }
        return home_dot_fno().join(expanded);
    }
    if let Some(home) = std::env::var_os("FNO_HOME") {
        return PathBuf::from(home);
    }
    home_dot_fno()
}

fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_iso(value: &str) -> Option<DateTime<Utc>> {
    // The stored stamps carry a literal `Z` and no numeric offset, so parse
    // naive and pin to UTC (`DateTime::parse_from_str` would refuse a format
    // with no offset specifier).
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|naive| naive.and_utc())
}

/// The record for one scope, with its defaults filled: `{worker, fed,
/// repairs}`. An absent or malformed file reads as the empty record - the
/// store is a cache of derived delivery state, never a decision input that
/// may fail a drain.
pub fn read_record(config_cwd: &Path, scope: &str) -> Value {
    let path = record_path(config_cwd, scope);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(mut v) = serde_json::from_str::<Value>(&raw) {
            if let Some(obj) = v.as_object_mut() {
                obj.entry("worker").or_insert(Value::Null);
                obj.entry("fed").or_insert_with(|| json!({}));
                obj.entry("repairs").or_insert_with(|| json!([]));
                return v;
            }
        }
    }
    json!({"worker": null, "fed": {}, "repairs": []})
}

/// Atomic record write: temp file + rename, `updated_at` stamped.
pub fn write_record(config_cwd: &Path, scope: &str, record: &mut Value) {
    let path = record_path(config_cwd, scope);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Some(obj) = record.as_object_mut() {
        obj.insert("updated_at".to_string(), json!(now_iso()));
    }
    let tmp = path.with_extension("tmp");
    if serde_json::to_string_pretty(record)
        .map(|s| std::fs::write(&tmp, s).is_ok())
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Drop fed-ledger rows for nodes that closed or vanished, so the record
/// never grows with shipped work.
fn prune_fed(record: &mut Value, entries: &[Value]) -> bool {
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e.get("id").and_then(Value::as_str).map(|id| (id, e)))
        .collect();
    let Some(fed) = record.get_mut("fed").and_then(Value::as_object_mut) else {
        return false;
    };
    let before = fed.len();
    fed.retain(|node, _| match by_id.get(node.as_str()) {
        Some(row) => !is_terminal(row),
        None => false,
    });
    fed.len() != before
}

fn is_terminal(row: &Value) -> bool {
    row.get("completed_at")
        .map(|v| !v.is_null())
        .unwrap_or(false)
        || matches!(
            row.get("status").and_then(Value::as_str),
            Some("done") | Some("superseded")
        )
}

// ---------------------------------------------------------------------------
// The blueprinter feed (x-e221 AC5/AC6)
// ---------------------------------------------------------------------------

/// A failed delivery re-attempts after this long; a delivered idea still
/// un-ready re-delivers after the longer one, so a blueprint that died
/// mid-flight (or mail the worker never drained) self-heals.
const RETRY_AFTER_SECS: i64 = 1800;
const REDO_AFTER_SECS: i64 = 86400;
const REPAIR_CAP: usize = 50;

/// Graph statuses a feed candidate must never carry.
const EXCLUDED_STATUSES: [&str; 6] = [
    "done",
    "blocked",
    "deferred",
    "superseded",
    "in_progress",
    "queued",
];

/// The plan rung a feed candidate must sit on: an idea with a linked doc that
/// declares no design yet, or an explicit design doc. Computed by the same
/// frontmatter status read the Python ladder uses.
const FEED_RUNGS: [&str; 2] = ["idea", "design"];

/// A stable, registry-safe worker label for one territory scope. The 6-char
/// digest only disambiguates scopes that share a normalized stem; WHICH digest
/// is load-bearing nowhere, only per-scope stability is (the Python verb used
/// sha1, the port uses sha256 - a pre-port record's worker name keeps winning
/// because a live record worker is reused before name_next is consulted).
pub fn worker_name_for_scope(scope: &str) -> String {
    use sha2::{Digest, Sha256};
    let stem: String = scope
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let stem = stem.trim_matches('-').to_string();
    let stem: String = stem.chars().take(24).collect();
    let stem = if stem.is_empty() { "scope" } else { &stem };
    let digest = Sha256::digest(scope.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("blueprinter-{stem}-{}", &hex[..6])
}

/// `plan_rung(entry)` for the one question the feed asks: is this node an
/// undesigned idea doc or an explicit design doc? Mirrors
/// `fno.graph.ladder.plan_rung` - resolve the linked plan the way the NODE
/// would (cwd-anchored), read its frontmatter `status`, and map it through
/// the canonical vocabulary with its aliases. Returns "" for NONE/UNREADABLE
/// and every non-candidate rung.
fn feed_rung(entry: &Value) -> &'static str {
    let Some(plan_path) = entry.get("plan_path").and_then(Value::as_str) else {
        return ""; // Rung.NONE - nothing on disk
    };
    if plan_path.is_empty() {
        return "";
    }
    // resolve_plan_probe: strip the anchor, expand ~, anchor a relative path
    // at the NODE's own cwd (refusing to guess when there is none).
    let probe = plan_path.split('#').next().unwrap_or("");
    let probe = probe.trim();
    if probe.is_empty() {
        return "";
    }
    let expanded = if let Some(rest) = probe.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => return "", // unreadable: no home to expand against
        }
    } else {
        PathBuf::from(probe)
    };
    let probe_path = if expanded.is_absolute() {
        expanded
    } else {
        match entry.get("cwd").and_then(Value::as_str) {
            Some(cwd) if !cwd.is_empty() => PathBuf::from(cwd).join(expanded),
            _ => return "", // unreadable: no anchor to resolve against
        }
    };
    let Ok(text) = std::fs::read_to_string(&probe_path) else {
        return "";
    };
    if text.trim().is_empty() {
        return "";
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.first().map(|l| l.trim()) != Some("---") {
        return "ready"; // readable, no frontmatter: pre-ladder default READY
    }
    let Some(end) = lines[1..].iter().position(|l| l.trim() == "---") else {
        return ""; // unclosed frontmatter: genuinely unparseable
    };
    let status = lines[1..1 + end].iter().find_map(|line| {
        let (key, raw) = line.split_once(':')?;
        if key.trim() != "status" {
            return None;
        }
        // _norm_status: strip whitespace and quote pairs, lowercase.
        let v = raw
            .trim()
            .trim_matches('\'')
            .trim_matches('"')
            .to_lowercase();
        Some(v)
    });
    match status {
        None => "ready", // readable, no status declared: pre-ladder default READY
        Some(raw) => match raw.as_str() {
            // canonical_status: the retired spellings resolve to survivors;
            // only the two candidate rungs name themselves, every other
            // vocabulary word (and every unknown word) is a non-candidate.
            "stub" | "idea" => "idea",
            "design" => "design",
            "shipped" | "archived" | "ready" | "in_progress" | "in_review" | "done"
            | "superseded" => "",
            _ => "", // unknown word: UNREADABLE, never its own rung
        },
    }
}

/// A feed candidate: a live triaged idea inside the membership whose plan doc
/// sits on an idea or design rung.
fn feed_candidate(row: &Value) -> bool {
    let Some(id) = row.get("id").and_then(Value::as_str) else {
        return false;
    };
    if id.is_empty()
        || row
            .get("completed_at")
            .map(|v| !v.is_null())
            .unwrap_or(false)
    {
        return false;
    }
    if let Some(status) = row.get("status").and_then(Value::as_str) {
        if EXCLUDED_STATUSES.contains(&status) {
            return false;
        }
    }
    FEED_RUNGS.contains(&feed_rung(row))
}

/// Whether a fed node is due for (re)delivery: a parseable `at` waits out its
/// window (delivered ok: the long redo window; failed: the retry window),
/// an unparseable one is overdue now.
fn fed_due(fed: Option<&Value>, now: DateTime<Utc>) -> bool {
    let Some(stamp) = fed else {
        return true;
    };
    let Some(at) = parse_iso(stamp.get("at").and_then(Value::as_str).unwrap_or("")) else {
        return true;
    };
    let window = if stamp.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        REDO_AFTER_SECS
    } else {
        RETRY_AFTER_SECS
    };
    (now - at) >= chrono::Duration::seconds(window)
}

/// One territory's blueprinter-feed STATUS receipt: the unfed triaged ideas
/// plus the standing worker's handle and liveness. The decision core the
/// supervisor's tick runs natively; delivery goes through the standard mail
/// transport.
pub fn blueprint_feed_status(config_cwd: &Path, registry_path: &Path, scope: &str) -> Value {
    let entries = match graph_entries(config_cwd) {
        Ok(e) => e,
        Err(TerritoryUnknown(reason)) => {
            return json!({"action": "unknown", "scope": scope, "reason": reason})
        }
    };
    let territories = match resolve_territories(config_cwd, registry_path) {
        Ok(ts) => ts,
        Err(TerritoryUnknown(reason)) => {
            return json!({"action": "unknown", "scope": scope, "reason": reason})
        }
    };
    let Some(territory) = territories.iter().find(|t| t.key == canonical_scope(scope)) else {
        return json!({"action": "unknown", "scope": scope, "reason": "no such territory"});
    };
    let projects = project_map(config_cwd);
    let (key, ids) = match compile_territory(&territory.key, &entries, &projects) {
        Ok(pair) => pair,
        Err(reason) => return json!({"action": "unknown", "scope": scope, "reason": reason}),
    };
    let territory_rung = territory.rung;
    let territory_kingless = territory.kingless;
    let mut record = read_record(config_cwd, &key);
    prune_fed(&mut record, &entries);
    let now = Utc::now();
    let ideas: Vec<Value> = entries
        .iter()
        .filter(|row| {
            row.get("id")
                .and_then(Value::as_str)
                .map(|id| ids.contains(id))
                .unwrap_or(false)
                && feed_candidate(row)
                && fed_due(
                    record
                        .get("fed")
                        .and_then(|f| f.get(row["id"].as_str().unwrap())),
                    now,
                )
        })
        .map(|row| json!({"id": row["id"], "rung": feed_rung(row)}))
        .collect();

    let worker = record.get("worker").filter(|w| !w.is_null()).cloned();
    let live = worker
        .as_ref()
        .and_then(|w| w.get("name").and_then(Value::as_str))
        .map(|name| {
            crate::spawn_gate::live_rows(registry_path, &mut Vec::new())
                .iter()
                .any(|r| r.name == name)
        })
        .unwrap_or(false);
    let worker_view = worker.map(|w| {
        json!({
            "name": w.get("name"),
            "spawned_at": w.get("spawned_at"),
            "live": live,
        })
    });
    json!({
        "action": "status",
        "scope": key,
        "rung": territory_rung,
        "kingless": territory_kingless,
        "worker": worker_view,
        "worker_name_next": worker_name_for_scope(&key),
        "ideas": ideas,
        "fed": record.get("fed").and_then(Value::as_object).map(|f| f.len()).unwrap_or(0),
    })
}

/// Record a refusal/repair reason; the ideas stay preserved.
pub fn blueprint_feed_repair(config_cwd: &Path, scope: &str, reason: &str) -> Value {
    let key = canonical_scope(scope);
    let mut record = read_record(config_cwd, &key);
    if let Some(repairs) = record.get_mut("repairs").and_then(Value::as_array_mut) {
        repairs.push(json!({"ts": now_iso(), "reason": reason}));
        let len = repairs.len();
        if len > REPAIR_CAP {
            repairs.drain(..len - REPAIR_CAP);
        }
    }
    write_record(config_cwd, &key, &mut record);
    json!({"action": "repair", "scope": key, "recorded": reason})
}

/// Mail each due idea to the standing worker and mark what sent. The mail
/// transport stays the standard `fno agents mail send` shell-out (a transport
/// crossing, not a decision crossing); `mail_fno` is injectable for tests.
pub fn blueprint_feed_deliver(
    config_cwd: &Path,
    registry_path: &Path,
    scope: &str,
    mail_fno: Option<&str>,
) -> Value {
    let status = blueprint_feed_status(config_cwd, registry_path, scope);
    let key = status["scope"].as_str().unwrap_or(scope).to_string();
    if status["action"] != "status" {
        return status;
    }
    let mut record = read_record(config_cwd, &key);
    let worker = record.get("worker").filter(|w| !w.is_null()).cloned();
    let worker_name = worker
        .as_ref()
        .and_then(|w| w.get("name").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let live = !worker_name.is_empty()
        && crate::spawn_gate::live_rows(registry_path, &mut Vec::new())
            .iter()
            .any(|r| r.name == worker_name);
    let rung = status["rung"].clone();
    let kingless = status["kingless"].clone();
    if worker.is_none() || !live {
        if let Some(repairs) = record.get_mut("repairs").and_then(Value::as_array_mut) {
            repairs.push(json!({
                "ts": now_iso(),
                "reason": format!("worker_not_live: {}", worker.unwrap_or(Value::Null)),
            }));
            let len = repairs.len();
            if len > REPAIR_CAP {
                repairs.drain(..len - REPAIR_CAP);
            }
        }
        write_record(config_cwd, &key, &mut record);
        return json!({
            "action": "blocked",
            "scope": key,
            "rung": rung,
            "kingless": kingless,
            "reason": "worker_not_live",
            "ideas": status["ideas"].as_array().map(|a| a.len()).unwrap_or(0),
        });
    }
    let now = Utc::now();
    let mut delivered: Vec<String> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    let bin = mail_fno
        .map(str::to_string)
        .unwrap_or_else(|| "fno".to_string());
    for idea in status["ideas"].as_array().cloned().unwrap_or_default() {
        let node_id = idea["id"].as_str().unwrap_or("").to_string();
        if node_id.is_empty() {
            continue;
        }
        let ok = deliver_one(&bin, &worker_name, &node_id);
        if let Some(fed) = record.get_mut("fed").and_then(Value::as_object_mut) {
            fed.insert(
                node_id.clone(),
                json!({"at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(), "ok": ok}),
            );
        }
        if ok {
            delivered.push(node_id);
        } else {
            failed.push(json!({"id": node_id, "reason": "mail send failed"}));
        }
    }
    write_record(config_cwd, &key, &mut record);
    json!({
        "action": "deliver",
        "scope": key,
        "rung": rung,
        "kingless": kingless,
        "worker": status["worker"],
        "delivered": delivered,
        "failed": failed,
        "fed": record.get("fed").and_then(Value::as_object).map(|f| f.len()).unwrap_or(0),
    })
}

fn deliver_one(bin: &str, worker_name: &str, node_id: &str) -> bool {
    use std::process::Command;
    let out = Command::new(bin)
        .args([
            "agents",
            "mail",
            "send",
            worker_name,
            &format!("/fno:blueprint {node_id}"),
        ])
        .output();
    match out {
        Ok(o) => o.status.success(),
        Err(_) => false,
    }
}

/// `fno-agents territory-rows`: the AC7 projection as JSON on stdout.
/// Daemon-free, invoked by the config passthroughs and the status fold.
pub fn run_territory_rows(args: &[String]) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = AgentsHome::from_env().registry_json();
    let rows = territory_rows(&cwd, &registry);
    println!(
        "{}",
        serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
    );
    0
}

/// `fno-agents active-backlog-receipt`: the drain-target receipt as JSON on
/// stdout. Exit 1 with the reason on stderr when a source is unreadable -
/// `unknown` must never print as an empty list. Invoked by the
/// `fno config active-backlog` passthrough and rank's dispatcher note.
pub fn run_active_backlog_receipt(args: &[String]) -> i32 {
    let _ = args;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = AgentsHome::from_env().registry_json();
    match crate::active_backlog::native_receipt(&cwd, &registry) {
        Ok(targets) => {
            println!(
                "{}",
                serde_json::to_string(&targets).unwrap_or_else(|_| "[]".to_string())
            );
            0
        }
        Err(reason) => {
            eprintln!("active-backlog: {reason}");
            1
        }
    }
}

/// `fno-agents blueprint-feed --scope <s> [--deliver] [--repair <r>]`: the
/// standing blueprinter's status, delivery, or repair receipt as JSON.
/// Invoked by the supervisor's tick (native) and the retired verb's shell.
pub fn run_blueprint_feed(args: &[String]) -> i32 {
    let mut scope: Option<String> = None;
    let mut deliver = false;
    let mut repair: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--scope" => scope = iter.next().cloned(),
            "--deliver" => deliver = true,
            "--repair" => repair = iter.next().cloned(),
            _ => {}
        }
    }
    let Some(scope) = scope else {
        eprintln!("blueprint-feed: --scope is required");
        return 2;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = AgentsHome::from_env().registry_json();
    let out = if deliver {
        blueprint_feed_deliver(&cwd, &registry, &scope, None)
    } else if let Some(reason) = repair {
        blueprint_feed_repair(&cwd, &scope, &reason)
    } else {
        blueprint_feed_status(&cwd, &registry, &scope)
    };
    println!(
        "{}",
        serde_json::to_string(&out).unwrap_or_else(|_| "{}".to_string())
    );
    0
}

// ---------------------------------------------------------------------------
// The readout projection (x-e221 AC7)
// ---------------------------------------------------------------------------

/// The per-territory cap: `agents.max_live_per_territory`, default 4, coerced
/// like every other positive-int knob.
pub fn territory_cap(config_cwd: &Path) -> u32 {
    config_lookup(config_cwd, &["agents", "max_live_per_territory"])
        .map(|v| coerce_positive_int(&v, 4))
        .unwrap_or(4)
}

/// One readout row per territory: scope, membership state, rung, kingless
/// state, crown holder, mission, live count against the cap, and the standing
/// blueprinter's handle. The projection the status payload, the hidden config
/// verb, the king check-in, and the operational probe share, so none of them
/// can disagree. Fail-safe per row: an unreadable source shrinks that row's
/// answer (membership "unknown", live null, no blueprinter), never the whole
/// projection.
pub fn territory_rows(config_cwd: &Path, registry_path: &Path) -> Vec<Value> {
    let cap = territory_cap(config_cwd);
    let territories = match resolve_territories(config_cwd, registry_path) {
        Ok(t) => t,
        Err(TerritoryUnknown(reason)) => {
            return vec![json!({"membership": "unknown", "reason": reason, "cap": cap})]
        }
    };
    let holders: HashMap<String, String> = live_crowns(registry_path)
        .unwrap_or_default()
        .into_iter()
        .map(|c| (c.scope, c.holder))
        .collect();
    let entries = graph_entries(config_cwd).unwrap_or_default();
    let live = crate::spawn_gate::live_rows(registry_path, &mut Vec::new());
    let live_names: HashSet<&str> = live.iter().map(|r| r.name.as_str()).collect();
    let live_nodes: Vec<Option<&str>> = live.iter().map(|r| r.node.as_deref()).collect();

    // Exclusive membership (x-e221): a crowned node counts for its crown
    // scope only, so a worker can never cost two territories at once - the
    // same rule the spawn gate enforces.
    let mut memberships: Vec<(&Territory, Result<(String, HashSet<String>), String>)> = Vec::new();
    let mut crown_ids: HashSet<String> = HashSet::new();
    for territory in &territories {
        let compiled = compile_territory(&territory.key, &entries, &project_map(config_cwd));
        if !territory.kingless {
            if let Ok((_, ids)) = &compiled {
                crown_ids.extend(ids.iter().cloned());
            }
        }
        memberships.push((territory, compiled));
    }

    memberships
        .into_iter()
        .map(|(territory, compiled)| {
            let (membership, ids) = match compiled {
                Ok((_, ids)) => ("ok", ids),
                Err(_) => ("unknown", HashSet::new()),
            };
            let mut ids = ids;
            if territory.kingless {
                ids.retain(|id| !crown_ids.contains(id));
            }
            let live_count = if membership == "ok" {
                Some(live_nodes.iter().filter_map(|n| *n).filter(|n| ids.contains(*n)).count())
            } else {
                None
            };
            let record = read_record(config_cwd, &territory.key);
            let worker = record.get("worker").filter(|w| !w.is_null());
            let blueprinter = worker.map(|w| {
                let name = w.get("name").and_then(Value::as_str).unwrap_or("");
                json!({
                    "name": name,
                    "live": live_names.contains(name),
                    "spawned_at": w.get("spawned_at"),
                    "fed": record.get("fed").and_then(Value::as_object).map(|f| f.len()).unwrap_or(0),
                    "repairs": record.get("repairs").and_then(Value::as_array).map(|r| r.len()).unwrap_or(0),
                })
            });
            json!({
                "scope": territory.key,
                "membership": membership,
                "rung": territory.rung,
                "kingless": territory.kingless,
                "holder": holders.get(&territory.key),
                "mission": if territory.rung == 2 { territory.members.first() } else { None },
                "live": live_count,
                "cap": cap,
                "blueprinter": blueprinter,
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

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn write_fixture(
        dir: &Path,
        config: &str,
        graph: Value,
        registry_rows: Value,
    ) -> (PathBuf, PathBuf) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("config.toml"), config).unwrap();
        std::fs::write(
            dir.join("graph.json"),
            serde_json::to_string(&graph).unwrap(),
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
    fn py_quote_matches_urllib_parse_quote_safe_empty() {
        let _env = env_guard();
        assert_eq!(py_quote("x-a792"), "x-a792");
        assert_eq!(py_quote("x-a,x-b"), "x-a%2Cx-b");
        assert_eq!(py_quote("a:b/c d"), "a%3Ab%2Fc%20d");
        assert_eq!(py_quote("t~i.l-d_x"), "t~i.l-d_x");
    }

    #[test]
    fn record_round_trips_with_defaults_filled() {
        let _env = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var("FNO_CONFIG", tmp.path().join("config.toml"));
        std::fs::write(tmp.path().join("config.toml"), "").unwrap();
        let mut rec = read_record(tmp.path(), "x-a,x-b");
        assert_eq!(rec["worker"], Value::Null);
        rec["worker"] =
            json!({"name": "blueprinter-x-a-x-b-abc123", "spawned_at": "2026-09-07T00:00:00Z"});
        rec["fed"]["e-9"] = json!({"at": "2026-09-07T00:00:00Z", "ok": true});
        write_record(tmp.path(), "x-a,x-b", &mut rec);
        let path = record_path(tmp.path(), "x-a,x-b");
        assert!(path.ends_with("blueprinters/x-a%2Cx-b.json"), "{path:?}");
        let reread = read_record(tmp.path(), "x-a,x-b");
        assert_eq!(reread["worker"]["name"], "blueprinter-x-a-x-b-abc123");
        assert_eq!(reread["fed"]["e-9"]["ok"], true);
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

    #[test]
    fn feed_rung_reads_the_plan_doc_the_way_the_node_would() {
        let tmp = tempfile::TempDir::new().unwrap();
        let design = tmp.path().join("design.md");
        std::fs::write(&design, "---\nstatus: design\n---\n\nbody\n").unwrap();
        let idea_doc = tmp.path().join("idea.md");
        std::fs::write(&idea_doc, "---\nstatus: stub\n---\nbody\n").unwrap();
        let silent = tmp.path().join("silent.md");
        std::fs::write(&silent, "# no frontmatter\n").unwrap();
        let unknown = tmp.path().join("unknown.md");
        std::fs::write(&unknown, "---\nstatus: wat\n---\n").unwrap();
        let anchored = tmp.path().join("anchored.md");
        std::fs::write(&anchored, "---\nstatus: design\n---\n").unwrap();

        assert_eq!(feed_rung(&json!({"id": "n"})), "", "no plan_path: NONE");
        assert_eq!(
            feed_rung(&json!({"id": "n", "plan_path": design.to_str().unwrap()})),
            "design"
        );
        assert_eq!(
            feed_rung(&json!({"id": "n", "plan_path": idea_doc.to_str().unwrap()})),
            "idea",
            "a stub reads idea"
        );
        assert_eq!(
            feed_rung(&json!({"id": "n", "plan_path": silent.to_str().unwrap()})),
            "ready"
        );
        assert_eq!(
            feed_rung(&json!({"id": "n", "plan_path": unknown.to_str().unwrap()})),
            "",
            "unknown word: UNREADABLE"
        );
        assert_eq!(
            feed_rung(
                &json!({"id": "n", "plan_path": "relative.md", "cwd": tmp.path().to_str().unwrap()})
            ),
            "",
            "relative without a file on disk: cannot tell, never ready"
        );
        assert_eq!(
            feed_rung(
                &json!({"id": "n", "plan_path": "anchored.md", "cwd": tmp.path().to_str().unwrap()})
            ),
            "design",
            "a relative path anchors at the NODE's cwd"
        );
    }

    #[test]
    fn feed_status_lists_only_due_candidates_inside_the_scope() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = env_guard();
        std::env::set_var("FNO_CONFIG", tmp.path().join("config.toml"));
        std::env::set_var("FNO_HOME", tmp.path());
        let plan = tmp.path().join("idea-plan.md");
        std::fs::write(&plan, "---\nstatus: design\n---\n").unwrap();
        let (_cwd, registry) = write_fixture(
            tmp.path(),
            BASE_CONFIG,
            json!({"entries": [
                {"id": "e-1", "type": "epic", "project": "alpha", "status": "in_progress", "priority": "p1"},
                {"id": "i-due", "parent": "e-1", "project": "alpha", "status": "idea", "priority": "p2", "plan_path": plan.to_str().unwrap()},
                {"id": "i-fed-ok", "parent": "e-1", "project": "alpha", "status": "idea", "priority": "p2", "plan_path": plan.to_str().unwrap()},
                {"id": "i-fed-fail", "parent": "e-1", "project": "alpha", "status": "idea", "priority": "p2", "plan_path": plan.to_str().unwrap()},
                {"id": "i-blocked", "parent": "e-1", "project": "alpha", "status": "blocked", "priority": "p2", "plan_path": plan.to_str().unwrap()},
                {"id": "i-loose", "project": "alpha", "status": "idea", "priority": "p2", "plan_path": plan.to_str().unwrap()}
            ]}),
            registry_fixture(),
        );
        // Pre-fed nodes: one delivered long ago (due again), one just now
        // (not due), one failed just now (inside the retry window).
        let mut rec = read_record(tmp.path(), "e-1");
        rec["fed"] = json!({
            "i-fed-ok": {"at": "2020-01-01T00:00:00Z", "ok": true},
            "i-due-later": {"at": "2020-01-01T00:00:00Z", "ok": false}
        });
        rec["fed"]["i-fed-fail"] =
            json!({"at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(), "ok": false});
        // i-fed-ok keeps the 2020 stamp: past the redo window, so it IS due.
        rec["fed"]["i-fed-ok"] = json!({"at": "2020-01-01T00:00:00Z", "ok": false});
        rec["worker"] =
            json!({"name": "blueprinter-e-1-abcdef", "spawned_at": "2020-01-01T00:00:00Z"});
        write_record(tmp.path(), "e-1", &mut rec);

        let status = blueprint_feed_status(tmp.path(), &registry, "e-1");
        assert_eq!(status["action"], "status", "{status}");
        let ids: Vec<&str> = status["ideas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"i-due"), "unfed: {ids:?}");
        assert!(
            ids.contains(&"i-fed-ok"),
            "ok stamp past the redo window is due again: {ids:?}"
        );
        assert!(
            !ids.contains(&"i-fed-fail"),
            "failed stamp inside the retry window: {ids:?}"
        );
        assert!(
            !ids.contains(&"i-blocked"),
            "a blocked graph status never feeds: {ids:?}"
        );
        assert!(
            !ids.contains(&"i-loose"),
            "loose nodes belong to the project territory, not the epic crown: {ids:?}"
        );
        assert_eq!(status["worker"]["name"], "blueprinter-e-1-abcdef");
        assert_eq!(
            status["worker"]["live"], false,
            "no such registry row: not live"
        );
    }

    #[test]
    fn worker_name_is_registry_safe_and_stable_per_scope() {
        let a1 = worker_name_for_scope("x-a792");
        let a2 = worker_name_for_scope("x-a792");
        let b = worker_name_for_scope("x-b, x-c");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert!(a1.len() <= 40, "{a1}");
        assert!(a1.starts_with("blueprinter-"), "{a1}");
        let slug: String = a1
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        assert_eq!(slug, a1, "registry-safe: {a1}");
    }

    #[test]
    fn deliver_blocks_when_no_worker_is_live_and_records_the_repair() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _env = env_guard();
        std::env::set_var("FNO_CONFIG", tmp.path().join("config.toml"));
        std::env::set_var("FNO_HOME", tmp.path());
        let (_cwd, registry) =
            write_fixture(tmp.path(), BASE_CONFIG, graph_fixture(), registry_fixture());
        let before = read_record(tmp.path(), "e-1");
        let out = blueprint_feed_deliver(tmp.path(), &registry, "e-1", Some("/bin/false"));
        assert_eq!(out["action"], "blocked", "{out}");
        assert_eq!(out["reason"], "worker_not_live");
        let after = read_record(tmp.path(), "e-1");
        let repairs = after["repairs"].as_array().unwrap();
        assert_eq!(
            repairs.len(),
            (before["repairs"].as_array().map(|a| a.len()).unwrap_or(0)) + 1,
            "{after}"
        );
        assert!(repairs.last().unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("worker_not_live"));
    }
}

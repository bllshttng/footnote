//! Config/path resolution and crown-scope compilation (king/lane.py, projects/resolve.py).
use super::s_str;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Config + path resolution (mirrors fno.paths / fno.claims.io defaults)
// ---------------------------------------------------------------------------

pub(crate) fn expand_home(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

pub(crate) fn home_dot_fno() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fno")
}

/// `paths.graph_json()`: a `paths.graph_json` override wins (a relative one
/// anchors under `~/.fno`, the same treatment the ledger override gets);
/// otherwise the state dir's `graph.json`. The default lands at
/// `~/.fno/graph.json`, which is also what `FNO_HOME` redirects.
pub(crate) fn graph_json_path(cwd: &Path) -> PathBuf {
    if let Some(v) = crate::agents_config::config_lookup(cwd, &["paths", "graph_json"])
        .and_then(|v| v.as_str().map(str::to_string))
    {
        let expanded = expand_home(&v);
        if expanded.is_absolute() {
            return expanded;
        }
        return home_dot_fno().join(expanded);
    }
    if let Some(home) = std::env::var_os("FNO_HOME") {
        return PathBuf::from(home).join("graph.json");
    }
    home_dot_fno().join("graph.json")
}

/// `paths.operator_lane()`: pinned global like the ledger - one file per
/// person, never per checkout.
pub(crate) fn operator_lane_path(cwd: &Path) -> PathBuf {
    if let Some(v) = crate::agents_config::config_lookup(cwd, &["paths", "operator_lane"])
        .and_then(|v| v.as_str().map(str::to_string))
    {
        let expanded = expand_home(&v);
        if expanded.is_absolute() {
            return expanded;
        }
        return home_dot_fno().join(expanded);
    }
    let state_dir = crate::agents_config::config_lookup(cwd, &["state_dir"])
        .and_then(|v| v.as_str().map(str::to_string))
        .map(|s| expand_home(&s));
    match state_dir {
        Some(dir) if dir.is_absolute() => dir.join("my-priorities.md"),
        _ => home_dot_fno().join("my-priorities.md"),
    }
}

/// `config.king.autonomous_merge`, fail-safe to off: an unreadable config
/// resolves an outward, hard-to-reverse action to off, which is the invariant
/// every gate resolver applies to itself.
pub(crate) fn autonomous_merge_enabled(cwd: &Path) -> bool {
    crate::agents_config::config_lookup(cwd, &["king", "autonomous_merge"])
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// The {alias: canonical} project map from `work.workspaces.*.projects[]`
/// (projects/resolve.py's cache builder). `Err` names why the map is absent so
/// a scope spelling that is neither project nor epic can be refused with the
/// Python resolver's wording.
pub(crate) fn project_map(cwd: &Path) -> Result<HashMap<String, String>, String> {
    let work = match crate::agents_config::config_lookup(cwd, &["work", "workspaces"]) {
        Some(v) => v,
        None => return Err("no work.workspaces in any candidate config.toml".to_string()),
    };
    let Some(table) = work.as_table() else {
        return Ok(HashMap::new());
    };
    let mut map: HashMap<String, String> = HashMap::new();
    for (_ws, ws_data) in table {
        let Some(projects) = ws_data.get("projects").and_then(|p| p.as_array()) else {
            continue;
        };
        for project in projects {
            let Some(project) = project.as_table() else {
                continue;
            };
            let Some(canonical) = project.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            if canonical.is_empty() {
                continue;
            }
            map.entry(canonical.to_string())
                .or_insert_with(|| canonical.to_string());
            if let Some(short) = project.get("short_name").and_then(|s| s.as_str()) {
                if !short.is_empty() && short != canonical {
                    map.entry(short.to_string())
                        .or_insert_with(|| canonical.to_string());
                }
            }
        }
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Scope
// ---------------------------------------------------------------------------

/// Compile a canonical crown scope into the graph node ids it contains
/// (board.compile_scope_ids).
pub(crate) fn compile_scope_ids(
    scope: &str,
    entries: &[Value],
    projects: &Result<HashMap<String, String>, String>,
) -> Result<HashSet<String>, String> {
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

    // resolve_crown: the (level, canonical) pair, derived together.
    let (level_two, canonical): (bool, String) = if members.len() > 1 {
        let mut resolved = Vec::new();
        for m in &members {
            match projects.get(m.as_str()) {
                Some(canon) => resolved.push(canon.clone()),
                None => {
                    return Err(format!(
                        "a multi-scope crown rules PROJECTS, but {m} is not a configured \
                         project. Name projects from your config, or pass a single epic instead."
                    ))
                }
            }
        }
        (false, canonical_scope(&resolved))
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
        let Some(root) = entry_by_id(&canonical) else {
            return Err(format!(
                "crown scope {canonical:?} is not an epic in the graph"
            ));
        };
        if s_str(root, "type") != Some("epic") {
            return Err(format!(
                "crown scope {canonical:?} is not an epic in the graph"
            ));
        }
        let mut ids: HashSet<String> = HashSet::new();
        ids.insert(canonical.clone());
        // descendants_of: BFS over parent links, cycle-safe.
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in entries {
            if let (Some(id), Some(parent)) = (s_str(e, "id"), s_str(e, "parent")) {
                children.entry(parent).or_default().push(id);
            }
        }
        let mut frontier: Vec<&str> = children
            .get(canonical.as_str())
            .cloned()
            .unwrap_or_default();
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
        return Ok(ids);
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
    Ok(ids)
}

/// King manifest frontmatter fields (king/state.parse_manifest): an unreadable
/// manifest reads as absent.
pub(crate) fn parse_manifest(path: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for line in text.lines() {
        if line.trim() == "---" {
            continue;
        }
        let Some((key, raw)) = line.split_once(':') else {
            continue;
        };
        let raw = raw.trim();
        let raw = if raw.starts_with('"') {
            serde_json::from_str::<String>(raw)
                .unwrap_or_else(|_| raw.trim_matches('"').to_string())
        } else {
            raw.to_string()
        };
        out.insert(key.trim().to_string(), raw);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
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
}

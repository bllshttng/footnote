//! Config/path resolution and crown-scope compilation (king/lane.py, projects/resolve.py).
use super::s_str;
use serde_json::Value;
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
// Scope compilation moved to `crate::territory` (x-e221 port): the board and
// the drain call the same home. This module keeps the config/path resolvers
// the board shares with it.
// ---------------------------------------------------------------------------

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

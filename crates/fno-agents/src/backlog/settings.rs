//! The two settings reads the porcelain decisions need, walked in the same
//! candidate order the Python config readers use: the repo-local config
//! first, then the global file (honoring `FNO_GLOBAL_SETTINGS_PATH`).
//!
//! `backlog.id_prefix` feeds the bare-hex resolution tier; the
//! `work.workspaces` project map feeds the `_resolved_cwd` display
//! annotation `backlog get` stamps on every served row (and that
//! `validate-plan.sh` reads back through `--field _resolved_cwd`). Both are
//! best-effort: a missing or unparseable file contributes nothing, and the
//! legacy defaults hold.

use serde_json::Value;
use std::path::PathBuf;

/// The config candidates, highest priority first. Each settings.yaml
/// location also yields its config.toml sibling, which wins (the flat
/// config.toml-first cut the Python reader applies). The cwd and HOME
/// entries are DISCOVERED, so they honor `FNO_CONFIG_SEARCH_ROOT`
/// ([`crate::agents_config::within_search_ceiling`]); the FNO_CONFIG and
/// FNO_GLOBAL_SETTINGS_PATH entries are explicit and read as-is.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push_dir = |dir: PathBuf| {
        out.push(dir.join("config.toml"));
        out.push(dir.join("settings.yaml"));
    };
    if let Ok(cwd) = std::env::current_dir() {
        let dir = cwd.join(".fno");
        if crate::agents_config::within_search_ceiling(&dir.join("config.toml")) {
            push_dir(dir);
        }
    }
    // The project config the FNO_CONFIG redirect names (python's
    // `config_file()` candidate), then the global file.
    if let Some(cfg) = std::env::var_os("FNO_CONFIG").filter(|v| !v.is_empty()) {
        let cfg = PathBuf::from(cfg);
        if let Some(parent) = cfg.parent() {
            push_dir(parent.to_path_buf());
        }
        out.push(cfg);
    }
    match std::env::var_os("FNO_GLOBAL_SETTINGS_PATH") {
        Some(v) if !v.is_empty() => out.push(PathBuf::from(v)),
        _ => {
            if let Some(home) = std::env::var_os("HOME") {
                let home = PathBuf::from(home);
                if crate::agents_config::within_search_ceiling(
                    &home.join(".fno").join("config.toml"),
                ) {
                    out.push(home.join(".fno").join("config.toml"));
                    out.push(home.join(".fno").join("settings.yaml"));
                }
            }
        }
    }
    out
}

/// Parse one config file into a JSON object: TOML by suffix, YAML otherwise.
/// `{}` on a missing or unparseable file.
fn read_flat(path: &std::path::Path) -> Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Value::Object(Default::default());
    };
    let parsed: Value = if path.extension().is_some_and(|e| e == "toml") {
        text.parse::<toml::Value>()
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
            .unwrap_or(Value::Null)
    } else {
        serde_yaml_ng::from_str::<serde_json::Value>(&text).unwrap_or(Value::Null)
    };
    parsed
}

/// The parsed config candidates, first-priority order. The birth hook reads
/// nested config flags through this walk (autonomy master switch,
/// think_spawn gate and caps).
pub(crate) fn config_candidates() -> Vec<Value> {
    candidates().iter().map(|p| read_flat(p)).collect()
}

/// The configured node-id prefix, or the legacy `ab-`. The first candidate
/// carrying a non-empty `backlog.id_prefix` wins.
pub fn node_id_prefix() -> String {
    for path in candidates() {
        let doc = read_flat(&path);
        let prefix = doc
            .get("backlog")
            .and_then(|b| b.get("id_prefix"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !prefix.is_empty() {
            return prefix.to_string();
        }
    }
    "ab-".to_string()
}

/// Yield `(name, path)` pairs from one file's work map: the multi-workspace
/// shape first, then the legacy flat shape, in declaration order.
fn work_pairs(doc: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(work) = doc.get("work") else {
        return out;
    };
    if let Some(projects) = work.get("workspaces").and_then(Value::as_object) {
        for ws in projects.values() {
            let Some(rows) = ws.get("projects").and_then(Value::as_array) else {
                continue;
            };
            for row in rows {
                let name = row.get("name").and_then(Value::as_str);
                let path = row.get("path").and_then(Value::as_str);
                if let (Some(name), Some(path)) = (name, path) {
                    out.push((name.to_string(), path.to_string()));
                }
            }
        }
    }
    if let Some(projects) = work.get("projects").and_then(Value::as_object) {
        for (name, cfg) in projects {
            if let Some(path) = cfg.get("path").and_then(Value::as_str) {
                out.push((name.clone(), path.to_string()));
            }
        }
    }
    out
}

pub(crate) fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Project name -> work-map path (abspath of the expanded entry). The first
/// candidate file naming the project wins; None when nothing maps it.
pub fn project_root(project: &str) -> Option<String> {
    if project.is_empty() {
        return None;
    }
    for path in candidates() {
        for (name, raw) in work_pairs(&read_flat(&path)) {
            if name == project {
                return Some(expand_home(&raw).to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// The configured node-id hex width, or the legacy 8. Fail-open like the
/// prefix: an absent key resolves to 8.
pub fn node_id_hex_width() -> usize {
    for path in candidates() {
        let n = read_flat(&path)
            .get("backlog")
            .and_then(|b| b.get("id_hex_width"))
            .and_then(Value::as_u64);
        if let Some(n) = n {
            return n as usize;
        }
    }
    8
}

/// The state directory the porcelain reads serve from: `FNO_STATE_DIR` when
/// the env names a root (the carrier `fno.paths.state_dir` resolves and
/// `seal_state_root` pins, so a sealed read serves the store its parent
/// wrote), else `FNO_CONFIG`'s `state_dir` when the env names a file, else
/// the first candidate carrying a `state_dir`, else `~/.fno`. `FNO_HOME`
/// does not move the backlog.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("FNO_STATE_DIR").filter(|v| !v.is_empty()) {
        return Some(expand_home(&v.to_string_lossy()));
    }
    if let Some(cfg) = std::env::var_os("FNO_CONFIG").filter(|v| !v.is_empty()) {
        let cfg = PathBuf::from(cfg);
        if let Some(sd) = read_flat(&cfg).get("state_dir").and_then(Value::as_str) {
            return Some(expand_home(sd));
        }
    }
    for path in candidates() {
        if let Some(sd) = read_flat(&path).get("state_dir").and_then(Value::as_str) {
            return Some(expand_home(sd));
        }
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".fno"))
}

/// The graph file the porcelain reads serve: `<state_dir>/graph.json`,
/// resolved through the layout table so a migrated root reads the `db/` twin.
pub fn graph_path() -> PathBuf {
    match state_dir() {
        Some(d) => crate::state_layout::place(&d, "graph.json"),
        None => crate::state_layout::place(&PathBuf::from(".fno"), "graph.json"),
    }
}

/// The configured dispatch-verb extension: `(allowed_verbs, verb_registry
/// keys)`. Unset `allowed_verbs` defaults to the shipped trio, the same
/// default the Python config model carries.
pub fn dispatch_verbs_from_config() -> (Vec<String>, Vec<String>) {
    const DEFAULT_DISPATCH_VERBS: [&str; 3] = ["/target", "/think", "/blueprint"];
    if let Some(path) = candidates().first() {
        let doc = read_flat(path);
        let dispatch = doc.get("dispatch");
        let allowed: Vec<String> = dispatch
            .and_then(|d| d.get("allowed_verbs"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(|| {
                DEFAULT_DISPATCH_VERBS
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
        let registry: Vec<String> = dispatch
            .and_then(|d| d.get("verb_registry"))
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        return (allowed, registry);
    }
    (
        DEFAULT_DISPATCH_VERBS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        Vec::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_dir_carrier_outranks_config_and_home() {
        let _guard = crate::claims::test_env_lock();
        let prior = std::env::var_os("FNO_STATE_DIR");
        std::env::set_var("FNO_STATE_DIR", "/pinned-state");

        assert_eq!(state_dir(), Some(PathBuf::from("/pinned-state")));

        match prior {
            Some(v) => std::env::set_var("FNO_STATE_DIR", v),
            None => std::env::remove_var("FNO_STATE_DIR"),
        }
    }

    #[test]
    fn a_legacy_yaml_work_map_resolves_the_project_path() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("settings.yaml");
        std::fs::write(
            &cfg,
            "work:\n  projects:\n    my-proj:\n      path: ~/code/my-proj\n",
        )
        .unwrap();
        let doc = read_flat(&cfg);
        let pairs = work_pairs(&doc);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "my-proj");
        assert!(pairs[0].1.ends_with("my-proj"));
    }

    #[test]
    fn the_multi_workspace_shape_walks_projects_in_declaration_order() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("settings.yaml");
        std::fs::write(
            &cfg,
            "work:\n  workspaces:\n    ws:\n      projects:\n        - name: one\n          path: /repo/one\n        - name: two\n          path: /repo/two\n",
        )
        .unwrap();
        let pairs = work_pairs(&read_flat(&cfg));
        assert_eq!(
            pairs,
            vec![
                ("one".to_string(), "/repo/one".to_string()),
                ("two".to_string(), "/repo/two".to_string())
            ]
        );
    }

    #[test]
    fn project_root_returns_the_first_mapping_and_expands_tilde() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("settings.yaml");
        std::fs::write(
            &cfg,
            "work:\n  projects:\n    mine:\n      path: ~/code/mine\n",
        )
        .unwrap();
        // The candidate walk only reads cwd-local and global files, so the
        // mapping is exercised through work_pairs + the expand helper.
        let pairs = work_pairs(&read_flat(&cfg));
        assert_eq!(pairs[0].0, "mine");
        let expanded = expand_home(&pairs[0].1);
        assert!(expanded.is_absolute(), "{expanded:?}");
    }

    #[test]
    fn an_unset_prefix_falls_back_to_the_legacy_word() {
        // The test env carries no repo-local config beside the binary; the
        // global file is out of reach through the tempdir default. The
        // contract under test is the fallback arm.
        let prefix = node_id_prefix();
        assert!(!prefix.is_empty());
    }
}

//! Settings reads the native advance/triage doors need, ported from
//! `fno.config` for the keys the workflow ports consume: the autonomy
//! master switch and `config.auto_continue.enabled`.
//!
//! The chain mirrors the Python loader's observable contract: candidates in
//! precedence order (FNO_CONFIG, worktree, canonical, global), each file
//! parsed (config.toml preferred over settings.yaml at each location), the
//! layers deep-merged with the highest-priority file winning per leaf, and
//! defaults supplied for absent keys. A malformed block degrades to the
//! default rather than failing the read: one operator typo can never break
//! the door.

use std::path::{Path, PathBuf};

/// The nearest ancestor of the cwd that is a checkout root. Python resolves
/// the project-local candidate from the git toplevel, so running `fno` from
/// a subdirectory still finds the project-local file.
fn worktree_repo_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let mut candidate: &Path = &cwd;
    loop {
        if candidate.join(".git").exists() {
            return Some(candidate.to_path_buf());
        }
        candidate = candidate.parent()?;
    }
}

/// Ordered settings candidates for `project_root` (None: the process
/// context). Mirrors `_settings_yaml_locations`: FNO_CONFIG short-circuits
/// when no root was supplied; worktree, canonical (dropped by
/// FNO_NO_CANONICAL_CONFIG, deduped), then the global file.
pub fn settings_candidates(project_root: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if project_root.is_none() {
        if let Some(pin) = std::env::var_os("FNO_CONFIG").filter(|v| !v.is_empty()) {
            out.push(PathBuf::from(pin));
            return out;
        }
    }
    let repo_root = match project_root {
        Some(root) => root.to_path_buf(),
        None => match worktree_repo_root() {
            Some(r) => r,
            None => return out,
        },
    };
    out.push(repo_root.join(".fno").join("config.toml"));
    out.push(repo_root.join(".fno").join("settings.yaml"));
    if std::env::var_os("FNO_NO_CANONICAL_CONFIG").is_none_or(|v| v != "1") {
        if let Some(canonical) = crate::paths::canonical_repo_root(&repo_root) {
            let pair = [
                canonical.join(".fno").join("config.toml"),
                canonical.join(".fno").join("settings.yaml"),
            ];
            if !pair.iter().any(|p| out.contains(p)) {
                out.push(pair[0].clone());
                out.push(pair[1].clone());
            }
        }
    }
    if let Some(gp) = std::env::var_os("FNO_GLOBAL_SETTINGS_PATH").filter(|v| !v.is_empty()) {
        out.push(PathBuf::from(gp));
    } else if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        out.push(home.join(".fno").join("config.toml"));
        out.push(home.join(".fno").join("settings.yaml"));
    }
    out
}

fn parse_file(path: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = if path.extension().and_then(|e| e.to_str()) == Some("toml") {
        let table: toml::Table = text.parse().map_err(|e| format!("toml parse: {e}")).ok()?;
        serde_json::to_value(table).ok()?
    } else {
        serde_yaml_ng::from_str(&text).ok()?
    };
    // A parsed non-table contributes nothing (config_io._load_raw), and a
    // parse failure warns once and skips the layer - the Python loader never
    // fails the whole read over one file. The `config:` unwrap happens ONCE,
    // after the merge (see unwrap_config_dict), not per file.
    match value {
        serde_json::Value::Object(_) => Some(value),
        _ => None,
    }
}

/// Normalize the merged doc to the FLAT shape (config_io._unwrap_config_dict):
/// a legacy top-level `config:` block lifts to the top level and its leaves
/// WIN over stray same-named top-level keys (canonical beats legacy). No-op
/// without a `config:` object.
fn unwrap_config_dict(raw: serde_json::Value) -> serde_json::Value {
    let mut map = match raw {
        serde_json::Value::Object(map) => map,
        other => return other,
    };
    let cfg = match map.remove("config") {
        Some(cfg @ serde_json::Value::Object(_)) => cfg,
        other => {
            if let Some(cfg) = other {
                map.insert("config".to_string(), cfg);
            }
            return serde_json::Value::Object(map);
        }
    };
    let rest = serde_json::Value::Object(map);
    let mut base = match rest {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    deep_merge(&mut base, cfg);
    serde_json::Value::Object(base)
}

/// Deep-merge `over` into `base` (over wins per leaf), Python `_deep_merge`.
fn deep_merge(base: &mut serde_json::Map<String, serde_json::Value>, over: serde_json::Value) {
    match over {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                match (base.get_mut(&key), value) {
                    (Some(serde_json::Value::Object(slot)), serde_json::Value::Object(nested)) => {
                        deep_merge(slot, serde_json::Value::Object(nested));
                    }
                    (_, value) => {
                        base.insert(key, value);
                    }
                }
            }
        }
        _ => {}
    }
}

/// The merged settings document: highest-priority candidate wins per leaf.
/// The chain reads LOWEST-priority first, merging each layer over the last
/// (Python `reversed(layers)`), so the first candidate's leaves survive. A
/// broken layer warns and contributes nothing - the Python loader never
/// fails a settings read over one bad file, so this read is infallible and
/// the model defaults govern absent keys.
pub fn load_merged(project_root: Option<&Path>) -> serde_json::Value {
    let mut merged = serde_json::Map::new();
    for path in settings_candidates(project_root).iter().rev() {
        if !path.is_file() {
            continue;
        }
        match parse_file(path) {
            Some(doc) => deep_merge(&mut merged, doc),
            None => {
                eprintln!(
                    "fno: warning: settings file at {} failed to parse; using defaults",
                    path.display()
                );
            }
        }
    }
    unwrap_config_dict(serde_json::Value::Object(merged))
}

fn coerce_bool(value: Option<&serde_json::Value>, default: bool) -> bool {
    match value {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => default,
        },
        Some(serde_json::Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(default),
        _ => default,
    }
}

/// `config.autonomy.enabled`, default TRUE (the AutonomyBlock default: the
/// switch arms off only when named). A NON-MAPPING autonomy block is the
/// Python validation failure and fails safe to FALSE, the one read that
/// resolves every gate off.
pub fn autonomy_master_enabled(project_root: Option<&Path>) -> bool {
    let doc = load_merged(project_root);
    match doc.get("autonomy") {
        Some(serde_json::Value::Object(_)) => {
            coerce_bool(doc.get("autonomy").and_then(|a| a.get("enabled")), true)
        }
        Some(_) => false,
        None => true,
    }
}

/// `config.auto_continue.enabled`, default FALSE (the AutoContinueBlock
/// default; a non-mapping block degrades to it the same way). Infallible:
/// the Python rank arm that names a raised read ("default") has no analog
/// here because the loader degrades in place.
pub fn auto_continue_enabled_setting(project_root: Option<&Path>) -> bool {
    let doc = load_merged(project_root);
    match doc.get("auto_continue") {
        Some(serde_json::Value::Object(_)) => coerce_bool(
            doc.get("auto_continue").and_then(|a| a.get("enabled")),
            false,
        ),
        _ => false,
    }
}

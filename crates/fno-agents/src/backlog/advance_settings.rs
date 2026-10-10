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
    // The Python loader unwraps a top-level `config:` dict before validating,
    // so `config:` keys and bare keys read the same.
    match value {
        serde_json::Value::Object(map) => match map.get("config") {
            Some(inner @ serde_json::Value::Object(_)) => Some(inner.clone()),
            _ => Some(serde_json::Value::Object(map)),
        },
        _ => None,
    }
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
/// Err names the parse failure of the first candidate that failed to parse
/// AFTER at least one candidate existed (the fail-safe contract names the
/// cause; an empty candidate list is a normal default read).
pub fn load_merged(project_root: Option<&Path>) -> Result<serde_json::Value, String> {
    let mut merged = serde_json::Map::new();
    let mut any_parse_fail: Option<String> = None;
    for path in settings_candidates(project_root) {
        if !path.is_file() {
            continue;
        }
        match parse_file(&path) {
            Some(doc) => deep_merge(&mut merged, doc),
            None => {
                if any_parse_fail.is_none() {
                    any_parse_fail = Some(format!("unparseable settings file: {}", path.display()));
                }
            }
        }
    }
    if merged.is_empty() {
        if let Some(err) = any_parse_fail {
            return Err(err);
        }
    }
    Ok(serde_json::Value::Object(merged))
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

/// `config.autonomy.enabled`, default TRUE. Fail-safe False on a read
/// failure: an unreadable config resolves every gate to off, never to on.
pub fn autonomy_master_enabled(project_root: Option<&Path>) -> bool {
    match load_merged(project_root) {
        Ok(doc) => coerce_bool(doc.get("autonomy").and_then(|a| a.get("enabled")), true),
        Err(_) => false,
    }
}

/// `config.auto_continue.enabled`, default FALSE. Err on a read failure so
/// the caller stamps rank "default" (fail-safe disabled).
pub fn auto_continue_enabled_setting(project_root: Option<&Path>) -> Result<bool, String> {
    let doc = load_merged(project_root)?;
    Ok(coerce_bool(
        doc.get("auto_continue").and_then(|a| a.get("enabled")),
        false,
    ))
}

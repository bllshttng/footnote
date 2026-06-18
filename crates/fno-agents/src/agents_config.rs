//! Read `config.agents.<provider>.headless_yolo` from settings.yaml (bounded-posture amendment).
//!
//! Mirror of the Python resolver `fno.config.agents_headless_yolo`.
//! `headless_yolo` selects FULL yolo (`true`, unsandboxed bypass) vs the BOUNDED
//! posture (`false`/absent, the default: sandboxed AND never-prompt). Both never
//! prompt, so an autonomous (headless, MODE==exec) codex/gemini worker cannot
//! hang either way.
//!
//! Both resolvers degrade to the hang-safe BOUNDED default (`false`) on any
//! read/parse failure: bounded never prompts, so a typo can never re-introduce
//! the headless hang AND never silently drops the sandbox into a full bypass.
//! `headless_yolo:` is NOT distinctive across providers, so a flat scan (like
//! `finalize::read_path_setting`) cannot tell codex from gemini; this is a
//! nesting-aware scan of `config: > agents: > <provider>: > headless_yolo:`.

use std::path::{Path, PathBuf};

/// Resolve `config.agents.<provider>.headless_yolo` for the autonomous exec lane.
///
/// Mirrors the Python loader's candidate precedence (`fno.config`) so an
/// operator opt-out is honored identically on the Rust daemon/client path:
///   1. `$FNO_CONFIG` - explicit path, short-circuits (the SOLE source
///      when set, matching the Python loader; an empty value is treated as unset)
///   2. `<cwd>/.fno/settings.yaml` - project-local (in a worktree this is
///      symlinked to canonical, so it follows through)
///   3. global: `$FNO_GLOBAL_SETTINGS_PATH` when set, else
///      `$HOME/.fno/settings.yaml`
/// Degrades to `false` (the BOUNDED default, hang-safe) when no candidate
/// carries a well-formed key.
pub fn headless_yolo_enabled(provider: &str, cwd: &Path) -> bool {
    // 1. FNO_CONFIG is the only candidate when set (key absent -> default).
    if let Some(explicit) = non_empty_env("FNO_CONFIG") {
        return read_file(Path::new(&explicit), provider).unwrap_or(false);
    }
    // 2. project-local.
    if let Some(v) = read_file(&cwd.join(".fno/settings.yaml"), provider) {
        return v;
    }
    // 3. global (env override mirrors Python's _global_settings_path).
    let global = non_empty_env("FNO_GLOBAL_SETTINGS_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".fno/settings.yaml")));
    if let Some(g) = global {
        if let Some(v) = read_file(&g, provider) {
            return v;
        }
    }
    false
}

/// `std::env::var_os` but an empty value reads as unset, matching the Python
/// loader's treatment of `FNO_GLOBAL_SETTINGS_PATH=` (and FNO_CONFIG).
fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
    match std::env::var_os(key) {
        Some(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Fold the headless default into an explicit `yolo` opt-in. An explicit
/// `yolo=true` always wins; otherwise the headless default decides. Pure mirror
/// of `gemini.py::_effective_yolo` / `codex.py::_effective_yolo`.
pub fn effective_yolo(yolo: bool, headless_default: bool) -> bool {
    yolo || headless_default
}

fn read_file(path: &Path, provider: &str) -> Option<bool> {
    let content = std::fs::read_to_string(path).ok()?;
    read_headless_yolo(&content, provider)
}

/// Scan a settings.yaml body for `config: > agents: > <provider>: > headless_yolo:`.
/// Indent-unit-agnostic (2- or 4-space), mirroring `loopcheck`'s parser. Returns
/// `None` when the key is absent or malformed so the caller falls through to the
/// next file (and ultimately the hang-safe default).
pub(crate) fn read_headless_yolo(content: &str, provider: &str) -> Option<bool> {
    let unit = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.len() - l.trim_start().len())
        .find(|&i| i > 0)
        .unwrap_or(2);
    let level = |line: &str| -> usize { (line.len() - line.trim_start().len()) / unit };
    let provider_key = format!("{provider}:");

    let mut in_config = false;
    let mut in_agents = false;
    let mut in_provider = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match level(line) {
            0 => {
                in_config = trimmed.starts_with("config:");
                in_agents = false;
                in_provider = false;
            }
            1 if in_config => {
                in_agents = trimmed.starts_with("agents:");
                in_provider = false;
            }
            2 if in_agents => {
                in_provider = trimmed.starts_with(&provider_key);
            }
            3 if in_provider => {
                if let Some(rest) = trimmed.strip_prefix("headless_yolo:") {
                    return parse_bool(rest);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_bool(rest: &str) -> Option<bool> {
    let v = rest
        .split('#')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_ascii_lowercase();
    match v.as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_yolo_default_true_when_absent() {
        // No agents block -> hang-safe no-prompt default.
        assert_eq!(read_headless_yolo("schema_version: 1\n", "gemini"), None);
        assert_eq!(read_headless_yolo("schema_version: 1\n", "codex"), None);
    }

    #[test]
    fn headless_yolo_reads_per_provider_optout() {
        let yaml = "config:\n  agents:\n    gemini:\n      headless_yolo: false\n";
        assert_eq!(read_headless_yolo(yaml, "gemini"), Some(false));
        // codex untouched -> absent -> falls through to default.
        assert_eq!(read_headless_yolo(yaml, "codex"), None);
    }

    #[test]
    fn headless_yolo_does_not_confuse_providers_or_sibling_keys() {
        // confirm + a2a siblings must not be mistaken for the provider block.
        let yaml = "config:\n  agents:\n    confirm: auto\n    a2a:\n      auto: true\n    codex:\n      headless_yolo: false\n    gemini:\n      headless_yolo: true\n";
        assert_eq!(read_headless_yolo(yaml, "codex"), Some(false));
        assert_eq!(read_headless_yolo(yaml, "gemini"), Some(true));
    }

    #[test]
    fn headless_yolo_handles_four_space_indent() {
        let yaml = "config:\n    agents:\n        gemini:\n            headless_yolo: false\n";
        assert_eq!(read_headless_yolo(yaml, "gemini"), Some(false));
    }

    #[test]
    fn headless_yolo_malformed_value_is_none_not_a_guess() {
        let yaml = "config:\n  agents:\n    gemini:\n      headless_yolo: banana\n";
        assert_eq!(read_headless_yolo(yaml, "gemini"), None);
    }

    #[test]
    fn headless_yolo_ignores_non_agents_config() {
        // A headless_yolo: under some other block must not match.
        let yaml = "config:\n  target:\n    headless_yolo: false\n";
        assert_eq!(read_headless_yolo(yaml, "gemini"), None);
    }

    #[test]
    fn effective_yolo_or_semantics() {
        // explicit yolo wins; otherwise the headless default decides.
        assert!(effective_yolo(true, false));
        assert!(effective_yolo(false, true));
        assert!(!effective_yolo(false, false));
        assert!(effective_yolo(true, true));
    }

    // FNO_CONFIG / FNO_GLOBAL_SETTINGS_PATH are process-global; serialize
    // every test whose result depends on headless_yolo_enabled's env precedence
    // so a concurrent test cannot observe a half-set env (the same discipline as
    // provider.rs's HOME_LOCK). The pure read_headless_yolo tests above touch no
    // env and need no lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_config_env() {
        std::env::remove_var("FNO_CONFIG");
        std::env::remove_var("FNO_GLOBAL_SETTINGS_PATH");
    }

    fn write_project_settings(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abi-headless-{}-{name}", std::process::id()));
        let abil = dir.join(".fno");
        std::fs::create_dir_all(&abil).unwrap();
        std::fs::write(abil.join("settings.yaml"), body).unwrap();
        dir
    }

    fn write_file(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abi-headless-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("explicit.yaml");
        std::fs::write(&f, body).unwrap();
        f
    }

    #[test]
    fn headless_yolo_enabled_reads_project_local_optout() {
        // A project-local opt-out is honored (no FNO_CONFIG override set).
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings(
            "optout",
            "config:\n  agents:\n    gemini:\n      headless_yolo: false\n",
        );
        assert!(!headless_yolo_enabled("gemini", &cwd));
    }

    #[test]
    fn headless_yolo_enabled_reads_project_local_on() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings(
            "on",
            "config:\n  agents:\n    gemini:\n      headless_yolo: true\n",
        );
        assert!(headless_yolo_enabled("gemini", &cwd));
    }

    #[test]
    fn headless_yolo_enabled_honors_abilities_config_short_circuit() {
        // FNO_CONFIG is the SOLE source when set (mirrors the Python
        // loader), so a full-yolo opt-in there wins even though the cwd carries
        // no settings (which would otherwise resolve to the BOUNDED default).
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let f = write_file(
            "explicit-fullyolo",
            "config:\n  agents:\n    gemini:\n      headless_yolo: true\n",
        );
        std::env::set_var("FNO_CONFIG", &f);
        let cwd = std::env::temp_dir().join(format!("abi-headless-{}-nocfg", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let got = headless_yolo_enabled("gemini", &cwd);
        clear_config_env();
        assert!(
            got,
            "FNO_CONFIG full-yolo opt-in must be honored on the Rust path"
        );
    }

    #[test]
    fn headless_yolo_enabled_abilities_config_absent_key_defaults_bounded() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let f = write_file("explicit-empty", "schema_version: 1\n");
        std::env::set_var("FNO_CONFIG", &f);
        let cwd = std::env::temp_dir().join(format!("abi-headless-{}-nocfg2", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let got = headless_yolo_enabled("codex", &cwd);
        clear_config_env();
        assert!(
            !got,
            "absent key under FNO_CONFIG -> hang-safe BOUNDED default (false)"
        );
    }
}

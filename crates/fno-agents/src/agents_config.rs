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

/// Default dead-row grace window: 1h (matches `config.agents.dead_row_grace`'s
/// Pydantic default). A finished agent-view row stays visible this long after the
/// GC first observes its process gone, before it is reaped (x-b1aa).
pub const DEFAULT_DEAD_ROW_GRACE_SECS: u64 = 3600;

/// Resolve `config.agents.dead_row_grace` (seconds) for the daemon GC sweep and
/// `fno agents reap`. Precedence, degrading to [`DEFAULT_DEAD_ROW_GRACE_SECS`]:
///   1. `$FNO_AGENTS_DEAD_ROW_GRACE_SECS` - test/tuning override (mirrors
///      `FNO_AGENTS_IDLE_EXIT_SECS`); an unparseable value is ignored.
///   2. `$FNO_CONFIG` - explicit settings path, the SOLE file source when set
///      (mirrors the Python loader and `headless_yolo_enabled`).
///   3. `<cwd>/.fno/settings.yaml` - project-local.
///   4. global: `$FNO_GLOBAL_SETTINGS_PATH` else `$HOME/.fno/settings.yaml`.
pub fn dead_row_grace_secs(cwd: &Path) -> u64 {
    if let Some(v) = non_empty_env("FNO_AGENTS_DEAD_ROW_GRACE_SECS")
        .and_then(|s| s.to_str().and_then(|s| s.trim().parse::<u64>().ok()))
    {
        return v;
    }
    if let Some(explicit) = non_empty_env("FNO_CONFIG") {
        return read_grace_file(Path::new(&explicit)).unwrap_or(DEFAULT_DEAD_ROW_GRACE_SECS);
    }
    if let Some(v) = read_grace_file(&cwd.join(".fno/settings.yaml")) {
        return v;
    }
    let global = non_empty_env("FNO_GLOBAL_SETTINGS_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".fno/settings.yaml")));
    if let Some(g) = global {
        if let Some(v) = read_grace_file(&g) {
            return v;
        }
    }
    DEFAULT_DEAD_ROW_GRACE_SECS
}

fn read_grace_file(path: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(path).ok()?;
    read_dead_row_grace(&content)
}

/// Scan a settings.yaml body for `config: > agents: > dead_row_grace:` (a direct
/// child of `agents:`, like `confirm`). Indent-unit-agnostic, mirroring
/// [`read_headless_yolo`]. `None` when absent or unparseable so the caller falls
/// through to the next file (and ultimately the default).
pub(crate) fn read_dead_row_grace(content: &str) -> Option<u64> {
    let unit = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.len() - l.trim_start().len())
        .find(|&i| i > 0)
        .unwrap_or(2);
    let level = |line: &str| -> usize { (line.len() - line.trim_start().len()) / unit };

    let mut in_config = false;
    let mut in_agents = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match level(line) {
            0 => {
                in_config = trimmed.starts_with("config:");
                in_agents = false;
            }
            1 if in_config => {
                in_agents = trimmed.starts_with("agents:");
            }
            2 if in_agents => {
                if let Some(rest) = trimmed.strip_prefix("dead_row_grace:") {
                    return parse_u64(rest);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_u64(rest: &str) -> Option<u64> {
    rest.split('#')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .parse::<u64>()
        .ok()
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

// --- Spawn-gate knobs (x-c5cc). Same precedence + fail-open degrade as
// `dead_row_grace_secs`; all three coerce invalid values to their defaults so
// a config typo can never brick the spawn primitive.

/// Default global cap on concurrent live worker processes (union of the fno
/// registry and claude's daemon roster). Matches the Pydantic default.
pub const DEFAULT_MAX_LIVE: u32 = 3;
/// Default available-RAM floor (GB) for spawn preflight. `<= 0` disables.
pub const DEFAULT_MIN_FREE_GB: f64 = 4.0;

/// Resolve `config.agents.max_live`. Values < 1 (or unparseable) coerce to
/// [`DEFAULT_MAX_LIVE`] — never 0, which would block all spawns.
pub fn max_live(cwd: &Path) -> u32 {
    match resolve_agents_value(cwd, "max_live").and_then(|raw| raw.parse::<u32>().ok()) {
        Some(v) if v >= 1 => v,
        _ => DEFAULT_MAX_LIVE,
    }
}

/// Resolve `config.agents.min_free_gb`. `<= 0` is a VALID value (guard
/// disabled); only an unparseable value falls back to [`DEFAULT_MIN_FREE_GB`].
pub fn min_free_gb(cwd: &Path) -> f64 {
    resolve_agents_value(cwd, "min_free_gb")
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(DEFAULT_MIN_FREE_GB)
}

/// Resolve `config.agents.worker_qos`: `true` = demote workers (the `utility`
/// default), `false` = `off`. Any other value coerces to the default.
pub fn worker_qos_enabled(cwd: &Path) -> bool {
    match resolve_agents_value(cwd, "worker_qos").as_deref() {
        Some("off") => false,
        _ => true,
    }
}

/// FNO_CONFIG-sole > project-local > global precedence for a direct child of
/// `agents:` (the `dead_row_grace_secs` chain, generalized). Returns the raw
/// trimmed scalar so each caller applies its own coercion.
fn resolve_agents_value(cwd: &Path, key: &str) -> Option<String> {
    if let Some(explicit) = non_empty_env("FNO_CONFIG") {
        return read_agents_value_file(Path::new(&explicit), key);
    }
    if let Some(v) = read_agents_value_file(&cwd.join(".fno/settings.yaml"), key) {
        return Some(v);
    }
    let global = non_empty_env("FNO_GLOBAL_SETTINGS_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".fno/settings.yaml")));
    if let Some(g) = global {
        if let Some(v) = read_agents_value_file(&g, key) {
            return Some(v);
        }
    }
    None
}

fn read_agents_value_file(path: &Path, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    read_agents_value(&content, key)
}

/// Scan a settings.yaml body for `config: > agents: > <key>:` (a direct child
/// of `agents:`, like `dead_row_grace`). Indent-unit-agnostic. Returns the raw
/// scalar (comment-stripped, quote-trimmed, lowercased) or `None` when absent.
pub(crate) fn read_agents_value(content: &str, key: &str) -> Option<String> {
    let unit = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.len() - l.trim_start().len())
        .find(|&i| i > 0)
        .unwrap_or(2);
    let level = |line: &str| -> usize { (line.len() - line.trim_start().len()) / unit };

    let mut in_config = false;
    let mut in_agents = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match level(line) {
            0 => {
                in_config = trimmed.starts_with("config:");
                in_agents = false;
            }
            1 if in_config => {
                in_agents = trimmed.starts_with("agents:");
            }
            2 if in_agents => {
                if let Some(rest) = trimmed.strip_prefix(key).and_then(|r| r.strip_prefix(':')) {
                    let v = rest
                        .split('#')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'')
                        .to_ascii_lowercase();
                    if v.is_empty() {
                        return None;
                    }
                    return Some(v);
                }
            }
            _ => {}
        }
    }
    None
}

/// `config.mux.notify_on_blocked` (default ON): the daemon fires an OS
/// notification when a badge ENTERS `blocked` (x-dd84). Same file precedence
/// and hang-safe degrade as [`dead_row_grace_secs`].
pub fn notify_on_blocked_enabled(cwd: &Path) -> bool {
    mux_bool(cwd, "notify_on_blocked", true)
}

/// `config.mux.notify_on_done` (default OFF): also notify on a terminal `done`
/// hook transition (the scrape path has no `done`, so this only affects the
/// inside-leg hook).
pub fn notify_on_done_enabled(cwd: &Path) -> bool {
    mux_bool(cwd, "notify_on_done", false)
}

/// Resolve a `config: > mux: > <key>` boolean, mirroring [`dead_row_grace_secs`]'s
/// candidate precedence (`$FNO_CONFIG` sole-when-set > project-local > global)
/// and degrading to `default` when no candidate carries the key.
fn mux_bool(cwd: &Path, key: &str, default: bool) -> bool {
    if let Some(explicit) = non_empty_env("FNO_CONFIG") {
        return read_mux_file(Path::new(&explicit), key).unwrap_or(default);
    }
    if let Some(v) = read_mux_file(&cwd.join(".fno/settings.yaml"), key) {
        return v;
    }
    let global = non_empty_env("FNO_GLOBAL_SETTINGS_PATH")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".fno/settings.yaml")));
    if let Some(g) = global {
        if let Some(v) = read_mux_file(&g, key) {
            return v;
        }
    }
    default
}

fn read_mux_file(path: &Path, key: &str) -> Option<bool> {
    let content = std::fs::read_to_string(path).ok()?;
    read_mux_bool(&content, key)
}

/// Scan a settings.yaml body for `config: > mux: > <key>:` (a direct child of
/// `mux:`, like `dead_row_grace` under `agents:`). Indent-unit-agnostic. `None`
/// when absent or non-boolean so the caller falls through to the default.
pub(crate) fn read_mux_bool(content: &str, key: &str) -> Option<bool> {
    let unit = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.len() - l.trim_start().len())
        .find(|&i| i > 0)
        .unwrap_or(2);
    let level = |line: &str| -> usize { (line.len() - line.trim_start().len()) / unit };

    let mut in_config = false;
    let mut in_mux = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match level(line) {
            0 => {
                in_config = trimmed.starts_with("config:");
                in_mux = false;
            }
            1 if in_config => {
                in_mux = trimmed.starts_with("mux:");
            }
            2 if in_mux => {
                if let Some(rest) = trimmed.strip_prefix(key).and_then(|r| r.strip_prefix(':')) {
                    return parse_bool(rest);
                }
            }
            _ => {}
        }
    }
    None
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
    fn dead_row_grace_reads_agents_child_key() {
        let yaml = "config:\n  agents:\n    confirm: auto\n    dead_row_grace: 7200\n";
        assert_eq!(read_dead_row_grace(yaml), Some(7200));
    }

    #[test]
    fn dead_row_grace_absent_is_none() {
        assert_eq!(
            read_dead_row_grace("config:\n  agents:\n    confirm: auto\n"),
            None
        );
        assert_eq!(read_dead_row_grace("schema_version: 1\n"), None);
    }

    #[test]
    fn dead_row_grace_ignores_provider_nested_and_bad_values() {
        // A key at provider depth (level 3) must NOT be read as the agents-child.
        let nested = "config:\n  agents:\n    codex:\n      dead_row_grace: 5\n";
        assert_eq!(read_dead_row_grace(nested), None);
        // Non-integer value -> None (falls through to default).
        let bad = "config:\n  agents:\n    dead_row_grace: banana\n";
        assert_eq!(read_dead_row_grace(bad), None);
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
    fn agents_value_reads_spawn_gate_keys() {
        let yaml = "config:\n  agents:\n    confirm: auto\n    max_live: 5\n    min_free_gb: 2.5\n    worker_qos: off\n";
        assert_eq!(read_agents_value(yaml, "max_live").as_deref(), Some("5"));
        assert_eq!(
            read_agents_value(yaml, "min_free_gb").as_deref(),
            Some("2.5")
        );
        assert_eq!(read_agents_value(yaml, "worker_qos").as_deref(), Some("off"));
    }

    #[test]
    fn agents_value_absent_nested_or_prefix_is_none() {
        assert_eq!(read_agents_value("schema_version: 1\n", "max_live"), None);
        // provider-depth key must not read as the agents child.
        let nested = "config:\n  agents:\n    codex:\n      max_live: 9\n";
        assert_eq!(read_agents_value(nested, "max_live"), None);
        // prefix keys must not match without the ':' boundary.
        let prefix = "config:\n  agents:\n    max_live_extra: 9\n";
        assert_eq!(read_agents_value(prefix, "max_live"), None);
    }

    #[test]
    fn spawn_gate_knobs_coerce_invalid_to_defaults() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        // max_live: 0 and banana both coerce to the default, never 0.
        let f = write_file(
            "gate-coerce",
            "config:\n  agents:\n    max_live: 0\n    min_free_gb: banana\n    worker_qos: turbo\n",
        );
        std::env::set_var("FNO_CONFIG", &f);
        let cwd = std::env::temp_dir();
        let (ml, mf, qos) = (max_live(&cwd), min_free_gb(&cwd), worker_qos_enabled(&cwd));
        clear_config_env();
        assert_eq!(ml, DEFAULT_MAX_LIVE);
        assert_eq!(mf, DEFAULT_MIN_FREE_GB);
        assert!(qos, "unknown worker_qos coerces to utility (enabled)");
    }

    #[test]
    fn spawn_gate_knobs_read_valid_values() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let f = write_file(
            "gate-valid",
            "config:\n  agents:\n    max_live: 7\n    min_free_gb: 0\n    worker_qos: off\n",
        );
        std::env::set_var("FNO_CONFIG", &f);
        let cwd = std::env::temp_dir();
        let (ml, mf, qos) = (max_live(&cwd), min_free_gb(&cwd), worker_qos_enabled(&cwd));
        clear_config_env();
        assert_eq!(ml, 7);
        assert_eq!(mf, 0.0, "min_free_gb: 0 is valid (guard disabled)");
        assert!(!qos);
    }

    #[test]
    fn mux_bool_reads_mux_child_key() {
        let yaml = "config:\n  mux:\n    notify_on_blocked: false\n    notify_on_done: true\n";
        assert_eq!(read_mux_bool(yaml, "notify_on_blocked"), Some(false));
        assert_eq!(read_mux_bool(yaml, "notify_on_done"), Some(true));
    }

    #[test]
    fn mux_bool_absent_is_none() {
        assert_eq!(
            read_mux_bool(
                "config:\n  agents:\n    confirm: auto\n",
                "notify_on_blocked"
            ),
            None
        );
        assert_eq!(read_mux_bool("schema_version: 1\n", "notify_on_done"), None);
    }

    #[test]
    fn mux_bool_ignores_nested_and_bad_values() {
        // A key one level too deep must NOT be read as the mux-child.
        let nested = "config:\n  mux:\n    pane:\n      notify_on_blocked: false\n";
        assert_eq!(read_mux_bool(nested, "notify_on_blocked"), None);
        // Non-boolean value -> None (falls through to the compiled default).
        let bad = "config:\n  mux:\n    notify_on_blocked: banana\n";
        assert_eq!(read_mux_bool(bad, "notify_on_blocked"), None);
        // A prefix key must not match without the ':' boundary.
        let prefix = "config:\n  mux:\n    notify_on_blocked_extra: true\n";
        assert_eq!(read_mux_bool(prefix, "notify_on_blocked"), None);
    }

    #[test]
    fn mux_bool_handles_four_space_indent() {
        let yaml = "config:\n    mux:\n        notify_on_done: on\n";
        assert_eq!(read_mux_bool(yaml, "notify_on_done"), Some(true));
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

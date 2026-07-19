//! Read `agents.<provider>.headless_yolo` (and sibling knobs) from config.toml.
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
//!
//! Stage 3 (x-8526): the on-disk file is flat `config.toml`, parsed with the
//! `toml` crate. A `config.toml`-only reader is safe because a Rust runtime is
//! spawned by Python flows that auto-migrate a legacy settings.yaml on their
//! first config load, so the flat file is already present by the time this runs.

use std::path::{Path, PathBuf};

use toml::Value;

/// `std::env::var_os` but an empty value reads as unset, matching the Python
/// loader's treatment of `FNO_GLOBAL_SETTINGS_PATH=` (and FNO_CONFIG).
fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
    match std::env::var_os(key) {
        Some(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// The per-user global config.toml, mirroring Python's `_global_settings_path` +
/// `_prefer_toml`: read the config.toml SIBLING of `$FNO_GLOBAL_SETTINGS_PATH`
/// when set, else `$HOME/.fno/config.toml`.
fn global_config_path() -> Option<PathBuf> {
    if let Some(p) = non_empty_env("FNO_GLOBAL_SETTINGS_PATH") {
        return Some(PathBuf::from(p).with_file_name("config.toml"));
    }
    std::env::var_os("HOME").map(|h| Path::new(&h).join(".fno/config.toml"))
}

/// Ordered config.toml read candidates, mirroring the Python loader precedence:
/// `$FNO_CONFIG` is the SOLE candidate when set (an explicit path, read as-is);
/// otherwise `<cwd>/.fno/config.toml` then the global config.toml.
fn config_candidates(cwd: &Path) -> Vec<PathBuf> {
    if let Some(explicit) = non_empty_env("FNO_CONFIG") {
        return vec![PathBuf::from(explicit)];
    }
    let mut out = vec![cwd.join(".fno/config.toml")];
    if let Some(g) = global_config_path() {
        out.push(g);
    }
    out
}

/// Parse a flat config.toml body into a table; `None` on any parse error (a
/// malformed file degrades every getter to its hang-safe default).
fn parse_config(content: &str) -> Option<toml::Table> {
    content.parse::<toml::Table>().ok()
}

/// First candidate config.toml that yields `Some(T)` via `extract`.
fn resolve<T>(cwd: &Path, extract: impl Fn(&toml::Table) -> Option<T>) -> Option<T> {
    for path in config_candidates(cwd) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Some(table) = parse_config(&content) {
                if let Some(v) = extract(&table) {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn table_headless_yolo(t: &toml::Table, provider: &str) -> Option<bool> {
    t.get("agents")?
        .as_table()?
        .get(provider)?
        .as_table()?
        .get("headless_yolo")?
        .as_bool()
}

/// A direct child scalar of `agents:` (e.g. `dead_row_grace`, `max_live`), NOT a
/// provider-nested key: `agents.<provider>.<key>` never matches here.
fn table_agents_scalar(t: &toml::Table, key: &str) -> Option<Value> {
    t.get("agents")?.as_table()?.get(key).cloned()
}

fn table_mux_bool(t: &toml::Table, key: &str) -> Option<bool> {
    t.get("mux")?.as_table()?.get(key)?.as_bool()
}

/// Normalize a scalar toml value to the raw string each caller re-coerces
/// (mirrors the old scanner contract: strings lowercased, numbers stringified).
fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.to_ascii_lowercase()),
        Value::Integer(i) => Some(i.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Resolve `agents.<provider>.headless_yolo` for the autonomous exec lane.
/// Degrades to `false` (the BOUNDED default, hang-safe) when no candidate
/// carries a well-formed key.
pub fn headless_yolo_enabled(provider: &str, cwd: &Path) -> bool {
    resolve(cwd, |t| table_headless_yolo(t, provider)).unwrap_or(false)
}

/// Fold the headless default into an explicit `yolo` opt-in. An explicit
/// `yolo=true` always wins; otherwise the headless default decides. Pure mirror
/// of `gemini.py::_effective_yolo` / `codex.py::_effective_yolo`.
pub fn effective_yolo(yolo: bool, headless_default: bool) -> bool {
    yolo || headless_default
}

/// Default dead-row grace window: 1h (matches `agents.dead_row_grace`'s
/// Pydantic default). A finished agent-view row stays visible this long after the
/// GC first observes its process gone, before it is reaped (x-b1aa).
pub const DEFAULT_DEAD_ROW_GRACE_SECS: u64 = 3600;

/// Resolve `agents.dead_row_grace` (seconds) for the daemon GC sweep and
/// `fno agents reap`. `$FNO_AGENTS_DEAD_ROW_GRACE_SECS` is a test/tuning
/// override; otherwise the config.toml chain, degrading to the default.
pub fn dead_row_grace_secs(cwd: &Path) -> u64 {
    if let Some(v) = non_empty_env("FNO_AGENTS_DEAD_ROW_GRACE_SECS")
        .and_then(|s| s.to_str().and_then(|s| s.trim().parse::<u64>().ok()))
    {
        return v;
    }
    resolve(cwd, |t| {
        table_agents_scalar(t, "dead_row_grace")?
            .as_integer()
            .and_then(|i| u64::try_from(i).ok())
    })
    .unwrap_or(DEFAULT_DEAD_ROW_GRACE_SECS)
}

// --- Spawn-gate knobs (x-c5cc). Same precedence + fail-open degrade as
// `dead_row_grace_secs`; all coerce invalid values to their defaults so a config
// typo can never brick the spawn primitive.

/// Default global cap on concurrent live worker processes (union of the fno
/// registry and claude's daemon roster). Matches the Pydantic default.
pub const DEFAULT_MAX_LIVE: u32 = 3;
/// Default available-RAM floor (GB) for spawn preflight. `<= 0` disables.
pub const DEFAULT_MIN_FREE_GB: f64 = 4.0;

/// Resolve `agents.max_live`. Values < 1 (or unparseable) coerce to
/// [`DEFAULT_MAX_LIVE`] — never 0, which would block all spawns.
pub fn max_live(cwd: &Path) -> u32 {
    match resolve_agents_value(cwd, "max_live").and_then(|raw| raw.parse::<u32>().ok()) {
        Some(v) if v >= 1 => v,
        _ => DEFAULT_MAX_LIVE,
    }
}

/// Resolve `agents.min_free_gb`. `<= 0` is a VALID value (guard disabled); only
/// an unparseable value falls back to [`DEFAULT_MIN_FREE_GB`].
pub fn min_free_gb(cwd: &Path) -> f64 {
    resolve_agents_value(cwd, "min_free_gb")
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(DEFAULT_MIN_FREE_GB)
}

/// Resolve `agents.worker_qos`: `true` = demote workers (the `utility` default),
/// `"off"` = no demotion. Any other value coerces to the default.
pub fn worker_qos_enabled(cwd: &Path) -> bool {
    !matches!(
        resolve_agents_value(cwd, "worker_qos").as_deref(),
        Some("off")
    )
}

/// Resolve `dispatch.auto_merge`: the per-project merge posture for autonomous
/// dispatch (x-4391). Degrades to `false` (no-merge) on any missing/malformed
/// value, so a config error never grants merge rights (Locked Decision 6). Only
/// a real TOML boolean grants merge — `as_bool()` returns None for a string like
/// `"yes"`, mirroring the Python `DispatchBlock` coercer.
pub fn dispatch_auto_merge(cwd: &Path) -> bool {
    resolve(cwd, |t| {
        t.get("dispatch")?.as_table()?.get("auto_merge")?.as_bool()
    })
    .unwrap_or(false)
}

/// The normalized raw scalar for a direct child of `agents:` (the generalized
/// `dead_row_grace_secs` chain), so each caller applies its own coercion.
fn resolve_agents_value(cwd: &Path, key: &str) -> Option<String> {
    resolve(cwd, |t| {
        table_agents_scalar(t, key)
            .as_ref()
            .and_then(scalar_to_string)
    })
}

/// `mux.notify_on_blocked` (default ON): the daemon fires an OS notification when
/// a badge ENTERS `blocked` (x-dd84).
pub fn notify_on_blocked_enabled(cwd: &Path) -> bool {
    mux_bool(cwd, "notify_on_blocked", true)
}

/// `mux.notify_on_done` (default OFF): also notify on a terminal `done` hook
/// transition (the scrape path has no `done`, so this only affects the
/// inside-leg hook).
pub fn notify_on_done_enabled(cwd: &Path) -> bool {
    mux_bool(cwd, "notify_on_done", false)
}

/// Resolve a `mux.<key>` boolean, degrading to `default` when no candidate
/// config.toml carries the key.
fn mux_bool(cwd: &Path, key: &str, default: bool) -> bool {
    resolve(cwd, |t| table_mux_bool(t, key)).unwrap_or(default)
}

// --- Pure content-based readers (test surface + the resolve() extractors). ---

/// `agents.<provider>.headless_yolo` from a config.toml body.
#[cfg(test)]
pub(crate) fn read_headless_yolo(content: &str, provider: &str) -> Option<bool> {
    table_headless_yolo(&parse_config(content)?, provider)
}

/// `agents.dead_row_grace` (a direct child of `agents:`) from a config.toml body.
#[cfg(test)]
pub(crate) fn read_dead_row_grace(content: &str) -> Option<u64> {
    table_agents_scalar(&parse_config(content)?, "dead_row_grace")?
        .as_integer()
        .and_then(|i| u64::try_from(i).ok())
}

/// A normalized `agents.<key>` scalar (direct child) from a config.toml body.
#[cfg(test)]
pub(crate) fn read_agents_value(content: &str, key: &str) -> Option<String> {
    scalar_to_string(&table_agents_scalar(&parse_config(content)?, key)?)
}

/// `mux.<key>` boolean from a config.toml body.
#[cfg(test)]
pub(crate) fn read_mux_bool(content: &str, key: &str) -> Option<bool> {
    table_mux_bool(&parse_config(content)?, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_yolo_default_true_when_absent() {
        // No agents block -> hang-safe no-prompt default.
        assert_eq!(read_headless_yolo("schema_version = 1\n", "gemini"), None);
        assert_eq!(read_headless_yolo("schema_version = 1\n", "codex"), None);
    }

    #[test]
    fn headless_yolo_reads_per_provider_optout() {
        let cfg = "[agents.gemini]\nheadless_yolo = false\n";
        assert_eq!(read_headless_yolo(cfg, "gemini"), Some(false));
        // codex untouched -> absent -> falls through to default.
        assert_eq!(read_headless_yolo(cfg, "codex"), None);
    }

    #[test]
    fn dead_row_grace_reads_agents_child_key() {
        let cfg = "[agents]\nconfirm = \"auto\"\ndead_row_grace = 7200\n";
        assert_eq!(read_dead_row_grace(cfg), Some(7200));
    }

    #[test]
    fn dead_row_grace_absent_is_none() {
        assert_eq!(read_dead_row_grace("[agents]\nconfirm = \"auto\"\n"), None);
        assert_eq!(read_dead_row_grace("schema_version = 1\n"), None);
    }

    #[test]
    fn dead_row_grace_ignores_provider_nested_and_bad_values() {
        // A key at provider depth must NOT be read as the agents-child.
        let nested = "[agents.codex]\ndead_row_grace = 5\n";
        assert_eq!(read_dead_row_grace(nested), None);
        // Non-integer value -> None (falls through to default).
        let bad = "[agents]\ndead_row_grace = \"banana\"\n";
        assert_eq!(read_dead_row_grace(bad), None);
    }

    #[test]
    fn headless_yolo_does_not_confuse_providers_or_sibling_keys() {
        // confirm + a2a siblings must not be mistaken for the provider block.
        let cfg = "[agents]\nconfirm = \"auto\"\n\n[agents.a2a]\nauto = true\n\n\
                   [agents.codex]\nheadless_yolo = false\n\n\
                   [agents.gemini]\nheadless_yolo = true\n";
        assert_eq!(read_headless_yolo(cfg, "codex"), Some(false));
        assert_eq!(read_headless_yolo(cfg, "gemini"), Some(true));
    }

    #[test]
    fn headless_yolo_reads_inline_provider_table() {
        // An inline-table provider entry resolves the same as a [agents.x] block.
        let cfg = "[agents]\ngemini = { headless_yolo = false }\n";
        assert_eq!(read_headless_yolo(cfg, "gemini"), Some(false));
    }

    #[test]
    fn headless_yolo_malformed_value_is_none_not_a_guess() {
        let cfg = "[agents.gemini]\nheadless_yolo = \"banana\"\n";
        assert_eq!(read_headless_yolo(cfg, "gemini"), None);
    }

    #[test]
    fn headless_yolo_ignores_non_agents_config() {
        // A headless_yolo under some other block must not match.
        let cfg = "[target]\nheadless_yolo = false\n";
        assert_eq!(read_headless_yolo(cfg, "gemini"), None);
    }

    #[test]
    fn agents_value_reads_spawn_gate_keys() {
        let cfg =
            "[agents]\nconfirm = \"auto\"\nmax_live = 5\nmin_free_gb = 2.5\nworker_qos = \"off\"\n";
        assert_eq!(read_agents_value(cfg, "max_live").as_deref(), Some("5"));
        assert_eq!(
            read_agents_value(cfg, "min_free_gb").as_deref(),
            Some("2.5")
        );
        assert_eq!(read_agents_value(cfg, "worker_qos").as_deref(), Some("off"));
    }

    #[test]
    fn agents_value_absent_nested_or_prefix_is_none() {
        assert_eq!(read_agents_value("schema_version = 1\n", "max_live"), None);
        // provider-depth key must not read as the agents child.
        let nested = "[agents.codex]\nmax_live = 9\n";
        assert_eq!(read_agents_value(nested, "max_live"), None);
        // prefix keys must not match without an exact key.
        let prefix = "[agents]\nmax_live_extra = 9\n";
        assert_eq!(read_agents_value(prefix, "max_live"), None);
    }

    #[test]
    fn spawn_gate_knobs_coerce_invalid_to_defaults() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        // max_live: 0 and a non-numeric min_free_gb both coerce to the default.
        let f = write_file(
            "gate-coerce",
            "[agents]\nmax_live = 0\nmin_free_gb = \"banana\"\nworker_qos = \"turbo\"\n",
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
            "[agents]\nmax_live = 7\nmin_free_gb = 0\nworker_qos = \"off\"\n",
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
        let cfg = "[mux]\nnotify_on_blocked = false\nnotify_on_done = true\n";
        assert_eq!(read_mux_bool(cfg, "notify_on_blocked"), Some(false));
        assert_eq!(read_mux_bool(cfg, "notify_on_done"), Some(true));
    }

    #[test]
    fn mux_bool_absent_is_none() {
        assert_eq!(
            read_mux_bool("[agents]\nconfirm = \"auto\"\n", "notify_on_blocked"),
            None
        );
        assert_eq!(
            read_mux_bool("schema_version = 1\n", "notify_on_done"),
            None
        );
    }

    #[test]
    fn mux_bool_ignores_nested_and_bad_values() {
        // A key one level too deep must NOT be read as the mux-child.
        let nested = "[mux.pane]\nnotify_on_blocked = false\n";
        assert_eq!(read_mux_bool(nested, "notify_on_blocked"), None);
        // Non-boolean value -> None (falls through to the compiled default).
        let bad = "[mux]\nnotify_on_blocked = \"banana\"\n";
        assert_eq!(read_mux_bool(bad, "notify_on_blocked"), None);
        // A prefix key must not match without the exact key.
        let prefix = "[mux]\nnotify_on_blocked_extra = true\n";
        assert_eq!(read_mux_bool(prefix, "notify_on_blocked"), None);
    }

    #[test]
    fn mux_bool_reads_true() {
        let cfg = "[mux]\nnotify_on_done = true\n";
        assert_eq!(read_mux_bool(cfg, "notify_on_done"), Some(true));
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
    // every test whose result depends on the env precedence so a concurrent test
    // cannot observe a half-set env (the same discipline as provider.rs's
    // HOME_LOCK). The pure content-based readers above touch no env and need no
    // lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_config_env() {
        std::env::remove_var("FNO_CONFIG");
        // Point the global tier at an empty directory rather than unsetting it:
        // an unset FNO_GLOBAL_SETTINGS_PATH falls back to the REAL
        // $HOME/.fno/config.toml, so every "absent key -> default" assertion
        // would read the developer's own config. Clean CI has no global config,
        // which is why this only ever failed on a configured machine.
        let iso = std::env::temp_dir().join(format!("fno-agents-noglobal-{}", std::process::id()));
        std::fs::create_dir_all(&iso).unwrap();
        std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", iso.join("settings.json"));
    }

    fn write_project_settings(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abi-headless-{}-{name}", std::process::id()));
        let abil = dir.join(".fno");
        std::fs::create_dir_all(&abil).unwrap();
        std::fs::write(abil.join("config.toml"), body).unwrap();
        dir
    }

    fn write_file(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abi-headless-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("explicit.toml");
        std::fs::write(&f, body).unwrap();
        f
    }

    #[test]
    fn headless_yolo_enabled_reads_project_local_optout() {
        // A project-local opt-out is honored (no FNO_CONFIG override set).
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("optout", "[agents.gemini]\nheadless_yolo = false\n");
        assert!(!headless_yolo_enabled("gemini", &cwd));
    }

    #[test]
    fn headless_yolo_enabled_reads_project_local_on() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("on", "[agents.gemini]\nheadless_yolo = true\n");
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
            "[agents.gemini]\nheadless_yolo = true\n",
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
        let f = write_file("explicit-empty", "schema_version = 1\n");
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

    // --- x-4391: dispatch.auto_merge reader ---------------------------------

    #[test]
    fn dispatch_auto_merge_absent_is_false() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("am-absent", "schema_version = 1\n");
        assert!(!dispatch_auto_merge(&cwd));
    }

    #[test]
    fn dispatch_auto_merge_true_grants() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("am-true", "[dispatch]\nauto_merge = true\n");
        assert!(dispatch_auto_merge(&cwd));
    }

    #[test]
    fn dispatch_auto_merge_false_no_merge() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("am-false", "[dispatch]\nauto_merge = false\n");
        assert!(!dispatch_auto_merge(&cwd));
    }

    #[test]
    fn dispatch_auto_merge_non_bool_degrades_to_false() {
        // Only a real TOML boolean grants merge (Locked Decision 6): a string
        // "yes" yields None from as_bool() -> false, mirroring the Python coercer.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("am-str", "[dispatch]\nauto_merge = \"yes\"\n");
        assert!(!dispatch_auto_merge(&cwd));
    }
}

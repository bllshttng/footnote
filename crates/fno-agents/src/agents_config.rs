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
//! Stage 3: the on-disk file is flat `config.toml`, parsed with the `toml`
//! crate. A `config.toml`-only reader is safe because a Rust runtime is spawned
//! by Python flows that auto-migrate a legacy settings.yaml on their first
//! config load, so the flat file is already present by the time this runs.
//!
//! That invariant has exactly one hole, and it is deliberate: `$FNO_CONFIG`
//! pinning a `.yaml` path is never migrated (Python parses it by suffix
//! instead), so this reader cannot see it. `warn_once_if_yaml` makes that case
//! loud rather than silent; see its comment for why it is not parsed here.

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

/// Ordered config read candidates, mirroring the Python loader precedence:
/// `$FNO_CONFIG` is the SOLE candidate when set (an explicit path, read as-is);
/// otherwise `<cwd>/.fno/config.toml`, the CANONICAL checkout's config.toml, then
/// the global one.
///
/// The canonical tier is load-bearing under this project's worktree-first
/// default. `setup-worktree.sh` symlinks `.fno/config.toml` into a worktree only
/// when it is run; a worktree made by `claude --worktree` or the harness
/// EnterWorktree has no local copy, so without this tier every getter skips the
/// project's real config and Rust silently disagrees with `fno config get`.
/// Deduped when the canonical root IS the cwd, and dropped entirely by
/// `FNO_NO_CANONICAL_CONFIG=1` (preflight's hermetic runner), exactly as Python
/// does at `config/__init__.py`'s `_settings_yaml_locations`.
fn config_candidates(cwd: &Path) -> Vec<PathBuf> {
    if let Some(explicit) = non_empty_env("FNO_CONFIG") {
        let path = PathBuf::from(explicit);
        warn_once_if_yaml(&path);
        return vec![path];
    }
    let mut out = vec![cwd.join(".fno/config.toml")];
    // Exactly "1" suppresses, any other value inert, matching the Python check.
    let suppress_canonical = std::env::var_os("FNO_NO_CANONICAL_CONFIG")
        .is_some_and(|v| v == *std::ffi::OsStr::new("1"));
    if !suppress_canonical {
        if let Some(canonical) = crate::paths::canonical_repo_root(cwd)
            .map(|root| root.join(".fno/config.toml"))
            .filter(|c| !out.contains(c))
        {
            out.push(canonical);
        }
    }
    if let Some(g) = global_config_path() {
        out.push(g);
    }
    out
}

/// The one config the Python loader reads and this one cannot.
///
/// A legacy `settings.yaml` at a STANDARD location is converted to a flat
/// config.toml on first load (`_ensure_migrated`), after which Python, Rust and
/// shell all see the same TOML. But that migration is skipped by design when
/// `$FNO_CONFIG` pins an explicit path, so Python keeps parsing the handed file
/// by suffix while this reader's TOML parse fails and EVERY getter silently
/// takes its default. Say so once instead of diverging in silence: a config
/// asking for `squash` that reads as `merge` is exactly the failure this module
/// exists to stop.
fn warn_once_if_yaml(path: &Path) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    if matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml" | "yml")
    ) {
        WARNED.call_once(|| {
            eprintln!(
                "fno-agents: $FNO_CONFIG points at {}, which this reader parses as TOML \
                 and cannot read; every config value falls back to its built-in default. \
                 Point it at a config.toml, or unset it to use the migrated file.",
                path.display()
            );
        });
    }
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

/// Normalize one configured GitHub login without changing its case. This
/// mirrors loop-check's TOML coercion: scalar strings/numbers/bools are legal
/// login spellings, while structured values are not.
fn review_login_scalar(v: &Value) -> Option<String> {
    let raw = match v {
        Value::String(s) => s.trim().to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Boolean(b) => b.to_string(),
        _ => return None,
    };
    (!raw.is_empty()).then_some(raw)
}

/// `review.optional_apps` from one parsed config candidate.
///
/// Presence always returns `Some`, including an explicit empty list or a
/// malformed structured value. That is the precedence boundary: project-local
/// `[]` must mask a global list, and malformed optional configuration degrades
/// to no optional reviewers instead of falling through to a lower tier.
fn table_review_optional_apps(t: &toml::Table) -> Option<Vec<String>> {
    let value = t.get("review")?.as_table()?.get("optional_apps")?;
    Some(match value {
        Value::Array(items) => items.iter().filter_map(review_login_scalar).collect(),
        Value::String(_) | Value::Integer(_) | Value::Float(_) | Value::Boolean(_) => {
            review_login_scalar(value).into_iter().collect()
        }
        _ => Vec::new(),
    })
}

/// Normalize a scalar toml value to the raw string each caller re-coerces
/// (mirrors the old scanner contract: strings lowercased, numbers stringified).
///
/// Trimmed as well as lowercased, matching Python's `_coerce_affirmative`, which
/// does `v.strip().lower()`. Without the trim a padded `" yes "` reads as false
/// here while `fno pr merge` reads it as true, and the numeric callers below
/// fail their `parse()` on padding the Python side tolerates. Trimming in this
/// one shared normalizer covers every caller; trimming per caller would leave
/// the siblings diverged.
fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.trim().to_ascii_lowercase()),
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

/// Resolve `agents.dead_row_grace` (seconds) for `harness`, for the daemon GC
/// sweep and `fno agents reap`. `$FNO_AGENTS_DEAD_ROW_GRACE_SECS` is a global
/// test/tuning override (unchanged by harness); otherwise the config.toml
/// chain, degrading to the default.
///
/// This is also the transcript-freshness window (`transcript_fresh_probe`
/// reuses whatever grace the caller resolves here), so a per-harness table is
/// the fix for a codex worker's normal multi-hour silence being misread as
/// staleness (x-9de7 task 6) -- not a second knob, the SAME one, keyed wider.
///
/// Two shapes are accepted at `agents.dead_row_grace`, mutually exclusive in
/// TOML by construction:
/// - a bare integer (today's shape): applies to every harness, unchanged.
/// - a table, `agents.dead_row_grace.<harness> = <seconds>`: looked up by
///   `harness` first; a harness with no key present falls through (to the
///   next config candidate, then the default) rather than inheriting a
///   sibling harness's number.
pub fn dead_row_grace_secs(cwd: &Path, harness: &str) -> u64 {
    if let Some(v) = non_empty_env("FNO_AGENTS_DEAD_ROW_GRACE_SECS")
        .and_then(|s| s.to_str().and_then(|s| s.trim().parse::<u64>().ok()))
    {
        return v;
    }
    resolve(cwd, |t| match table_agents_scalar(t, "dead_row_grace")? {
        Value::Integer(i) => u64::try_from(i).ok(),
        Value::Table(per_harness) => per_harness
            .get(harness)?
            .as_integer()
            .and_then(|i| u64::try_from(i).ok()),
        _ => None,
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

/// Resolve `auto_merge.grant`: the actor-scope merge grant for autonomous
/// dispatch (x-4391, one table since x-4be1). `true` = a dispatched worker may
/// merge (`grant = "dispatch"` or the legacy `dispatch.auto_merge = true`,
/// readable for one release); `false` = no-merge. Canonical first WITHIN each
/// candidate file, so a project that has migrated is never masked by its own
/// legacy key. Degrades to `false` on any missing/malformed value, so a config
/// error never grants merge rights (Locked Decision 6). Only the real literal
/// grants: `"dispatch"` trimmed-exact for the string, a real TOML boolean for
/// the legacy spelling (`as_bool()` rejects `"yes"`), mirroring the Python
/// `AutoMergeBlock` coercer and its `_alias_legacy_keys` arm. Parity with that
/// arm extends to fall-through: a legacy key that is PRESENT but not a bool is
/// a no-grant DECISION in this file (the Python per-layer fold writes
/// `grant = "none"`), never a fall-through that lets a lower-precedence file
/// resurrect the grant.
pub fn auto_merge_grant(cwd: &Path) -> bool {
    resolve(cwd, |t| {
        let grant = t
            .get("auto_merge")
            .and_then(|v| v.as_table())
            .and_then(|tbl| tbl.get("grant"));
        if let Some(g) = grant {
            // A PRESENT canonical key decides even when malformed (degrade to
            // no-grant): the same file's legacy spelling must not resurrect a
            // value the coercer just refused - that would make a typo grant.
            return Some(g.as_str().map(|s| s.trim() == "dispatch").unwrap_or(false));
        }
        // No canonical grant in this file; it may still carry the legacy
        // spelling. An ABSENT key falls through to the next candidate file
        // (same as every other resolve() extractor); a present-but-non-bool
        // value is a no-grant decision, mirroring the Python fold.
        let legacy = t.get("dispatch")?.as_table()?.get("auto_merge")?;
        Some(legacy.as_bool().unwrap_or(false))
    })
    .unwrap_or(false)
}

/// Resolve `review.optional_apps`, the GitHub App logins whose findings are
/// honored when present but whose absence never blocks `DonePRGreen`.
///
/// Finalize reads this list only to decide whether it may arm GitHub's native
/// auto-merge. Missing or malformed configuration resolves to an empty list;
/// an explicit empty list remains a real project-level override and masks any
/// lower-precedence global list.
pub fn review_optional_apps(cwd: &Path) -> Vec<String> {
    resolve(cwd, table_review_optional_apps).unwrap_or_default()
}

/// The `config.auto_merge.*` block, the knobs that shape the `gh pr merge`
/// argv. Distinct from `dispatch.auto_merge` above, which is the per-project
/// POSTURE (may we merge at all); these decide HOW, once something has already
/// decided to.
///
/// Both mirror `fno.config.AutoMergeBlock`, which is the source of truth;
/// `cli/tests/test_config_schema_drift.py` fails when the allowlist here drifts
/// from the model's. Both degrade to the value `finalize` hardcoded before it
/// read config at all, so a malformed file is never worse than the old behavior.
///
/// One known parity edge, shared with `auto_merge_grant`: a NON-STRING value
/// (`merge_strategy = 3`) yields `None` from the extractor and so falls through
/// to the next config candidate, where Python's merge-then-coerce loader would
/// let the malformed project value mask the global one. A misspelled string (the
/// realistic typo) is handled correctly, because the extractor takes any string
/// and the allowlist filter runs after resolution.
pub fn auto_merge_strategy(cwd: &Path) -> String {
    resolve(cwd, |t| {
        t.get("auto_merge")?
            .as_table()?
            .get("merge_strategy")?
            .as_str()
            .map(|s| s.trim().to_string())
    })
    .filter(|v| matches!(v.as_str(), "merge" | "squash" | "rebase"))
    .unwrap_or_else(|| "merge".to_string())
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

/// `agents.dead_row_grace`, resolved for `harness` -- either a bare scalar
/// (applies to every harness) or `agents.dead_row_grace.<harness>` (x-9de7
/// task 6). Same extraction `dead_row_grace_secs` runs against a config.toml
/// body, exposed directly so the resolution logic is tested without the
/// candidate-file walk.
#[cfg(test)]
pub(crate) fn read_dead_row_grace_for_harness(content: &str, harness: &str) -> Option<u64> {
    match table_agents_scalar(&parse_config(content)?, "dead_row_grace")? {
        Value::Integer(i) => u64::try_from(i).ok(),
        Value::Table(per_harness) => per_harness
            .get(harness)?
            .as_integer()
            .and_then(|i| u64::try_from(i).ok()),
        _ => None,
    }
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
    fn dead_row_grace_bare_integer_applies_to_every_harness() {
        // x-9de7 task 6 AC1: today's shape, unchanged behavior for any harness.
        let cfg = "[agents]\ndead_row_grace = 3600\n";
        assert_eq!(read_dead_row_grace_for_harness(cfg, "codex"), Some(3600));
        assert_eq!(read_dead_row_grace_for_harness(cfg, "claude"), Some(3600));
    }

    #[test]
    fn dead_row_grace_per_harness_table_does_not_leak_across_harnesses() {
        // x-9de7 task 6 AC2: agents.dead_row_grace.codex set, no claude key ->
        // codex gets its own value, claude falls through (to the default,
        // resolved one level up in dead_row_grace_secs -- this pure reader
        // just proves the table lookup itself does not leak).
        let cfg = "[agents.dead_row_grace]\ncodex = 28800\n";
        assert_eq!(read_dead_row_grace_for_harness(cfg, "codex"), Some(28800));
        assert_eq!(read_dead_row_grace_for_harness(cfg, "claude"), None);
    }

    #[test]
    fn dead_row_grace_secs_resolves_per_harness_table() {
        let dir = std::env::temp_dir().join(format!(
            "fno-agents-config-test-grace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".fno")).unwrap();
        std::fs::write(
            dir.join(".fno/config.toml"),
            "[agents.dead_row_grace]\ncodex = 28800\n",
        )
        .unwrap();
        // Isolate from any real env override so this reads the file alone.
        // Under the crate-wide lock: removing the var is itself a write, and
        // the resolver also reads $FNO_CONFIG, which sibling tests repoint at
        // their own files. Unlocked, this read landed on someone else's config
        // and fell back to the default.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("FNO_AGENTS_DEAD_ROW_GRACE_SECS");
        assert_eq!(dead_row_grace_secs(&dir, "codex"), 28800);
        assert_eq!(
            dead_row_grace_secs(&dir, "claude"),
            DEFAULT_DEAD_ROW_GRACE_SECS,
            "a harness absent from the table falls back to the default, not the codex value"
        );
        std::fs::remove_dir_all(&dir).ok();
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
        let dir = std::env::temp_dir().join(format!("fno-headless-{}-{name}", std::process::id()));
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(fnodir.join("config.toml"), body).unwrap();
        dir
    }

    fn write_file(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fno-headless-{}-{name}", std::process::id()));
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
    fn headless_yolo_enabled_honors_fno_config_short_circuit() {
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
        let cwd = std::env::temp_dir().join(format!("fno-headless-{}-nocfg", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let got = headless_yolo_enabled("gemini", &cwd);
        clear_config_env();
        assert!(
            got,
            "FNO_CONFIG full-yolo opt-in must be honored on the Rust path"
        );
    }

    #[test]
    fn headless_yolo_enabled_fno_config_absent_key_defaults_bounded() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let f = write_file("explicit-empty", "schema_version = 1\n");
        std::env::set_var("FNO_CONFIG", &f);
        let cwd = std::env::temp_dir().join(format!("fno-headless-{}-nocfg2", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let got = headless_yolo_enabled("codex", &cwd);
        clear_config_env();
        assert!(
            !got,
            "absent key under FNO_CONFIG -> hang-safe BOUNDED default (false)"
        );
    }

    // --- x-4391/x-4be1: auto_merge.grant reader -----------------------------

    #[test]
    fn auto_merge_grant_absent_is_false() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("am-absent", "schema_version = 1\n");
        // Pin FNO_CONFIG (the SOLE-source short-circuit) to the project file:
        // without it the candidate chain falls through to the developer's real
        // global config, whose grant masks the default under test.
        std::env::set_var("FNO_CONFIG", cwd.join(".fno/config.toml"));
        let got = auto_merge_grant(&cwd);
        clear_config_env();
        assert!(!got);
    }

    #[test]
    fn auto_merge_grant_legacy_true_grants() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("am-legacy-true", "[dispatch]\nauto_merge = true\n");
        assert!(auto_merge_grant(&cwd));
    }

    #[test]
    fn auto_merge_grant_legacy_false_no_merge() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("am-legacy-false", "[dispatch]\nauto_merge = false\n");
        assert!(!auto_merge_grant(&cwd));
    }

    #[test]
    fn auto_merge_grant_canonical_dispatch_grants() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("am-grant", "[auto_merge]\ngrant = \"dispatch\"\n");
        assert!(auto_merge_grant(&cwd));
    }

    #[test]
    fn auto_merge_grant_canonical_none_beats_legacy_true_same_file() {
        // x-4be1: within one file the canonical spelling wins; a migrated
        // project is never masked by its own leftover legacy key.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings(
            "am-canonical-wins",
            "[auto_merge]\ngrant = \"none\"\n[dispatch]\nauto_merge = true\n",
        );
        // Pin FNO_CONFIG so the None from the canonical arm cannot fall
        // through to the developer's real global config either.
        std::env::set_var("FNO_CONFIG", cwd.join(".fno/config.toml"));
        let got = auto_merge_grant(&cwd);
        clear_config_env();
        assert!(!got);
    }

    #[test]
    fn auto_merge_grant_malformed_legacy_in_project_masks_global() {
        // Parity with the Python per-layer fold (x-4be1): a present-but-non-bool
        // legacy value is a no-grant DECISION in the project file. It must not
        // fall through and let the global's canonical grant resurrect merge -
        // the Python fold writes grant="none" into the project layer, which
        // masks the global on merge.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd =
            write_project_settings("am-malformed-legacy", "[dispatch]\nauto_merge = \"yes\"\n");
        let gdir =
            std::env::temp_dir().join(format!("fno-agents-global-am-{}", std::process::id()));
        std::fs::create_dir_all(&gdir).unwrap();
        std::fs::write(
            gdir.join("config.toml"),
            "[auto_merge]\ngrant = \"dispatch\"\n",
        )
        .unwrap();
        std::env::set_var("FNO_GLOBAL_SETTINGS_PATH", gdir.join("settings.json"));
        let got = auto_merge_grant(&cwd);
        clear_config_env();
        assert!(!got);
    }

    // --- review.optional_apps reader ---------------------------------------

    #[test]
    fn review_optional_apps_accepts_list_and_scalar_forms() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for (name, body, want) in [
            (
                "optional-list",
                "[review]\noptional_apps = [\"chatgpt-codex-connector\", \"gemini-code-assist\"]\n",
                vec!["chatgpt-codex-connector", "gemini-code-assist"],
            ),
            (
                "optional-scalar",
                "[review]\noptional_apps = \"chatgpt-codex-connector\"\n",
                vec!["chatgpt-codex-connector"],
            ),
        ] {
            clear_config_env();
            let cwd = write_project_settings(name, body);
            assert_eq!(review_optional_apps(&cwd), want);
        }
    }

    #[test]
    fn review_optional_apps_explicit_local_empty_masks_global() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let global = write_file(
            "optional-global",
            "[review]\noptional_apps = [\"chatgpt-codex-connector\"]\n",
        );
        std::env::set_var(
            "FNO_GLOBAL_SETTINGS_PATH",
            global.with_file_name("settings.json"),
        );
        let cwd = write_project_settings("optional-local-empty", "[review]\noptional_apps = []\n");
        let got = review_optional_apps(&cwd);
        clear_config_env();
        assert!(
            got.is_empty(),
            "explicit local [] must mask the global list"
        );
    }

    #[test]
    fn review_optional_apps_malformed_value_degrades_to_empty() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings(
            "optional-malformed",
            "[review]\noptional_apps = { login = \"chatgpt-codex-connector\" }\n",
        );
        assert!(review_optional_apps(&cwd).is_empty());
    }

    // --- auto_merge.{merge_strategy,delete_branch_on_merge} readers ---
    // `finalize` hardcoded `--merge` and never passed `--delete-branch`, so a
    // squash-only repo was armed with a method it forbids and auto-merge
    // silently never worked. Every default below is the old hardcode, so a
    // malformed config lands exactly on the pre-fix behavior.

    #[test]
    fn auto_merge_strategy_absent_is_merge() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("ams-absent", "schema_version = 1\n");
        assert_eq!(auto_merge_strategy(&cwd), "merge");
    }

    #[test]
    fn auto_merge_strategy_honors_squash_and_rebase() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for want in ["squash", "rebase", "merge"] {
            clear_config_env();
            let cwd = write_project_settings(
                &format!("ams-{want}"),
                &format!("[auto_merge]\nmerge_strategy = \"{want}\"\n"),
            );
            assert_eq!(auto_merge_strategy(&cwd), want);
        }
    }

    #[test]
    fn auto_merge_strategy_invalid_degrades_to_merge() {
        // Mirrors the bash + Pydantic coercers, which both fall back to `merge`
        // on an out-of-allowlist value. No FNO_CONFIG pin needed, unlike the
        // absent case: the extractor takes ANY string, so a misspelled project
        // value resolves and is then rejected by the filter, and the global tier
        // is never consulted.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("ams-bad", "[auto_merge]\nmerge_strategy = \"octopus\"\n");
        assert_eq!(auto_merge_strategy(&cwd), "merge");
    }

    /// Build a main checkout + linked worktree, with `body` as the CANONICAL
    /// config and nothing in the worktree. Returns the linked worktree path, or
    /// None when git is unavailable (mirrors the skip in `paths.rs`).
    fn linked_worktree_with_canonical_config(name: &str, body: &str) -> Option<PathBuf> {
        fn git(dir: &Path, args: &[&str]) -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .ok()?;
        let base = std::env::temp_dir().join(format!("fno-canon-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let main = base.join("main");
        std::fs::create_dir_all(&main).ok()?;
        git(&main, &["init", "-q"]).then_some(())?;
        git(&main, &["config", "user.email", "t@t"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        std::fs::create_dir_all(main.join(".fno")).ok()?;
        std::fs::write(main.join(".fno/config.toml"), body).ok()?;
        let linked = base.join("wt");
        git(
            &main,
            &["worktree", "add", "-q", linked.to_str()?, "-b", "feat"],
        )
        .then_some(())?;
        // The whole point: the worktree has no config of its own.
        assert!(!linked.join(".fno/config.toml").exists());
        Some(linked)
    }

    #[test]
    fn canonical_checkout_config_is_read_from_a_linked_worktree() {
        // Worktree-first is this project's default and setup-worktree.sh only
        // symlinks .fno/config.toml when it runs, so a `claude --worktree` tree
        // has none. Without the canonical tier every getter here silently
        // disagrees with `fno config get`, and a squash repo arms `--merge`.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let Some(linked) = linked_worktree_with_canonical_config(
            "read",
            "[auto_merge]\nmerge_strategy = \"squash\"\n",
        ) else {
            return;
        };
        assert_eq!(auto_merge_strategy(&linked), "squash");
    }

    #[test]
    fn canonical_checkout_config_is_dropped_by_the_suppress_env() {
        // FNO_NO_CANONICAL_CONFIG=1 drops ONLY this tier (preflight's hermetic
        // runner). Exactly "1"; the local and global tiers still apply.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let Some(linked) = linked_worktree_with_canonical_config(
            "suppress",
            "[auto_merge]\nmerge_strategy = \"squash\"\n",
        ) else {
            return;
        };
        std::env::set_var("FNO_NO_CANONICAL_CONFIG", "1");
        let got = auto_merge_strategy(&linked);
        std::env::remove_var("FNO_NO_CANONICAL_CONFIG");
        assert_eq!(
            got, "merge",
            "suppressed canonical must fall to the default"
        );
    }

    #[test]
    fn auto_merge_strategy_tolerates_padding() {
        // The sibling half of the trim above, pinned separately because this
        // reader trims in its own extractor rather than via scalar_to_string.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd =
            write_project_settings("ams-pad", "[auto_merge]\nmerge_strategy = \" squash \"\n");
        assert_eq!(auto_merge_strategy(&cwd), "squash");
    }

    #[test]
    fn auto_merge_grant_non_bool_degrades_to_false() {
        // Only a real TOML boolean grants merge (Locked Decision 6): a string
        // "yes" yields None from as_bool() -> false, mirroring the Python coercer.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings("am-str", "[dispatch]\nauto_merge = \"yes\"\n");
        // Pinned for the same reason as the absent case: a non-bool yields None,
        // which would otherwise fall through to the real global config.
        std::env::set_var("FNO_CONFIG", cwd.join(".fno/config.toml"));
        let got = auto_merge_grant(&cwd);
        clear_config_env();
        assert!(!got);
    }

    #[test]
    fn auto_merge_grant_malformed_canonical_key_decides_no_grant() {
        // A present-but-malformed canonical `grant` is a DECISION (no-grant),
        // never a fall-through to the same file's legacy spelling: a typo must
        // not resurrect the merge right the coercer just refused.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_config_env();
        let cwd = write_project_settings(
            "am-bad-grant",
            "[auto_merge]\ngrant = 3\n[dispatch]\nauto_merge = true\n",
        );
        std::env::set_var("FNO_CONFIG", cwd.join(".fno/config.toml"));
        let got = auto_merge_grant(&cwd);
        clear_config_env();
        assert!(!got);
    }
}

//! The one native owner of every non-journal input to `king-verdict` :
//! caller crown, canonical scope, manifest, config values, graph rows, the
//! check-in window, and the inherited/filed delivery split. `king_history.rs`
//! keeps the journal scan, verdict, payload, and rendering it already owns.
//!
//! Python assembled these nine values and passed them on the argv (PR 1911);
//! this module is the port that lets the Python shell shrink to a transport
//! (law d-b6cc1a2a: new code in `crates/`, existing Python ported, never
//! extended). Every refusal is a typed `Err` naming the failed reading -
//! nothing here degrades to fleet-wide, zero, or a default verdict.

use crate::AgentStatus;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The default check-in interval (`config.king.checkin_interval`, "30m").
const DEFAULT_CHECKIN_SECS: i64 = 1800;
/// The window is three check-in intervals (Python `verdict_counts`).
const WINDOW_INTERVALS: i64 = 3;
/// The default compaction ceiling (`config.king.compaction_ceiling`).
const DEFAULT_COMPACTION_CEILING: i64 = 3;

/// The registry statuses Python reads as terminal (`registry.TERMINAL_STATUSES`).
fn row_status_word(status: &AgentStatus) -> Option<&'static str> {
    match status {
        AgentStatus::Exited => Some("exited"),
        AgentStatus::Orphaned => Some("orphaned"),
        AgentStatus::Failed => Some("failed"),
        AgentStatus::PermanentDead => Some("permanent_dead"),
        _ => None,
    }
}

/// A sortable instant: the `Z` and `+00:00` UTC spellings must compare equal,
/// so a Z-suffixed manifest date and a +00:00 graph date cannot misorder at
/// the same-second crowning boundary (Python `_ts_key`, scope.py).
fn ts_key(raw: &str) -> String {
    if let Some(stripped) = raw.strip_suffix('Z') {
        format!("{stripped}+00:00")
    } else {
        raw.to_string()
    }
}

/// The delivery trend split so filing cannot inflate it (Python `scope_split`,
/// king/scope.py). A king filing real nodes raises undelivered by working
/// well, so the convergence signal is the INHERITED set (created before
/// `crowned_at`) only. A row with no birth date reads as inherited: an old
/// undated backlog is work the reign can be stalled on, never fresh filing
/// that excuses it.
pub(crate) fn scope_split(
    ids: &std::collections::HashSet<String>,
    entries: &[Value],
    crowned_at: &str,
    window_start: &str,
) -> (u64, u64, u64) {
    let crowned = ts_key(crowned_at);
    let window = ts_key(window_start);
    let mut inherited_undelivered: u64 = 0;
    let mut filed_undelivered: u64 = 0;
    let mut inherited_closed_in_window: u64 = 0;
    for row in entries {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        if !ids.contains(id) {
            continue;
        }
        let created = ts_key(row.get("created_at").and_then(Value::as_str).unwrap_or(""));
        let inherited = created < crowned;
        if !crate::graph_store::is_terminal_entry(row) {
            if inherited {
                inherited_undelivered += 1;
            } else {
                filed_undelivered += 1;
            }
        } else if inherited {
            let completed = ts_key(
                row.get("completed_at")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            if completed >= window {
                inherited_closed_in_window += 1;
            }
        }
    }
    (
        inherited_undelivered,
        filed_undelivered,
        inherited_closed_in_window,
    )
}

/// `f"{window_s // 3600}h" if window_s % 3600 == 0 else f"{window_s // 60}m"`
/// - the display spelling Python passed as `--window`.
fn window_display(window_secs: i64) -> String {
    if window_secs % 3600 == 0 {
        format!("{}h", window_secs / 3600)
    } else {
        format!("{}m", window_secs / 60)
    }
}

/// Every input the verdict read needs, resolved. `window` is the display
/// spelling the payload carries.
#[derive(Debug)]
pub(crate) struct VerdictInputs {
    pub scope: String,
    /// The parsed manifest, carried so the verdict verb never re-reads it.
    pub manifest: crate::loopcheck::KingManifest,
    pub window: String,
    pub compaction_ceiling: u64,
    pub inherited_undelivered: u64,
    pub filed_undelivered: u64,
    pub inherited_closed_in_window: u64,
}

/// The caller's canonical crown scope: explicit `--scope` wins (canonicalized,
/// refused when it names nothing); else the live registry row for this
/// session's own identity, requiring a stamped, non-terminal crown (Python
/// `resolve_scope` + `resolve_king_manifest_path`, whose wording is matched).
fn resolve_scope(explicit_scope: Option<&str>, registry_path: &Path) -> Result<String, String> {
    if let Some(raw) = explicit_scope {
        let canonical = crate::territory::canonical_scope(raw.trim());
        if canonical.is_empty() {
            return Err("--scope names no crown territory.".to_string());
        }
        return Ok(canonical);
    }
    let (session, harness) = crate::claims::resolve_identity();
    let (Some(session), Some(harness)) = (session, harness) else {
        return Err(
            "cannot resolve the caller's crown: no crowned registry row. \
             Pass --scope explicitly."
                .to_string(),
        );
    };
    let registry = crate::state::load_registry(registry_path).map_err(|e| {
        format!("cannot resolve the caller's crown: the agent registry could not be read: {e}")
    })?;
    let Some(row) = crate::loop_reign::find_by_session(&registry.entries, &session, Some(&harness))
    else {
        return Err(format!(
            "cannot resolve the caller's crown: no registry row names session {session}"
        ));
    };
    if let Some(word) = row_status_word(&row.status) {
        return Err(format!(
            "the registry row for {session} is {word}, a terminal state"
        ));
    }
    let crown = row
        .crown_scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "the registry row for {session} carries no crown_scope: the row \
                 is unstamped, so no crown proves this session's authority"
            )
        })?;
    let canonical = crate::territory::canonical_scope(crown);
    if canonical.is_empty() {
        return Err("this session holds no crown. Pass --scope <territory>.".to_string());
    }
    Ok(canonical)
}

/// The manifest the verdict reads: explicit and authoritative, else
/// `<space>/kings/<scope>.md` for the named scope (a deliberate change from
/// Python, which resolved the CALLER's manifest even under an explicit scope
/// - reading one crown's reign through another crown's manifest is the bug
/// closes).
fn resolve_manifest(
    cwd: &Path,
    scope: &str,
    explicit_manifest: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(p) = explicit_manifest {
        let path = p.to_path_buf();
        if !path.is_file() {
            return Err(format!("{}: manifest not found", path.display()));
        }
        return Ok(path);
    }
    let path = crate::loop_reign::manifest_path(&crate::paths::space_dir(cwd), scope)?;
    if !path.is_file() {
        return Err(format!("{}: manifest not found", path.display()));
    }
    Ok(path)
}

/// Alias members (a project's `short_name` spelling) resolve to their
/// canonical project, so the manifest path and the graph compile select what
/// the crown actually wrote at `<space>/kings/<canonical>.md`. Canonical
/// members map to themselves, so a registry-derived scope is unchanged.
fn canonical_members_with_aliases(scope: &str, cwd: &Path) -> String {
    let map = crate::king_board::scope::project_map(cwd).unwrap_or_default();
    let members: Vec<String> = scope
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|m| map.get(m).cloned().unwrap_or_else(|| m.to_string()))
        .collect();
    crate::territory::canonical_scope(&members.join(","))
}

/// `king.checkin_interval` through the layered config lookup, with Python's
/// default. An unparseable interval IS the default (the Pydantic validator's
/// behavior). Shared by the verdict window and the hook beat's due check.
pub(crate) fn checkin_interval_secs(cwd: &Path) -> i64 {
    match crate::agents_config::config_lookup(cwd, &["king", "checkin_interval"]) {
        Some(v) => crate::territory::parse_duration_to_seconds(&v).unwrap_or(DEFAULT_CHECKIN_SECS),
        None => DEFAULT_CHECKIN_SECS,
    }
}

/// `king.checkin_interval` and `king.compaction_ceiling` through the layered
/// config lookup, with Python's defaults. A non-integer or negative ceiling
/// is a named refusal, never zero. Zero itself is a legitimate ceiling: it
/// declares that no compaction is ever acceptable.
fn read_config(cwd: &Path) -> Result<(i64, i64), String> {
    let interval = checkin_interval_secs(cwd);
    let ceiling = match crate::agents_config::config_lookup(cwd, &["king", "compaction_ceiling"]) {
        Some(v) => match v.as_integer() {
            Some(n) if n >= 0 => n,
            Some(_) => {
                return Err(
                    "config king.compaction_ceiling must be a non-negative integer".to_string(),
                )
            }
            None => {
                return Err("config king.compaction_ceiling must be an integer".to_string());
            }
        },
        None => DEFAULT_COMPACTION_CEILING,
    };
    Ok((interval, ceiling))
}

/// The one entry point. `now` is injectable so tests pin the window; `cwd`
/// anchors the config candidates, the graph, and the space dir exactly as the
/// Python shell did.
pub(crate) fn resolve_verdict_inputs(
    cwd: &Path,
    explicit_scope: Option<&str>,
    explicit_manifest: Option<&Path>,
    registry_path: &Path,
    now: impl Fn() -> chrono::DateTime<chrono::Utc>,
) -> Result<VerdictInputs, String> {
    if crate::graph_get::external_backend_selected() {
        return Err("tracker metadata unavailable under an external tracker backend".to_string());
    }
    let scope = resolve_scope(explicit_scope, registry_path)?;
    let scope = canonical_members_with_aliases(&scope, cwd);
    let manifest_path = resolve_manifest(cwd, &scope, explicit_manifest)?;
    let (interval, ceiling) = read_config(cwd)?;

    let content = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{}: unreadable manifest: {e}", manifest_path.display()))?;
    let manifest = crate::loopcheck::parse_king_manifest(&content)
        .ok_or_else(|| "king manifest has no frontmatter".to_string())?;
    let crowned_at = manifest.created_at.clone().ok_or_else(|| {
        format!(
            "{}: no created_at; no measurable split.",
            manifest_path.display()
        )
    })?;

    let window_secs = WINDOW_INTERVALS * interval;
    let window_start = (now() - chrono::Duration::seconds(window_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false);

    let entries = crate::graph_store::read_defaulted_opts(
        &crate::king_board::scope::graph_json_path(cwd),
        false,
        false,
    )
    .map_err(|e| format!("scope {scope} graph unreadable: {e}"))?;
    let projects: Result<HashMap<String, String>, String> =
        crate::king_board::scope::project_map(cwd);
    let ids = crate::territory::compile_scope_ids(&scope, &entries, &projects)
        .map_err(|e| format!("scope {scope} unreadable: {e}"))?;
    let (inherited_undelivered, filed_undelivered, inherited_closed_in_window) =
        scope_split(&ids, &entries, &crowned_at, &window_start);

    Ok(VerdictInputs {
        scope,
        manifest,
        window: window_display(window_secs),
        compaction_ceiling: ceiling as u64,
        inherited_undelivered,
        filed_undelivered,
        inherited_closed_in_window,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kvi-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(id: &str, parent: &str, created_at: &str, extra: Value) -> Value {
        let mut v = serde_json::json!({"id": id, "type": "feature", "project": "fno"});
        if !parent.is_empty() {
            v["parent"] = Value::String(parent.to_string());
        }
        if !created_at.is_empty() {
            v["created_at"] = Value::String(created_at.to_string());
        }
        if let Value::Object(map) = extra {
            for (k, val) in map {
                v[k] = val;
            }
        }
        v
    }

    fn epic_ids(entries: &[Value], root: &str) -> std::collections::HashSet<String> {
        crate::territory::compile_scope_ids(root, entries, &Ok(HashMap::new()))
            .expect("epic scope compiles")
    }

    const CROWNED: &str = "2026-09-10T00:00:00Z";

    // --- AC2-HP: the split's fixture values, byte-matched from the Python
    // tests in cli/tests/unit/test_king_scope.py ---

    #[test]
    fn scope_split_counts_inherited_and_filed_apart() {
        let entries = vec![
            entry(
                "x-root",
                "",
                "2026-09-01T00:00:00Z",
                serde_json::json!({"type": "epic"}),
            ),
            entry("x-old", "x-root", "2026-09-05T00:00:00Z", Value::Null),
            entry("x-new", "x-root", "2026-09-12T00:00:00Z", Value::Null),
        ];
        let (inh, filed, closed) = scope_split(
            &epic_ids(&entries, "x-root"),
            &entries,
            CROWNED,
            "2026-09-01T00:00:00Z",
        );
        assert_eq!((inh, filed, closed), (2, 1, 0));
    }

    #[test]
    fn scope_split_counts_only_inherited_closures_in_the_window() {
        let entries = vec![
            entry("x-root", "", "", serde_json::json!({"type": "epic"})),
            entry(
                "x-closed-now",
                "x-root",
                "2026-09-05T00:00:00Z",
                serde_json::json!({"status": "done", "completed_at": "2026-09-11T00:00:00Z"}),
            ),
            entry(
                "x-closed-early",
                "x-root",
                "2026-09-05T00:00:00Z",
                serde_json::json!({"status": "done", "completed_at": "2026-08-01T00:00:00Z"}),
            ),
            entry(
                "x-filed-closed",
                "x-root",
                "2026-09-12T00:00:00Z",
                serde_json::json!({"status": "done", "completed_at": "2026-09-12T12:00:00Z"}),
            ),
        ];
        let (inh, filed, closed) = scope_split(
            &epic_ids(&entries, "x-root"),
            &entries,
            CROWNED,
            "2026-09-10T00:00:00Z",
        );
        assert_eq!(closed, 1);
        assert_eq!(filed, 0);
        // The undated root reads as inherited work; it is open, so the
        // inherited count carries it even though this fixture pins closure.
        assert_eq!(inh, 1);
    }

    #[test]
    fn scope_split_rows_without_created_at_read_as_inherited() {
        let entries = vec![
            entry(
                "x-root",
                "",
                "2026-09-01T00:00:00Z",
                serde_json::json!({"type": "epic"}),
            ),
            entry("x-undated", "x-root", "", Value::Null),
        ];
        let (inh, filed, _closed) = scope_split(
            &epic_ids(&entries, "x-root"),
            &entries,
            CROWNED,
            "2026-09-01T00:00:00Z",
        );
        assert_eq!((inh, filed), (2, 0));
    }

    #[test]
    fn scope_split_treats_z_and_offset_utc_as_the_same_instant() {
        let entries = vec![
            entry(
                "x-root",
                "",
                "2026-09-01T00:00:00Z",
                serde_json::json!({"type": "epic"}),
            ),
            entry("x-edge", "x-root", "2026-09-10T00:00:00+00:00", Value::Null),
        ];
        let (inh, filed, _closed) = scope_split(
            &epic_ids(&entries, "x-root"),
            &entries,
            CROWNED,
            "2026-09-01T00:00:00Z",
        );
        assert_eq!((inh, filed), (1, 1));
    }

    #[test]
    fn a_deferred_row_stays_undelivered() {
        // Deferral is a RETURNABLE rung; the legacy `deferred:` completed_at
        // sentinel must never read as closed (graph_store::is_terminal_entry).
        let entries = vec![
            entry(
                "x-root",
                "",
                "2026-09-01T00:00:00Z",
                serde_json::json!({"type": "epic"}),
            ),
            entry("x-def", "x-root", "2026-09-05T00:00:00Z", Value::Null),
        ];
        let (inh, filed, closed) = scope_split(
            &epic_ids(&entries, "x-root"),
            &entries,
            CROWNED,
            "2026-09-01T00:00:00Z",
        );
        assert_eq!((inh, filed, closed), (2, 0, 0));
    }

    // --- the window spelling ---

    #[test]
    fn window_display_matches_the_python_formatting() {
        assert_eq!(window_display(3 * 1800), "90m");
        assert_eq!(window_display(3 * 3600), "3h");
        assert_eq!(window_display(3 * 86400), "72h");
    }

    // --- AC1/AC2-ERR: the resolver's refusal paths ---

    fn pinned_now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(1789500000, 0).unwrap()
    }

    fn write_manifest(dir: &Path, scope: &str) -> PathBuf {
        let path = dir.join(format!("kings/{scope}.md"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "---\nscope: x\nshape: pass\nfno_id: kd-test\ncreated_at: 2026-09-10T00:00:00Z\n\
             harness_session_id: sess-k\nmax_iterations: 40\n---\n",
        )
        .unwrap();
        path
    }

    #[test]
    fn an_explicit_empty_scope_refuses() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmp("empty-scope");
        let err = resolve_verdict_inputs(
            &dir,
            Some("  , "),
            None,
            &dir.join("registry.json"),
            pinned_now,
        )
        .expect_err("empty scope names nothing");
        assert!(err.contains("names no crown territory"), "{err}");
    }

    #[test]
    fn a_missing_manifest_refuses() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // This test holds the env lock; declare() takes it too, so the
        // held variant is the one that cannot deadlock.
        let _root = crate::paths::DeclaredRoot::declare_held("kvi");
        let dir = tmp("no-manifest");
        let err = resolve_verdict_inputs(
            &dir,
            Some("x-root"),
            None,
            &dir.join("registry.json"),
            pinned_now,
        )
        .expect_err("no manifest exists");
        assert!(err.contains("manifest not found"), "{err}");
    }

    #[test]
    fn an_explicit_manifest_wins_and_must_exist() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmp("explicit-manifest");
        let missing = dir.join("elsewhere.md");
        let err = resolve_verdict_inputs(
            &dir,
            Some("x-root"),
            Some(&missing),
            &dir.join("registry.json"),
            pinned_now,
        )
        .expect_err("explicit manifest is authoritative");
        assert!(err.contains("manifest not found"), "{err}");
    }

    #[test]
    fn a_manifest_without_created_at_refuses_the_split() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tmp("no-created-at");
        let path = dir.join("kings/x-root.md");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "---\nscope: x-root\nshape: pass\nfno_id: kd-test\n---\n",
        )
        .unwrap();
        let err = resolve_verdict_inputs(
            &dir,
            Some("x-root"),
            Some(&path),
            &dir.join("registry.json"),
            pinned_now,
        )
        .expect_err("no created_at means no measurable split");
        assert!(err.contains("no created_at"), "{err}");
    }

    #[test]
    fn an_external_tracker_backend_refuses() {
        let dir = tmp("external-backend");
        write_manifest(&dir, "x-root");
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FNO_TRACKER_BACKEND", "github");
        let err = resolve_verdict_inputs(
            &dir,
            Some("x-root"),
            None,
            &dir.join("registry.json"),
            pinned_now,
        );
        std::env::remove_var("FNO_TRACKER_BACKEND");
        let err = err.expect_err("external backend cannot measure the split");
        assert!(err.contains("tracker metadata unavailable"), "{err}");
    }

    #[test]
    fn a_zero_ceiling_is_a_legitimate_strict_setting() {
        // 0 declares "no compaction is ever acceptable" - a declared bound
        // with ceiling 0, never a refusal.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("FNO_CONFIG");
        let dir = tmp("zero-ceiling");
        let manifest = write_manifest(&dir, "x-root");
        fs::create_dir_all(dir.join(".fno")).unwrap();
        fs::write(
            dir.join(".fno/config.toml"),
            format!(
                "[paths]\ngraph_json = {:?}\n[[work.workspaces.t.projects]]\nname = \"fno\"\n[king]\ncompaction_ceiling = 0\n",
                dir.join("home/graph.json").display()
            ),
        )
        .unwrap();
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::write(
            dir.join("home/graph.json"),
            "{\"entries\": [{\"id\": \"x-root\", \"type\": \"epic\"}]}",
        )
        .unwrap();
        let inputs = resolve_verdict_inputs(
            &dir,
            Some("x-root"),
            Some(&manifest),
            &dir.join("registry.json"),
            pinned_now,
        )
        .expect("a zero ceiling is declared, not refused");
        assert_eq!(inputs.compaction_ceiling, 0);
    }

    #[test]
    fn an_alias_scope_resolves_to_the_canonical_manifest() {
        // A project short_name spelling must select the crown manifest the
        // crown wrote under the canonical project name.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("FNO_CONFIG");
        let dir = tmp("alias-scope");
        let manifest = write_manifest(&dir, "fdata");
        fs::create_dir_all(dir.join(".fno")).unwrap();
        fs::write(
            dir.join(".fno/config.toml"),
            format!(
                "[paths]\ngraph_json = {:?}\n[[work.workspaces.t.projects]]\nname = \"fdata\"\nshort_name = \"etl\"\n",
                dir.join("home/graph.json").display()
            ),
        )
        .unwrap();
        fs::create_dir_all(dir.join("home")).unwrap();
        fs::write(
            dir.join("home/graph.json"),
            "{\"entries\": [{\"id\": \"fdata-root\", \"type\": \"epic\", \"project\": \"fdata\"}]}",
        )
        .unwrap();
        let inputs = resolve_verdict_inputs(
            &dir,
            Some("etl"),
            Some(&manifest),
            &dir.join("registry.json"),
            pinned_now,
        )
        .expect("the alias scope selects the canonical crown manifest");
        assert_eq!(inputs.scope, "fdata");
    }

    #[test]
    fn an_unparseable_ceiling_refuses_and_never_reads_zero() {
        // Hold the env lock: the external-backend test flips
        // FNO_TRACKER_BACKEND process-wide, and this read must not race it.
        // The config anchors on the test's own cwd, so a dangling FNO_CONFIG
        // left by an earlier test's window is cleared first - under the same
        // lock, so no setter can race the clear.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("FNO_CONFIG");
        let dir = tmp("bad-ceiling");
        let manifest = write_manifest(&dir, "x-root");
        fs::create_dir_all(dir.join(".fno")).unwrap();
        fs::write(
            dir.join(".fno/config.toml"),
            "[king]\ncompaction_ceiling = \"three\"\n",
        )
        .unwrap();
        let err = resolve_verdict_inputs(
            &dir,
            Some("x-root"),
            Some(&manifest),
            &dir.join("registry.json"),
            pinned_now,
        )
        .expect_err("a non-integer ceiling is a named refusal");
        assert!(err.contains("compaction_ceiling"), "{err}");
    }

    #[test]
    fn a_corrupt_graph_refuses_instead_of_reading_zero() {
        // Same discipline as the ceiling test: lock held, dangling
        // FNO_CONFIG cleared, the config anchors on this test's own cwd.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("FNO_CONFIG");
        let dir = tmp("corrupt-graph");
        let manifest = write_manifest(&dir, "x-root");
        let graph = dir.join("graph.json");
        fs::write(&graph, "{not json").unwrap();
        // Point the graph read at the corrupt file through the cwd-anchored
        // config: no env pin, no race with a parallel test's resolution.
        fs::create_dir_all(dir.join(".fno")).unwrap();
        fs::write(
            dir.join(".fno/config.toml"),
            format!("[paths]\ngraph_json = {:?}\n", graph.display()),
        )
        .unwrap();
        let err = resolve_verdict_inputs(
            &dir,
            Some("x-root"),
            Some(&manifest),
            &dir.join("registry.json"),
            pinned_now,
        )
        .expect_err("a corrupt graph must never read as drained");
        assert!(
            err.contains("unreadable") || err.contains("corrupt"),
            "{err}"
        );
    }
}

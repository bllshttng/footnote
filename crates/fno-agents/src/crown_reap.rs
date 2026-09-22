//! The dead-crown sweep: a manifest-only crown whose holder session is
//! proven dead leaves its territory. The court (`court_orphans`) DETECTS
//! these crowns; this module is the REMEDY, run by the daemon retire arm and
//! the manual `fno agents reap` verb. A vacate hands territory back, so it
//! refuses over guessing: holder death and territory emptiness are both
//! re-checked under the manifest flock and the registry lock before anything
//! is removed.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::claude_roster::ClaudeAgentsSnapshot;

/// Is the manifest's holder session dead? One function so the court and the
/// sweep cannot disagree about who is dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HolderVerdict {
    /// Positive death evidence; the string is the receipt.
    Dead(String),
    /// The holder answers live; the crown keeps.
    Live(String),
    /// No witness answered; the crown keeps and the reason is named.
    Unknown(String),
}

/// The one non-claude answer, so the verdict and the callers that skip the
/// roster read for a predetermined case cannot drift apart on the wording.
pub(crate) fn no_witness_reason(harness: Option<&str>) -> Option<String> {
    harness
        .filter(|h| !h.is_empty() && *h != "claude")
        .map(|h| format!("no death witness for harness {h}"))
}

/// Order matters: the roster witness (a) answers first because it needs no
/// transcript, then presence in a non-terminal state reads live, then the
/// transcript witness (b) fires only on a KNOWN, warning-free roster - a
/// partial list could hide the row, and absence from it is not death. The
/// absence proof is the shared predicate, so it cannot diverge between the
/// readers that apply it.
pub(crate) fn holder_verdict(
    harness: Option<&str>,
    session: &str,
    snapshot: &ClaudeAgentsSnapshot,
    transcript_age_s: &dyn Fn(&str) -> Option<i64>,
    window_s: i64,
) -> HolderVerdict {
    if let Some(reason) = no_witness_reason(harness) {
        return HolderVerdict::Unknown(reason);
    }
    // The synthesized holder is a query object, never a minted row; built
    // exactly as court_orphans does today so both readers key identically.
    let mut holder = crate::state::RegistryEntry::new(
        Some(session.to_string()),
        crate::state::Lineage::captured((None, None, None)),
    );
    holder.harness = Some("claude".into());
    if let Some(reason) = crate::gc_sweep::claude_death_reason(&holder, snapshot) {
        return HolderVerdict::Dead(reason);
    }
    let row_id = crate::daemon::roster_death::claude_row_id(&holder);
    if let Some(row) = row_id.as_deref().and_then(|id| snapshot.find(id)) {
        let word = row.state.clone().unwrap_or_else(|| "listed".to_string());
        return HolderVerdict::Live(format!("roster {word}"));
    }
    if !crate::daemon::roster_death::claude_row_provably_absent(Some(snapshot), row_id.as_deref()) {
        // Either the row IS listed (non-terminal state, live pid) or the
        // read cannot prove absence. Name which.
        if !snapshot.is_known() {
            return HolderVerdict::Unknown("roster unreadable".to_string());
        }
        let warnings = snapshot.warning_text();
        if !warnings.is_empty() {
            return HolderVerdict::Unknown(format!("roster partial: {warnings}"));
        }
        let word = row_id
            .as_deref()
            .and_then(|id| snapshot.find(id))
            .and_then(|row| row.state.clone())
            .unwrap_or_else(|| "listed".to_string());
        return HolderVerdict::Live(format!("roster {word}"));
    }
    let Some(age) = transcript_age_s(session) else {
        return HolderVerdict::Unknown("transcript not found".to_string());
    };
    if age > window_s {
        HolderVerdict::Dead(format!(
            "absent from the roster; transcript quiet {age}s > window {window_s}s"
        ))
    } else {
        HolderVerdict::Live(format!("transcript quiet {age}s <= {window_s}s"))
    }
}

/// One pass of the dead-crown sweep. Every candidate lands in exactly one
/// bucket: `vacated` (applied when `apply`, projected only otherwise),
/// `kept` with its reason, or the whole sweep lands in `unread` when the
/// registry could not be read - a failed read never vacates.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct CrownReap {
    pub vacated: Vec<VacatedCrown>,
    pub kept: Vec<KeptCrown>,
    pub unread: Option<String>,
}

/// A crown this sweep vacated (or would vacate, under a dry run).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VacatedCrown {
    pub scope: String,
    pub level: Option<u32>,
    pub manifest_path: String,
    pub holder_session: Option<String>,
    /// The death receipt: which witness answered and what it saw.
    pub evidence: String,
    /// The live crown one rung up that takes the territory, else `operator`.
    /// The sweep grants nothing; territory falls back by construction.
    pub inheritor: String,
    /// Terminal rows whose stale crown fields cleared in the vacate's
    /// registry write. Empty under a dry run.
    pub cleared_rows: Vec<String>,
}

/// A crown this sweep kept, with the reason nothing was written.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeptCrown {
    pub scope: String,
    pub reason: String,
}

#[allow(clippy::too_many_arguments)]
pub fn sweep(
    root: &Path,
    registry_path: &Path,
    cwd: &Path,
    events: &crate::events::EventEmitter,
    apply: bool,
    roster: &dyn Fn() -> ClaudeAgentsSnapshot,
    transcript_age_s: &dyn Fn(&str) -> Option<i64>,
    now: DateTime<Utc>,
) -> CrownReap {
    // The window a reign is already judged by: three check-in intervals.
    let window_s = 3 * crate::king_verdict_inputs::checkin_interval_secs(cwd);
    let live = match crate::territory::live_crowns(registry_path) {
        Ok(live) => live,
        Err(e) => {
            return CrownReap {
                unread: Some(e.0),
                ..Default::default()
            }
        }
    };
    let held_keys: std::collections::BTreeSet<String> = live
        .iter()
        .map(|c| crate::loop_reign::territory_key(&c.scope))
        .collect();
    let manifests = crate::loop_reign::collect_king_manifests(root);
    let superseded = crate::loop_reign::superseded_manifests(&manifests);
    let mut roster_read: Option<ClaudeAgentsSnapshot> = None;
    let mut out = CrownReap::default();
    let mut dead: Vec<VacatedCrown> = Vec::new();
    for (path, content) in &manifests {
        let Some(scope) = crate::claude_adopt::manifest_field(content, "crown_scope") else {
            continue;
        };
        if superseded.contains(path) {
            continue;
        }
        // A live row holding the territory (a split crown, a succession heir)
        // is never a candidate, whatever the manifest names.
        let key = crate::loop_reign::territory_key(&scope);
        if held_keys.contains(&key) {
            continue;
        }
        let mut keep = |reason: String| {
            out.kept.push(KeptCrown {
                scope: scope.clone(),
                reason,
            })
        };
        let Some(session) = crate::claude_adopt::manifest_field(content, "harness_session_id")
        else {
            keep("manifest names no holder session".to_string());
            continue;
        };
        let harness = crate::claude_adopt::manifest_field(content, "harness");
        // Same laziness contract as the court: a non-claude harness has a
        // predetermined verdict, so it spends no roster read.
        if let Some(reason) = no_witness_reason(harness.as_deref()) {
            keep(reason);
            continue;
        }
        let snapshot = roster_read.get_or_insert_with(roster);
        let evidence = match holder_verdict(
            harness.as_deref(),
            &session,
            snapshot,
            transcript_age_s,
            window_s,
        ) {
            crate::crown_reap::HolderVerdict::Dead(evidence) => evidence,
            crate::crown_reap::HolderVerdict::Live(reason) => {
                keep(reason);
                continue;
            }
            crate::crown_reap::HolderVerdict::Unknown(reason) => {
                keep(reason);
                continue;
            }
        };
        let crown_age = crate::claude_adopt::manifest_field(content, "created_at")
            .and_then(|c| DateTime::parse_from_rfc3339(&c).ok())
            .map(|c| {
                now.signed_duration_since(c.with_timezone(&Utc))
                    .num_seconds()
                    > window_s
            });
        match crown_age {
            Some(true) => {}
            Some(false) => {
                keep("crowned inside the window".to_string());
                continue;
            }
            None => {
                keep("manifest has no parsable created_at".to_string());
                continue;
            }
        }
        let level = crate::claude_adopt::manifest_field(content, "crown_level")
            .and_then(|v| v.parse().ok());
        let inheritor = inheritor_for(level, &scope, &live, cwd);
        dead.push(VacatedCrown {
            scope,
            level,
            manifest_path: path.display().to_string(),
            holder_session: Some(session),
            evidence,
            inheritor,
            cleared_rows: Vec::new(),
        });
    }
    if apply {
        for v in dead {
            match vacate(
                Path::new(&v.manifest_path),
                &v.scope,
                v.holder_session.as_deref(),
                registry_path,
                &v.evidence,
                &v.inheritor,
                events,
            ) {
                Ok(cleared_rows) => out.vacated.push(VacatedCrown { cleared_rows, ..v }),
                Err(reason) => out.kept.push(KeptCrown {
                    scope: v.scope.clone(),
                    reason,
                }),
            }
        }
    } else {
        // A dry run reports the same list and removes and emits nothing.
        out.vacated = dead;
    }
    out
}

/// The vacate, in the spawn path's own lock order: REGISTRY FIRST, then the
/// manifest flock. The crowned spawn path arms its manifest from inside its
/// `update_registry` closure (`dispatch.py` `_write`), so this reaper must
/// never hold the manifest lock while waiting for the registry lock - that
/// AB/BA ordering would deadlock both operations. Under the registry lock a
/// non-terminal row holding the scope refuses the whole vacate; stale crown
/// fields on terminal rows clear. Then, under the manifest flock: re-read
/// the manifest and refuse a successor armed mid-sweep, remove the manifest
/// and its cancel sentinel, and journal one `agent_crown_vacated` with
/// cause `holder_dead`. A refusal at the registry write touches nothing; a
/// refusal at the manifest re-read leaves the manifest standing.
pub(crate) fn vacate(
    manifest_path: &Path,
    scope: &str,
    expect_session: Option<&str>,
    registry_path: &Path,
    evidence: &str,
    inheritor: &str,
    events: &crate::events::EventEmitter,
) -> Result<Vec<String>, String> {
    let key = crate::loop_reign::territory_key(scope);
    let claims = |e: &crate::state::RegistryEntry| {
        e.crown_scope
            .as_deref()
            .map(|s| crate::loop_reign::territory_key(s) == key)
            .unwrap_or(false)
    };
    let mut cleared: Vec<String> = Vec::new();
    let mut refused: Option<String> = None;
    let write = crate::state::update_registry(registry_path, |reg| {
        if let Some(row) = reg
            .entries
            .iter()
            .find(|e| !crate::loop_reign::is_terminal(e) && claims(e))
        {
            refused = Some(format!("row {} took the scope mid-sweep", row.name));
            return;
        }
        for row in reg
            .entries
            .iter_mut()
            .filter(|e| crate::loop_reign::is_terminal(e) && claims(e))
        {
            row.crown_level = None;
            row.crown_scope = None;
            row.crown_grantor = None;
            cleared.push(row.name.clone());
        }
    });
    if let Some(reason) = refused {
        return Err(reason);
    }
    write.map_err(|e| format!("registry write refused: {e}"))?;
    let lock_path = manifest_path.with_extension("md.lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let result = (|| {
        let content = fs::read_to_string(manifest_path)
            .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;
        let named = crate::claude_adopt::manifest_field(&content, "harness_session_id");
        if let (Some(expect), Some(named)) = (expect_session, named.as_deref()) {
            if named != expect {
                return Err(format!(
                    "refusing to vacate {scope:?}: the manifest now names session {named}, not {expect}"
                ));
            }
        }
        let grantor = crate::claude_adopt::manifest_field(&content, "crown_grantor");
        let level = crate::claude_adopt::manifest_field(&content, "crown_level")
            .and_then(|v| v.parse::<u32>().ok());
        fs::remove_file(manifest_path)
            .map_err(|e| format!("cannot remove {}: {e}", manifest_path.display()))?;
        let cancelled = manifest_path.with_extension("cancelled");
        if cancelled.is_file() {
            let _ = fs::remove_file(&cancelled);
        }
        let _ = events.emit(
            "agent_crown_vacated",
            &serde_json::json!({
                "scope": scope, "level": level, "holder_session": expect_session,
                "grantor": grantor, "cause": "holder_dead",
                "evidence": evidence, "inheritor": inheritor,
            }),
        );
        Ok(cleared)
    })();
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
    result
}

/// The live crown one rung up that inherits the territory: for a level-2
/// scope, the level-1 crown over the single project every member belongs to
/// (graph read); for level 1, the level-0 crown containing it; else
/// `operator`. Mirrors the court's `find_presiding_crown`. The sweep never
/// crowns anyone; territory falls back to its project by construction.
fn inheritor_for(
    level: Option<u32>,
    scope: &str,
    live: &[crate::territory::Crown],
    cwd: &Path,
) -> String {
    let members = |s: &str| -> Vec<String> {
        s.split(',')
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect()
    };
    match level {
        Some(2) => {
            let entries = crate::territory::graph_entries(cwd).unwrap_or_default();
            let project_of = |member: &str| {
                entries
                    .iter()
                    .find(|e| e.get("id").and_then(serde_json::Value::as_str) == Some(member))
                    .and_then(|e| e.get("project").and_then(serde_json::Value::as_str))
                    .filter(|p| !p.is_empty())
            };
            let projects: std::collections::BTreeSet<&str> = members(scope)
                .iter()
                .filter_map(|m| project_of(m))
                .collect();
            if projects.len() != 1 {
                return "operator".to_string();
            }
            let project = crate::territory::canonical_scope(projects.into_iter().next().unwrap());
            live.iter()
                .find(|c| c.level == 1 && c.scope == project)
                .map(|c| c.holder.clone())
                .unwrap_or_else(|| "operator".to_string())
        }
        Some(1) => live
            .iter()
            .find(|c| c.level == 0 && members(&c.scope).iter().any(|m| m == scope))
            .map(|c| c.holder.clone())
            .unwrap_or_else(|| "operator".to_string()),
        _ => "operator".to_string(),
    }
}

/// Production transcript-age seam: seconds since the session's claude
/// transcript was last written. `None` is "no transcript", never "fresh".
/// Public: the `fno-agents` bin client's `reap` verb runs the sweep with it.
pub fn transcript_age_now(session: &str) -> Option<i64> {
    let path = crate::claude_drive::find_transcript(session)?;
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    std::time::SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs() as i64)
}

/// The daemon arm and manual verb entry: resolve the spaces root, registry
/// and events paths from `home`, the window from `cwd`'s config.
pub fn production_sweep(home: &crate::paths::AgentsHome, cwd: &Path, apply: bool) -> CrownReap {
    let events = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
    sweep(
        &crate::paths::spaces_root(),
        &home.registry_json(),
        cwd,
        &events,
        apply,
        &crate::claude_roster::read_all_agents_union,
        &transcript_age_now,
        Utc::now(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_roster::{ClaudeAgentRow, ClaudeAgentsSnapshot};
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crown-reap-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Pin the window and (optionally) the graph path: the project config
    /// beats the machine's global one, so `3 * 4h = 12h` holds whatever
    /// `~/.fno/config.toml` says, and the graph read answers from the fixture
    /// instead of the operator's live graph.
    fn pin_window(dir: &Path, graph: Option<&Path>) {
        let cfg = dir.join(".fno");
        fs::create_dir_all(&cfg).unwrap();
        let mut body = String::from("[king]\ncheckin_interval = \"4h\"\n");
        if let Some(graph) = graph {
            body.push_str(&format!("[paths]\ngraph_json = \"{}\"\n", graph.display()));
        }
        fs::write(cfg.join("config.toml"), body).unwrap();
    }

    fn write_crown_manifest(
        root: &Path,
        scope: &str,
        session: &str,
        created_at: &str,
        harness: &str,
    ) -> PathBuf {
        let path = root
            .join("space-a")
            .join("kings")
            .join(format!("{scope}.md"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = format!(
            "---\nfno_id: 20260919T000000Z-kg1-deadbeef\nscope: {scope}\nshape: pass\n\
             harness: {harness}\nharness_session_id: {session}\nowner_pid: 1\n\
             created_at: {created_at}\ncrown_scope: {scope}\ncrown_level: 2\n\
             crown_grantor: operator\n---\n"
        );
        fs::write(&path, body).unwrap();
        path
    }

    fn reg_row(
        name: &str,
        status: &str,
        scope: Option<&str>,
        level: Option<u32>,
    ) -> serde_json::Value {
        serde_json::json!({
            "name": name, "cwd": "/tmp", "status": status,
            "created_at": "2026-09-01T00:00:00Z",
            "harness": "claude", "harness_session_id": format!("{name}-session"),
            "crown_level": level, "crown_scope": scope,
            "crown_grantor": scope.map(|_| "operator"),
        })
    }

    fn registry_file(dir: &Path, rows: &[serde_json::Value]) -> PathBuf {
        let path = dir.join("registry.json");
        fs::write(
            &path,
            serde_json::json!({"schema_version": 11, "agents": rows}).to_string(),
        )
        .unwrap();
        path
    }

    fn old_created() -> &'static str {
        "2026-08-01T00:00:00Z"
    }

    /// A clean roster that lists nobody: absence from it plus an old
    /// transcript is the transcript death witness.
    fn empty_roster() -> ClaudeAgentsSnapshot {
        ClaudeAgentsSnapshot::known(vec![])
    }

    fn transcript(age_s: Option<i64>) -> impl Fn(&str) -> Option<i64> {
        move |_| age_s
    }

    fn events_of(dir: &Path) -> crate::events::EventEmitter {
        crate::events::EventEmitter::new(dir.join("events.jsonl"), "test")
    }

    fn read_events(dir: &Path) -> Vec<serde_json::Value> {
        // The store commit is the write boundary: committed rows carry the
        // event, raw journal bytes are only the pre-store fallback.
        let events = dir.join("events.jsonl");
        if let Ok(rows) = crate::event_store::query_events(&events, &Default::default()) {
            return rows
                .iter()
                .filter_map(|row| serde_json::from_str(&row.line).ok())
                .collect();
        }
        let Ok(content) = fs::read_to_string(&events) else {
            return vec![];
        };
        content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// The AC1 shape: dead holder, old transcript, old manifest.
    fn dead_fixture(tag: &str, session: &str) -> (PathBuf, PathBuf, PathBuf, String) {
        let dir = tmp(tag);
        pin_window(&dir, None);
        let manifest = write_crown_manifest(&dir, "zed", session, old_created(), "claude");
        fs::write(manifest.with_extension("cancelled"), "cancel").unwrap();
        let registry = registry_file(
            &dir,
            &[reg_row("stale-row", "exited", Some("zed"), Some(2))],
        );
        (dir, manifest, registry, session.to_string())
    }

    #[test]
    fn a_dead_holder_vacates_under_apply() {
        let (dir, manifest, registry, session) =
            dead_fixture("vacates", "aaaa1111-0000-4000-8000-000000000001");
        let emitter = events_of(&dir);
        let out = sweep(
            &dir,
            &registry,
            &dir,
            &emitter,
            true,
            &empty_roster,
            &transcript(Some(30 * 86_400)),
            Utc::now(),
        );
        assert!(out.unread.is_none(), "{:?}", out.unread);
        assert_eq!(out.vacated.len(), 1, "{:?}", out.kept);
        let v = &out.vacated[0];
        assert_eq!(v.scope, "zed");
        assert_eq!(v.holder_session.as_deref(), Some(session.as_str()));
        assert_eq!(v.inheritor, "operator");
        assert!(
            v.evidence.contains("absent from the roster"),
            "{}",
            v.evidence
        );
        assert_eq!(v.cleared_rows, vec!["stale-row".to_string()]);
        assert!(!manifest.exists(), "manifest must be gone");
        assert!(
            !manifest.with_extension("cancelled").exists(),
            "sentinel must be gone"
        );
        let events = read_events(&dir);
        let vacated: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "agent_crown_vacated")
            .collect();
        assert_eq!(vacated.len(), 1, "{:?}", events);
        assert_eq!(vacated[0]["data"]["cause"], "holder_dead");
        assert_eq!(vacated[0]["data"]["scope"], "zed");
        assert_eq!(vacated[0]["data"]["inheritor"], "operator");
        assert!(vacated[0]["data"]["evidence"].as_str().is_some());
        // AC7: the terminal row's stale crown fields cleared in the same write.
        let rows: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&registry).unwrap()).unwrap();
        let stale = &rows["agents"][0];
        assert!(stale["crown_scope"].is_null(), "{stale}");
        assert!(stale["crown_level"].is_null(), "{stale}");
        assert!(stale["crown_grantor"].is_null(), "{stale}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_blocked_holder_keeps_its_crown() {
        let (dir, manifest, registry, session) =
            dead_fixture("blocked", "bbbb2222-0000-4000-8000-000000000002");
        let roster = || {
            ClaudeAgentsSnapshot::known(vec![ClaudeAgentRow::new(&session[..8], Some("blocked"))])
        };
        let emitter = events_of(&dir);
        let out = sweep(
            &dir,
            &registry,
            &dir,
            &emitter,
            true,
            &roster,
            &transcript(Some(30 * 86_400)),
            Utc::now(),
        );
        assert!(out.vacated.is_empty(), "{:?}", out.vacated);
        assert_eq!(out.kept.len(), 1);
        assert_eq!(out.kept[0].reason, "roster blocked");
        assert!(manifest.exists(), "a kept crown is never removed");
        assert!(read_events(&dir).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_or_partial_roster_keeps_everything() {
        for (name, roster) in [
            (
                "unknown",
                Box::new(|| ClaudeAgentsSnapshot::unknown("no roster"))
                    as Box<dyn Fn() -> ClaudeAgentsSnapshot>,
            ),
            (
                "partial",
                Box::new(|| ClaudeAgentsSnapshot::Known {
                    rows: vec![],
                    warnings: vec!["2 rows skipped".to_string()],
                }) as Box<dyn Fn() -> ClaudeAgentsSnapshot>,
            ),
        ] {
            let tag = format!("roster-{name}");
            let (dir, manifest, registry, _session) =
                dead_fixture(&tag, "cccc3333-0000-4000-8000-000000000003");
            let emitter = events_of(&dir);
            let out = sweep(
                &dir,
                &registry,
                &dir,
                &emitter,
                true,
                &*roster,
                &transcript(Some(30 * 86_400)),
                Utc::now(),
            );
            assert!(out.vacated.is_empty(), "{name}: {:?}", out.vacated);
            assert_eq!(out.kept.len(), 1, "{name}");
            assert!(
                out.kept[0].reason.starts_with("roster"),
                "{}",
                out.kept[0].reason
            );
            assert!(manifest.exists());
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn a_codex_holder_has_no_death_witness() {
        let dir = tmp("codex");
        pin_window(&dir, None);
        write_crown_manifest(
            &dir,
            "x-codex",
            "cccc9999-0000-4000-8000-000000000009",
            old_created(),
            "codex",
        );
        let registry = registry_file(&dir, &[]);
        let emitter = events_of(&dir);
        let out = sweep(
            &dir,
            &registry,
            &dir,
            &emitter,
            true,
            &empty_roster,
            &transcript(Some(30 * 86_400)),
            Utc::now(),
        );
        assert!(out.vacated.is_empty(), "{:?}", out.vacated);
        assert_eq!(out.kept[0].reason, "no death witness for harness codex");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_transcript_keeps_the_crown() {
        let (dir, manifest, registry, _session) =
            dead_fixture("no-transcript", "dddd4444-0000-4000-8000-000000000004");
        let emitter = events_of(&dir);
        let out = sweep(
            &dir,
            &registry,
            &dir,
            &emitter,
            true,
            &empty_roster,
            &transcript(None),
            Utc::now(),
        );
        assert!(out.vacated.is_empty(), "{:?}", out.vacated);
        assert_eq!(out.kept[0].reason, "transcript not found");
        assert!(manifest.exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_fresh_transcript_or_a_young_manifest_keeps_the_crown() {
        let young_stamp = (Utc::now() - chrono::Duration::hours(1))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        for (name, age, created) in [
            ("fresh-transcript", Some(60), old_created().to_string()),
            ("young-manifest", Some(30 * 86_400), young_stamp),
        ] {
            let tag = format!("young-{name}");
            let dir = tmp(&tag);
            pin_window(&dir, None);
            let manifest = write_crown_manifest(
                &dir,
                "x-young",
                "eeee5555-0000-4000-8000-000000000005",
                &created,
                "claude",
            );
            let registry = registry_file(&dir, &[]);
            let emitter = events_of(&dir);
            let out = sweep(
                &dir,
                &registry,
                &dir,
                &emitter,
                true,
                &empty_roster,
                &transcript(age),
                Utc::now(),
            );
            assert!(out.vacated.is_empty(), "{name}: {:?}", out.vacated);
            assert_eq!(out.kept.len(), 1, "{name}");
            assert!(manifest.exists());
            fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn a_live_row_holding_the_scope_is_not_a_candidate() {
        let dir = tmp("live-row");
        pin_window(&dir, None);
        write_crown_manifest(
            &dir,
            "x-live",
            "ffff6666-0000-4000-8000-000000000006",
            old_created(),
            "claude",
        );
        // The split-crown shape: a live row holds the scope while the
        // manifest names a dead session. The scope is not a candidate.
        let registry = registry_file(
            &dir,
            &[reg_row("live-king", "busy", Some("x-live"), Some(2))],
        );
        let emitter = events_of(&dir);
        let out = sweep(
            &dir,
            &registry,
            &dir,
            &emitter,
            true,
            &empty_roster,
            &transcript(Some(30 * 86_400)),
            Utc::now(),
        );
        assert!(out.vacated.is_empty(), "{:?}", out.vacated);
        assert!(out.kept.is_empty(), "{:?}", out.kept);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rewritten_manifest_refuses_the_vacate() {
        let (dir, manifest, registry, _session) =
            dead_fixture("rewritten", "eeee5555-0000-4000-8000-000000000005");
        let emitter = events_of(&dir);
        // A successor armed between classify and apply: the re-read under the
        // manifest lock names a different session, so the manifest stands.
        // The registry leg ran first (the spawn path's own lock order) and
        // cleared the stale terminal crown fields, which is correct whatever
        // the manifest now names.
        let err = vacate(
            &manifest,
            "zed",
            Some("successor-session"),
            &registry,
            "evidence",
            "operator",
            &emitter,
        )
        .unwrap_err();
        assert!(err.contains("now names session"), "{err}");
        assert!(
            manifest.exists(),
            "a refused vacate leaves the manifest standing"
        );
        assert!(read_events(&dir).is_empty());
        let rows: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&registry).unwrap()).unwrap();
        assert!(rows["agents"][0]["crown_scope"].is_null(), "{rows}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_non_terminal_row_taking_the_scope_refuses_the_vacate() {
        let (dir, manifest, registry, session) =
            dead_fixture("row-took", "ffff6666-0000-4000-8000-000000000006");
        // Same race on the registry side: classify saw no holder row, the
        // vacate finds a live one holding the scope.
        let _ = fs::remove_file(&registry);
        let registry = registry_file(&dir, &[reg_row("new-king", "busy", Some("zed"), Some(2))]);
        let emitter = events_of(&dir);
        let err = vacate(
            &manifest,
            "zed",
            Some(&session),
            &registry,
            "evidence",
            "operator",
            &emitter,
        )
        .unwrap_err();
        assert!(err.contains("took the scope"), "{err}");
        assert!(manifest.exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_inheritor_names_the_presiding_l1_or_operator() {
        let dir = tmp("inheritor");
        // One epic list mapping to one project, so the presiding level-1
        // crown resolves through the graph path the config override pins.
        let graph_fixture = dir.join("graph-fixture.json");
        let graph = serde_json::json!({"entries": [
            {"id": "x-epic1", "project": "fno"},
            {"id": "x-epic2", "project": "fno"},
        ]});
        fs::write(&graph_fixture, graph.to_string()).unwrap();
        pin_window(&dir, Some(&graph_fixture));
        // Live level-1 crown over fno: a busy row holding scope fno at rung 1.
        let l1 = reg_row("crown-l1", "busy", Some("fno"), Some(1));
        let session = "aaaa1111-0000-4000-8000-000000000001";
        write_crown_manifest(&dir, "x-epic1,x-epic2", session, old_created(), "claude");
        let registry = registry_file(&dir, &[l1]);
        let emitter = events_of(&dir);
        let out = sweep(
            &dir,
            &registry,
            &dir,
            &emitter,
            false,
            &empty_roster,
            &transcript(Some(30 * 86_400)),
            Utc::now(),
        );
        assert_eq!(out.vacated.len(), 1, "{:?}", out.kept);
        assert_eq!(out.vacated[0].inheritor, "crown-l1");
        // Same world without the level-1 crown: the operator inherits.
        let registry = registry_file(&dir, &[]);
        let out = sweep(
            &dir,
            &registry,
            &dir,
            &emitter,
            false,
            &empty_roster,
            &transcript(Some(30 * 86_400)),
            Utc::now(),
        );
        assert_eq!(out.vacated[0].inheritor, "operator");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dry_run_reports_without_touching_anything() {
        let (dir, manifest, registry, session) =
            dead_fixture("dry-run", "aabb0000-0000-4000-8000-000000000007");
        let emitter = events_of(&dir);
        let out = sweep(
            &dir,
            &registry,
            &dir,
            &emitter,
            false,
            &empty_roster,
            &transcript(Some(30 * 86_400)),
            Utc::now(),
        );
        assert_eq!(out.vacated.len(), 1, "{:?}", out.kept);
        assert_eq!(out.vacated[0].scope, "zed");
        assert_eq!(
            out.vacated[0].holder_session.as_deref(),
            Some(session.as_str())
        );
        assert!(
            out.vacated[0].cleared_rows.is_empty(),
            "a dry run clears nothing"
        );
        assert!(manifest.exists());
        assert!(manifest.with_extension("cancelled").exists());
        assert!(read_events(&dir).is_empty());
        // The terminal row keeps its crown fields: only the apply clears them.
        let rows: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&registry).unwrap()).unwrap();
        assert_eq!(rows["agents"][0]["crown_scope"], "zed");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unreadable_registry_vacates_nothing() {
        let dir = tmp("unread");
        pin_window(&dir, None);
        write_crown_manifest(
            &dir,
            "zed",
            "bbbb2222-0000-4000-8000-000000000002",
            old_created(),
            "claude",
        );
        let registry = dir.join("registry.json");
        fs::write(&registry, "{not json").unwrap();
        let emitter = events_of(&dir);
        let out = sweep(
            &dir,
            &registry,
            &dir,
            &emitter,
            true,
            &empty_roster,
            &transcript(Some(30 * 86_400)),
            Utc::now(),
        );
        assert!(out.unread.is_some(), "{:?}", out);
        assert!(out.vacated.is_empty() && out.kept.is_empty());
        assert!(read_events(&dir).is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}

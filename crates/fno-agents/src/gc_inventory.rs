//! The retirement census (x-70e1 task 1): every applicable session, joined
//! to one identity, from every source that can see it.
//!
//! The sweep's row population is the registry, and the registry is not a
//! census. A completed native session fno never adopted is invisible to it
//! (the operator's original complaint), and a registry row names neither its
//! mux membership nor its native store context. The census is a read-only
//! union over the registry, the harness's own active/completed listing, the
//! transcript stores, and mux membership - and never adopting historical
//! transcripts into the active fleet.
//!
//! Every source read is injectable ([`SourceReaders`]) so the e2e target
//! stages the world; [`census`] wires the production readers. A source that
//! cannot be read is reported in [`Inventory::incomplete`] and contributes
//! nothing: a partial listing must not read as a complete one (AC1-EDGE).

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::paths::AgentsHome;
use crate::state;

/// One per-sweep index of the harness transcript stores. A registry-wide
/// question ("which rows' sessions still exist in their own store") is answered
/// by ONE walk per harness and in-memory lookups, never a walk per row: the
/// first cut of this walked `~/.claude/projects` twice per past-grace row, and
/// a live dry-run against a 125-row registry outran any operator's patience.
///
/// `None` from [`HarnessStoreIndex::matches`] means "cannot judge": no session
/// id recorded, an unknown harness, or a store directory that could not be
/// read (AC5 fail-closed - an unreadable store has two explanations and only
/// one of them is a dead session).
///
/// claude: `~/.claude/projects/*/<session_id>*.jsonl`, across EVERY project
/// dir because a session's transcript can live in more than one (EnterWorktree
/// re-keys it; the target dir keeps a stub). codex: the rollout jsonl under
/// `~/.codex/sessions/` embedding the session id in its filename - the same
/// shape `fno.agents.discover.codex_rollout_for_session` resolves. A harness a
/// reaper cannot speak for is NEVER judged by another harness's store (AC3): a
/// codex row has no claude transcript by construction, so a claude-keyed probe
/// would reap every codex worker on the machine.
#[derive(Default)]
pub struct HarnessStoreIndex {
    /// Resolved store roots; `None` until the first lookup resolves them from
    /// `$HOME` (or forever, for an index built `with_roots` in tests).
    claude_root: Option<std::path::PathBuf>,
    codex_root: Option<std::path::PathBuf>,
    /// `(filename, path)` for every candidate file, or `None` until the first
    /// lookup walks the store. `Some(Err(()))` marks a walk that hit an
    /// unreadable directory: every later lookup answers None, fail closed.
    claude: Option<Result<Vec<(String, std::path::PathBuf)>, ()>>,
    codex: Option<Result<Vec<(String, std::path::PathBuf)>, ()>>,
}

impl HarnessStoreIndex {
    /// Test seam: fixed roots, so the per-harness keying is unit-testable
    /// against temp trees instead of the developer's real `~/.claude`/`~/.codex`.
    /// (Called only from the lib test suite. Deliberately NOT gated with the
    /// test cfg attribute: the emit-kind scanner in lib.rs truncates each file
    /// at the first byte-level occurrence of that attribute's text, so a
    /// mid-file gate would classify every later production emit as test-only.)
    #[allow(dead_code)]
    fn with_roots(claude_root: std::path::PathBuf, codex_root: std::path::PathBuf) -> Self {
        HarnessStoreIndex {
            claude_root: Some(claude_root),
            codex_root: Some(codex_root),
            ..Default::default()
        }
    }

    fn root(&self, harness: &str) -> Option<std::path::PathBuf> {
        let slot = match harness {
            "claude" => &self.claude_root,
            "codex" => &self.codex_root,
            _ => return None,
        };
        slot.clone().or_else(|| {
            let home = std::path::PathBuf::from(std::env::var("HOME").ok()?);
            match harness {
                "claude" => Some(home.join(".claude").join("projects")),
                // Resolve the codex home the way codex itself does, so a
                // CODEX_HOME redirect never reads this reaper into a store
                // the worker never wrote (an empty wrong-store read would
                // read as "session gone" - death evidence from an absence).
                _ => crate::client_verbs::codex_home().map(|h| h.join("sessions")),
            }
        })
    }

    /// Every transcript candidate this row's harness store holds for its
    /// session id. Empty vector = the session is GONE from its own store.
    pub(crate) fn matches(&mut self, e: &state::RegistryEntry) -> Option<Vec<std::path::PathBuf>> {
        let sid = e.harness_session_id.as_deref().filter(|s| !s.is_empty())?;
        let harness = e.harness_name();
        let root = match harness {
            "claude" | "codex" => self.root(harness)?,
            // Unknown/unsupported harness (gemini, opencode, ...): no store
            // this reaper can read. Answer None, never another harness's store.
            _ => return None,
        };
        let cached_empty = match harness {
            "claude" => self.claude.is_none(),
            _ => self.codex.is_none(),
        };
        if cached_empty {
            // First lookup for this harness: one walk, ~100 files per walk,
            // cached for the sweep (an unreadable store caches as Err, so it
            // stays fail-closed for every later row instead of re-walking).
            let indexed = index_tree(&root, 0);
            let parked = match harness {
                "claude" => &mut self.claude,
                _ => &mut self.codex,
            };
            *parked = Some(indexed);
        }
        let files = match harness {
            "claude" => self.claude.as_ref()?,
            _ => self.codex.as_ref()?,
        };
        let files = files.as_ref().ok()?;
        Some(
            files
                .iter()
                .filter(|(name, _)| match harness {
                    // `<uuid>.jsonl` and its stub artifacts (`<uuid>.orphaned-...`)
                    // all prove the session still EXISTS in the store; which of
                    // them carries conversation is a content question this
                    // existence probe does not need to answer.
                    "claude" => name.starts_with(sid) && name.ends_with(".jsonl"),
                    _ => crate::client_verbs::codex_rollout_matches(&name, sid),
                })
                .map(|(_, p)| p.clone())
                .collect(),
        )
    }
}

/// Bounded walk collecting `(filename, path)` for every regular file under
/// `dir` (claude is two levels, codex four; depth 5 covers both). `Err` on any
/// unreadable directory: an unreadable store answers nothing, fail closed.
pub(crate) fn index_tree(
    dir: &std::path::Path,
    depth: usize,
) -> Result<Vec<(String, std::path::PathBuf)>, ()> {
    let mut out = Vec::new();
    if depth > 5 {
        return Ok(out);
    }
    for entry in std::fs::read_dir(dir).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|_| ())?;
        if file_type.is_dir() {
            out.extend(index_tree(&path, depth + 1)?);
        } else {
            out.push((entry.file_name().to_string_lossy().into_owned(), path));
        }
    }
    Ok(out)
}

/// Where one session was seen. A session the registry never adopted but the
/// native listing or the transcript store holds is exactly the population
/// the operator sees and the registry cannot name; the source tags make the
/// join auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A fno registry row names it.
    Registry,
    /// The harness's own active/completed listing names it.
    NativeListing,
    /// A transcript file exists in the harness's project store.
    Store,
    /// A mux squad membership names it.
    Mux,
}

/// One discovered session: the full native identity plus every surface that
/// names it. An inventory record, never a registry write: discovering a
/// session here does not adopt it into the active fleet.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct InventorySession {
    pub harness: String,
    /// The full native session id (a claude uuid, a codex thread id) - the
    /// identity receipts and resume key on; short labels are attributes,
    /// never substitutes.
    pub session_id: String,
    /// The registry row label when the registry names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_name: Option<String>,
    /// Every source that names this session.
    pub sources: Vec<Source>,
    /// Transcript candidates in the harness's own store.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub transcripts: Vec<PathBuf>,
    /// Seconds since the newest transcript was written; None unresolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_age_s: Option<i64>,
}

impl InventorySession {
    /// Fold a second sighting of the same normalized identity into this
    /// record: union the sources, keep the first registry label, union the
    /// transcripts, keep the freshest age.
    fn merge(&mut self, other: InventorySession) {
        for s in other.sources {
            if !self.sources.contains(&s) {
                self.sources.push(s);
            }
        }
        if self.registry_name.is_none() {
            self.registry_name = other.registry_name;
        }
        for t in other.transcripts {
            if !self.transcripts.contains(&t) {
                self.transcripts.push(t);
            }
        }
        match (self.transcript_age_s, other.transcript_age_s) {
            (Some(a), Some(b)) => self.transcript_age_s = Some(a.min(b)),
            (None, b) => self.transcript_age_s = b,
            _ => {}
        }
    }
}

/// The one sweep's census plus the coverage report that says how complete it
/// is. A census with an incomplete source has NOT enumerated the world, and
/// its consumers must not retire from the missing parts (AC1-EDGE).
#[derive(Debug, Default, serde::Serialize)]
pub struct Inventory {
    pub sessions: Vec<InventorySession>,
    /// `(source, reason)` for every source read that failed or was partial.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<(String, String)>,
}

/// The source readers, injected so tests stage the world. Each returns the
/// raw rows its source holds; a read failure is `Err(reason)` and lands in
/// `Inventory::incomplete`.
pub struct SourceReaders {
    /// `(session id, registry label)` for every entry the harness's native
    /// listing reports (active AND completed populations both).
    pub claude_listing: Box<dyn Fn() -> Result<Vec<(String, Option<String>)>, String> + 'static>,
    /// `(harness, session id)` for every mux member that carries a native
    /// session id.
    pub mux_members: Box<dyn Fn() -> Result<Vec<(String, String)>, String> + 'static>,
    /// normalized session id -> transcript paths, claude project store.
    pub claude_store: Box<dyn Fn() -> Result<BTreeMap<String, Vec<PathBuf>>, String> + 'static>,
    /// normalized session id -> transcript paths, codex rollout store.
    pub codex_store: Box<dyn Fn() -> Result<BTreeMap<String, Vec<PathBuf>>, String> + 'static>,
}

/// `(session id, registry label)` over the claude roster: the active AND
/// completed background sessions the claude daemon still knows. A read
/// failure is returned, never flattened to an empty list - an empty list
/// would read as "nothing native exists".
fn read_claude_listing() -> Result<Vec<(String, Option<String>)>, String> {
    let roster = crate::claude_roster::ClaudeRoster::load_default()
        .map_err(|e| format!("claude roster unreadable: {e}"))?;
    Ok(roster
        .workers
        .values()
        .map(|w| (w.session_id.clone(), None))
        .collect())
}

/// `(harness, session id)` for every non-tombstoned mux member carrying a
/// native session id.
fn read_mux_members() -> Result<Vec<(String, String)>, String> {
    let path = squads_path();
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("squads.json unreadable: {e}"))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("squads.json malformed: {e}"))?;
    let mut out = Vec::new();
    for member in value
        .get("squads")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("members").and_then(serde_json::Value::as_array))
        .flatten()
    {
        if member.get("tombstone").and_then(serde_json::Value::as_bool) == Some(true) {
            continue;
        }
        if let (Some(h), Some(sid)) = (
            member.get("harness").and_then(serde_json::Value::as_str),
            member
                .get("harness_session_id")
                .and_then(serde_json::Value::as_str),
        ) {
            if !sid.is_empty() {
                out.push((h.to_string(), sid.to_string()));
            }
        }
    }
    Ok(out)
}

/// The mux squad store, beside graph.json in the state root.
fn squads_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(".fno").join("squads.json")
}

/// The claude project store walked once: normalized `<uuid>` -> transcript
/// paths, over `<uuid>.jsonl` filenames in every project dir. Stub artifacts
/// (`<uuid>.orphaned-...`) are existence evidence for a KNOWN session, not
/// identities of their own, so they never mint an inventory row.
fn claude_store_sessions() -> Result<BTreeMap<String, Vec<PathBuf>>, String> {
    let root = std::path::PathBuf::from(std::env::var("HOME").map_err(|_| "no HOME")?)
        .join(".claude")
        .join("projects");
    let files =
        index_tree(&root, 0).map_err(|_| format!("claude store unreadable: {}", root.display()))?;
    let mut out: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for (name, path) in files {
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if !is_uuid_like(stem) {
            continue;
        }
        out.entry(stem.to_ascii_lowercase()).or_default().push(path);
    }
    Ok(out)
}

/// The codex rollout store walked once: the thread id embedded in each
/// `rollout-<stamp>-<uuid>.jsonl` filename under `$CODEX_HOME/sessions`.
fn codex_store_sessions() -> Result<BTreeMap<String, Vec<PathBuf>>, String> {
    let root = crate::client_verbs::codex_home()
        .ok_or_else(|| "no codex home".to_string())?
        .join("sessions");
    let files =
        index_tree(&root, 0).map_err(|_| format!("codex store unreadable: {}", root.display()))?;
    let mut out: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for (name, path) in files {
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        let Some(sid) = stem.rsplit('-').next() else {
            continue;
        };
        if !is_uuid_like(sid) && sid.len() != 32 {
            continue;
        }
        out.entry(sid.to_ascii_lowercase()).or_default().push(path);
    }
    Ok(out)
}

fn newest_age(paths: &[PathBuf], now: i64) -> Option<i64> {
    paths
        .iter()
        .filter_map(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
        })
        .max()
        .map(|newest| now.saturating_sub(newest))
}

/// 8-4-4-4-12 hex uuid shape, case-insensitive. The store walks key
/// identities on this; anything else in a projects tree is not a session.
fn is_uuid_like(s: &str) -> bool {
    let mut groups = s.split('-');
    for expected in [8usize, 4, 4, 4, 12] {
        let Some(g) = groups.next() else {
            return false;
        };
        if g.len() != expected || !g.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    groups.next().is_none()
}

/// The production readers.
pub fn production_readers() -> SourceReaders {
    SourceReaders {
        claude_listing: Box::new(read_claude_listing),
        mux_members: Box::new(read_mux_members),
        claude_store: Box::new(claude_store_sessions),
        codex_store: Box::new(codex_store_sessions),
    }
}

/// The census union over the registry, the native listing, the transcript
/// stores and mux. Read-only: nothing here writes the registry or adopts a
/// discovered session into the active fleet.
pub fn census(home: &AgentsHome) -> Inventory {
    census_with(home, production_readers())
}

/// One source's rows folded into the census under one normalized identity
/// key. Shared by every source so the join rule cannot drift between them.
fn fold(
    by_key: &mut BTreeMap<String, InventorySession>,
    harness: &str,
    session_id: String,
    registry_name: Option<String>,
    source: Source,
    transcripts: Vec<PathBuf>,
    transcript_age_s: Option<i64>,
) {
    let key = session_id.to_ascii_lowercase();
    let row = InventorySession {
        harness: harness.to_string(),
        session_id,
        registry_name,
        sources: vec![source],
        transcripts,
        transcript_age_s,
    };
    match by_key.get_mut(&key) {
        Some(existing) => existing.merge(row),
        None => {
            by_key.insert(key, row);
        }
    }
}

/// [`census`] with injectable readers.
pub fn census_with(home: &AgentsHome, readers: SourceReaders) -> Inventory {
    let mut inventory = Inventory::default();
    let mut by_key: BTreeMap<String, InventorySession> = BTreeMap::new();
    let now = crate::daemon::now_epoch_secs();

    match state::load_registry(&home.registry_json()) {
        Ok(registry) => {
            for e in &registry.entries {
                if let Some(sid) = e.harness_session_id.as_deref().filter(|s| !s.is_empty()) {
                    fold(
                        &mut by_key,
                        &e.harness_name(),
                        sid.to_string(),
                        Some(e.name.clone()),
                        Source::Registry,
                        Vec::new(),
                        None,
                    );
                }
            }
        }
        Err(reason) => inventory
            .incomplete
            .push(("registry".into(), format!("registry unreadable: {reason}"))),
    }

    match (readers.claude_listing)() {
        Ok(rows) => {
            for (sid, label) in rows {
                fold(
                    &mut by_key,
                    "claude",
                    sid,
                    label,
                    Source::NativeListing,
                    Vec::new(),
                    None,
                );
            }
        }
        Err(reason) => inventory.incomplete.push(("native-listing".into(), reason)),
    }

    for (source_tag, walked) in [
        ("store", (readers.claude_store)()),
        ("codex-store", (readers.codex_store)()),
    ] {
        match walked {
            Ok(store) => {
                let (harness, tag) = if source_tag == "store" {
                    ("claude", "store")
                } else {
                    ("codex", "codex-store")
                };
                let _ = tag;
                for (sid, paths) in store {
                    let age = newest_age(&paths, now);
                    fold(
                        &mut by_key,
                        harness,
                        sid,
                        None,
                        if harness == "claude" {
                            Source::Store
                        } else {
                            Source::Store
                        },
                        paths,
                        age,
                    );
                }
            }
            Err(reason) => inventory.incomplete.push((source_tag.into(), reason)),
        }
    }

    match (readers.mux_members)() {
        Ok(rows) => {
            for (harness, sid) in rows {
                fold(
                    &mut by_key,
                    &harness,
                    sid,
                    None,
                    Source::Mux,
                    Vec::new(),
                    None,
                );
            }
        }
        Err(reason) => inventory.incomplete.push(("mux".into(), reason)),
    }

    inventory.sessions = by_key.into_values().collect();
    inventory
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, harness: &str, sid: &str) -> state::RegistryEntry {
        serde_json::from_str(&format!(
            r#"{{"name":"{name}","short_id":"{name}","harness":"{harness}","harness_session_id":"{sid}","cwd":"/tmp/x","created_at":"2026-09-01T00:00:00Z","status":"live"}}"#
        ))
        .unwrap()
    }

    fn home_with(entries: &[state::RegistryEntry]) -> AgentsHome {
        let dir = std::env::temp_dir().join(format!(
            "fno-gc-inv-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let home = AgentsHome::at(dir);
        home.ensure_root().unwrap();
        state::update_registry(&home.registry_json(), |r| {
            r.entries.extend(entries.iter().cloned())
        })
        .unwrap();
        home
    }

    fn silent_readers() -> SourceReaders {
        SourceReaders {
            claude_listing: Box::new(|| Ok(Vec::new())),
            mux_members: Box::new(|| Ok(Vec::new())),
            claude_store: Box::new(|| Ok(BTreeMap::new())),
            codex_store: Box::new(|| Ok(BTreeMap::new())),
        }
    }

    #[test]
    fn inventory_keys_join_by_normalized_session_id_across_sources() {
        let home = home_with(&[entry("w1", "claude", "UUID-1234")]);
        let mut readers = silent_readers();
        readers.claude_listing = Box::new(|| Ok(vec![("uuid-1234".to_string(), None)]));
        readers.mux_members =
            Box::new(|| Ok(vec![("claude".to_string(), "UUID-1234".to_string())]));
        let inv = census_with(&home, readers);
        assert_eq!(inv.sessions.len(), 1, "{:?}", inv.sessions);
        let s = &inv.sessions[0];
        assert_eq!(s.sources.len(), 3, "{:?}", s.sources);
        assert_eq!(s.registry_name.as_deref(), Some("w1"));
        assert!(inv.incomplete.is_empty());
    }

    #[test]
    fn a_completed_native_session_with_no_registry_row_is_enumerated_not_adopted() {
        // AC1-HP: the native listing holds a completed session the registry
        // never named. The census reports it; nothing is written to the
        // registry.
        let home = home_with(&[]);
        let mut readers = silent_readers();
        readers.claude_listing = Box::new(|| Ok(vec![("deadbeef-1-2-3-4".to_string(), None)]));
        let inv = census_with(&home, readers);
        assert_eq!(inv.sessions.len(), 1);
        assert_eq!(inv.sessions[0].sources, vec![Source::NativeListing]);
        assert!(inv.sessions[0].registry_name.is_none());
        // The adoption prohibition: the registry on disk is untouched.
        assert!(state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .is_empty());
    }

    #[test]
    fn a_failed_source_read_is_reported_and_never_reads_as_empty() {
        // AC1-EDGE: an unreadable native listing lands in `incomplete`; the
        // census never presents the remaining sources as the whole world.
        let home = home_with(&[entry(
            "w1",
            "claude",
            "aaaaaaaa-1111-2222-3333-444444444444",
        )]);
        let mut readers = silent_readers();
        readers.claude_listing = Box::new(|| Err("claude listing failed".into()));
        let inv = census_with(&home, readers);
        assert_eq!(inv.incomplete.len(), 1);
        assert_eq!(inv.incomplete[0].0, "native-listing");
        assert!(inv
            .sessions
            .iter()
            .any(|s| s.registry_name == Some("w1".into())));
    }

    #[test]
    fn uuid_shape_is_the_store_identity_key() {
        assert!(is_uuid_like("ee99ff00-7777-8888-9999-aaaabbbbcccc"));
        assert!(is_uuid_like("EE99FF00-7777-8888-9999-AAAABBBBCCCC"));
        assert!(!is_uuid_like("short"));
        assert!(!is_uuid_like("ee99ff00_7777-8888-9999-aaaabbbbcccc"));
        assert!(!is_uuid_like("ee99ff00-7777-8888-9999-aaaabbbbcccc-extra"));
    }
}

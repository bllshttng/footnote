//! The spawn-journal read that restore classifies against (x-7b5e, x-9052):
//! the still-held spawn receipts, their revocations, and the never-bound
//! removal markers, walked in file order across rotated segments. Extracted
//! from server.rs under the file budget's shrink-only rule; the question the
//! module answers is "what does the journal still hold". The spawn re-entry
//! verdict types ride here because their construction parses the same text.

use std::collections::HashMap;

use crate::agents_view::{self, RegistryAgent};
use crate::server::agent_harness_session_id;

#[derive(Debug, Clone)]
pub(crate) struct HeldWorker {
    pub(crate) name: String,
    pub(crate) harness: String,
    pub(crate) harness_session_id: String,
    pub(crate) cwd: String,
}

/// A worker pane removed from every visible tree but still backed by a live
/// PTY. Keeper-hosted panes can outlive this server, so the row identity is
/// retained separately from the in-process pane map and persisted on its
/// squad member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetachedPane {
    pub(crate) name: String,
    pub(crate) harness: Option<String>,
    pub(crate) harness_session_id: Option<String>,
    pub(crate) cwd: String,
    pub(crate) squad: u64,
    pub(crate) squad_name: String,
    pub(crate) squad_key: String,
    pub(crate) origins: Vec<String>,
    pub(crate) tab_name: Option<String>,
}

impl DetachedPane {
    pub(crate) fn from_agent(
        agent: &RegistryAgent,
        squad: u64,
        squad_name: String,
        squad_key: String,
        origins: Vec<String>,
        tab_name: Option<String>,
    ) -> Self {
        Self {
            name: agent.name.clone(),
            harness: agent.harness.clone(),
            harness_session_id: agent_harness_session_id(agent).map(str::to_string),
            cwd: agent.cwd.clone(),
            squad,
            squad_name,
            squad_key,
            origins,
            tab_name,
        }
    }

    pub(crate) fn from_member(
        member: &crate::squad_store::StoredMember,
        squad: u64,
        squad_name: String,
        squad_key: String,
        origins: Vec<String>,
    ) -> Option<Self> {
        Some(Self {
            name: member.worker.as_deref()?.to_string(),
            harness: member.harness.clone(),
            harness_session_id: member.harness_session_id.clone(),
            cwd: member.cwd.clone().unwrap_or_default(),
            squad,
            squad_name,
            squad_key,
            origins,
            tab_name: member.tab_name.clone(),
        })
    }

    pub(crate) fn matches_agent(&self, agent: &RegistryAgent) -> bool {
        self.name == agent.name
            && self
                .harness
                .as_deref()
                .is_none_or(|h| agent.harness.as_deref() == Some(h))
            && self
                .harness_session_id
                .as_deref()
                .is_none_or(|id| agent_harness_session_id(agent) == Some(id))
    }

    pub(crate) fn matches_member(&self, member: &crate::squad_store::StoredMember) -> bool {
        member.worker.as_deref() == Some(self.name.as_str())
            && member
                .harness
                .as_deref()
                .is_none_or(|h| self.harness.as_deref() == Some(h))
            && member
                .harness_session_id
                .as_deref()
                .is_none_or(|id| self.harness_session_id.as_deref() == Some(id))
    }
}

/// (x-d285) The canonical re-entry verdict a mux gesture consumes, parsed from
/// `fno-agents reentry-plan`'s JSON. `argv` is the provider invocation (ids
/// and file PATHS only - never a settings value), `env` the `KEY=VALUE`
/// assignments (the account's `CLAUDE_CONFIG_DIR`) that prefix it through
/// `env(1)`, and `config_dir` the bare dir for the pane-title roster lookup.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReentryVerdict {
    pub(crate) argv: Vec<String>,
    pub(crate) env: Vec<String>,
    pub(crate) config_dir: Option<std::path::PathBuf>,
}

impl ReentryVerdict {
    /// Parse the resolver's machine output. Every non-conforming shape is the
    /// same refusal the raw subprocess failure is: `resolved != true` carries
    /// no launch, so nothing may spawn off it.
    pub(crate) fn from_plan_json(raw: &[u8]) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_slice(raw).map_err(|e| format!("malformed re-entry verdict: {e}"))?;
        if value.get("resolved").and_then(|v| v.as_bool()) != Some(true) {
            return Err("re-entry verdict is not resolved".to_string());
        }
        let argv = value
            .get("argv")
            .and_then(|v| v.as_array())
            .and_then(|a| {
                a.iter()
                    .map(|x| x.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| "re-entry verdict carries no argv".to_string())?;
        if argv.is_empty() {
            return Err("re-entry verdict carries an empty argv".to_string());
        }
        let env = value
            .get("env")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .map(|(k, v)| {
                        v.as_str()
                            .map(|s| format!("{k}={s}"))
                            .ok_or_else(|| format!("env value for {k} is not a string"))
                    })
                    .collect::<Result<Vec<_>, String>>()
            })
            .transpose()
            .map_err(|e| format!("re-entry verdict carries a malformed env: {e}"))?
            .unwrap_or_default();
        let config_dir = value
            .get("claude_config_dir")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
        Ok(Self {
            argv,
            env,
            config_dir,
        })
    }

    /// The argv the pane runs: `env(1)` assignments prefixed onto the provider
    /// invocation, argv tokens throughout (never a shell string).
    pub(crate) fn prefixed_argv(&self) -> Vec<String> {
        if self.env.is_empty() {
            return self.argv.clone();
        }
        let mut argv = vec!["env".to_string()];
        argv.extend(self.env.iter().cloned());
        argv.extend(self.argv.iter().cloned());
        argv
    }
}

/// (x-d285) What the core loop re-enters once a gesture's re-entry plan
/// arrives. The gesture arms re-run with the verdict in hand: every gate
/// (shape, catalog, reconcile-focus, placement) is an idempotent read, so the
/// second pass makes exactly the placement decision the first pass would
/// have - only the argv construction moved off-loop into the canonical
/// resolver.
#[derive(Debug, Clone)]
pub(crate) enum ReentrySpawnRequest {
    /// Re-enter `attach_agent_gesture` for `attach_id` under its original
    /// placement (open-here, split/drop, or the thread-pane reach).
    Attach {
        attach_id: String,
        placement: crate::proto::PanePlacement,
    },
    /// Re-enter `resume_agent_gesture` for a claude row.
    Resume { name: String },
    /// Re-enter the held-worker resume behind `Command::FocusPane(pid)` -
    /// restore's interactive arm replays with the verdict staged.
    FocusHeld { pid: u64 },
}

/// (x-d285) What a batch re-enters once its pre-resolved plans land. The
/// single-verdict slot covers one-gesture-one-spawn replays; a batch is
/// N spawns under one gesture, so its plans arrive keyed by attach id and
/// this names the loop that consumes them. Every gate inside those loops
/// is an idempotent read, so a replay after partial progress skips the
/// already-recruited members and consumes only what remains.
#[derive(Debug, Clone)]
pub(crate) enum BatchReplay {
    /// Re-enter restore's member loop (once per server lifetime).
    Restore { home_sid: u64, rows: u16, cols: u16 },
    /// Re-enter the picker's bulk recruit for the selected ids.
    Recruit { squad: String, ids: Vec<String> },
}

/// The still-held spawn receipts from journal text: a thin view over
/// [`parse_journal_events`], which walks receipts and never-bound markers in
/// one pass.
///
/// Test-only, and gated as such: production reads both halves in one pass, so
/// this accessor exists purely to keep the receipts half independently
/// testable (as the sibling doc below says). Without the gate it reads as dead
/// code in a release build.
#[cfg(test)]
pub(crate) fn parse_spawn_receipts(raw: &str) -> HashMap<(String, String), HeldWorker> {
    parse_journal_events(raw).receipts
}

/// One walk over journal text yielding both restore inputs: the still-held
/// spawn receipts (build and revoke in file order) and the never-bound
/// removal markers. One pass, because restore reads this journal twice
/// otherwise; the split accessors below keep the two halves independently
/// testable.
pub(crate) struct JournalEvents {
    pub(crate) receipts: HashMap<(String, String), HeldWorker>,
    pub(crate) never_bound: HashMap<String, String>,
}

pub(crate) fn parse_journal_events(raw: &str) -> JournalEvents {
    let mut receipts: HashMap<(String, String), HeldWorker> = HashMap::new();
    // Recency, not presence: a removal older than the name's latest spawn is
    // a dead name that came back to life.
    let mut last_spawn: HashMap<String, usize> = HashMap::new();
    let mut removals: HashMap<String, (usize, String)> = HashMap::new();
    for (idx, line) in raw.lines().enumerate() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(data) = value.get("data").and_then(|v| v.as_object()) else {
            continue;
        };
        match value.get("type").and_then(|v| v.as_str()) {
            Some("agent_row_reaped")
                if data.get("resumable").and_then(|v| v.as_bool()) == Some(true) =>
            {
                continue;
            }
            Some("agent_removed") | Some("agent_row_reaped") => {
                let session_id = data.get("harness_session_id").and_then(|v| v.as_str());
                let harness = data
                    .get("harness")
                    .or_else(|| data.get("provider"))
                    .and_then(|v| v.as_str());
                receipts.retain(|(receipt_harness, receipt_session), _| {
                    if let (Some(harness), Some(session_id)) = (harness, session_id) {
                        receipt_harness != harness || receipt_session != session_id
                    } else {
                        // An incomplete event cannot identify one receipt.
                        // Reused names are not lifecycle identity.
                        true
                    }
                });
                continue;
            }
            // A spawn both anchors marker recency and, when it carries a full
            // session identity, mint a receipt - the fall-through below.
            Some("agent_spawned") => {
                if let Some(name) = data.get("name").and_then(|v| v.as_str()) {
                    last_spawn.insert(name.to_string(), idx);
                }
            }
            Some("registry_row_removed") => {
                // (x-6b0b) A worker name the journal positively records as
                // never bound: the row's own session field is absent or empty
                // and the reason names the missing identity. An
                // `agent_removed` row with a null session is deliberately NOT
                // a marker - that shape also fires for probe workers
                // (py3probe) that must stay `Unknown`. The set is keyed on
                // the worker NAME because a never-bound member has no other
                // identity; reused names are not lifecycle identity, so the
                // recency filter below re-checks every marker against the
                // name's last spawn.
                let sessionless = data
                    .get("harness_session_id")
                    .map(|v| v.as_str().unwrap_or_default().is_empty())
                    .unwrap_or(true);
                let names_identity = data
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .is_some_and(|reason| reason.contains("identity"));
                if sessionless && names_identity {
                    let reason = data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if let Some(name) = data.get("name").and_then(|v| v.as_str()) {
                        removals.insert(name.to_string(), (idx, reason));
                    }
                }
                continue;
            }
            _ => continue,
        }
        if !matches!(
            data.get("substrate").and_then(|v| v.as_str()),
            Some("pane") | Some("thread") | Some("bg")
        ) {
            continue;
        }
        let Some(session_id) = data
            .get("harness_session_id")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let Some(name) = data
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let Some(harness) = data
            .get("provider")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        receipts.insert(
            (harness.to_string(), session_id.to_string()),
            HeldWorker {
                name: name.to_string(),
                harness: harness.to_string(),
                harness_session_id: session_id.to_string(),
                cwd: data
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            },
        );
    }
    let never_bound = removals
        .into_iter()
        // No spawn on record: the name was never spawned in this journal, so
        // the removal is trivially the newest fact about it (a journal whose
        // FIRST line is the removal must still mark).
        .filter(|(name, (idx, _))| last_spawn.get(name).is_none_or(|spawn| idx > spawn))
        .map(|(name, (_, reason))| (name, reason))
        .collect();
    JournalEvents {
        receipts,
        never_bound,
    }
}

/// (x-6b0b) Worker names the journal positively records as never bound, with
/// the removal reason each carries.
pub(crate) fn parse_never_bound_removals(raw: &str) -> HashMap<String, String> {
    parse_journal_events(raw).never_bound
}

/// The journal's retained segment paths, OLDEST first: `<stem>.<N>` with `N`
/// all digits ordered high `N` first (rotation renames the live file to `.1`,
/// so a higher generation is older), the live file last. Enumerated, never a
/// hardcoded `.1` - the emitter keeps one generation today
/// (fno-agents `events.rs` ROTATE_AT_BYTES), and a reader that survives a
/// retention change is free.
fn spawn_receipt_segments(dir: &std::path::Path, stem: &str) -> Vec<std::path::PathBuf> {
    let prefix = format!("{stem}.");
    let mut segments: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let generation = name.strip_prefix(&prefix)?.parse::<u64>().ok()?;
            Some((generation, entry.path()))
        })
        .collect();
    segments.sort_by(|(a, _), (b, _)| b.cmp(a));
    segments.into_iter().map(|(_, path)| path).collect()
}

/// What one pass over the agent journal yields for restore: the still-held
/// spawn receipts, the never-bound removal markers (x-6b0b), and the first
/// read error. Segments are concatenated OLDEST FIRST before parsing, because
/// `parse_spawn_receipts` revokes in file order - a revocation in the live
/// file must land on a receipt from `.1`, and reading newest-first would
/// resurrect reaped sessions.
pub(crate) struct SpawnJournal {
    pub(crate) receipts: HashMap<(String, String), HeldWorker>,
    pub(crate) never_bound: HashMap<String, String>,
    pub(crate) error: Option<String>,
}

pub(crate) fn scan_spawn_journal() -> SpawnJournal {
    let live = agents_view::registry_path().with_file_name("events.jsonl");
    scan_journal_at(&live)
}

/// The path-injected core of [`scan_spawn_journal`], so the segment walk is
/// unit-testable without touching the operator's registry location.
pub(crate) fn scan_journal_at(live: &std::path::Path) -> SpawnJournal {
    let (combined, error) = read_journal_text_at(live);
    let events = parse_journal_events(&combined);
    SpawnJournal {
        receipts: events.receipts,
        never_bound: events.never_bound,
        error,
    }
}

/// (x-6b0b) The journal's retained segments plus the live file, concatenated
/// OLDEST FIRST and newline-terminated per segment, with the first read
/// error. Shared with the mux CLI's prune evidence (`member_evidence`), so
/// the sweep modal and the CLI apply read the same durable rows the server
/// sweep reads - one reader shape, never two.
pub(crate) fn read_journal_text_at(live: &std::path::Path) -> (String, Option<String>) {
    let dir = live.parent().map(std::path::Path::to_path_buf);
    let mut paths = dir
        .map(|dir| spawn_receipt_segments(&dir, "events.jsonl"))
        .unwrap_or_default();
    paths.push(live.to_path_buf());
    let mut combined = String::new();
    let mut error = None;
    for path in &paths {
        match std::fs::read_to_string(path) {
            // A segment whose last row lacks its newline would fuse with the
            // next segment's first row into one unparseable line - every
            // segment is newline-terminated before the next begins.
            Ok(raw) => {
                combined.push_str(&raw);
                if !raw.ends_with('\n') {
                    combined.push('\n');
                }
            }
            // A missing segment is skipped; an unreadable one surfaces its
            // error while the readable segments still contribute - unreadable
            // is not missing, and neither folds into "no receipt".
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                error.get_or_insert_with(|| {
                    format!("spawn receipt store unreadable at {}: {e}", path.display())
                });
            }
        }
    }
    (combined, error)
}

pub(crate) fn receipt_for_member<'a>(
    receipts: &'a HashMap<(String, String), HeldWorker>,
    member: &crate::squad_store::StoredMember,
) -> Option<&'a HeldWorker> {
    let session_id = member.harness_session_id.as_deref()?;
    if let Some(harness) = member.harness.as_deref() {
        return receipts.get(&(harness.to_string(), session_id.to_string()));
    }
    let mut match_ = None;
    for receipt in receipts
        .values()
        .filter(|receipt| receipt.harness_session_id == session_id)
    {
        if match_.is_some() {
            return None;
        }
        match_ = Some(receipt);
    }
    match_
}

pub(crate) fn worker_binding_key(member: &crate::squad_store::StoredMember) -> Option<String> {
    let worker = member.worker.as_deref()?;
    match (
        member.harness.as_deref(),
        member.harness_session_id.as_deref(),
    ) {
        (Some(harness), Some(session_id)) => Some(format!("worker:{harness}:{session_id}")),
        _ => Some(worker.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_receipt_recovers_the_session_after_the_registry_name_is_gone() {
        let raw = r#"{"type":"agent_spawned","data":{"name":"old-name","provider":"codex","harness_session_id":"full-session","cwd":"/repo","model":"gpt-5.6-sol","substrate":"pane"}}"#;
        let receipts = parse_spawn_receipts(raw);
        let receipt = receipts
            .get(&(String::from("codex"), String::from("full-session")))
            .expect("full receipt");
        assert_eq!(receipt.name, "old-name");
        assert_eq!(receipt.harness, "codex");
        assert_eq!(receipt.cwd, "/repo");
    }

    #[test]
    fn spawn_receipt_accepts_thread_and_one_release_bg_alias() {
        let raw = concat!(
            r#"{"type":"agent_spawned","data":{"name":"thread-worker","provider":"codex","harness_session_id":"thread-session","substrate":"thread"}}"#,
            "\n",
            r#"{"type":"agent_spawned","data":{"name":"legacy-worker","provider":"claude","harness_session_id":"legacy-session","substrate":"bg"}}"#,
        );
        let receipts = parse_spawn_receipts(raw);
        assert_eq!(receipts.len(), 2);
        assert_eq!(
            receipts
                .get(&(String::from("codex"), String::from("thread-session")))
                .map(|receipt| receipt.harness.as_str()),
            Some("codex")
        );
        assert_eq!(
            receipts
                .get(&(String::from("claude"), String::from("legacy-session")))
                .map(|receipt| receipt.harness.as_str()),
            Some("claude")
        );
    }

    #[test]
    fn spawn_receipts_keep_same_session_ids_distinct_across_harnesses() {
        let raw = concat!(
            r#"{"type":"agent_spawned","data":{"name":"codex-worker","provider":"codex","harness_session_id":"same-session","substrate":"thread"}}"#,
            "\n",
            r#"{"type":"agent_spawned","data":{"name":"claude-worker","provider":"claude","harness_session_id":"same-session","substrate":"thread"}}"#,
            "\n",
            r#"{"type":"agent_removed","data":{"name":"codex-worker","provider":"codex","harness_session_id":"same-session"}}"#,
        );
        let receipts = parse_spawn_receipts(raw);
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            receipts
                .get(&(String::from("claude"), String::from("same-session")))
                .map(|receipt| receipt.name.as_str()),
            Some("claude-worker")
        );
    }

    #[test]
    fn incomplete_removal_event_cannot_revoke_a_reused_name() {
        let raw = concat!(
            r#"{"type":"agent_spawned","data":{"name":"reused","provider":"codex","harness_session_id":"old-session","substrate":"thread"}}"#,
            "\n",
            r#"{"type":"agent_spawned","data":{"name":"reused","provider":"claude","harness_session_id":"new-session","substrate":"thread"}}"#,
            "\n",
            r#"{"type":"agent_removed","data":{"name":"reused","harness_session_id":"old-session"}}"#,
        );
        let receipts = parse_spawn_receipts(raw);
        assert_eq!(receipts.len(), 2);
    }

    /// A throwaway journal directory that never touches the operator's
    /// registry location (same per-test isolation rule as StoreScratch).
    struct JournalScratch(std::path::PathBuf);
    impl JournalScratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("fno-journal-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            JournalScratch(dir)
        }
        fn segment(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for JournalScratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn spawn_receipt_segments_read_oldest_generation_first() {
        // Rotation renames the live file to `.1`, so a higher generation is
        // older; the live file parses LAST.
        let s = JournalScratch::new("segments");
        for name in ["events.jsonl.2", "events.jsonl.1", "events.jsonl"] {
            std::fs::write(s.segment(name), "").unwrap();
        }
        let paths = spawn_receipt_segments(&s.0, "events.jsonl");
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Only the retained `.N` segments; the live file is appended by the
        // scan itself, always last.
        assert_eq!(names, vec!["events.jsonl.2", "events.jsonl.1"]);
    }

    #[test]
    fn journal_scan_crosses_rotation_and_honors_live_file_revocation() {
        // AC1-HP: a receipt in a rotated segment is still a receipt. AC2-HP:
        // its revocation in the LIVE file still lands on it - segments are
        // concatenated oldest first, so parse's revoke-in-file-order keeps
        // applying. Reading newest first would resurrect the reaped session.
        let s = JournalScratch::new("rotation");
        std::fs::write(
                s.segment("events.jsonl.2"),
                r#"{"type":"agent_spawned","data":{"name":"ancient","provider":"codex","harness_session_id":"ancient-session","substrate":"pane"}}"#,
            )
            .unwrap();
        std::fs::write(
                s.segment("events.jsonl.1"),
                r#"{"type":"agent_spawned","data":{"name":"rotated","provider":"codex","harness_session_id":"rotated-session","substrate":"pane"}}"#,
            )
            .unwrap();
        std::fs::write(
                s.segment("events.jsonl"),
                concat!(
                    r#"{"type":"agent_removed","data":{"name":"rotated","provider":"codex","harness_session_id":"rotated-session"}}"#,
                    "\n",
                    r#"{"type":"agent_spawned","data":{"name":"live","provider":"codex","harness_session_id":"live-session","substrate":"pane"}}"#,
                ),
            )
            .unwrap();
        let journal = scan_journal_at(&s.segment("events.jsonl"));
        assert!(journal.error.is_none());
        assert!(
            journal
                .receipts
                .contains_key(&(String::from("codex"), String::from("ancient-session"))),
            "AC1-HP: the rotated segment's receipt survives"
        );
        assert!(
            !journal
                .receipts
                .contains_key(&(String::from("codex"), String::from("rotated-session"))),
            "AC2-HP: the live file's revocation lands on the rotated receipt"
        );
        assert!(journal
            .receipts
            .contains_key(&(String::from("codex"), String::from("live-session"))));
    }

    #[test]
    fn journal_scan_surfaces_an_unreadable_segment_and_keeps_readable_receipts() {
        // AC3-EDGE: unreadable is not missing. The unreadable segment's error
        // surfaces; the readable segments still contribute their receipts.
        let s = JournalScratch::new("unreadable");
        std::fs::create_dir_all(s.segment("events.jsonl.1")).unwrap();
        std::fs::write(
                s.segment("events.jsonl"),
                r#"{"type":"agent_spawned","data":{"name":"rescued","provider":"codex","harness_session_id":"rescued-session","substrate":"pane"}}"#,
            )
            .unwrap();
        let journal = scan_journal_at(&s.segment("events.jsonl"));
        assert!(
            journal.error.as_deref().unwrap().contains("events.jsonl.1"),
            "the unreadable segment is named, not folded into no-receipt"
        );
        assert!(journal
            .receipts
            .contains_key(&(String::from("codex"), String::from("rescued-session"))));
    }

    #[test]
    fn never_bound_markers_come_only_from_sessionless_registry_removals() {
        let raw = concat!(
            // The live marker shape (t-a0cd-identity-residue-gpt):
            r#"{"type":"registry_row_removed","data":{"name":"residue","harness":"codex","harness_session_id":"","reason":"row 'residue' carries no resumable identity (harness='codex', session=False)"}}"#,
            "\n",
            // The daemon shape (t-b783-verb-prefix-agy):
            r#"{"type":"registry_row_removed","data":{"name":"verb-prefix","harness":"agy","harness_session_id":"","reason":"missing harness session identity"}}"#,
            "\n",
            // A row WITH a session id is a normal removal, never a name marker:
            r#"{"type":"registry_row_removed","data":{"name":"normal","harness":"claude","harness_session_id":"has-session","reason":"missing harness session identity"}}"#,
            "\n",
            // Generic write bookkeeping names no identity gap:
            r#"{"type":"registry_row_removed","data":{"name":"write","harness":"claude","harness_session_id":"","reason":"removed by an update_registry write"}}"#,
            "\n",
            // py3probe's shape: an agent_removed with a null session is NOT a
            // never-bound marker - probe workers stay Unknown and unswept.
            r#"{"type":"agent_removed","data":{"name":"py3probe","harness":"python3","harness_session_id":null}}"#,
            "\n",
        );
        let markers = parse_never_bound_removals(raw);
        assert_eq!(
            markers.get("residue").map(String::as_str),
            Some("row 'residue' carries no resumable identity (harness='codex', session=False)")
        );
        assert_eq!(
            markers.get("verb-prefix").map(String::as_str),
            Some("missing harness session identity")
        );
        assert!(!markers.contains_key("normal"));
        assert!(!markers.contains_key("write"));
        assert!(!markers.contains_key("py3probe"));
    }

    #[test]
    fn never_bound_marker_requires_the_removal_newer_than_the_last_spawn() {
        // AC6-EDGE: a name respawned after its removal is live again.
        let respawned = concat!(
            r#"{"type":"agent_spawned","data":{"name":"w","provider":"codex","harness_session_id":"s1","substrate":"pane"}}"#,
            "\n",
            r#"{"type":"registry_row_removed","data":{"name":"w","harness_session_id":"","reason":"missing harness session identity"}}"#,
            "\n",
            r#"{"type":"agent_spawned","data":{"name":"w","provider":"codex","harness_session_id":"s2","substrate":"pane"}}"#,
            "\n",
        );
        assert!(parse_never_bound_removals(respawned).is_empty());
        let removed_last = concat!(
            r#"{"type":"agent_spawned","data":{"name":"v","provider":"codex","harness_session_id":"s1","substrate":"pane"}}"#,
            "\n",
            r#"{"type":"registry_row_removed","data":{"name":"v","harness_session_id":"","reason":"missing harness session identity"}}"#,
            "\n",
        );
        assert_eq!(
            parse_never_bound_removals(removed_last)
                .get("v")
                .map(String::as_str),
            Some("missing harness session identity")
        );
    }
}

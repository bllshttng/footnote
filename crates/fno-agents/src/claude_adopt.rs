//! Transcript reads, the manifest line reader, and the adopted-row registry
//! write for claude workers.
//!
//! The live heal path in `client_verbs` mints its rows directly; this module
//! carries what stays shared: the transcript readers (`transcript_stamp`,
//! `transcript_model`, `provider_from_route_settings`), the manifest line
//! reader (`manifest_field`), and `upsert_adopted_row`. The claim helpers
//! (`pty_claim_holder`, `acquire_pty_claim`) answer the `pty:<short_id>`
//! single-writer claim for an adopted session. The claim is anchored to the
//! long-lived holder pid from the first acquire, so it is live from birth.
//! The `session:<uuid>` key routes to the host-global claims root, so two
//! checkouts cannot take separate project-local claims for the same session.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::state::{update_registry, RegistryEntry, StateError};

/// The single-writer claim holder for an adopted session: `pty:<short_id>`. The
/// claimed RESOURCE is `session:<uuid>` (the durable session identity); the holder
/// string names WHO holds it. Matches the daemon's interactive-claim holder.
pub fn pty_claim_holder(short_id: &str) -> String {
    format!("pty:{short_id}")
}

/// The registry `name` for an adopted session: `cc-<short_id>`. Stable and
/// derivable from the roster, so re-adopting the same session upserts one row.
pub fn adopted_name(short_id: &str) -> String {
    format!("cc-{short_id}")
}

/// Read the transcript's last filesystem activity before publishing an adopt
/// row. A missing or unreadable transcript is unknown, never "now".
pub fn transcript_activity(session_id: &str) -> Option<(String, u64)> {
    let path = crate::claude_drive::find_transcript(session_id)?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = std::time::SystemTime::now()
        .duration_since(modified)
        .ok()?
        .as_secs();
    let stamp = chrono::DateTime::<chrono::Utc>::from(modified)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    Some((stamp, age))
}

/// The adopted row's `last_message_at` stamp: the truth probe's
/// `last_event_at`, the NEWEST TIMESTAMPED transcript entry. The file mtime
/// overstates liveness, never understates it - trailing untimestamped records
/// keep the file young while the conversation is silent - so the
/// mtime reading is only the fallback for a session the probe cannot resolve.
/// The existence question stays on [`transcript_activity`]: a probe that
/// fails to answer must not read as "no transcript".
pub fn transcript_stamp(session_id: &str) -> Option<String> {
    let probed =
        crate::truth_probe::family1_truth_probe(session_id).and_then(|probe| probe.last_event_at);
    transcript_stamp_from_probe(session_id, probed)
}

/// [`transcript_stamp`] with the probe answer injected, so the precedence is
/// unit-testable without shelling the truth probe.
fn transcript_stamp_from_probe(session_id: &str, probed: Option<String>) -> Option<String> {
    probed.or_else(|| transcript_activity(session_id).map(|(stamp, _)| stamp))
}

/// The model the session is actually running, read from its transcript.
/// The LAST `message.model` on the file wins: a session can be switched
/// mid-run, and the most recent value is the only one that answers "what is
/// this worker running now". `None` for a missing/unreadable transcript or a
/// session that has stated no model yet - an absence, never a guess.
pub fn transcript_model(session_id: &str) -> Option<String> {
    let path = crate::claude_drive::find_transcript(session_id)?;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut model = None;
    for line in reader.lines().map_while(Result::ok) {
        // Parse lazily per line; a transcript is append-only JSONL and the
        // model lives at ["message"]["model"] on assistant entries.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(m) = v
            .get("message")
            .and_then(|m| m.get("model"))
            .and_then(|m| m.as_str())
        {
            // A synthetic tail turn is not the running model; the last real
            // one is.
            if !m.is_empty() && m != crate::resume_pin::SYNTHETIC_MODEL {
                model = Some(m.to_string());
            }
        }
    }
    model
}

/// The thread title an adopt names its row after: the transcript's FIRST
/// `summary` entry. `None` for a missing/unreadable transcript or a session
/// that stated no summary yet - the caller falls through to the next naming
/// source, never to a guess.
pub fn transcript_title(session_id: &str) -> Option<String> {
    transcript_title_in(&crate::claude_drive::claude_projects_dir(), session_id)
}

/// The synthesized registry row's display name: the thread title when the
/// transcript carries one (capped, so a long title still fits the sideline
/// cell), else the linked node id, else the derivable `t-<short>` form
/// (`t-` is the bridge's manual form: no provenance). A bare short id reads
/// as a phantom row, never as work.
pub fn synthesized_entry_name(session: &str, fno_id: &str, short: &str) -> String {
    transcript_title(session)
        .map(|t| t.chars().take(48).collect::<String>())
        .or_else(|| (!fno_id.is_empty()).then(|| fno_id.to_string()))
        .unwrap_or_else(|| format!("t-{short}"))
}

/// [`transcript_title`] under an explicit projects base, so the read is
/// unit-testable without touching the ambient `~/.claude`.
pub fn transcript_title_in(base: &Path, session_id: &str) -> Option<String> {
    let path = crate::claude_drive::find_transcript_in(base, session_id)?;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok) {
        // A transcript is append-only JSONL; summaries live on their own
        // `type: "summary"` entries and the first one is the thread's title.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("summary") {
            if let Some(title) = v
                .get("summary")
                .and_then(|s| s.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(title.to_string());
            }
        }
    }
    None
}

/// The model-provider this session's observed model is recorded to run on,
/// matched against `~/.fno/route-settings/*.json`. The file's
/// `FNO_ROUTE_PROVIDER` stamp is the source - the observed model only SELECTS
/// which recorded routes to consult, so this is a lookup, never the barred
/// derive-provider-from-model-string inference. `None` when no file matches or
/// the matches disagree: a row with no real provider source records none.
///
/// `FNO_ROUTE_SETTINGS_DIR` overrides the directory for tests.
pub fn provider_from_route_settings(model: Option<&str>) -> Option<String> {
    let model = model.filter(|m| !m.is_empty())?;
    let dir = std::env::var_os("FNO_ROUTE_SETTINGS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".fno")
                .join("route-settings")
        });
    let entries = std::fs::read_dir(dir).ok()?;
    let mut providers: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(env) = v.get("env").and_then(|e| e.as_object()) else {
            continue;
        };
        let file_model = env.get("ANTHROPIC_MODEL").and_then(|m| m.as_str());
        if file_model != Some(model) {
            continue;
        }
        let provider = env
            .get("FNO_ROUTE_PROVIDER")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty());
        if let Some(p) = provider {
            if !providers.iter().any(|x| x == p) {
                providers.push(p.to_string());
            }
        }
    }
    if providers.len() == 1 {
        providers.pop()
    } else {
        None
    }
}

/// One frontmatter scalar from a king manifest: `key: value`, quotes stripped,
/// empty reads as absent. The line-scan idiom `cleanup_king_manifest` uses; a
/// full parser lives in loopcheck (shrink-only) and carries no crown fields.
pub(crate) fn manifest_field(content: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    content
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix))
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
}

/// Upsert an adopted row into `registry.json`, keyed by the full
/// `claude_session_uuid` (the row identity), replacing in place or pushing.
/// Idempotent: re-adopting the same session refreshes the row rather than
/// duplicating it.
pub fn upsert_adopted_row(registry_path: &Path, entry: RegistryEntry) -> Result<(), StateError> {
    update_registry(registry_path, |reg| {
        // Find the row index by the session uuid first (the borrow of `entry`
        // ends here), then move `entry` into place -- no clone of the key.
        let key = entry.claude_session_uuid.as_deref();
        let idx = key.and_then(|k| {
            reg.entries
                .iter()
                .position(|e| e.claude_session_uuid.as_deref() == Some(k))
        });
        match idx {
            Some(i) => {
                // `delivery_policy` is a stamp the SESSION declared
                // about itself; the adopt path does not own it, so a refresh
                // carries it forward instead of reverting the row to the
                // injectable default (a re-adopted leader must stay bus-only).
                let policy = reg.entries[i].delivery_policy.clone();
                // x-aaaa: same for `node` - adoption observed nothing about
                // the node, so replacing the row must not erase one a spawn
                // or register path stamped.
                let node = reg.entries[i].node.clone();
                // The birth context is what adoption must
                // PRESERVE, not rewrite. Origin, the parent edge, the door's
                // structured provenance, the requested axes, and the
                // predecessor/fork edges all belong to the row's birth; the
                // fresh mint observed none of them. Only the adopter's own
                // voucher (adopted_by_session) refreshes.
                let prev = reg.entries[i].clone();
                reg.entries[i] = entry;
                reg.entries[i].delivery_policy = policy;
                if reg.entries[i].node.is_none() {
                    reg.entries[i].node = node;
                }
                reg.entries[i].origin = prev.origin;
                reg.entries[i].spawned_by_session = prev.spawned_by_session;
                reg.entries[i].spawned_by_harness = prev.spawned_by_harness;
                reg.entries[i].spawned_by_cwd = prev.spawned_by_cwd;
                reg.entries[i].lineage_reason = prev.lineage_reason;
                reg.entries[i].spawn_id = prev.spawn_id;
                reg.entries[i].spawn_provenance = prev.spawn_provenance;
                reg.entries[i].requested_model = prev.requested_model;
                reg.entries[i].requested_provider = prev.requested_provider;
                reg.entries[i].requested_effort = prev.requested_effort;
                reg.entries[i].forked_from_session_id = prev.forked_from_session_id;
                reg.entries[i].predecessor_session_ids = prev.predecessor_session_ids;
                reg.entries[i].created_at = prev.created_at;
            }
            None => reg.entries.push(entry),
        }
    })
}

/// Outcome of acquiring the `pty:<short_id>` single-writer claim.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    /// We hold `session:<uuid>` (fresh acquire or idempotent re-acquire).
    Acquired,
    /// Another live writer holds it; refuse to double-adopt (AC1-EDGE).
    HeldByOther(String),
    /// The claim substrate could not be consulted (io / validation error from
    /// the native acquire). Fail OPEN -- the file-claim is the cross-process
    /// coordination record, best-effort like the daemon's.
    Unavailable(String),
}

/// Acquire the `session:<uuid>` claim for `holder`, anchored to `holder_pid`
/// (the long-lived attach holder). Native `crate::claims` call — no subprocess.
/// Pinning `--pid` to the long-lived holder from the very first acquire is what
/// keeps the claim from being born `stale`; with the native path there is no
/// transient `fno agents claim` subprocess to record in the first place, but the
/// explicit holder pid is preserved so the record still names the real writer
/// (codex P1). `session:<uuid>` keys route to the host-global claims root, so
/// two checkouts cannot take separate project-local claims for the same
/// session. Fails OPEN (`Unavailable`) on an unconsultable substrate.
pub fn acquire_pty_claim(uuid: &str, holder: &str, holder_pid: u32) -> ClaimOutcome {
    match crate::claims::acquire(
        &format!("session:{uuid}"),
        holder,
        crate::claims::AcquireOpts {
            pid: Some(holder_pid),
            ..Default::default()
        },
    ) {
        crate::claims::AcquireOutcome::Acquired(_) => ClaimOutcome::Acquired,
        crate::claims::AcquireOutcome::HeldByOther { holder, .. } => {
            ClaimOutcome::HeldByOther(holder)
        }
        crate::claims::AcquireOutcome::Error(e) => ClaimOutcome::Unavailable(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_roster::RosterWorker;
    use crate::state::{Lineage, HOST_MODE_ATTACHED};
    use crate::AgentStatus;

    fn worker() -> RosterWorker {
        RosterWorker {
            session_id: "a1b2c3d4-1111-2222-3333-444455556666".into(),
            pid: Some(5001),
            repl_pid: None,
            proc_start: Some(99887766),
            pty_sock: Some("/tmp/cc-daemon-501/deadbeef/spare/a1b2c3d4.pty.sock".into()),
            pty_auth: Some("cccc3333dddd4444".into()),
            cli_version: Some("2.1.195".into()),
            cwd: "/Users/x/code/proj".into(),
            worktree_path: None,
        }
    }

    /// The mint the retired adopt chain produced, reduced to what the upsert
    /// merge tests need: one claude attached row keyed by the session uuid.
    fn fixture_entry(now: &str) -> RegistryEntry {
        let w = worker();
        RegistryEntry {
            name: adopted_name(&w.short_id()),
            harness: Some("claude".into()),
            host_mode: Some(HOST_MODE_ATTACHED.into()),
            status: AgentStatus::Live,
            last_message_at: Some(now.to_string()),
            created_at: now.to_string(),
            cwd: w.cwd.clone(),
            pid: w.pid,
            pid_start_time: w.proc_start,
            short_id: w.short_id().to_string(),
            claude_session_uuid: Some(w.session_id.clone()),
            ..RegistryEntry::new(
                Some(w.session_id.clone()),
                Lineage::captured((None, None, None)),
            )
        }
    }

    #[test]
    fn transcript_title_reads_the_first_summary_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let uuid = "a1b2c3d4-1111-2222-3333-444455556666";
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(format!("{uuid}.jsonl")),
            concat!(
                r#"{"type":"user","message":{"role":"user"}}"#,
                "\n",
                r#"{"type":"summary","summary":"Fix the sideline phantom rows","leafUuid":"x"}"#,
                "\n",
                r#"{"type":"summary","summary":"A later summary never wins","leafUuid":"y"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(
            transcript_title_in(tmp.path(), uuid).as_deref(),
            Some("Fix the sideline phantom rows"),
            "the first summary entry is the thread title"
        );
        // A transcript with no summary answers nothing: the caller falls
        // through to the next naming source, never to a guess.
        let bare = tmp.path().join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(
            bare.join(format!("{uuid}.jsonl")),
            r#"{"type":"user","message":{"role":"user"}}"#,
        )
        .unwrap();
        assert_eq!(transcript_title_in(bare.as_path(), uuid), None);
        // A missing transcript answers nothing too.
        assert_eq!(
            transcript_title_in(tmp.path(), "b1c2d3e4-1111-2222-3333-444455556666"),
            None
        );
    }

    #[test]
    fn holder_and_name_formats() {
        assert_eq!(pty_claim_holder("a1b2c3d4"), "pty:a1b2c3d4");
        assert_eq!(adopted_name("a1b2c3d4"), "cc-a1b2c3d4");
    }

    #[test]
    fn synthesized_entry_name_prefers_title_then_node_then_short_form() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let uuid = "a1b2c3d4-1111-2222-3333-444455556666";
        // The transcript title wins, capped at 48 chars.
        let long = "x".repeat(80);
        seed_transcript(
            "entry-name",
            uuid,
            &[format!(r#"{{"type":"summary","summary":"{long}"}}"#)],
        );
        let named = synthesized_entry_name(uuid, "x-e4b0", "a1b2c3d4");
        assert_eq!(named.chars().count(), 48, "the title is capped");
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        // No title: the linked node id names the row.
        assert_eq!(synthesized_entry_name(uuid, "x-e4b0", "a1b2c3d4"), "x-e4b0");
        // Neither: the derivable t- form (the bridge's manual form).
        assert_eq!(synthesized_entry_name(uuid, "", "a1b2c3d4"), "t-a1b2c3d4");
    }

    #[test]
    fn transcript_activity_does_not_fabricate_missing_files() {
        assert_eq!(transcript_activity("not-a-session"), None);
    }

    #[test]
    fn transcript_stamp_prefers_the_probed_entry_over_mtime() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let uuid = "a1b2c3d4-1111-2222-3333-444455556666";
        let base = seed_transcript("stamp-probe", uuid, &[transcript_line("glm-5.3")]);
        // The probe answered: its stamp wins no matter how fresh the file stat
        // is, because the stat rides untimestamped trailing records.
        assert_eq!(
            transcript_stamp_from_probe(uuid, Some("2030-01-01T00:00:00Z".into())),
            Some("2030-01-01T00:00:00Z".into())
        );
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn transcript_stamp_falls_back_to_mtime_when_the_probe_is_silent() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let uuid = "b2c3d411-1111-2222-3333-444455556666";
        let base = seed_transcript("stamp-fallback", uuid, &[transcript_line("glm-5.3")]);
        let stamp = transcript_stamp_from_probe(uuid, None);
        assert!(stamp.is_some(), "the mtime fallback answers when it can");
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::fs::remove_dir_all(&base).ok();
    }

    /// Write a minimal transcript for `uuid` under a fresh temp projects dir
    /// and point `FNO_CLAUDE_PROJECTS_DIR` at it. The transcript lives one
    /// project dir down, the shape `find_transcript` scans. Returns the dir.
    fn seed_transcript(dir_tag: &str, uuid: &str, lines: &[String]) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "fno-adopt-{dir_tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let proj = base.join("-Users-bb16-code-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join(format!("{uuid}.jsonl")), lines.join("\n") + "\n").unwrap();
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, &base);
        base
    }

    fn transcript_line(model: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","model":"{model}"}},"timestamp":"2026-08-31T00:00:00Z"}}"#
        )
    }

    #[test]
    fn transcript_model_reads_the_stated_model() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let uuid = "a1b2c3d4-1111-2222-3333-444455556666";
        let base = seed_transcript("model", uuid, &[transcript_line("glm-5.3-flash[1m]")]);
        assert_eq!(transcript_model(uuid).as_deref(), Some("glm-5.3-flash[1m]"));
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn transcript_model_takes_the_most_recent_value_when_a_session_switched() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // A session can be switched mid-run; the LAST stated model is the one
        // answering what the worker is running now.
        let uuid = "a1b2c3d4-1111-2222-3333-444455556666";
        let base = seed_transcript(
            "model-switch",
            uuid,
            &[
                transcript_line("glm-5.3"),
                transcript_line("glm-5.3-flash[1m]"),
            ],
        );
        assert_eq!(transcript_model(uuid).as_deref(), Some("glm-5.3-flash[1m]"));
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn transcript_model_skips_a_synthetic_tail_turn() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // AC2-HP: a synthetic tail turn is not the running model; the last
        // real model answers.
        let uuid = "a1b2c3d4-1111-2222-3333-444455557777";
        let base = seed_transcript(
            "model-synthetic-tail",
            uuid,
            &[
                transcript_line("glm-5.3-flash[1m]"),
                transcript_line(crate::resume_pin::SYNTHETIC_MODEL),
            ],
        );
        assert_eq!(transcript_model(uuid).as_deref(), Some("glm-5.3-flash[1m]"));
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn transcript_model_records_none_for_a_missing_transcript() {
        // Absence is None, never a fabricated value - the same discipline as
        // transcript_activity.
        assert_eq!(transcript_model("not-a-uuid"), None);
    }

    fn seed_route_settings(dir_tag: &str, files: &[&str]) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "fno-adopt-{dir_tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        for (i, provider) in files.iter().enumerate() {
            std::fs::write(
                base.join(format!("{i:04}.json")),
                format!(
                    r#"{{"env":{{"ANTHROPIC_MODEL":"glm-5.3-flash[1m]","ANTHROPIC_BASE_URL":"https://api.example.test","FNO_ROUTE_PROVIDER":"{provider}"}}}}"#
                ),
            )
            .unwrap();
        }
        std::env::set_var("FNO_ROUTE_SETTINGS_DIR", &base);
        base
    }

    #[test]
    fn provider_matches_the_recorded_route_files() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // The observed model SELECTS the recorded routes; the provider comes
        // from the file's FNO_ROUTE_PROVIDER stamp - a lookup, never the
        // barred derive-from-model-string inference.
        let base = seed_route_settings("route-one", &["zai"]);
        assert_eq!(
            provider_from_route_settings(Some("glm-5.3-flash[1m]")).as_deref(),
            Some("zai")
        );
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn provider_records_none_when_routes_disagree() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Two recorded providers for the same model is genuine ambiguity:
        // guessing here is how the wrong bill gets paid.
        let base = seed_route_settings("route-ambig", &["zai", "anthropic"]);
        assert_eq!(
            provider_from_route_settings(Some("glm-5.3-flash[1m]")),
            None
        );
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn provider_records_none_without_a_real_source() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // No matching file: the row records no provider rather than deriving
        // one from the model string. A model alone must never name a vendor.
        let base = seed_route_settings("route-none", &["zai"]);
        assert_eq!(provider_from_route_settings(Some("claude-sonnet-5")), None);
        assert_eq!(provider_from_route_settings(None), None);
        assert_eq!(provider_from_route_settings(Some("")), None);
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn upsert_refresh_keeps_a_stamped_node() {
        let _root = crate::paths::DeclaredRoot::declare("upsert_refresh_keeps_a_stamp");
        // A spawn-stamped node survives a re-adopt: adoption observed nothing
        // about the node, so replacing the row must not erase the stamp.
        let dir = std::env::temp_dir().join(format!(
            "fno-adopt-node-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");
        let mut stamped = fixture_entry("2026-06-27T17:00:00Z");
        stamped.node = Some("x-aaaa".into());
        upsert_adopted_row(&reg, stamped).unwrap();
        upsert_adopted_row(&reg, fixture_entry("2026-06-27T18:00:00Z")).unwrap();
        let loaded = crate::state::load_registry(&reg).unwrap();
        assert_eq!(loaded.entries[0].node.as_deref(), Some("x-aaaa"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upsert_refresh_preserves_birth_provenance() {
        // A re-adopt is an observed-field merge.
        // The door's structured provenance, the spawn id, the birth parent
        // edge, and the requested axes all belong to the row's BIRTH, so a
        // whole-row replace that dropped them is the defect this test pins.
        // The one field that DOES refresh is the new adopter's voucher.
        let _root = crate::paths::DeclaredRoot::declare("upsert_birth_provenance");
        let dir = std::env::temp_dir().join(format!(
            "fno-adopt-birth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");
        // Birth: a door-shaped spawn row.
        let provenance = crate::spawn_contract::SpawnProvenance {
            origin: crate::spawn_contract::SpawnOrigin::Session {
                parent: crate::spawn_contract::SessionRef {
                    harness: "claude".into(),
                    session_id: "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9".into(),
                    cwd: "/repo".into(),
                },
                invocation: None,
            },
            owner: crate::spawn_contract::SpawnOwner::Session(crate::spawn_contract::SessionRef {
                harness: "claude".into(),
                session_id: "0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9".into(),
                cwd: "/repo".into(),
            }),
        };
        let mut born = crate::state::RegistryEntry::new_spawn("sp-abc123", &provenance);
        born.name = "adopted-ab12cd34".into();
        born.claude_session_uuid = Some("a1b2c3d4-1111-2222-3333-444455556666".into());
        born.harness = Some("claude".into());
        born.cwd = "/work".into();
        born.requested_model = Some("glm-5.3-flash[1m]".into());
        upsert_adopted_row(&reg, born).unwrap();
        // Re-adopt mints a fresh adopted row for the same session uuid.
        upsert_adopted_row(&reg, fixture_entry("2026-06-27T18:00:00Z")).unwrap();
        let loaded = crate::state::load_registry(&reg).unwrap();
        assert_eq!(loaded.entries.len(), 1, "same uuid, one row");
        let row = &loaded.entries[0];
        assert_eq!(
            row.spawn_id.as_deref(),
            Some("sp-abc123"),
            "spawn id survives"
        );
        assert!(
            row.spawn_provenance.is_some(),
            "structured provenance survives"
        );
        assert_eq!(
            row.spawned_by_session.as_deref(),
            Some("0f0e7865-86b8-4a9e-8d99-6bd94b0ea9c9"),
            "birth parent edge survives"
        );
        assert_eq!(
            row.requested_model.as_deref(),
            Some("glm-5.3-flash[1m]"),
            "requested axes survive"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upsert_replaces_by_session_uuid() {
        let _root = crate::paths::DeclaredRoot::declare("upsert_replaces_by_session_u");
        let dir = std::env::temp_dir().join(format!(
            "fno-adopt-upsert-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");

        let e1 = fixture_entry("2026-06-27T17:00:00Z");
        upsert_adopted_row(&reg, e1).unwrap();
        // Second adopt of the SAME session refreshes the row, not duplicates it.
        let mut e2 = fixture_entry("2026-06-27T18:00:00Z");
        e2.cwd = "/Users/x/code/moved".into();
        upsert_adopted_row(&reg, e2).unwrap();

        let loaded = crate::state::load_registry(&reg).unwrap();
        let rows: Vec<_> = loaded
            .entries
            .iter()
            .filter(|e| {
                e.claude_session_uuid.as_deref() == Some("a1b2c3d4-1111-2222-3333-444455556666")
            })
            .collect();
        assert_eq!(rows.len(), 1, "upsert must not duplicate");
        assert_eq!(rows[0].cwd, "/Users/x/code/moved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn upsert_refresh_carries_a_declared_delivery_policy_forward() {
        let _root = crate::paths::DeclaredRoot::declare("upsert_refresh_carries_a_dec");
        // the replace path swaps the WHOLE row for a fresh mint, which
        // would silently revert a session's self-declared bus-only stamp to
        // injectable on re-adopt -- the delivery defect again, one adopt later.
        let dir = std::env::temp_dir().join(format!(
            "fno-adopt-policy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");

        let mut stamped = fixture_entry("2026-06-27T17:00:00Z");
        stamped.delivery_policy = Some("bus-only".into());
        upsert_adopted_row(&reg, stamped).unwrap();
        // A later re-adopt of the SAME session mints a fresh default row.
        upsert_adopted_row(&reg, fixture_entry("2026-06-27T18:00:00Z")).unwrap();

        let loaded = crate::state::load_registry(&reg).unwrap();
        assert_eq!(
            loaded.entries[0].delivery_policy.as_deref(),
            Some("bus-only"),
            "a re-adopt must not revert a declared delivery policy"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn acquire_pty_claim_anchors_to_holder_pid_and_maps_outcomes() {
        // The native acquire records the given holder pid immediately (no
        // transient fno subprocess, no stale window, codex P1) and maps the
        // native outcome onto ClaimOutcome.
        let td = std::env::temp_dir().join(format!(
            "fno-adopt-claim-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&td).unwrap();
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FNO_CLAIMS_ROOT", &td);
        // Fresh acquire pinned to a live pid (our own, so it classifies live).
        let me = std::process::id();
        assert_eq!(
            acquire_pty_claim("uuid-1", "pty:a1b2c3d4", me),
            ClaimOutcome::Acquired
        );
        let (_, rec) = crate::claims::status("session:uuid-1", None);
        assert_eq!(rec.unwrap().pid, Some(me as i32));
        // A different holder against the live claim -> HeldByOther.
        assert_eq!(
            acquire_pty_claim("uuid-1", "pty:other", me),
            ClaimOutcome::HeldByOther("pty:a1b2c3d4".into())
        );
        std::env::remove_var("FNO_CLAIMS_ROOT");
        std::fs::remove_dir_all(&td).ok();
    }
}

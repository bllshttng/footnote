//! Adopt an externally-spawned `claude --bg` worker into the fno registry.
//!
//! G1 held-attach substrate (epic x-07c1, node x-26df). Adoption is what makes an
//! external Claude session reachable by the rest of footnote: it mints an fno
//! registry row (so grid/relay can `resolve_worker_short_id` it) and takes the
//! single-writer `pty:<short_id>` claim (so two writers can't drive one session).
//! The roster read is [`crate::claude_roster`]; the held attach is
//! [`crate::claude_attach`].
//!
//! The claim is **anchored to the long-lived HOLDER pid from the first acquire**
//! (footnote's attach-holder process), so it is live from birth. The daemon's
//! historical shell-out recorded the transient `fno agents claim` subprocess pid, so
//! the claim went instantly `stale` and a concurrent adopter could reclaim it
//! (the daemon-claim-reanchor lesson, PR#53; codex P1 on this PR); the native
//! acquire records the holder pid directly and closes that window. The
//! `session:<uuid>` key routes to the host-global claims root, so two checkouts
//! cannot take separate project-local claims for the same session.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::claude_roster::RosterWorker;
use crate::state::{update_registry, RegistryEntry, StateError, HOST_MODE_ATTACHED};
use crate::AgentStatus;

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

/// The model the session is actually running, read from its transcript (x-98ab).
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
            if !m.is_empty() {
                model = Some(m.to_string());
            }
        }
    }
    model
}

/// The model-provider this session's observed model is recorded to run on,
/// matched against `~/.fno/route-settings/*.json` (x-98ab). The file's
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

/// Build the registry row for an adopted held session. Pure (the `now` stamp is
/// injected) so the row shape is asserted without a clock or a live spawn.
/// `host_mode = "attached"` distinguishes it from a footnote-spawned interactive
/// PTY; `claude_session_uuid` is the full resume key AND the row's identity,
/// `pid`/`pid_start_time` are the EXTERNAL claude worker's (for reuse-detection).
///
/// footnote-side addressing keys on the full `claude_session_uuid`, but since v9
/// the wire-derived 8-hex short (the value the `control.sock` boundary + the
/// `pty:<short>` claim holder use) lives in the unified `short_id` field, same as
/// every other claude transport key. The `pty:<short>` claim holder is computed
/// from the roster worker directly (see [`adopt`]), not from this field, so the
/// storage move does not affect claim/control.sock routing.
pub fn mint_adopted_entry(w: &RosterWorker, now: &str) -> RegistryEntry {
    let short = w.short_id().to_string();
    // The adopting session's ambient identity (x-132c): adoption runs in the
    // session that found the worker, and that session is the best answer the
    // registry can hold for "who is responsible for this row".
    let (parent_session, parent_harness, parent_cwd) = crate::claims::ambient_parent_edge();
    RegistryEntry {
        name: adopted_name(&short),
        // Birth marker: a claude worker found in the roster, not one footnote
        // started. "adopted" is the honest answer and neither other stamp
        // would be: adopt takes in BOTH a session a human started by hand and
        // a footnote /target orphan (that is what `fno_id` below is for), so
        // "operator" and "spawn" would each be a confident value nothing
        // measured. Both watchdog lanes read the word rather than an absence -
        // retire acts only on "spawn", and reap protects "adopted" the same
        // way it protects a row nothing ever stamped.
        origin: Some("adopted".into()),
        // x-98ab: adoption observed nothing about the session's node, so the
        // axis stays unknown - never parsed out of the name.
        node: None,
        // x-d285: adopted, not launched here - HOW this session got its
        // account is unobserved, so the account axis stays unknown (never
        // "default").
        launch_account: None,
        related_session_id: None,
        short_id: short,
        legacy_provider: String::new(),
        // x-98ab: provider is stamped by `adopt` from a REAL source (the
        // route-settings match) or stays None. The old unconditional
        // "anthropic" here was a guess - an adopted claude worker may be
        // running on any routed provider, and a wrong stamp is exactly how a
        // backgrounded session bills the wrong account unobserved.
        provider: None,
        model: None,
        model_basis: None,
        effort: None,
        harness: Some("claude".into()),
        harness_session_id: Some(w.session_id.clone()),
        predecessor_session_ids: Vec::new(),
        forked_from_session_id: None,
        cwd: w.cwd.clone(),
        project_root: w.worktree_path.clone().unwrap_or_else(|| w.cwd.clone()),
        session_id: None,
        claude_session_uuid: Some(w.session_id.clone()),
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        cc_session_id: None,
        host_mode: Some(HOST_MODE_ATTACHED.into()),
        status: AgentStatus::Live,
        last_message_at: Some(now.to_string()),
        created_at: now.to_string(),
        pid: w.pid,
        pid_start_time: w.proc_start,
        keeper_child_pid: None,
        log_path: None,
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
        crown_level: None,
        crown_scope: None,
        crown_grantor: None,
        route_settings_path: None,
        fno_id: None,
        delivery_policy: None,
        sandbox_posture: None,
        spawn_trigger: None,
        spawned_by_session: parent_session,
        spawned_by_harness: parent_harness,
        spawned_by_cwd: parent_cwd,
        legacy_claude_short_id: None,
    }
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
                // x-e21e: `delivery_policy` is a stamp the SESSION declared
                // about itself; the adopt path does not own it, so a refresh
                // carries it forward instead of reverting the row to the
                // injectable default (a re-adopted leader must stay bus-only).
                let policy = reg.entries[i].delivery_policy.clone();
                // x-98ab: same for `node` - adoption observed nothing about
                // the node, so replacing the row must not erase one a spawn
                // or register path stamped.
                let node = reg.entries[i].node.clone();
                reg.entries[i] = entry;
                reg.entries[i].delivery_policy = policy;
                if reg.entries[i].node.is_none() {
                    reg.entries[i].node = node;
                }
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

/// Adopt a roster worker: take the `pty:<short>` single-writer claim ANCHORED to
/// `holder_pid` (the long-lived caller, not the transient `fno agents claim` subprocess)
/// in one acquire, refusing if another live writer holds the session, THEN mint +
/// upsert its fno registry row. The claim is secured before the row is published,
/// so a concurrent adopter cannot reclaim the session in a stale window. Returns
/// the row so the caller can drive it via [`crate::claude_drive`]. No keepalive is
/// taken -- the Phase-0 spike retired the held-attach layer; idle `claude --bg`
/// sessions persist on their own.
///
/// ponytail: live glue over registry io -- not unit-tested here; every composed
/// piece (mint, upsert, native `acquire_pty_claim`) is.
pub fn adopt(
    registry_path: &Path,
    worker: &RosterWorker,
    holder_pid: u32,
) -> Result<RegistryEntry, AdoptError> {
    let short = worker.short_id().to_string();
    let holder = pty_claim_holder(&short);

    // Claim anchored to the holder pid FIRST (no stale window), then publish.
    match acquire_pty_claim(&worker.session_id, &holder, holder_pid) {
        ClaimOutcome::HeldByOther(who) => {
            return Err(AdoptError::HeldByOther(who));
        }
        ClaimOutcome::Acquired | ClaimOutcome::Unavailable(_) => {}
    }

    let mut entry = mint_adopted_entry(worker, &crate::daemon::now_rfc3339_like());
    entry.last_message_at = transcript_activity(&worker.session_id).map(|(stamp, _)| stamp);
    // x-98ab: close the missing-model class at adopt - the transcript states
    // the model outright, so an adopted row stops attesting nothing and the
    // attest-model guard gets its premise. Provider comes only from the
    // route-settings match; with no match it records None rather than a guess.
    if let Some(model) = transcript_model(&worker.session_id) {
        entry.provider = provider_from_route_settings(Some(&model));
        entry.model = Some(model);
        entry.model_basis = Some("verified".to_string());
    }
    upsert_adopted_row(registry_path, entry.clone()).map_err(AdoptError::Registry)?;
    Ok(entry)
}

/// Why an adopt did not complete.
#[derive(Debug)]
pub enum AdoptError {
    /// Another live writer holds the session; refused (AC1-EDGE).
    HeldByOther(String),
    /// The registry write failed.
    Registry(StateError),
}

impl std::fmt::Display for AdoptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdoptError::HeldByOther(who) => write!(f, "session already held by {who}"),
            AdoptError::Registry(e) => write!(f, "registry write failed: {e}"),
        }
    }
}

impl std::error::Error for AdoptError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::HOST_MODE_INTERACTIVE;

    fn worker() -> RosterWorker {
        RosterWorker {
            session_id: "a1b2c3d4-1111-2222-3333-444455556666".into(),
            pid: Some(5001),
            proc_start: Some(99887766),
            pty_sock: Some("/tmp/cc-daemon-501/deadbeef/spare/a1b2c3d4.pty.sock".into()),
            pty_auth: Some("cccc3333dddd4444".into()),
            cli_version: Some("2.1.195".into()),
            cwd: "/Users/x/code/proj".into(),
            worktree_path: None,
        }
    }

    #[test]
    fn holder_and_name_formats() {
        assert_eq!(pty_claim_holder("a1b2c3d4"), "pty:a1b2c3d4");
        assert_eq!(adopted_name("a1b2c3d4"), "cc-a1b2c3d4");
    }

    #[test]
    fn transcript_activity_does_not_fabricate_missing_files() {
        assert_eq!(transcript_activity("not-a-session"), None);
    }

    #[test]
    fn mint_sets_attached_marker_and_resume_key() {
        let e = mint_adopted_entry(&worker(), "2026-06-27T17:00:00Z");
        assert_eq!(e.name, "cc-a1b2c3d4");
        assert_eq!(e.harness_name(), "claude");
        // x-98ab: mint stamps NO provider - the old unconditional "anthropic"
        // was a guess about how the session is routed, and a wrong stamp is
        // how a session bills the wrong account unobserved. `adopt` fills it
        // from the route-settings match or leaves it None.
        assert_eq!(e.provider, None);
        // x-98ab: adoption observed nothing about the node; the field reads
        // unknown, never a value parsed out of the name.
        assert_eq!(e.node, None);
        assert_eq!(e.host_mode.as_deref(), Some("attached"));
        // Addressing identity is the full uuid; since v9 the wire short lives in
        // the unified short_id field (was claude_short_id).
        assert_eq!(
            e.claude_session_uuid.as_deref(),
            Some("a1b2c3d4-1111-2222-3333-444455556666")
        );
        assert_eq!(e.short_id, "a1b2c3d4");
        assert_eq!(e.pid, Some(5001));
        assert_eq!(e.pid_start_time, Some(99887766));
        assert_eq!(e.status, AgentStatus::Live);
    }

    // -- x-98ab: row identity + a sweep that names what it kept --------------

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
    fn adopt_fills_model_and_provider_from_real_sources() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // The full adopt path: the transcript states the model, the route
        // files name its provider, and the row carries both plus a verified
        // basis - the premise the attest-model guard needs.
        let uuid = "a1b2c3d4-1111-2222-3333-444455556666";
        let projects = seed_transcript("adopt-fill", uuid, &[transcript_line("glm-5.3-flash[1m]")]);
        let routes = seed_route_settings("adopt-fill", &["zai"]);
        let dir = std::env::temp_dir().join(format!(
            "fno-adopt-adoptfill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");
        std::env::set_var("FNO_CLAIMS_ROOT", &dir);
        let entry = adopt(&reg, &worker(), std::process::id()).unwrap();
        assert_eq!(entry.model.as_deref(), Some("glm-5.3-flash[1m]"));
        assert_eq!(entry.model_basis.as_deref(), Some("verified"));
        assert_eq!(entry.provider.as_deref(), Some("zai"));
        assert_eq!(entry.node, None);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        std::env::remove_var("FNO_ROUTE_SETTINGS_DIR");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&projects).ok();
        std::fs::remove_dir_all(&routes).ok();
    }

    #[test]
    fn upsert_refresh_keeps_a_stamped_node() {
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
        let mut stamped = mint_adopted_entry(&worker(), "2026-06-27T17:00:00Z");
        stamped.node = Some("x-98ab".into());
        upsert_adopted_row(&reg, stamped).unwrap();
        upsert_adopted_row(&reg, mint_adopted_entry(&worker(), "2026-06-27T18:00:00Z")).unwrap();
        let loaded = crate::state::load_registry(&reg).unwrap();
        assert_eq!(loaded.entries[0].node.as_deref(), Some("x-98ab"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn attached_row_is_not_interactive_and_not_one_shot() {
        // Reconcile must NOT treat an adopted row as a footnote-managed
        // interactive worker, nor settle it as a finished one-shot ask.
        let e = mint_adopted_entry(&worker(), "2026-06-27T17:00:00Z");
        assert!(!e.is_interactive());
        assert_ne!(e.host_mode_or_default(), HOST_MODE_INTERACTIVE);
        // Wire short in short_id + a live pid -> not a one-shot ask either.
        assert!(!e.is_one_shot_ask(), "adopted row with pid present");
    }

    #[test]
    fn upsert_replaces_by_session_uuid() {
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

        let e1 = mint_adopted_entry(&worker(), "2026-06-27T17:00:00Z");
        upsert_adopted_row(&reg, e1).unwrap();
        // Second adopt of the SAME session refreshes the row, not duplicates it.
        let mut e2 = mint_adopted_entry(&worker(), "2026-06-27T18:00:00Z");
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
        // x-e21e: the replace path swaps the WHOLE row for a fresh mint, which
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

        let mut stamped = mint_adopted_entry(&worker(), "2026-06-27T17:00:00Z");
        stamped.delivery_policy = Some("bus-only".into());
        upsert_adopted_row(&reg, stamped).unwrap();
        // A later re-adopt of the SAME session mints a fresh default row.
        upsert_adopted_row(&reg, mint_adopted_entry(&worker(), "2026-06-27T18:00:00Z")).unwrap();

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

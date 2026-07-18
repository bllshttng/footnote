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
//! historical shell-out recorded the transient `fno claim` subprocess pid, so
//! the claim went instantly `stale` and a concurrent adopter could reclaim it
//! (the daemon-claim-reanchor lesson, PR#53; codex P1 on this PR); the native
//! acquire records the holder pid directly and closes that window. The
//! `session:<uuid>` key routes to the host-global claims root, so two checkouts
//! cannot take separate project-local claims for the same session.

use std::path::Path;

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
    RegistryEntry {
        name: adopted_name(&short),
        short_id: short,
        legacy_provider: String::new(),
        harness: Some("claude".into()),
        harness_session_id: Some(w.session_id.clone()),
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
        log_path: None,
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
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
            Some(i) => reg.entries[i] = entry,
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
/// transient `fno claim` subprocess to record in the first place, but the
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
/// `holder_pid` (the long-lived caller, not the transient `fno claim` subprocess)
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

    let entry = mint_adopted_entry(worker, &crate::daemon::now_rfc3339_like());
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
    fn mint_sets_attached_marker_and_resume_key() {
        let e = mint_adopted_entry(&worker(), "2026-06-27T17:00:00Z");
        assert_eq!(e.name, "cc-a1b2c3d4");
        assert_eq!(e.harness_name(), "claude");
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
        assert_eq!(rec.unwrap().pid, me as i32);
        // A different holder against the live claim -> HeldByOther.
        assert_eq!(
            acquire_pty_claim("uuid-1", "pty:other", me),
            ClaimOutcome::HeldByOther("pty:a1b2c3d4".into())
        );
        std::env::remove_var("FNO_CLAIMS_ROOT");
        std::fs::remove_dir_all(&td).ok();
    }
}

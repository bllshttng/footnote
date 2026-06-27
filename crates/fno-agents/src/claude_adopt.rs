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
//! (footnote's attach-holder process via `--pid`), never to the transient `fno
//! claim` subprocess -- a claim anchored to a process that exits the instant the
//! acquire returns goes instantly `stale` and a concurrent adopter could reclaim
//! it (the daemon-claim-reanchor lesson, PR#53; codex P1 on this PR). The
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
/// The `short_id` FIELD is left empty: footnote-side addressing keys on the full
/// `claude_session_uuid`, never the wire-derived 8-hex short. That short lives in
/// `claude_short_id` (the value the `control.sock` boundary + the `pty:<short>`
/// claim holder use); it is not an fno-worker-socket identity, which is what the
/// `short_id` field means.
pub fn mint_adopted_entry(w: &RosterWorker, now: &str) -> RegistryEntry {
    let short = w.short_id().to_string();
    RegistryEntry {
        name: adopted_name(&short),
        short_id: String::new(),
        provider: "claude".into(),
        cwd: w.cwd.clone(),
        project_root: w.worktree_path.clone().unwrap_or_else(|| w.cwd.clone()),
        session_id: None,
        claude_short_id: Some(short),
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
    /// The claim substrate could not be consulted (no `fno` on PATH, exec error,
    /// unparseable output). Fail OPEN -- the file-claim is the cross-process
    /// coordination record, best-effort like the daemon's.
    Unavailable(String),
}

/// `fno claim acquire session:<uuid> --holder <holder> --pid <pid> -J` -- the
/// acquire argv. `--pid` anchors PID-liveness to the LONG-LIVED holder from the
/// very first acquire: an omitted `--pid` would record the transient `fno claim`
/// subprocess (which exits the instant `.output()` returns), so the claim would be
/// `stale` before any reanchor could fire and a concurrent adopter could reclaim
/// it -- defeating the single-writer guard (codex P1). `session:<uuid>` keys route
/// to the host-global claims root in the CLI, so two checkouts cannot take
/// separate project-local claims for the same session.
fn claim_acquire_argv(uuid: &str, holder: &str, holder_pid: u32) -> Vec<String> {
    vec![
        "claim".into(),
        "acquire".into(),
        format!("session:{uuid}"),
        "--holder".into(),
        holder.into(),
        "--pid".into(),
        holder_pid.to_string(),
        "-J".into(),
    ]
}

/// Acquire the `session:<uuid>` claim for `holder`, anchored to `holder_pid` (the
/// long-lived attach holder). Shells the Python `fno claim` CLI (the claim
/// substrate is Python-only + cross-worktree). Fails OPEN on an unconsultable
/// substrate.
pub fn acquire_pty_claim(uuid: &str, holder: &str, holder_pid: u32) -> ClaimOutcome {
    let output = std::process::Command::new("fno")
        .args(claim_acquire_argv(uuid, holder, holder_pid))
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Err(e) => ClaimOutcome::Unavailable(format!("fno claim acquire failed to run: {e}")),
        Ok(o) if !o.status.success() => {
            let detail = String::from_utf8_lossy(&o.stderr).trim().to_string();
            ClaimOutcome::HeldByOther(if detail.is_empty() {
                "held by another writer".into()
            } else {
                detail
            })
        }
        Ok(o) => match serde_json::from_slice::<serde_json::Value>(&o.stdout) {
            Ok(v) if v.get("holder").and_then(serde_json::Value::as_str) == Some(holder) => {
                ClaimOutcome::Acquired
            }
            Ok(_) => ClaimOutcome::Unavailable("claim acquired but holder mismatch".into()),
            Err(e) => ClaimOutcome::Unavailable(format!("unparseable claim output: {e}")),
        },
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
/// ponytail: live glue -- shells the real `fno claim` CLI, so it is not
/// unit-tested; every composed piece (mint, upsert, claim argv) is.
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
        assert_eq!(e.provider, "claude");
        assert_eq!(e.host_mode.as_deref(), Some("attached"));
        // Addressing identity is the full uuid; the worker-socket short_id field
        // is empty (no fno worker), the wire short lives in claude_short_id.
        assert_eq!(e.short_id, "");
        assert_eq!(
            e.claude_session_uuid.as_deref(),
            Some("a1b2c3d4-1111-2222-3333-444455556666")
        );
        assert_eq!(e.claude_short_id.as_deref(), Some("a1b2c3d4"));
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
        // Empty short_id but a live pid -> not a one-shot ask either.
        assert!(!e.is_one_shot_ask(), "empty short_id but pid present");
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
    fn claim_argv_anchors_to_holder_pid_from_the_first_acquire() {
        // The single acquire carries --pid <holder_pid> (not the transient fno
        // subprocess) AND -J, so the claim is holder-anchored immediately and
        // parseable -- no separate reanchor, no stale window (codex P1).
        assert_eq!(
            claim_acquire_argv("uuid-1", "pty:a1b2c3d4", 4242),
            vec![
                "claim",
                "acquire",
                "session:uuid-1",
                "--holder",
                "pty:a1b2c3d4",
                "--pid",
                "4242",
                "-J"
            ]
        );
    }
}

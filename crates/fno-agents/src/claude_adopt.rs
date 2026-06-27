//! Adopt an externally-spawned `claude --bg` worker into the fno registry.
//!
//! G1 held-attach substrate (epic x-07c1, node x-26df). Adoption is what makes an
//! external Claude session reachable by the rest of footnote: it mints an fno
//! registry row (so grid/relay can `resolve_worker_short_id` it) and takes the
//! single-writer `pty:<short_id>` claim (so two writers can't drive one session).
//! The roster read is [`crate::claude_roster`]; the held attach is
//! [`crate::claude_attach`].
//!
//! The claim is **pid-reanchored to the long-lived HOLDER** (footnote's
//! attach-holder process), never to the transient `fno claim` subprocess -- a
//! claim anchored to a shell that exits goes instantly `stale` and clobbers the
//! good claim (the daemon-claim-reanchor lesson, PR#53). Mirrors the daemon's
//! `session:<uuid>` claim machinery but for the adopt lane.

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
/// PTY; `claude_session_uuid` is the full resume key, `pid`/`pid_start_time` are
/// the EXTERNAL claude worker's (for reuse-detection on the row).
pub fn mint_adopted_entry(w: &RosterWorker, now: &str) -> RegistryEntry {
    let short = w.short_id().to_string();
    RegistryEntry {
        name: adopted_name(&short),
        short_id: short.clone(),
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

/// Upsert an adopted row into `registry.json` (replace by `short_id`, else push).
/// Idempotent: re-adopting the same session refreshes the row rather than
/// duplicating it.
pub fn upsert_adopted_row(registry_path: &Path, entry: RegistryEntry) -> Result<(), StateError> {
    update_registry(registry_path, |reg| {
        if let Some(existing) = reg
            .entries
            .iter_mut()
            .find(|e| e.short_id == entry.short_id)
        {
            *existing = entry;
        } else {
            reg.entries.push(entry);
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

/// `fno claim acquire session:<uuid> --holder <holder> -J` -- the acquire argv.
fn claim_acquire_argv(uuid: &str, holder: &str) -> Vec<String> {
    vec![
        "claim".into(),
        "acquire".into(),
        format!("session:{uuid}"),
        "--holder".into(),
        holder.into(),
        "-J".into(),
    ]
}

/// `fno claim acquire session:<uuid> --holder <holder> --pid <pid>` -- the
/// pid-reanchor argv (re-acquire pinning liveness to the long-lived holder pid).
fn claim_acquire_pid_argv(uuid: &str, holder: &str, pid: u32) -> Vec<String> {
    vec![
        "claim".into(),
        "acquire".into(),
        format!("session:{uuid}"),
        "--holder".into(),
        holder.into(),
        "--pid".into(),
        pid.to_string(),
    ]
}

/// Acquire the `session:<uuid>` claim held by `holder`. Shells the Python `fno
/// claim` CLI (the claim substrate is Python-only + cross-worktree), exactly as
/// the daemon does. Fails OPEN on an unconsultable substrate.
pub fn acquire_pty_claim(uuid: &str, holder: &str) -> ClaimOutcome {
    let output = std::process::Command::new("fno")
        .args(claim_acquire_argv(uuid, holder))
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

/// Re-acquire the claim pinned to `holder_pid` (the long-lived attach holder).
/// Fire-and-forget: the daemon idle-tick reaps it, and a failure here only means
/// the claim keeps its prior (session-pid) anchor, never a wrong one.
pub fn reanchor_pty_claim(uuid: &str, holder: &str, holder_pid: u32) {
    if uuid.is_empty() {
        return;
    }
    let _ = std::process::Command::new("fno")
        .args(claim_acquire_pid_argv(uuid, holder, holder_pid))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

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
        assert_eq!(e.short_id, "a1b2c3d4");
        assert_eq!(e.provider, "claude");
        assert_eq!(e.host_mode.as_deref(), Some("attached"));
        // The full uuid is the resume key, not the 8-hex short.
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
        assert!(!e.is_one_shot_ask(), "has a short_id + pid");
    }

    #[test]
    fn upsert_replaces_by_short_id() {
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
            .filter(|e| e.short_id == "a1b2c3d4")
            .collect();
        assert_eq!(rows.len(), 1, "upsert must not duplicate");
        assert_eq!(rows[0].cwd, "/Users/x/code/moved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn claim_argv_shapes() {
        assert_eq!(
            claim_acquire_argv("uuid-1", "pty:a1b2c3d4"),
            vec![
                "claim",
                "acquire",
                "session:uuid-1",
                "--holder",
                "pty:a1b2c3d4",
                "-J"
            ]
        );
        assert_eq!(
            claim_acquire_pid_argv("uuid-1", "pty:a1b2c3d4", 4242),
            vec![
                "claim",
                "acquire",
                "session:uuid-1",
                "--holder",
                "pty:a1b2c3d4",
                "--pid",
                "4242"
            ]
        );
    }

    #[test]
    fn reanchor_noops_on_empty_uuid() {
        // Must not shell anything for an empty uuid (no claim to reanchor).
        reanchor_pty_claim("", "pty:x", 1);
    }
}

//! Which claude session uuids a live process argv still names: the liveness
//! witness a resumed job carries. A resume or compaction ends the recorded
//! pid and the job returns under a NEW pid whose argv still names the
//! session uuid (`claude --resume <uuid>`), so argv carries the truth the
//! registry's pid cannot.

use serde_json::Value;

/// The resume rescue: a pid-falsified registry row is NOT stale when a
/// live argv names its session uuid - the pid died, the session did not.
/// A reboot leaves no argv witness, so the reboot falsification still
/// holds.
pub fn row_rescued(row: &Value, argvs: &[String]) -> bool {
    row.get("claude_session_uuid")
        .or_else(|| row.get("harness_session_id"))
        .and_then(|v| v.as_str())
        .is_some_and(|uuid| !uuid.is_empty() && argvs.iter().any(|a| a.contains(uuid)))
}

/// The joined argv of every live process, one pass. Empty on a platform
/// without a reader, where the caller keeps the pid-only verdict
/// (fail-safe).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn live_process_argvs() -> Vec<String> {
    list_pids()
        .into_iter()
        .filter_map(crate::pane_argv::process_argv)
        .map(|argv| argv.join(" "))
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn live_process_argvs() -> Vec<String> {
    Vec::new()
}

/// Every pid in the process table right now.
#[cfg(target_os = "macos")]
fn list_pids() -> Vec<u32> {
    let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::pid_t; needed as usize];
    let bytes = i32::try_from(pids.len() * std::mem::size_of::<libc::pid_t>()).unwrap_or(i32::MAX);
    let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if found < 0 {
        return Vec::new();
    }
    pids.into_iter()
        .take(found as usize)
        .filter(|p| *p > 0)
        .map(|p| p as u32)
        .collect()
}

#[cfg(target_os = "linux")]
fn list_pids() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.parse::<u32>().ok())
        .collect()
}

/// Process start time in the REGISTRY'S own units: macOS folds
/// `proc_bsdinfo` to microseconds, Linux keeps the raw `/proc/<pid>/stat`
/// starttime ticks. Deliberately NOT `probe_pid`: the registry's
/// `pid_start_time` is a per-host, per-boot quantity compared only for
/// equality against a value captured for the SAME pid, so it must be read
/// with the same units `fno-agents`' registry writer used (daemon.rs
/// `process_start_time`), not converted to epoch ms.
#[cfg(target_os = "macos")]
fn registry_start_time(pid: u32) -> Option<u64> {
    use std::mem;
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::pid_t,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if written != size {
        return None;
    }
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(target_os = "linux")]
fn registry_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn registry_start_time(_pid: u32) -> Option<u64> {
    None
}

/// POSITIVE falsification of one non-terminal row's liveness by its own
/// recorded pid. A machine restart writes nothing to the registry,
/// so every row keeps the status it last had on disk - including "working"
/// for workers that died with the reboot. `restore_squads` reads that set to
/// decide which members to respawn, so an unverified status read respawns
/// `claude attach` into sessions that no longer exist. Falsified ONLY on
/// positive evidence, never on an inability to check:
///
/// - a recorded pid that is provably invalid (0, 1, or out of `pid_t` range -
///   no real worker ever holds one), or
/// - a recorded pid whose process is gone (`kill(pid, 0)` -> ESRCH), or
/// - a recorded `(pid, pid_start_time)` pair whose live process now starts at
///   a different time (the pid was reused after the worker died).
///
/// A row with no recorded pid, an unparsable document, an EPERM (alive but
/// not ours to signal), or an unreadable start time keeps its status-field
/// verdict - the fail-safe posture everywhere else in this file.
fn row_falsified(row: &serde_json::Value, status: &str) -> bool {
    if matches!(status, "exited" | "permanent-dead" | "permanent_dead") {
        return false; // already terminal; nothing to falsify
    }
    let Some(pid) = row.get("pid").and_then(|v| v.as_u64()) else {
        return false; // no recorded pid: the status field stays the verdict
    };
    if pid <= 1 || pid > i32::MAX as u64 {
        return true; // a recorded pid no worker can hold is corruption, not liveness
    }
    // SAFETY: signal 0 performs no delivery, only an existence/permission probe.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc != 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    }
    // Alive: only a start-time mismatch (a reused pid) may still falsify.
    match (
        row.get("pid_start_time").and_then(|v| v.as_u64()),
        registry_start_time(pid as u32),
    ) {
        (Some(recorded), Some(now)) => recorded != now,
        _ => false, // no basis to prove reuse -> trust existence
    }
}

/// The attach-ids (`short_id`s) of claude rows whose claimed-live status
/// their own recorded pid falsifies (see [`row_falsified`]). The restore-time
/// liveness read subtracts this set so a reboot's stale "working" rows read
/// dead instead of respawning. Tolerant of a malformed document: it
/// contributes nothing, exactly like `derive_rows`. One rescue: a row whose
/// session uuid a LIVE argv names came back under a new pid (resume or
/// compaction restart) and is NOT stale - the pid died, the session did not.
pub fn stale_live_attach_ids(reg_raw: &str) -> std::collections::HashSet<String> {
    let mut stale = std::collections::HashSet::new();
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(reg_raw) else {
        return stale;
    };
    let Some(rows) = doc
        .get("agents")
        .or_else(|| doc.get("entries"))
        .and_then(|v| v.as_array())
    else {
        return stale;
    };
    let mut falsified: Vec<(&serde_json::Value, &str)> = Vec::new();
    for row in rows {
        let status = row.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let is_claude = row
            .get("harness")
            .or_else(|| row.get("provider"))
            .and_then(|v| v.as_str())
            == Some("claude");
        let attach_id = row
            .get("short_id")
            .or_else(|| row.get("claude_short_id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if is_claude && row_falsified(row, status) {
            if let Some(id) = attach_id {
                falsified.push((row, id));
            }
        }
    }
    // The resume rescue: a falsified row whose session uuid a LIVE argv
    // names came back under a new pid. The pid is dead but the session is
    // not; a reboot leaves no argv witness, so that falsification holds.
    if !falsified.is_empty() {
        let argvs = live_process_argvs();
        for (row, id) in falsified {
            if !row_rescued(row, &argvs) {
                stale.insert(id.to_string());
            }
        }
    }
    stale
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(rows: &str) -> String {
        format!(r#"{{"schema_version": 6, "agents": [{rows}]}}"#)
    }

    /// A claude row with a recorded pid that ESRCHs must land in the stale
    /// set - the reboot case, the exact respawn the fix removes; a live pid
    /// with a matching start time stays live; a reused pid reads stale.
    #[test]
    fn stale_live_attach_ids_rows() {
        let mut child = crate::pty::ChildGuard::spawn(&mut std::process::Command::new("true"));
        let pid = child.id();
        child.wait_now();
        let raw = reg(&format!(
            r#"{{"name":"ghost","cwd":"/w","status":"working","harness":"claude",
                 "short_id":"deadbeef","pid":{pid},"pid_start_time":99887766}}"#
        ));
        let stale = stale_live_attach_ids(&raw);
        assert!(
            stale.contains("deadbeef"),
            "a reaped pid ({pid}) with a working status must read stale"
        );

        let pid = std::process::id();
        let Some(start) = registry_start_time(pid) else {
            return; // platform without start-time support; existence arm only
        };
        let matching = reg(&format!(
            r#"{{"name":"me","cwd":"/w","status":"working","harness":"claude",
                 "short_id":"aaaaaaaa","pid":{pid},"pid_start_time":{start}}}"#
        ));
        assert!(
            !stale_live_attach_ids(&matching).contains("aaaaaaaa"),
            "a live pid with a matching start time is not stale"
        );
        let reused = reg(&format!(
            r#"{{"name":"me","cwd":"/w","status":"working","harness":"claude",
                 "short_id":"aaaaaaaa","pid":{pid},"pid_start_time":{}}}"#,
            start + 1
        ));
        assert!(
            stale_live_attach_ids(&reused).contains("aaaaaaaa"),
            "a live pid with a mismatched start time is a reused pid, not the worker"
        );

        let no_pid = reg(
            r#"{"name":"a","cwd":"/w","status":"working","harness":"claude","short_id":"aaaaaaaa"}"#,
        );
        assert!(
            stale_live_attach_ids(&no_pid).is_empty(),
            "no pid -> not stale"
        );
        let mut child = crate::pty::ChildGuard::spawn(&mut std::process::Command::new("true"));
        let pid = child.id();
        child.wait_now();
        let codex = reg(&format!(
            r#"{{"name":"b","cwd":"/w","status":"working","harness":"codex",
                 "short_id":"bbbbbbbb","pid":{pid}}}"#
        ));
        assert!(
            stale_live_attach_ids(&codex).is_empty(),
            "a non-claude short_id is not an attach target"
        );
        let terminal = reg(&format!(
            r#"{{"name":"c","cwd":"/w","status":"exited","harness":"claude",
                 "short_id":"cccccccc","pid":{pid}}}"#
        ));
        assert!(
            stale_live_attach_ids(&terminal).is_empty(),
            "a terminal row is dead by status; nothing to falsify"
        );
        assert!(
            stale_live_attach_ids("{not json").is_empty(),
            "a malformed document contributes nothing"
        );
    }

    /// The rescue predicate: a row whose uuid a live argv names is rescued;
    /// no uuid, or a uuid no argv names, is not.
    #[test]
    fn row_rescued_keys_on_a_live_argv_uuid() {
        let uuid = "e96c0000-1111-2222-3333-444455556666";
        let row = serde_json::json!({
            "claude_session_uuid": uuid,
        });
        assert!(row_rescued(&row, &[format!("claude --resume {uuid}")]));
        assert!(!row_rescued(&row, &["claude attach deadbee1".to_string()]));
        let bare = serde_json::json!({});
        assert!(!row_rescued(&bare, &[format!("claude --resume {uuid}")]));
    }

    /// End to end through the stale set: a falsified row whose uuid a live
    /// argv carries is NOT stale; a uuid no argv names still reads stale.
    #[test]
    fn stale_live_attach_ids_resume_rescue() {
        let mut ghost = crate::pty::ChildGuard::spawn(&mut std::process::Command::new("true"));
        let ghost_pid = ghost.id();
        ghost.wait_now();
        let uuid = "e96c0000-1111-2222-3333-444455556666";
        let witness = crate::pty::ChildGuard::spawn(
            &mut std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("sleep 25; : # {uuid}")),
        );
        let rescued_row = format!(
            r#"{{"schema_version": 6, "agents": [{{"name":"resumed","cwd":"/w","status":"working","harness":"claude",
                 "short_id":"eeee0000","pid":{ghost_pid},"pid_start_time":99887766,
                 "claude_session_uuid":"{uuid}"}}]}}"#
        );
        assert!(
            !stale_live_attach_ids(&rescued_row).contains("eeee0000"),
            "a live argv naming the session uuid rescues the row"
        );
        let stranded_row = format!(
            r#"{{"schema_version": 6, "agents": [{{"name":"stranded","cwd":"/w","status":"working","harness":"claude",
                 "short_id":"eeee0001","pid":{ghost_pid},"pid_start_time":99887766,
                 "claude_session_uuid":"ffff0000-1111-2222-3333-444455556666"}}]}}"#
        );
        assert!(
            stale_live_attach_ids(&stranded_row).contains("eeee0001"),
            "a uuid no live argv names still reads stale"
        );
        drop(witness);
    }
}

//! `fno-agents hook session-state --harness <h> <event>`: every harness
//! pushes its turn state, model, effort and permission posture to the daemon
//! row through this one entry. Payload in on stdin, fire-and-forget out:
//! exit 0 on every path, never blocks or reds a turn.
//!
//! This is the one Rust replacement for hooks/inside-leg-report.sh; the shell
//! producer's contracts move here one for one:
//!
//! - the picker reclassification (AskUserQuestion / ExitPlanMode reports
//!   blocked, naming what the session is waiting on),
//! - a monotonic seq (`MonotonicTimestamp`, boot-global, never steps back
//!   within one boot; the daemon drops `seq <= last_seq`),
//! - OSC 133 turn-block markers gated on FNO_PANE plus the first-writer pin
//!   under the 0700 runtime dir, the continuation re-open when a `/target`
//!   manifest exists,
//! - the 120 s transition gate with mark-after-send (a failed send never
//!   suppresses a retry),
//! - a 2 s bound on the RPC and the tty write, exit 0 on every path.
//!
//! The per-harness event map is data: `[harness.<h>.hooks.session_state]`
//! in harness_capabilities.toml names each event's state word and the
//! harness's blocked event ("none" = no permission-prompt producer). The
//! adapter layer turns the raw payload into one [`crate::hook::adapter::HookEvent`];
//! this module owns what it means.

#[cfg(test)]
mod tests;

use crate::harness_capabilities::HookJobDecl;
use crate::hook::adapter::{self, HookEvent};
use serde_json::json;
use std::io::Read;

/// The picker tools whose PreToolUse call is the session ASKING the operator,
/// not working: PreToolUse would report `working`, so the call is
/// reclassified to blocked before the report. The next PreToolUse for any
/// other tool, or Stop, reports working/done and clears it.
const ASK_TOOLS: [&str; 2] = ["AskUserQuestion", "ExitPlanMode"];

/// The static reason when a picker payload carried no question text.
const ASKING_FALLBACK: &str = "asking the user";
const PLAN_APPROVAL: &str = "plan approval requested";

/// `fno-agents hook session-state`: parse, normalize, decide, report.
pub fn run(args: &[String]) -> i32 {
    let Some((harness, event)) = parse_args(args) else {
        return 0;
    };
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    let payload: serde_json::Value =
        serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
    process(&harness, &event, &payload);
    0
}

/// `--harness <h> <event>` in either the space or `=` spelling.
fn parse_args(args: &[String]) -> Option<(String, String)> {
    let args = crate::client_verbs::expand_eq(args);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--harness" {
            let harness = it.next()?.clone();
            let event = it.next()?.clone();
            if !harness.is_empty() && !event.is_empty() {
                return Some((harness, event));
            }
            return None;
        }
    }
    None
}

/// One decided fire: the state word and the reason the report carries. The
/// model/effort/posture axes ride the HookEvent untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    /// `working` | `blocked` | `done` | `model` (the report wire vocabulary).
    pub state: &'static str,
    /// The blocked reason; empty on the other states.
    pub reason: String,
    pub posture: Option<String>,
}

/// The state word + reason for one normalized event, read off the harness
/// row's `session_state` map. `None` = this event is not wired for the
/// harness (unknown event, or the row declares no session-state producer).
pub(crate) fn decide(job: &HookJobDecl, ev: &HookEvent) -> Option<Decision> {
    if job.blocked == ev.event {
        return Some(Decision {
            state: "blocked",
            reason: ev.message.clone(),
            posture: ev.posture.clone(),
        });
    }
    match job.events.get(&ev.event).map(String::as_str) {
        Some("working") => Some(reclassify(ev)),
        Some("done") => Some(Decision {
            state: "done",
            reason: String::new(),
            posture: ev.posture.clone(),
        }),
        // The PostModelSwitch word: no inside-leg transition, the report only
        // diffs the row's served axes; it needs at least one of them.
        Some("model") if !ev.model.is_empty() || !ev.effort.is_empty() => Some(Decision {
            state: "model",
            reason: String::new(),
            posture: ev.posture.clone(),
        }),
        _ => None,
    }
}

/// A picker tool call on a working event is the session ASKING: reclassify
/// to blocked with the question text when the payload carried one, the
/// static fallbacks when it did not (the C14 feed contract). Any other tool
/// keeps plain working.
fn reclassify(ev: &HookEvent) -> Decision {
    if ASK_TOOLS.contains(&ev.tool.as_str()) {
        let reason = if ev.tool == "ExitPlanMode" {
            PLAN_APPROVAL.to_string()
        } else if ev.message.is_empty() {
            ASKING_FALLBACK.to_string()
        } else {
            ev.message.clone()
        };
        return Decision {
            state: "blocked",
            reason,
            posture: ev.posture.clone(),
        };
    }
    Decision {
        state: "working",
        reason: String::new(),
        posture: ev.posture.clone(),
    }
}

/// OSC 133 turn-block markers, mux panes only, and only from THE pane host.
/// `133;C` opens a turn block on a turn-start working, `133;D;0` closes one
/// on done; a mid-turn working (PreToolUse) must NOT open, or one turn with
/// N tool calls fragments into N sub-blocks (gated on the source event, not
/// the state). The done re-open segments every leg of a blocked Stop (the
/// /target loop), gated on the mere PRESENCE of the manifest.
const MARKER_C: &str = "\x1b]133;C\x07";
const MARKER_D: &str = "\x1b]133;D;0\x07";

/// The marker bytes this fire emits, mux panes only. `manifest_present` is
/// the `/target` continuation re-open gate.
pub(crate) fn marker_bytes(state: &str, event: &str, manifest_present: bool) -> Vec<&'static str> {
    match (state, event) {
        ("working", "PreToolUse") => Vec::new(),
        ("working", _) => vec![MARKER_C],
        ("done", _) => {
            let mut m = vec![MARKER_D];
            if manifest_present {
                m.push(MARKER_C);
            }
            m
        }
        _ => Vec::new(),
    }
}
/// The sink the marker bytes go to: /dev/tty (the pane PTY, this process's
/// controlling terminal); FNO_TURN_MARKER_TTY overrides it for tests.
/// Append, not truncate: a done fire can emit two markers (D then a re-open
/// C) and each write reopens the sink.
fn tty_sink() -> std::path::PathBuf {
    match std::env::var_os("FNO_TURN_MARKER_TTY") {
        Some(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => std::path::PathBuf::from("/dev/tty"),
    }
}

/// Write the markers under a 2 s wall-clock bound: a write to a pane whose
/// reader stalled blocks rather than fails, so the write runs on a throwaway
/// thread and the fire walks away at the cap (a stalled PTY costs the cap,
/// the marker is dropped, the turn proceeds).
fn emit_markers(markers: &[&str]) {
    if markers.is_empty() {
        return;
    }
    let sink = tty_sink();
    let bytes: Vec<u8> = markers.iter().flat_map(|m| m.as_bytes().to_vec()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Write;
        let res = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&sink)
            .and_then(|mut f| f.write_all(&bytes));
        let _ = tx.send(res);
    });
    let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
}
/// The hardened rendezvous dir shared by the first-writer pin and the
/// transition gate: XDG_RUNTIME_DIR, else the macOS per-user TMPDIR, else
/// /tmp; `fno-turn-pins-<euid>`, created atomically at mode 700 (libc mkdir
/// with the mode, no loose-perms window, no check-then-create TOCTOU). A
/// pre-existing path is trusted only as a real, self-owned, non-symlink dir,
/// re-chmodded to 700 since we did not create it. `None` = anomaly, degrade
/// to the caller's fail-open behavior.
fn runtime_pin_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("TMPDIR").filter(|v| !v.is_empty()))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp").into_os_string());
    let dir = std::path::PathBuf::from(base).join(format!("fno-turn-pins-{}", euid()));
    // libc mkdir with the mode: created 700 atomically, no loose-perms
    // window (plain create_dir would race its own chmod).
    let cpath = match std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()) {
        Ok(p) => p,
        Err(_) => return None,
    };
    let rc = unsafe { libc::mkdir(cpath.as_ptr(), 0o700) };
    if rc != 0 {
        // Exists (or failed): accept only a real, self-owned, non-symlink dir.
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(&dir).ok()?;
        if !md.is_dir() || md.file_type().is_symlink() {
            return None;
        }
        if md.uid() != euid() {
            return None;
        }
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

fn euid() -> u32 {
    unsafe { libc::geteuid() }
}
/// First-writer session-identity gate: only the pty.rs process spawned INTO
/// the pane emits. It fires its first marker at turn start, BEFORE it can
/// spawn a nested session, so it always wins the pin; a nested session
/// (which inherits FNO_PANE + the ctty) reads a different pinned id and
/// stays silent instead of splitting the outer turn. Degrades to the v1
/// presence gate (emit) when there is no recycle-safe key: no
/// FNO_PANE_EPOCH, or no session id. The key carries FNO_PANE_EPOCH because
/// pane ids recycle across server restarts.
fn is_pane_host(session_id: &str) -> bool {
    if std::env::var_os("FNO_PANE_EPOCH").is_none_or(|v| v.is_empty()) || session_id.is_empty() {
        return true;
    }
    let Some(dir) = runtime_pin_dir() else {
        return true;
    };
    // Every pin path component is env-controlled, so a hostile env could
    // smuggle `/` or `..` out of the dir. FNO_PANE/FNO_PANE_EPOCH are
    // numeric by contract (pty.rs), so require digits (degrade-to-emit
    // otherwise); FNO_SERVER (FNO_SESSION on pre-rename panes) is free-form,
    // so sanitize its separators. Deterministic, so a host and its nested
    // session still compute the same pin path.
    let numeric = |k: &str| {
        std::env::var(k).is_ok_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
    };
    let pane_ok = numeric("FNO_PANE");
    let epoch_ok = numeric("FNO_PANE_EPOCH");
    if !pane_ok || !epoch_ok {
        return true;
    }
    let server = std::env::var("FNO_SERVER").unwrap_or_default();
    let server = if server.is_empty() {
        std::env::var("FNO_SESSION").unwrap_or_default()
    } else {
        server
    };
    let safe_server: String = if server.is_empty() {
        "_".to_string()
    } else {
        server
            .chars()
            .map(|c| if c == '/' || c == '.' { '_' } else { c })
            .collect()
    };
    let pane = std::env::var("FNO_PANE").unwrap_or_default();
    let epoch = std::env::var("FNO_PANE_EPOCH").unwrap_or_default();
    let pin = dir.join(format!("{safe_server}-{pane}-{epoch}"));
    // noclobber: the create fails when the pin exists -> exactly one winner.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pin)
    {
        Ok(mut f) => {
            use std::io::Write;
            let _ = writeln!(f, "{session_id}");
            return true;
        }
        Err(_) => {}
    }
    // An empty pin means our own create half-succeeded (e.g. ENOSPC after
    // the O_EXCL create): degrade to the presence gate (emit) instead of
    // latching the host into permanent silence. A real nested session always
    // wrote a non-empty id, so it still mismatches below and stays silent.
    match std::fs::metadata(&pin) {
        Ok(m) if m.len() > 0 => {}
        _ => return true,
    }
    // An unreadable/corrupt pin leaves pinned_id empty -> degrade-to-emit,
    // never latch the host silent on a read failure.
    let pinned = std::fs::read_to_string(&pin).unwrap_or_default();
    let pinned = pinned.lines().next().unwrap_or("");
    !pinned.is_empty() && pinned == session_id
}
/// How young a same-state-and-reason record may be before the report is
/// skipped: the ceiling against a sidecar that says `working` while the
/// registry lost the row to a daemon restart (the state re-asserts itself
/// within two minutes rather than latching silent forever).
const STATE_REFRESH_AFTER_SECS: u64 = 120;

/// The repo root, resolved by walking up from the cwd looking for `.git` so
/// a session launched in a subdir still finds `.fno/target-state.md`
/// deterministically, not relative to $PWD.
fn repo_root() -> std::path::PathBuf {
    let mut dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    loop {
        if dir.join(".git").exists() {
            return dir;
        }
        if !dir.pop() {
            return std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        }
    }
}
/// The transition-gate record path for a session id, in the same rendezvous
/// dir as the pin.
fn state_record_path(session_id: &str) -> Option<std::path::PathBuf> {
    let dir = runtime_pin_dir()?;
    let safe: String = session_id
        .chars()
        .map(|c| if c == '/' || c == '.' { '_' } else { c })
        .collect();
    Some(dir.join(format!("state-{safe}")))
}

/// The transition gate: a report is skipped when the recorded state AND
/// reason for this session already match AND the record is younger than the
/// refresh ceiling. Fail OPEN in both directions: an unreadable record means
/// send; the gate is an optimisation and must never be the reason a state
/// goes unreported.
fn should_report(session_id: &str, decision: &Decision) -> bool {
    let Some(record) = state_record_path(session_id) else {
        return true;
    };
    let Ok(prev) = std::fs::read_to_string(&record) else {
        return true;
    };
    let mut parts = prev.splitn(3, ' ');
    let prev_state = parts.next().unwrap_or("");
    let prev_epoch = parts.next().unwrap_or("");
    let prev_message = parts.next().unwrap_or("");
    if prev_state == decision.state
        && prev_message == decision.reason
        && prev_epoch.chars().all(|c| c.is_ascii_digit())
        && !prev_epoch.is_empty()
    {
        let now = now_secs();
        let age = now.saturating_sub(prev_epoch.parse::<u64>().unwrap_or(0));
        return age >= STATE_REFRESH_AFTER_SECS;
    }
    true
}

/// Persist "sent" AFTER a confirmed send, never before: persisting first
/// would suppress every same-state retry for up to the ceiling on a timeout
/// or a down daemon socket.
fn mark_reported(session_id: &str, decision: &Decision) {
    let Some(record) = state_record_path(session_id) else {
        return;
    };
    let _ = std::fs::write(
        &record,
        format!("{} {} {}", decision.state, now_secs(), decision.reason),
    );
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
/// Send the report RPC in process (the same client `report` runs, not a
/// child process), bounded at 2 s: the daemon already stores `reason` and
/// uses it as the notify body, so a non-empty Notification message reaches
/// the operator's Waiting notification with no further plumbing. The answer
/// mirrors the shell's mark-after-send: the exit-0 outcomes mark, a timeout
/// or a real send failure never does.
fn send_report(ev: &HookEvent, decision: &Decision, seq: u64) -> bool {
    let mut params = serde_json::Map::new();
    params.insert("session_id".into(), json!(ev.session_id));
    params.insert("seq".into(), json!(seq));
    params.insert("state".into(), json!(decision.state));
    if !decision.reason.is_empty() {
        params.insert("reason".into(), json!(decision.reason));
    }
    if !ev.model.is_empty() {
        params.insert("model".into(), json!(ev.model));
    }
    if !ev.effort.is_empty() {
        params.insert("effort".into(), json!(ev.effort));
    }
    if let Some(p) = &decision.posture {
        params.insert("posture".into(), json!(p));
    }
    let req = crate::protocol::Request::new(1, "agent.report", serde_json::Value::Object(params));
    let home = crate::paths::AgentsHome::from_env();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    let Ok(rt) = rt else {
        return false;
    };
    rt.block_on(async {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            crate::client::call_if_running(&home, &req),
        )
        .await
        {
            // Bound elapsed, or the daemon answered: the exit-0 outcomes.
            Ok(Ok(_)) | Ok(Err(crate::client::ClientError::DaemonNotRunning)) => true,
            Ok(Err(crate::client::ClientError::DaemonUnresponsive { .. })) => true,
            _ => false,
        }
    })
}
/// The whole fire: normalize, decide, mark, report. Every failure inside is
/// silent and fire-and-forget; `run` still exits 0.
pub(crate) fn process(harness: &str, event: &str, payload: &serde_json::Value) {
    let ev = adapter::normalize(harness, event, payload);
    // The row map is the data: a harness (or event) not wired for
    // session-state declares it, and the entry does nothing.
    let Ok(contract) = crate::harness_capabilities::HarnessContract::packaged() else {
        return;
    };
    let Some(job) = contract.hook_job(harness, "session_state") else {
        return;
    };
    let Some(decision) = decide(job, &ev) else {
        return;
    };
    // Marker lane: mux panes only, and independent of the parse (a malformed
    // payload still emits via the presence-gate degrade), so a pane's block
    // scanner segments every turn even when the report cannot fly.
    if std::env::var_os("FNO_PANE").is_some_and(|v| !v.is_empty()) && is_pane_host(&ev.session_id) {
        let manifest = repo_root().join(".fno/target-state.md");
        let markers = marker_bytes(decision.state, &ev.event, manifest.is_file());
        emit_markers(&markers);
    }
    // The report needs a parsed session id; a malformed/empty payload already
    // had its marker lane above, so just skip the report here.
    if ev.session_id.is_empty() {
        return;
    }
    if !should_report(&ev.session_id, &decision) {
        return;
    }
    let seq = crate::MonotonicTimestamp::now().as_nanos();
    if send_report(&ev, &decision, seq) {
        mark_reported(&ev.session_id, &decision);
    }
}

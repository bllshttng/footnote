//! Whether a pane-substrate worker's process actually stopped (x-1b90).
//!
//! Three stop answerers decided "this pane worker stopped" and none asked
//! the process: the reap asked a roster or a worker socket that never held
//! the pane, and `fno agents rm` killed the row's STORED pane id and read a
//! missing pane as absent - while a server that re-adopts a keeper re-mints
//! pane ids, so the stored id is not an address for a process. One helper
//! answers the question the same way for every caller: verify the row's
//! pid, find the LIVE pane by child pid, kill it, and confirm on ESRCH
//! only. Extracted from daemon.rs under the file-budget gate; `run_mux_pane_kill`
//! and `mux_pane_is_absent` moved with the code that uses them.

use crate::state::RegistryEntry;
use std::time::Duration;

/// What stopping one pane worker measured. `confirmed` is true only on
/// `pid_is_gone` (ESRCH) - a pane kill that "succeeded", an absent pane,
/// and an unreachable socket are none of them a death. `detail` names what
/// actually ran, so the receipt's native-stop record and rm's printed line
/// are measurements, not assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneStop {
    pub confirmed: bool,
    pub detail: String,
}

/// One pane a listing read found live: the session that hosts it, its pane
/// id AT READ TIME, and the child pid the keeper holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneSighting {
    pub session: String,
    pub pane_id: u64,
    pub child_pid: Option<u32>,
}

/// The seams of [`stop_pane_process_confirmed_with`], injectable so a test
/// stages a stale stored pane id, a pid that survives the pane kill, and a
/// missing pid without spawning anything.
pub(crate) struct PaneStopSeams {
    /// Live panes for `Some(session)`, or every session when the row has no
    /// mux ref. An empty Vec means the listing could not answer; that is
    /// never a death - the escalation below still reaches the pid directly.
    pub pane_lookup: Box<dyn Fn(Option<&str>) -> Vec<PaneSighting>>,
    /// Kill one pane. `Ok(true)` removed, `Ok(false)` already absent.
    pub pane_kill: Box<dyn Fn(&str, u64) -> Result<bool, String>>,
    /// The ownership-checked liveness probe that gates every action.
    pub pid_ours: Box<dyn Fn(u32, Option<u64>) -> bool>,
    /// The existence-specific death probe (ESRCH only) - the one
    /// confirmation.
    pub pid_gone: Box<dyn Fn(u32) -> bool>,
    /// Send a signal to a pid; answer whether it was sent.
    pub signal: Box<dyn Fn(u32, i32) -> bool>,
    /// The poll tick. Production sleeps `POLL_TICK_MS`; a test passes a
    /// no-op so the escalation windows run at loop speed.
    pub sleep: Box<dyn Fn(u64)>,
}

/// Post-kill poll budget: 50 ticks x 100 ms (5 s), then the SIGTERM window
/// 50 x 100 ms (5 s), then the SIGKILL window 20 x 100 ms (2 s) - the
/// escalation shape `stop_worker_confirmed_for_home` uses for socket-held
/// workers.
const KILL_POLL_TICKS: usize = 50;
const TERM_POLL_TICKS: usize = 50;
const KILL9_POLL_TICKS: usize = 20;
const POLL_TICK_MS: u64 = 100;

/// The production entry point: stop a pane row's process through the real
/// pane listing, the real pane kill, and the real pid probes.
pub(crate) fn stop_pane_process_confirmed(e: &RegistryEntry) -> PaneStop {
    stop_pane_process_confirmed_with(
        e,
        &PaneStopSeams {
            pane_lookup: Box::new(pane_list_via_fno),
            pane_kill: Box::new(run_mux_pane_kill),
            pid_ours: Box::new(crate::daemon::pid_is_ours),
            pid_gone: Box::new(crate::daemon::pid_is_gone),
            signal: Box::new(signal_pid),
            sleep: Box::new(|ms| std::thread::sleep(Duration::from_millis(ms))),
        },
    )
}

/// The one pane-stop body, shared by the reap and `fno agents rm` (x-1b90
/// change 1). Confirms ONLY on the pid reading gone; every earlier answer
/// is a not-confirmed detail naming what ran.
pub(crate) fn stop_pane_process_confirmed_with(
    e: &RegistryEntry,
    seams: &PaneStopSeams,
) -> PaneStop {
    // 1. Verified pid. With no verified pid, kill nothing, send no signal.
    let Some(pid) = e.pid else {
        return PaneStop {
            confirmed: false,
            detail: "pane row carries no verified pid; the stop cannot be proven".into(),
        };
    };
    if !(seams.pid_ours)(pid, e.pid_start_time) {
        return PaneStop {
            confirmed: false,
            detail: format!("pid {pid} is gone, recycled or foreign; the stop cannot be proven"),
        };
    }
    let mut ran: Vec<String> = Vec::new();
    // 2. Find the live pane by child pid. Never address the stored
    //    mux.pane_id alone: the server re-mints pane ids when it re-adopts a
    //    keeper, and the cascade that trusted the stored id killed a stale
    //    id and read the missing pane as absent.
    let sightings = (seams.pane_lookup)(e.mux.as_ref().map(|m| m.session.as_str()));
    match sightings.iter().find(|p| p.child_pid == Some(pid)) {
        Some(pane) => match (seams.pane_kill)(&pane.session, pane.pane_id) {
            Ok(true) => {
                ran.push(format!(
                    "pane {}:{} (child {pid}) killed",
                    pane.session, pane.pane_id
                ));
            }
            Ok(false) => {
                ran.push(format!(
                    "pane {}:{} (child {pid}) already absent",
                    pane.session, pane.pane_id
                ));
            }
            Err(err) => ran.push(format!(
                "pane {}:{} (child {pid}) kill failed: {err}",
                pane.session, pane.pane_id
            )),
        },
        None => {
            // A keeper can hold the child with no pane at all (a server
            // restart between listing and kill). The pid is still ours; the
            // escalation reaches it directly.
            match &e.mux {
                Some(mux) => ran.push(format!(
                    "no live pane hosts pid {pid} (stored {}:{} is stale or re-minted)",
                    mux.session, mux.pane_id
                )),
                None => ran.push(format!("no live pane hosts pid {pid}")),
            }
        }
    }
    // 3. Poll for death after the pane kill.
    if poll_gone(seams, pid, KILL_POLL_TICKS) {
        return confirmed(ran, pid);
    }
    // 4. Escalate while the pid lives, re-checking ownership before every
    //    signal: a recycled pid is never ours to signal.
    if (seams.pid_ours)(pid, e.pid_start_time) && (seams.signal)(pid, libc::SIGTERM) {
        ran.push(format!("SIGTERM sent to {pid}"));
        if poll_gone(seams, pid, TERM_POLL_TICKS) {
            return confirmed(ran, pid);
        }
    }
    if (seams.pid_ours)(pid, e.pid_start_time) && (seams.signal)(pid, libc::SIGKILL) {
        ran.push(format!("SIGKILL sent to {pid}"));
        if poll_gone(seams, pid, KILL9_POLL_TICKS) {
            return confirmed(ran, pid);
        }
    }
    // 5. Only ESRCH confirms; anything else holds the caller's row.
    PaneStop {
        confirmed: false,
        detail: format!("{}; pid {pid} still alive", ran.join("; ")),
    }
}

fn confirmed(ran: Vec<String>, pid: u32) -> PaneStop {
    PaneStop {
        confirmed: true,
        detail: format!("{}; pid {pid} gone", ran.join("; ")),
    }
}

fn poll_gone(seams: &PaneStopSeams, pid: u32, ticks: usize) -> bool {
    for _ in 0..ticks {
        if (seams.pid_gone)(pid) {
            return true;
        }
        (seams.sleep)(POLL_TICK_MS);
    }
    (seams.pid_gone)(pid)
}

/// The rm handler's pane-arm mapping: `pane_removed` follows `confirmed`,
/// and the stop's own measurement is the printed reason in every branch.
/// On a confirmed stop the detail rides BESIDE the outcome (`Removed`
/// carries no reason of its own); on an unconfirmed stop the detail IS the
/// failure reason - without `--force` rm refuses on it, with `--force` the
/// row drops and the audit line records that the pid is still alive.
pub(crate) fn rm_pane_outcome(stop: &PaneStop) -> (crate::daemon::CascadeOutcome, Option<String>) {
    if stop.confirmed {
        (
            crate::daemon::CascadeOutcome::Removed,
            Some(stop.detail.clone()),
        )
    } else {
        (
            crate::daemon::CascadeOutcome::Failed(stop.detail.clone()),
            None,
        )
    }
}

fn signal_pid(pid: u32, sig: i32) -> bool {
    // SAFETY: a real signal to a pid the caller already verified through
    // `pid_is_ours`; the range guard there rejects the broadcast forms.
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    rc == 0
}

/// The production pane listing: `fno mux pane ls --session <s> --json` for
/// the row's own session, or one ls per session the mux dir holds a socket
/// for when the row carries no mux ref. A failed or unparseable ls answers
/// an empty Vec - the caller treats a missing LISTING as no panes, and the
/// escalation still reaches the pid directly.
fn pane_list_via_fno(session: Option<&str>) -> Vec<PaneSighting> {
    let sessions: Vec<String> = match session {
        Some(s) => vec![s.to_string()],
        None => mux_session_names(),
    };
    let mut found = Vec::new();
    for s in sessions {
        let Ok(output) = std::process::Command::new("fno")
            .args(["mux", "pane", "ls", "--server", &s, "--json"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
        else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let Ok(panes) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) else {
            continue;
        };
        for pane in panes {
            found.push(PaneSighting {
                session: s.clone(),
                pane_id: pane
                    .get("pane_id")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                child_pid: pane
                    .get("child_pid")
                    .and_then(serde_json::Value::as_u64)
                    .map(|p| p as u32),
            });
        }
    }
    found
}

/// Session names for the all-sessions listing: the `*.sock` stems of the mux
/// dir (`FNO_MUX_DIR`, else the state root's `mux/`). Mirrors the client's
/// own session enumeration; an unreadable dir answers empty.
fn mux_session_names() -> Vec<String> {
    let dir = match std::env::var_os("FNO_MUX_DIR") {
        Some(d) => std::path::PathBuf::from(d),
        None => match crate::paths::AgentsHome::from_env_opt() {
            Some(home) => home
                .root()
                .parent()
                .map(|root| root.join("mux"))
                .unwrap_or_default(),
            None => return Vec::new(),
        },
    };
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().map(|x| x == "sock").unwrap_or(false))
                .filter_map(|e| {
                    e.path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

/// Kill one pane through the `fno` CLI, tolerating an already-absent pane.
/// Moved from daemon.rs with the callers that share it. `Ok(false)` is the
/// absent-pane word, never a death verdict: the caller confirms on the pid,
/// not on the pane.
pub(crate) fn run_mux_pane_kill(session: &str, pane_id: u64) -> Result<bool, String> {
    let pane_id = pane_id.to_string();
    let mut child = std::process::Command::new("fno")
        .args(["mux", "pane", "kill", "--server", session, &pane_id])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("mux pane kill failed to start: {error}"))?;
    let deadline = std::time::Instant::now() + crate::daemon::CASCADE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(true),
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                let output = child.wait_with_output().ok();
                let detail = output
                    .as_ref()
                    .map(|output| String::from_utf8_lossy(&output.stderr).to_ascii_lowercase())
                    .unwrap_or_default();
                if mux_pane_is_absent(&detail) {
                    return Ok(false);
                }
                return Err(format!("mux pane kill exited {code}: {}", detail.trim()));
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("mux pane kill timed out".into());
            }
            Err(error) => return Err(format!("mux pane kill wait failed: {error}")),
        }
    }
}

/// The absence vocabulary the kill cascade and the pane probe both trust.
pub(crate) fn mux_pane_is_absent(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("no such pane")
        || detail.contains("no live pane owns")
        || (detail.contains("cannot reach session")
            && (detail.contains("no such file or directory")
                || detail.contains("connection refused")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RegistryEntry;
    use std::cell::RefCell;

    /// A pane row with no live identity beyond what the test sets.
    fn pane_row(pid: Option<u32>, session: Option<(&str, u64)>) -> RegistryEntry {
        let mut e = RegistryEntry::default();
        e.name = "pane-worker".into();
        e.substrate = Some("pane".into());
        e.pid = pid;
        e.pid_start_time = Some(42);
        e.mux = session.map(|(s, p)| crate::state::MuxRef {
            session: s.into(),
            pane_id: p,
        });
        e
    }

    /// Counting harness shared by the escalation tests.
    #[derive(Default)]
    struct Log {
        kills: Vec<(String, u64)>,
        signals: Vec<(u32, i32)>,
    }

    /// Death phase: 0 nothing ran, 1 pane killed, 2 SIGTERM sent, 3 SIGKILL
    /// sent. `pid_gone` answers each phase's configured outcome.
    type Shared<T> = std::rc::Rc<std::cell::RefCell<T>>;

    fn seams(
        log: Shared<Log>,
        sightings: Vec<PaneSighting>,
        kill_result: Result<bool, String>,
        gone_after_kill: bool,
        gone_after_term: bool,
        gone_after_kill9: bool,
    ) -> PaneStopSeams {
        let phase: Shared<u8> = Shared::new(RefCell::new(0));
        let kill_log = Shared::clone(&log);
        let kill_phase = Shared::clone(&phase);
        let sig_log = Shared::clone(&log);
        let sig_phase = Shared::clone(&phase);
        PaneStopSeams {
            pane_lookup: Box::new(move |_| sightings.clone()),
            pane_kill: Box::new(move |s, p| {
                kill_log.borrow_mut().kills.push((s.to_string(), p));
                *kill_phase.borrow_mut() = 1;
                kill_result.clone()
            }),
            pid_ours: Box::new(|_, _| true),
            pid_gone: {
                let phase = Shared::clone(&phase);
                Box::new(move |_| match *phase.borrow() {
                    1 => gone_after_kill,
                    2 => gone_after_term,
                    3 => gone_after_kill9,
                    _ => false,
                })
            },
            signal: Box::new(move |pid, sig| {
                sig_log.borrow_mut().signals.push((pid, sig));
                *sig_phase.borrow_mut() = if sig == libc::SIGTERM { 2 } else { 3 };
                true
            }),
            sleep: Box::new(|_| {}),
        }
    }

    /// Poll budget for tests: the injected pid_gone answers immediately, so
    /// the real 100 ms tick never sleeps more than the first poll.
    fn test_row() -> RegistryEntry {
        pane_row(Some(22287), Some(("main", 1991)))
    }

    /// AC1-HP: the pane found by child pid is killed and the stop confirms
    /// only after the pid reads gone, with a detail naming pane and pid.
    #[test]
    fn ac1_hp_kills_pane_found_by_child_pid_and_confirms_on_esrch() {
        let log = Shared::new(RefCell::new(Log::default()));
        let seams = seams(
            log.clone(),
            vec![PaneSighting {
                session: "main".into(),
                pane_id: 2034,
                child_pid: Some(22287),
            }],
            Ok(true),
            true,
            true,
            true,
        );
        let stop = stop_pane_process_confirmed_with(&test_row(), &seams);
        assert!(stop.confirmed);
        assert_eq!(log.borrow().kills, vec![("main".into(), 2034)]);
        assert_eq!(log.borrow().signals, vec![]);
        assert!(
            stop.detail.contains("pane main:2034 (child 22287) killed")
                && stop.detail.ends_with("pid 22287 gone"),
            "detail: {}",
            stop.detail
        );
    }

    /// AC1-STALE: the stored pane id names no live pane; the pane found by
    /// child pid is killed and the stale id is never read as already absent.
    #[test]
    fn ac1_stale_kills_the_pane_found_by_child_pid_never_the_stored_id() {
        let log = Shared::new(RefCell::new(Log::default()));
        let seams = seams(
            log.clone(),
            vec![PaneSighting {
                session: "main".into(),
                pane_id: 2034,
                child_pid: Some(22287),
            }],
            Ok(true),
            true,
            true,
            true,
        );
        let stop = stop_pane_process_confirmed_with(&test_row(), &seams);
        assert!(stop.confirmed);
        // The row's stored id is 1991; the kill went to the live 2034.
        assert_eq!(log.borrow().kills, vec![("main".into(), 2034)]);
    }

    /// AC1-ESC: a pid that survives the pane kill gets SIGTERM, and the
    /// stop confirms only after the pid reads gone.
    #[test]
    fn ac1_esc_surviving_pid_gets_sigterm_before_confirmation() {
        let log = Shared::new(RefCell::new(Log::default()));
        let seams = seams(
            log.clone(),
            vec![PaneSighting {
                session: "main".into(),
                pane_id: 2034,
                child_pid: Some(22287),
            }],
            Ok(true),
            false,
            true,
            true,
        );
        let stop = stop_pane_process_confirmed_with(&test_row(), &seams);
        assert!(stop.confirmed);
        assert_eq!(
            log.borrow().signals,
            vec![(22287, libc::SIGTERM)],
            "SIGKILL must not fire when SIGTERM landed the pid"
        );
        assert!(stop.detail.contains("SIGTERM sent to 22287"));
    }

    /// AC1-ERR: a pid that survives every window never confirms; the
    /// caller's refusal carries what ran.
    #[test]
    fn ac1_err_surviving_everything_reads_not_confirmed() {
        let log = Shared::new(RefCell::new(Log::default()));
        let seams = seams(
            log.clone(),
            vec![PaneSighting {
                session: "main".into(),
                pane_id: 2034,
                child_pid: Some(22287),
            }],
            Ok(true),
            false,
            false,
            false,
        );
        let stop = stop_pane_process_confirmed_with(&test_row(), &seams);
        assert!(!stop.confirmed);
        assert_eq!(
            log.borrow().signals,
            vec![(22287, libc::SIGTERM), (22287, libc::SIGKILL)]
        );
        assert!(
            stop.detail.ends_with("pid 22287 still alive"),
            "{}",
            stop.detail
        );
    }

    /// AC1-EDGE: no verified pid, or a pid whose start time does not match,
    /// kills nothing and signals nothing.
    #[test]
    fn ac1_edge_unverified_pid_never_kills_or_signals() {
        let log = Shared::new(RefCell::new(Log::default()));
        let no_pid = pane_row(None, Some(("main", 1991)));
        let sx = seams(
            log.clone(),
            vec![PaneSighting {
                session: "main".into(),
                pane_id: 2034,
                child_pid: Some(22287),
            }],
            Ok(true),
            true,
            true,
            true,
        );
        let stop = stop_pane_process_confirmed_with(&no_pid, &sx);
        assert!(!stop.confirmed);
        assert_eq!(
            stop.detail,
            "pane row carries no verified pid; the stop cannot be proven"
        );
        assert!(log.borrow().kills.is_empty() && log.borrow().signals.is_empty());

        // Recycled pid: the ownership probe refuses, nothing runs.
        let log2 = Shared::new(RefCell::new(Log::default()));
        let mut sx2 = seams(
            log2.clone(),
            vec![PaneSighting {
                session: "main".into(),
                pane_id: 2034,
                child_pid: Some(22287),
            }],
            Ok(true),
            true,
            true,
            true,
        );
        sx2.pid_ours = Box::new(|_, _| false);
        let stop2 = stop_pane_process_confirmed_with(&test_row(), &sx2);
        assert!(!stop2.confirmed);
        assert!(log2.borrow().kills.is_empty() && log2.borrow().signals.is_empty());
    }

    /// The no-pane arm: a keeper can hold the child with no pane; SIGTERM
    /// alone can be what ends it.
    #[test]
    fn no_pane_hosts_pid_escalates_to_sigterm_and_confirms() {
        let log = Shared::new(RefCell::new(Log::default()));
        let seams = seams(log.clone(), vec![], Ok(true), false, true, true);
        let stop = stop_pane_process_confirmed_with(&test_row(), &seams);
        assert!(stop.confirmed);
        assert!(log.borrow().kills.is_empty());
        assert!(stop.detail.contains("no live pane hosts pid 22287"));
    }

    /// AC1-RM, the mapping half: `pane_removed` follows `confirmed`, and
    /// the stop detail is the printed reason in every branch.
    #[test]
    fn rm_pane_outcome_maps_confirmed_to_removed_and_names_the_pid() {
        let yes = PaneStop {
            confirmed: true,
            detail: "pane main:2034 (child 22287) killed; pid 22287 gone".into(),
        };
        let (outcome, detail) = rm_pane_outcome(&yes);
        assert_eq!(outcome, crate::daemon::CascadeOutcome::Removed);
        assert_eq!(
            detail.as_deref(),
            Some("pane main:2034 (child 22287) killed; pid 22287 gone")
        );

        let no = PaneStop {
            confirmed: false,
            detail: "pane row carries no verified pid; the stop cannot be proven".into(),
        };
        let (outcome, detail) = rm_pane_outcome(&no);
        assert_eq!(
            outcome,
            crate::daemon::CascadeOutcome::Failed(no.detail.clone())
        );
        assert!(detail.is_none());
    }

    /// The absence vocabulary survived the move unchanged.
    #[test]
    fn mux_pane_is_absent_vocabulary_is_intact() {
        assert!(mux_pane_is_absent("fno mux: no such pane: 24"));
        assert!(mux_pane_is_absent(
            "cannot reach session main: no such file or directory"
        ));
        assert!(!mux_pane_is_absent("mux configuration not found"));
        assert!(!mux_pane_is_absent("fno mux: permission denied"));
    }
}

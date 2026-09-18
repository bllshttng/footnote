//! The verb-level daemon swap (`fno-agents restart`): run the swap and
//! render its receipts. Lives in the library so the bin stays under the
//! file budget; the pure renderer's unit tests ride the bin's test module.

use serde_json::json;

use crate::client::{
    check_daemon_drift, resolve_daemon_bin, restart_daemon, RestartError, RestartOutcome,
};
use crate::drift::DriftState;
use crate::paths::AgentsHome;
use crate::AgentStatus;

/// One live thread row, read from the registry before and after the daemon
/// swap: the preserved/lost comparison keys on the FULL harness session id,
/// never the name (a row that comes back under the same name with a
/// different session id is lost, and the receipt says so).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ThreadRow {
    pub(crate) name: String,
    pub(crate) harness: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) keeper_child_pid: Option<u32>,
}

/// Read the registry's live thread rows from disk (the same shared-lock
/// read every other reader takes). Best-effort: an unreadable registry is
/// an empty snapshot, and the preserved/lost receipt names that.
pub(crate) fn read_thread_rows(home: &AgentsHome) -> Vec<ThreadRow> {
    let Ok(registry) = crate::state::load_registry(&home.registry_json()) else {
        return Vec::new();
    };
    registry
        .entries
        .iter()
        .filter(|row| {
            matches!(
                row.status,
                AgentStatus::Ready | AgentStatus::Idle | AgentStatus::Spawning | AgentStatus::Live
            )
        })
        .filter(|row| row.harness_session_id.is_some() || row.keeper_child_pid.is_some())
        .map(|row| ThreadRow {
            name: row.name.clone(),
            harness: row.harness.clone(),
            session_id: row.harness_session_id.clone(),
            keeper_child_pid: row.keeper_child_pid,
        })
        .collect()
}

/// The `--if-drifted` gate: only a measured `Drifted` daemon earns a swap. A
/// down daemon runs no old build, and `Unknown` never swaps on a guess; the
/// plain restart stays the remedy when a caller really wants one.
fn restart_gate(state: &DriftState) -> bool {
    matches!(state, DriftState::Drifted { .. })
}

/// Render the codex upgrade outcome into (stdout lines, stderr lines,
/// failed). Pure and unit-tested: `failed` ONLY on [`UpgradeOutcome::Failed`]
/// - a held or refused upgrade is reported and NOT failed, because the
/// transaction refused BEFORE mutating and the fleet is otherwise healed.
pub(crate) fn render_upgrade(
    outcome: &crate::codex_daemon_upgrade::UpgradeOutcome,
) -> (Vec<String>, Vec<String>, bool) {
    use crate::codex_daemon_upgrade::UpgradeOutcome;
    match outcome {
        UpgradeOutcome::Absent => (
            Vec::new(),
            vec!["fno agents restart: no codex CLI on this machine; the shared-daemon upgrade leg is skipped.".to_string()],
            false,
        ),
        UpgradeOutcome::ReusedCurrent { installed, live } => {
            let say = format!(
                "fno agents restart: codex app-server reused (installed {}, live {}).",
                installed.as_deref().unwrap_or("unreadable"),
                live.as_deref().unwrap_or("unreadable"),
            );
            (vec![say], Vec::new(), false)
        }
        UpgradeOutcome::Held {
            kind: _,
            reason,
            installed,
            live,
            pid,
        } => {
            let say = format!(
                "fno agents restart: codex app-server held ({reason}; installed {}, live {}, pid {}).",
                installed.as_deref().unwrap_or("unreadable"),
                live.as_deref().unwrap_or("unreadable"),
                pid.map(|p| p.to_string()).unwrap_or_else(|| "?".to_string()),
            );
            (Vec::new(), vec![say], false)
        }
        UpgradeOutcome::Refused { reason, threads } => {
            let say = format!(
                "fno agents restart: codex upgrade refused before mutation ({reason}); {} thread(s) hold the daemon as-is.",
                threads.len()
            );
            (Vec::new(), vec![say], false)
        }
        UpgradeOutcome::Failed {
            reason,
            threads,
            missing_ids,
        } => {
            let say = format!(
                "fno agents restart: codex upgrade FAILED ({reason}); snapshot {} thread(s), missing after restart: [{}]. NO success is claimed.",
                threads.len(),
                missing_ids.join(", "),
            );
            (Vec::new(), vec![say], true)
        }
        UpgradeOutcome::Upgraded {
            before,
            after,
            threads,
            config_unchanged,
            model_context_window_before,
            model_context_window_after,
        } => {
            let say = format!(
                "fno agents restart: codex app-server upgraded ({} -> {}, {} thread(s) re-read; config {}; model_context_window {:?} -> {:?}).",
                before.get("live").and_then(|v| v.as_str()).unwrap_or("?"),
                after.get("live").and_then(|v| v.as_str()).unwrap_or("?"),
                threads.len(),
                if *config_unchanged { "unchanged" } else { "CHANGED" },
                model_context_window_before,
                model_context_window_after,
            );
            (vec![say], Vec::new(), false)
        }
    }
}

/// The verdict fold: a failed daemon leg, a failed mux leg, a failed codex
/// upgrade leg, or a spared store keeper each mean the fleet is NOT healed.
/// Pure so the contract "every unhealed leg fails the verb" is unit-testable.
fn verdict(
    code: i32,
    mux_failed: bool,
    spared_keeper: bool,
    upgrade_failed: bool,
) -> (bool, &'static str) {
    let ok = code == 0 && !mux_failed && !spared_keeper && !upgrade_failed;
    (ok, if ok { "ok" } else { "FAILED" })
}

/// Render a restart outcome into (stdout line, optional stderr line, exit code).
/// Pure so the observable states (swapped / forced / was-down / failed) are unit
/// testable without spawning a daemon. A failure always carries a stderr line
/// and a nonzero code (Locked Decision: a failed restart is loud, never a silent
/// "restarted"); the forced arm records that a process was KILLED, not drained,
/// and a note (e.g. --force declining a recycled pid) rides on stderr at exit 0.
pub fn render_restart(
    outcome: &Result<RestartOutcome, RestartError>,
) -> (Option<String>, Option<String>, i32) {
    match outcome {
        Ok(RestartOutcome {
            old_pid: Some(old),
            new_pid,
            forced: false,
            note,
        }) => (
            Some(format!("restarted: pid {old} -> {new_pid}")),
            note.clone(),
            0,
        ),
        Ok(RestartOutcome {
            old_pid: Some(old),
            new_pid,
            forced: true,
            note: Some(note),
        }) => (
            // Escalated graceful restart: `forced: true` + a note only arises
            // here, so the note is the discriminator.
            Some(format!("restarted (escalated): pid {old} -> {new_pid}")),
            Some(note.clone()),
            0,
        ),
        Ok(RestartOutcome {
            old_pid: Some(old),
            new_pid,
            forced: true,
            note: None,
        }) => (
            Some(format!("forced: killed pid {old} -> {new_pid}")),
            None,
            0,
        ),
        Ok(RestartOutcome {
            old_pid: None,
            new_pid,
            forced: _,
            note,
        }) => (
            Some(format!(
                "daemon was not running; started fresh (pid {new_pid})"
            )),
            note.clone(),
            0,
        ),
        Err(e) => (None, Some(format!("fno-agents: {e}")), 1),
    }
}

/// Dispatch `fno-agents restart`: swap a (possibly stale) daemon for one built
/// from the current binary. SIGTERM the running daemon (graceful drain; PTY
/// workers survive), wait for the socket to clear, lazy-start fresh. With
/// `force`, SIGKILL the lockfile holder first and lazy-start fresh. The mux
/// leg (default `--stale-idle`; every live session with `mux`) rides the
/// shared kill-selector in the fno crate, so the refusal and the preserved
/// list reach the operator instead of being captured and dropped.
/// With `json`, stdout carries ONE machine line (the summary object the
/// Python adapter parses); every human receipt moves to stderr.
pub async fn run_restart(force: bool, json: bool, if_drifted: bool, mux: bool) -> i32 {
    let home = AgentsHome::from_env();
    if if_drifted && !restart_gate(&check_daemon_drift(&home).await) {
        return 0;
    }
    let daemon_bin = resolve_daemon_bin();
    // The thread snapshot: taken BEFORE the swap, compared after by FULL
    // session id - a row that comes back under the same name with a
    // different id is lost, and the receipt says so.
    let before = read_thread_rows(&home);
    let outcome = restart_daemon(&home, &daemon_bin, force).await;
    let old_pid = outcome.as_ref().ok().and_then(|o| o.old_pid);
    let new_pid = outcome.as_ref().ok().map(|o| o.new_pid);
    let (out, err, code) = render_restart(&outcome);
    // In --json mode stdout stays parseable: the keepers line is the machine
    // surface, so human receipts land on stderr beside the errors.
    let say = |line: &str| {
        if json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };
    if let Some(line) = out {
        say(&line);
    }
    if let Some(line) = err {
        eprintln!("{line}");
    }
    if code != 0 {
        return code;
    }
    // The preserved/lost read: after the fresh daemon answers, re-read the
    // registry and compare by full session id. Every before-row that is
    // still present with the same id prints as preserved; one that is gone
    // or changed prints as lost with what the row now carries.
    let after = read_thread_rows(&home);
    for row in &before {
        let kept = after.iter().find(|candidate| {
            candidate.session_id.is_some() && candidate.session_id == row.session_id
        });
        match kept {
            Some(_) => say(&format!(
                "fno agents restart: thread {} ({}) preserved with session {}.",
                row.name,
                row.harness.as_deref().unwrap_or("unknown"),
                row.session_id.clone().unwrap_or_default(),
            )),
            None => say(&format!(
                "fno agents restart: thread {} ({}) LOST across the restart (session {:?}).",
                row.name,
                row.harness.as_deref().unwrap_or("unknown"),
                row.session_id,
            )),
        }
    }
    // change 6: cycle the stale store keepers. A store cycle ends
    // nothing a person can see (the graph on disk survives; the next read
    // respawns the keeper on this binary), so this leg is not behind
    // --force/--mux gating. Spared keepers fail the verb: a spared keeper
    // was NOT healed.
    let (cycled, stale_panes) = crate::census::cycle_stale_store_keepers().await;
    for c in &cycled {
        if c.result == "cycled" {
            say(&format!(
                "fno agents restart: store keeper {} pid {:?} shut down (stale build; respawns on next read).",
                c.graph.as_deref().unwrap_or("unknown graph"),
                c.old_pid
            ));
        } else {
            eprintln!(
                "fno agents restart: store keeper {} {}; it was NOT refreshed.",
                c.graph.as_deref().unwrap_or("unknown graph"),
                c.result
            );
        }
    }
    if stale_panes > 0 {
        say(&format!(
            "fno agents restart: {stale_panes} pane keeper(s) run an older build; kept with their panes, current when each pane ends."
        ));
    }
    // The codex shared-daemon leg: the session-preserving upgrade
    // transaction, riding the SAME restart receipt as every other
    // component. Reused-current, held, refused, upgraded, or failed - and
    // ONLY a post-restart verification failure fails the verb.
    let upgrade_outcome = crate::codex_daemon_upgrade::codex_daemon_upgrade_transaction().await;
    let (up_out, up_err, upgrade_failed) = render_upgrade(&upgrade_outcome);
    for line in &up_out {
        say(line);
    }
    for line in &up_err {
        eprintln!("{line}");
    }
    // The pr-watch LaunchAgent embeds an absolute binary path that a daemon
    // swap never re-renders (`fno agents restart` reaches no launchd job);
    // re-render and bounce it, the same tail `fno doctor update` appends.
    // The verb self-gates on pr_watch.enabled and prints its own skip line
    // then, so this is not behind --force, matching the keeper cycle above.
    // Receipt lands BEFORE the summary, which stays the last stdout line.
    match std::process::Command::new(crate::scrape::fno_bin())
        .args(["do", "pr", "watch", "refresh"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let said = if out.stderr.is_empty() {
                String::from_utf8_lossy(&out.stdout)
            } else {
                String::from_utf8_lossy(&out.stderr)
            };
            let said = said.trim();
            if said.is_empty() {
                say("fno agents restart: pr-watch refreshed.");
            } else {
                say(&format!("fno agents restart: {said}"));
            }
        }
        Ok(out) => eprintln!(
            "fno agents restart: pr-watch refresh failed (rc={}); run `fno do pr watch refresh` by hand.",
            out.status.code().unwrap_or(-1)
        ),
        Err(e) => eprintln!("fno agents restart: pr-watch refresh not run: {e}."),
    }
    // The mux leg: pane-less stale-wire servers heal automatically; --mux
    // adds stale-with-panes and every current-wire session. The shared
    // kill-selector owns the session policy and its refusal - this verb
    // holds none of its own. Its JSON object folds into the summary below,
    // so the operator sees the preserved list the kill measured.
    let selector = if mux { "--all" } else { "--stale-idle" };
    let mux_out = std::process::Command::new(crate::scrape::fno_bin())
        .args(["mux", "kill-server", selector, "--json"])
        .output();
    // The selector's stderr carries the human narratives (the spared line,
    // the unkept refusal): relay them - capturing and dropping them is the
    // exact gap that deleted this leg from the operator's view.
    if let Ok(out) = &mux_out {
        let stderr = String::from_utf8_lossy(&out.stderr);
        for line in stderr.lines().filter(|l| !l.trim().is_empty()) {
            eprintln!("{line}");
        }
    }
    let mux_summary: Option<serde_json::Value> = match &mux_out {
        Ok(out) => serde_json::from_slice(&out.stdout).ok().or(None),
        Err(_) => None,
    };
    let mux_failed = match &mux_out {
        Ok(out) => !out.status.success(),
        Err(_) => true,
    };
    if mux_failed {
        eprintln!("fno agents restart: the mux leg failed; its refusal is above.");
    }

    // Machine-readable summary; the LAST stdout line, so an orchestrator
    // parses it without guessing. `components` names one row per component
    // (old pid -> new pid, or unchanged); `preserved` folds every session's
    // kept panes and threads into one list.
    let mut components = vec![json!({
        "kind": "daemon",
        "name": "agents-daemon",
        "old_pid": old_pid,
        "new_pid": new_pid,
    })];
    components.push(json!({
        "kind": "codex-app-server",
        "outcome": serde_json::to_value(&upgrade_outcome).unwrap_or(serde_json::Value::Null),
    }));
    let mut preserved: Vec<serde_json::Value> = Vec::new();
    for row in &after {
        if let Some(sid) = &row.session_id {
            preserved.push(json!({
                "kind": "thread",
                "name": row.name,
                "session_id": sid,
            }));
        }
    }
    if let Some(mux) = &mux_summary {
        if let Some(sessions) = mux.get("sessions").and_then(serde_json::Value::as_array) {
            for session in sessions {
                let name = session
                    .get("session")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                if session.get("killed").and_then(serde_json::Value::as_bool) == Some(true) {
                    components.push(json!({
                        "kind": "mux-server",
                        "name": name,
                        "old_pid": serde_json::Value::Null,
                        "new_pid": "unchanged",
                    }));
                }
                if let Some(rows) = session
                    .get("preserved")
                    .and_then(serde_json::Value::as_array)
                {
                    preserved.extend(rows.iter().cloned());
                }
            }
        }
    }
    let summary = json!({
        "daemon": if code == 0 { "restarted" } else { "failed" },
        "components": components,
        "preserved": preserved,
        "store_keepers": cycled.iter().map(|c| serde_json::json!({
            "graph": c.graph, "old_pid": c.old_pid, "result": c.result,
        })).collect::<Vec<_>>(),
        "pane_keepers_stale": stale_panes,
        "mux": mux_summary,
        "ok": verdict(code, mux_failed, cycled.iter().any(|c| c.result != "cycled"), upgrade_failed).0,
        "verdict": verdict(code, mux_failed, cycled.iter().any(|c| c.result != "cycled"), upgrade_failed).1,
    });
    println!("fno agents restart: keepers {summary}");
    u8::from(cycled.iter().any(|c| c.result != "cycled") || mux_failed || upgrade_failed) as i32
}

#[cfg(test)]
mod tests {
    use super::{render_upgrade, restart_gate, verdict};
    use crate::drift::{classify, ExeFingerprint};
    use std::path::PathBuf;

    #[test]
    fn every_unhealed_leg_fails_the_verdict() {
        assert_eq!(verdict(0, false, false, false), (true, "ok"));
        assert_eq!(
            verdict(1, false, false, false),
            (false, "FAILED"),
            "a failed daemon leg"
        );
        assert_eq!(
            verdict(0, true, false, false),
            (false, "FAILED"),
            "a failed mux leg"
        );
        assert_eq!(
            verdict(0, false, true, false),
            (false, "FAILED"),
            "a spared store keeper"
        );
        assert_eq!(
            verdict(0, false, false, true),
            (false, "FAILED"),
            "a failed codex upgrade leg"
        );
    }

    #[test]
    fn render_restart_names_both_pids_and_the_forced_path() {
        use super::render_restart;
        use crate::client::{RestartError, RestartOutcome};
        let ok = Ok(RestartOutcome {
            old_pid: Some(100),
            new_pid: 200,
            forced: false,
            note: None,
        });
        let (out, err, code) = render_restart(&ok);
        assert_eq!(out.as_deref(), Some("restarted: pid 100 -> 200"));
        assert!(err.is_none());
        assert_eq!(code, 0);

        let was_down = Ok(RestartOutcome {
            old_pid: None,
            new_pid: 200,
            forced: false,
            note: None,
        });
        let (out, err, code) = render_restart(&was_down);
        assert!(out
            .unwrap()
            .starts_with("daemon was not running; started fresh"));
        assert!(err.is_none());
        assert_eq!(code, 0);

        let failed: Result<RestartOutcome, RestartError> = Err(RestartError::SigkillFailed {
            pid: 5,
            reason: "EPERM".into(),
        });
        let (out, err, code) = render_restart(&failed);
        assert!(out.is_none());
        assert!(err.unwrap().contains("fno-agents:"));
        assert_eq!(
            code, 1,
            "a failed restart is loud, never a silent restarted"
        );
    }

    #[test]
    fn render_restart_relayed_the_note_once_on_success() {
        use super::render_restart;
        use crate::client::RestartOutcome;
        let escalated = Ok(RestartOutcome {
            old_pid: Some(1),
            new_pid: 2,
            forced: true,
            note: Some("note: declining recycled pid".into()),
        });
        let (out, err, _code) = render_restart(&escalated);
        assert!(out.unwrap().contains("restarted (escalated)"));
        assert_eq!(
            err.as_deref(),
            Some("note: declining recycled pid"),
            "the note rides once, on stderr"
        );
    }

    #[test]
    fn render_upgrade_says_reused_when_current() {
        use crate::codex_daemon_upgrade::UpgradeOutcome;
        let (out, err, failed) = render_upgrade(&UpgradeOutcome::ReusedCurrent {
            installed: Some("0.154.0".into()),
            live: Some("0.154.0".into()),
        });
        assert!(out[0].contains("reused"));
        assert!(out[0].contains("0.154.0"));
        assert!(err.is_empty());
        assert!(!failed);
    }

    #[test]
    fn render_upgrade_refusal_is_loud_but_not_failed() {
        use crate::codex_daemon_upgrade::{HoldKind, UpgradeOutcome};
        let thread = crate::codex_daemon_upgrade::SnapshotThread {
            id: "thr-1".into(),
            cwd: "/w".into(),
            status: Some("active".into()),
            fno_row: None,
        };
        let refused = UpgradeOutcome::Refused {
            reason: "thread thr-1 has an active turn".into(),
            threads: vec![thread],
        };
        let (out, err, failed) = render_upgrade(&refused);
        assert!(out.is_empty());
        assert!(err[0].contains("refused before mutation"));
        assert!(!failed, "refusal is a hold, not a failure");
        let held = UpgradeOutcome::Held {
            kind: HoldKind::NotStale,
            reason: "readiness unreadable".into(),
            installed: None,
            live: None,
            pid: Some(9),
        };
        let (out, err, failed) = render_upgrade(&held);
        assert!(out.is_empty());
        assert!(err[0].contains("held"));
        assert!(!failed, "a hold is reported, not failed");
    }

    #[test]
    fn render_upgrade_failed_claims_no_success() {
        use crate::codex_daemon_upgrade::UpgradeOutcome;
        let failed = UpgradeOutcome::Failed {
            reason: "threads missing after the restart".into(),
            threads: vec![],
            missing_ids: vec!["thr-9".into()],
        };
        let (out, err, failed) = render_upgrade(&failed);
        assert!(out.is_empty());
        assert!(err[0].contains("FAILED"));
        assert!(err[0].contains("thr-9"));
        assert!(failed, "only a failed upgrade fails the verb");
    }

    #[test]
    fn gate_swaps_only_on_measured_drift() {
        let fp = ExeFingerprint {
            path: PathBuf::from("/x"),
            mtime_nanos: 1,
            size: 1,
        };
        let drifted = classify(
            Some(&fp),
            Some(&ExeFingerprint {
                size: 2,
                ..fp.clone()
            }),
        );
        assert!(restart_gate(&drifted));
        assert!(!restart_gate(&classify(Some(&fp), Some(&fp))));
        assert!(!restart_gate(&classify(None, Some(&fp))));
    }
}

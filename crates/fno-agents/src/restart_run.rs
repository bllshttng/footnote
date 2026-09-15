//! The verb-level daemon swap (`fno-agents restart`): run the swap and
//! render its receipts. Lives in the library so the bin stays under the
//! file budget; the pure renderer's unit tests ride the bin's test module.

use serde_json::json;

use crate::client::{
    check_daemon_drift, resolve_daemon_bin, restart_daemon, RestartError, RestartOutcome,
};
use crate::drift::DriftState;
use crate::paths::AgentsHome;

/// The `--if-drifted` gate: only a measured `Drifted` daemon earns a swap. A
/// down daemon runs no old build, and `Unknown` never swaps on a guess; the
/// plain restart stays the remedy when a caller really wants one.
fn restart_gate(state: &DriftState) -> bool {
    matches!(state, DriftState::Drifted { .. })
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
/// `force`, SIGKILL the lockfile holder first and lazy-start fresh (x-3498).
/// With `json`, stdout carries ONE machine line (the keepers summary the
/// Python adapter parses); every human receipt moves to stderr.
pub async fn run_restart(force: bool, json: bool, if_drifted: bool) -> i32 {
    let home = AgentsHome::from_env();
    if if_drifted && !restart_gate(&check_daemon_drift(&home).await) {
        return 0;
    }
    let daemon_bin = resolve_daemon_bin();
    let outcome = restart_daemon(&home, &daemon_bin, force).await;
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
    // x-f188 change 6: cycle the stale store keepers. A store cycle ends
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
    // Machine-readable summary; the LAST stdout line, so an orchestrator
    // parses it without guessing.
    let summary = json!({"store_keepers": cycled.iter().map(|c| serde_json::json!({
            "graph": c.graph, "old_pid": c.old_pid, "result": c.result,
        })).collect::<Vec<_>>(), "pane_keepers_stale": stale_panes});
    println!("fno agents restart: keepers {summary}");
    u8::from(cycled.iter().any(|c| c.result != "cycled")) as i32
}

#[cfg(test)]
mod tests {
    use super::restart_gate;
    use crate::drift::{classify, ExeFingerprint};
    use std::path::PathBuf;

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

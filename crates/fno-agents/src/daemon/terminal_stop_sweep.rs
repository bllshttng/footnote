//! The terminal-stop sweep: every tick, spend the terminal-stop markers the
//! client left for bg sessions that reached end-of-turn. Extracted verbatim
//! from daemon.rs (shrink-only) beside its claude_stop arms; the wake-name
//! tombstone stamp rides the confirmed-stop write.

use super::claude_stop;
use super::claude_stop::end_survivors;
use super::update_registry_offloaded;
use crate::events::EventEmitter;
use crate::paths::AgentsHome;
use crate::state;
use serde_json::json;
use std::time::Duration;

pub(crate) async fn terminal_stop_sweep(home: &AgentsHome, emitter: &EventEmitter) {
    // read_markers (dir list + N file reads) and the roster load/parse are
    // blocking fs; run them off the async runtime so a slow disk or a large
    // marker dir never stalls a tokio worker thread. Returns the markers plus
    // the roster load result (an ERROR is kept distinct from a MISSING roster).
    let home_read = home.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let markers = crate::terminal_stop::read_markers(&home_read);
        if markers.is_empty() {
            return (markers, None);
        }
        let roster = crate::claude_roster::ClaudeRoster::load_default();
        (markers, Some(roster))
    })
    .await;
    let (markers, roster) = match loaded {
        Ok(v) => v,
        Err(e) => {
            eprintln!("daemon: terminal-stop sweep: read task failed: {e}");
            return;
        }
    };
    if markers.is_empty() {
        return;
    }
    // A load ERROR (e.g. a torn read while Claude rewrites roster.json, or a
    // future roster-format drift) must NOT be read as "session absent" — that
    // would delete every marker as stale and permanently leak the parked
    // workers this sweep exists to stop. Skip the tick and retry next time;
    // markers persist. A MISSING roster is a benign empty (Ok), correctly
    // yielding RemoveStale for a genuinely untracked session.
    let roster = match roster {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            eprintln!("daemon: terminal-stop sweep: roster load failed: {e} (retry next tick)");
            return;
        }
        None => return,
    };
    for marker in markers {
        let short = roster.find(&marker.uuid).map(|w| w.short_id().to_string());
        match crate::terminal_stop::stop_decision(short) {
            crate::terminal_stop::StopAction::Stop(short) => {
                // Bound the subprocess so a hung `claude` can never wedge the
                // sweep. A timeout leaves the marker for the next tick, since
                // it is retried every tick, which is the failure this feature
                // exists to prevent.
                let stopped =
                    crate::lifecycle_child::bounded_claude_stop(&short, Duration::from_secs(15))
                        .await;
                match stopped {
                    // retired-ok: a daemon log line naming its own teardown call.
                    Err(_) => eprintln!("daemon: claude stop {short} timed out (retry next tick)"),
                    Ok(Ok(o)) if o.status.success() => {
                        // A stop exit is a receipt, not a proof. The marker is
                        // only spent on a proved end: a survivor keeps its
                        // marker, so the next tick retries instead of the row
                        // reading stopped over a live process. No proof at
                        // all (roster unreadable, no worker named) refuses
                        // the same way: no record, no marker spend.
                        match claude_stop::prove_target(&short, Some(marker.uuid.as_str())) {
                            Ok(members) => {
                                let (_signalled, survivors) = end_survivors(&members).await;
                                if !survivors.is_empty() {
                                    let listed = survivors
                                        .iter()
                                        .map(|pid| pid.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    eprintln!(
                                        // retired-ok: a daemon log line naming its own teardown call.
                                        "daemon: terminal-stop sweep: claude stop {short} returned \
                                         but pid {listed} survived the signal; the marker stays for \
                                         the next tick. The override for a session claude's own \
                                         supervisor respawns is `fno agents rm`."
                                    );
                                    continue;
                                }
                            }
                            Err(reason) => {
                                eprintln!(
                                    "daemon: terminal-stop sweep: no process proof for \
                                     {short} ({reason}); the marker stays for the next tick"
                                );
                                continue;
                            }
                        }
                        let _ = emitter.emit(
                            "bg_worker_terminal_stopped",
                            &json!({
                                "short_id": short,
                                "session_id": marker.uuid,
                                "reason": marker.reason,
                            }),
                        );
                        // The row learns fno did this: the stamp keeps the
                        // sweep from reading the harness `stopped` state as
                        // finished work on a later tick. The write is
                        // offloaded like every other registry write on the
                        // async runtime.
                        let sweep_home = home.clone();
                        let stopped_session = marker.uuid.clone();
                        let stopped_reason = marker.reason.clone();
                        let _ = update_registry_offloaded(sweep_home.registry_json(), move |r| {
                            if let Some(entry) = r.entries.iter_mut().find(|e| {
                                e.harness_session_id.as_deref() == Some(stopped_session.as_str())
                            }) {
                                state::record_stop(entry, "terminal-sweep", Some(stopped_reason));
                                // best-effort: a failed stamp costs a later
                                // wake its name, never the stop.
                                let sid = entry.harness_session_id.clone().unwrap_or_default();
                                let n = entry.name.clone();
                                crate::wake_name::record(&sweep_home, &sid, &n);
                            }
                        })
                        .await;
                        crate::terminal_stop::remove_marker(home, &marker.uuid);
                    }
                    // Non-fatal: leave the marker so the next tick retries.
                    Ok(Ok(o)) => eprintln!(
                        // retired-ok: a daemon log line naming its own teardown call.
                        "daemon: claude stop {short} failed: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                    Ok(Err(e)) => eprintln!("daemon: could not exec `claude stop`: {e}"),
                }
            }
            // The session already exited on its own (or a prior tick stopped it):
            // drop the stale marker so the dir does not grow without bound.
            crate::terminal_stop::StopAction::RemoveStale => {
                crate::terminal_stop::remove_marker(home, &marker.uuid);
            }
        }
    }
}

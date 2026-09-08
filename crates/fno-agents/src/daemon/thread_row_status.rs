//! Writers for a codex thread row's status (the file-budget home of the
//! thread-status question in `daemon.rs`): the per-completion row bump and
//! the driver-sourced inside-leg reports. Everything here is invoked from
//! `daemon.rs`'s spawn/resume paths; the seq gate is shared with the claude
//! hook's buffered flush.

use super::{
    entry_holds_session, inside_leg_state_str, is_non_terminal, notify_transition,
    now_rfc3339_like, update_registry_offloaded,
};
use crate::codex_thread::TurnReceipt;
use crate::events::EventEmitter;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// The per-completion hook every codex-thread actor gets at construction:
/// bump the row (`Live` + `last_message_at`) and emit `agent_ask_done`, for
/// every submitter class (ask, seed, mail steer) in ONE place - previously
/// the ask path and the seed task each kept their own copy of this.
pub(super) fn codex_thread_on_done(
    emitter: &EventEmitter,
    registry_path: std::path::PathBuf,
    name: &str,
) -> Arc<dyn Fn(TurnReceipt) + Send + Sync> {
    let emitter = emitter.clone();
    let name = name.to_string();
    Arc::new(move |receipt: TurnReceipt| {
        let emitter = emitter.clone();
        let name = name.clone();
        let turn_id = receipt.turn_id.clone();
        let status = receipt.status.clone();
        let registry_path = registry_path.clone();
        tokio::spawn(async move {
            let bump_name = name.clone();
            let _ = update_registry_offloaded(registry_path, move |registry| {
                if let Some(entry) = registry.find_mut(&bump_name) {
                    // The completion of an INTERRUPTED turn must not
                    // resurrect a row the stop path just settled Exited:
                    // the two writes race on the offloaded queue, and only
                    // a non-terminal row reads Live (the same rule the
                    // recovery pass applies to its own writes).
                    if is_non_terminal(entry.status) {
                        entry.status = crate::AgentStatus::Live;
                        entry.last_message_at = Some(now_rfc3339_like());
                    }
                }
            })
            .await;
            let _ = emitter.emit(
                "agent_ask_done",
                &json!({
                    "name": name,
                    "backend": "codex-thread",
                    "turn_id": turn_id,
                    "turn_status": status,
                }),
            );
        });
    })
}

/// The codex thread driver's inside-leg write (x-fd66): the daemon maps the
/// actor's turn phases onto reports and lands them on the row that holds the
/// thread's session id, through the same seq-gated core the claude hook's
/// flush uses, so the Done transition notifies exactly as a claude Stop hook
/// does. `first_seq` starts at 1 for a fresh spawn; a resume seeds it above
/// the row's current seq so the resumed thread's first report clears the gate
/// instead of dying under the previous incarnation's seq.
pub(super) fn codex_thread_on_status(
    emitter: &EventEmitter,
    registry_path: std::path::PathBuf,
    name: &str,
    session_id: &str,
    first_seq: u64,
    notify_on_blocked: bool,
    notify_on_done: bool,
) -> Arc<dyn Fn(crate::codex_thread::ThreadTurnPhase) + Send + Sync> {
    let emitter = emitter.clone();
    let name = name.to_string();
    let session_id = session_id.to_string();
    let seq = Arc::new(std::sync::atomic::AtomicU64::new(first_seq));
    Arc::new(move |phase| {
        let seq = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let rep = crate::state::InsideLegReport {
            state: match phase {
                crate::codex_thread::ThreadTurnPhase::Working => {
                    crate::state::InsideLegState::Working
                }
                crate::codex_thread::ThreadTurnPhase::Done => crate::state::InsideLegState::Done,
            },
            seq,
            reason: None,
            received_at: now_rfc3339_like(),
            ttl_ms: match phase {
                // A working report ages out when the driver stops answering;
                // a done report is a stable fact until the next turn.
                crate::codex_thread::ThreadTurnPhase::Working => {
                    Some(crate::state::THREAD_TURN_TTL_MS)
                }
                crate::codex_thread::ThreadTurnPhase::Done => None,
            },
        };
        tokio::spawn(write_thread_inside_leg(
            registry_path.clone(),
            emitter.clone(),
            name.clone(),
            session_id.clone(),
            rep,
            notify_on_blocked,
            notify_on_done,
        ));
    })
}

/// Land one thread-driver report (x-fd66): off the actor task, through the
/// shared seq gate, notifying on the done/blocked episode edge exactly as the
/// claude hook's flush does, and emitting one event per accepted write.
async fn write_thread_inside_leg(
    registry_path: PathBuf,
    emitter: EventEmitter,
    name: String,
    session_id: String,
    rep: crate::state::InsideLegReport,
    notify_on_blocked: bool,
    notify_on_done: bool,
) {
    let (seq, state_str) = (rep.seq, inside_leg_state_str(rep.state));
    let session_for_emit = session_id.clone();
    let notify = update_registry_offloaded(registry_path, move |registry| {
        gate_inside_leg_onto_row(registry, &session_id, rep)
    })
    .await
    .unwrap_or(None);
    if let Some((body, is_done)) = notify {
        let want = if is_done {
            notify_on_done
        } else {
            notify_on_blocked
        };
        if want {
            notify_transition(name.clone(), body);
        }
    }
    let _ = emitter.emit(
        "codex_thread_inside_leg",
        &json!({
            "name": name,
            "session_id": session_for_emit,
            "state": state_str,
            "seq": seq,
        }),
    );
}

/// The ONE seq-gated inside-leg writer core: find the row holding
/// `session_uuid`, apply `rep` only when its seq is newer, clear the scrape
/// verdict on the flip (hook beats scrape), and return the transition notify
/// intent `(body, is_done)` when the report ENTERED blocked/done (x-dd84).
/// Both writers route through here - the claude hook's buffered flush and the
/// codex thread driver's direct write (x-fd66) - so the gate, the capability
/// clear, and the episode edge cannot drift apart. `None` on a stale-seq drop
/// or a row that holds no such session.
pub(super) fn gate_inside_leg_onto_row(
    registry: &mut crate::state::Registry,
    session_uuid: &str,
    rep: crate::state::InsideLegReport,
) -> Option<(String, bool)> {
    let (state_str, rep_state, rep_reason) = (
        inside_leg_state_str(rep.state),
        rep.state,
        rep.reason.clone(),
    );
    let mut notify: Option<(String, bool)> = None;
    if let Some(e) = registry
        .entries
        .iter_mut()
        .find(|e| entry_holds_session(e, session_uuid))
    {
        let newer = e.inside_leg.as_ref().is_none_or(|cur| rep.seq > cur.seq);
        if newer {
            let prev_state = e.inside_leg.as_ref().map(|r| r.state);
            let body = rep_reason.unwrap_or_else(|| state_str.to_string());
            if crate::state::enters(prev_state, rep_state, crate::state::InsideLegState::Blocked) {
                notify = Some((body, false));
            } else if crate::state::enters(
                prev_state,
                rep_state,
                crate::state::InsideLegState::Done,
            ) {
                notify = Some((body, true));
            }
            e.inside_leg = Some(rep);
            e.screen_state = None;
        }
    }
    notify
}

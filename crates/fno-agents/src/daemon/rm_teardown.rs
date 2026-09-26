//! The process-end legs `fno agents rm` owns since (law d-81c6da7e:
//! remove needs no prior stop).
//!
//! Two legs moved out of daemon.rs so the lifecycle verbs share them: the
//! codex thread's interrupt-settle-shutdown-actor-drop (shared with the stop
//! verb, which stamps and emits around it) and the claude background
//! thread's bounded stop, which rm now runs itself before it re-reads the
//! roster.

use super::{Ctx, InterruptOutcome};
use crate::codex_thread::stop_settle_bound;
use crate::state::RegistryEntry;
use serde_json::json;

/// Interrupt the codex thread's in-flight turn, shut the actor down, and
/// drop it from the map. Shared by the stop verb and rm: the caller that
/// keeps the row stamps and emits around the answer.
///
/// `Ok(report)` names the settled outcome (`no-turn`, `actor-gone`, an
/// interrupted-turn receipt status) and leaves a torn-down thread behind:
/// the caller may proceed to drop the row. `Err(report)` says the turn is
/// still running: the handle stays in the map, and the caller keeps the
/// row non-terminal, or refuses, for rm.
pub(crate) async fn end_codex_thread(ctx: &Ctx, name: &str) -> Result<String, String> {
    let handle = ctx.codex_threads.lock().await.get(name).cloned();
    let mut interrupt_report = "no-turn".to_string();
    let mut settled = true;
    if let Some(handle) = handle.as_ref() {
        let outcome = match tokio::time::timeout(stop_settle_bound(), handle.interrupt()).await {
            Ok(Ok(InterruptOutcome::NoTurnInFlight)) => "no-turn".to_string(),
            Ok(Ok(InterruptOutcome::Interrupted(receipt))) => receipt.status,
            Ok(Ok(InterruptOutcome::Timeout)) => {
                settled = false;
                "timeout-turn-still-running".to_string()
            }
            Ok(Err(_)) => {
                // The only Err interrupt() produces is a dead actor task
                // (send fails or the ack sender was dropped): no turn can
                // still be running and the interrupt handle died with it,
                // so teardown is already done.
                "actor-gone".to_string()
            }
            Err(_) => {
                settled = false;
                "interrupt-failed-turn-still-running: stop exchange timed out".to_string()
            }
        };
        interrupt_report = outcome;
        if settled {
            let _ = handle.shutdown().await;
        }
    }
    if !settled {
        return Err(interrupt_report);
    }
    ctx.codex_threads.lock().await.remove(name);
    Ok(interrupt_report)
}

/// rm's codex-thread arm: end the thread, then refuse unless `--force` was
/// passed. `Some` carries the Busy refusal text that keeps the row; `None`
/// means rm proceeds to drop the row (teardown done, or the force override).
pub(crate) async fn codex_rm_refusal(ctx: &Ctx, name: &str, force: bool) -> Option<String> {
    if let Err(interrupt_report) = end_codex_thread(ctx, name).await {
        if !force {
            return Some(format!(
                "agent {name}: the codex thread's turn did not settle \
                 ({interrupt_report}); the registry row and the codex index \
                 entry are kept"
            ));
        }
        // The row drops without a settled teardown, so the map entry goes
        // with it: ensure_codex_thread_handle hands a respawned row the
        // stale actor when the name matches.
        ctx.codex_threads.lock().await.remove(name);
    }
    None
}

/// The production claude stop rm runs itself (law d-81c6da7e): one bounded
/// `claude stop <short>` whose success is the exit status. Blocking; the
/// caller hops through `off_executor`, whose blocking-permitted context a
/// fresh current-thread runtime is legal inside.
pub(crate) fn claude_stop_confirmed(short: &str) -> bool {
    let short = short.to_string();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|rt| {
            rt.block_on(async {
                matches!(
                    crate::lifecycle_child::bounded_claude_stop(&short, std::time::Duration::from_secs(15)).await,
                    Ok(Ok(output)) if output.status.success()
                )
            })
        })
        .unwrap_or(false)
}

/// The row is gone; stamp the tombstone so the harness-store healer
/// does not adopt the same session back under a fresh short-id name (the
/// adopted duplicate that then blocked resume). Any harness: the store
/// fallback adopts claude transcripts by the same door. A failed write is
/// an event, never a refused removal - the row is already gone.
pub(crate) fn stamp_removed_session_tombstone(ctx: &Ctx, entry: &RegistryEntry, name: &str) {
    let Some(session_id) = entry
        .harness_session_id
        .as_deref()
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
    else {
        return;
    };
    if let Err(tombstone_error) =
        crate::rm_tombstone::record(&ctx.home, entry.harness_name(), session_id)
    {
        let _ = ctx.emitter.emit(
            "rm_tombstone_failed",
            &json!({
                "name": name,
                "session_id": session_id,
                "error": tombstone_error,
            }),
        );
    }
}

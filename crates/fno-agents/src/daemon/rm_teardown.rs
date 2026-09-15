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

/// Interrupt the codex thread's in-flight turn, shut the actor down, and
/// drop it from the map. Shared by the stop verb and rm: the caller that
/// keeps the row stamps and emits around the answer.
///
/// `Ok(report)` names the settled outcome (`no-turn`, an interrupted-turn
/// receipt status) and leaves a torn-down thread behind: the caller may
/// proceed to drop the row. `Err(report)` says the turn is still running:
/// the handle stays in the map, and the caller keeps the row
/// non-terminal, or refuses, for rm.
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
            Ok(Err(error)) => {
                settled = false;
                format!("interrupt-failed-turn-still-running: {error}")
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

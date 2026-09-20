//! The `agent.convert` transaction: move a live pane session onto the
//! thread lane under its own session id.
//!
//! The daemon owns this rather than the client for the reason every other
//! lifecycle move is daemon-owned: the agent lock serializes it, and the
//! daemon's pid is what a mid-move claim can be pinned to. A client-side
//! conversion would pin its claims to a process that exits seconds later.
//!
//! The classification runs TWICE. Once client-side, so a refusal costs no
//! lock, and again here under the lock against a fresh read, because the
//! world moves between the two: a pane can exit, and a concurrent request
//! can convert the row first. The second answer is the one that acts.

use super::*;
use crate::convert::{ConvertPlan, ConvertRefusal, KeeperSighting, PaneRead};
use std::time::Duration;

/// `handle_rename` lives here rather than in daemon.rs for the file budget:
/// daemon.rs is over 5,000 lines and shrink-only, so the dispatch arm this
/// change adds is paid for by moving an existing body out. The behavior is
/// unchanged.
pub(super) fn handle_rename(ctx: &Ctx, req: &Request) -> Response {
    state::rename_response(&ctx.home.registry_json(), req)
}

/// How long to wait for the agent lock before refusing. A conversion is
/// seconds of work, so a wait longer than this means another writer is
/// mid-move on the same row and the caller should hear that rather than
/// queue behind it indefinitely.
const CONVERT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// What the locked classification decided. The lock travels with the plan
/// so the strategy runs while it is still held: a conversion that released
/// the lock between deciding and acting is one a second request can
/// interleave with, which is the double-writer this whole module exists to
/// prevent.
pub(super) enum Prepared {
    /// Nothing left to do: a refusal, a dry-run plan, or an idempotent hit.
    Answer(Box<Response>),
    /// Run this strategy, with the lock still held.
    Act(crate::agent_lock::AgentLock, Box<ConvertPlan>),
}

/// The async door. The locked phase is classification plus IO, so it runs
/// on the blocking pool exactly as `run_blocking` would; the strategies
/// that must await (a Codex app-server resume) then run in async context
/// with the lock still held.
pub(super) async fn handle_convert(ctx: &Arc<Ctx>, req: &Request) -> Response {
    let id = req.id;
    let prepared = {
        let ctx = Arc::clone(ctx);
        let req = req.clone();
        match tokio::task::spawn_blocking(move || prepare(&ctx, &req)).await {
            Ok(prepared) => prepared,
            Err(_) => {
                return Response::err(
                    id,
                    ErrorCode::ShuttingDown,
                    "convert: the handler was dropped before it ran",
                )
            }
        }
    };
    let (lock, plan) = match prepared {
        Prepared::Answer(response) => return *response,
        Prepared::Act(lock, plan) => (lock, plan),
    };
    match plan.strategy.as_str() {
        "keeper-rebind" => {
            let ctx = Arc::clone(ctx);
            let req = req.clone();
            let plan = plan.clone();
            match tokio::task::spawn_blocking(move || run_keeper_rebind(&ctx, &req, &plan)).await {
                Ok(response) => {
                    drop(lock);
                    response
                }
                Err(_) => Response::err(
                    id,
                    ErrorCode::ShuttingDown,
                    "convert: the rebind was dropped before it ran",
                ),
            }
        }
        "server-resume" => {
            let response = run_server_resume(ctx, req, &plan).await;
            drop(lock);
            response
        }
        "client-resume" => {
            let ctx = Arc::clone(ctx);
            let req = req.clone();
            let plan = plan.clone();
            let allow_new_id = allow_new_id(&req);
            match tokio::task::spawn_blocking(move || {
                run_client_resume(&ctx, &req, &plan, allow_new_id)
            })
            .await
            {
                Ok(response) => {
                    drop(lock);
                    response
                }
                Err(_) => Response::err(
                    id,
                    ErrorCode::ShuttingDown,
                    "convert: the relaunch was dropped before it ran",
                ),
            }
        }
        // A strategy the contract names and no arm runs refuses while NAMING
        // the plan it would have run: a refusal that shows the plan is still
        // a receipt, and it can never leave a row mid-move.
        other => Response::err(
            req.id,
            ErrorCode::Internal,
            format!(
                "convert {}: the {other} strategy is not wired yet. The plan is sound - re-run \
                 with --dry-run to read it - but no step of it has run, and the row is untouched.",
                plan.name
            ),
        ),
    }
}

/// The first hop: find this session's claims by the writer pid about to
/// stop, and park them on the daemon. Returns what it carried, so the
/// second hop moves that exact set.
///
/// The pane child pid belongs to one session, so searching by it is exact
/// here. The daemon pid is shared, so the second hop must NOT search by it:
/// a codex thread parks its claims on the daemon for good, and a concurrent
/// conversion parks its own there mid-move. Searching would sweep up both.
fn park_session_claims(
    writer_pid: u32,
    daemon_pid: u32,
) -> Result<Vec<crate::convert::claim_repin::HeldClaim>, String> {
    let rows = match crate::claim_store::list_db(None, false, None) {
        Ok(rows) => rows,
        // A claim store that cannot be read is not a claim that moved. Say
        // so rather than proceed as though there were none to carry.
        Err(error) => return Err(format!("the claim store could not be read: {error}")),
    };
    let carried = crate::convert::claim_repin::claims_to_carry(&rows, writer_pid);
    match repin_failures(&carried, daemon_pid) {
        None => Ok(carried),
        Some(failure) => Err(failure),
    }
}

/// Move an already-known set of claims to `pid`. The set comes from the
/// first hop, never from a fresh search.
fn repin_session_claims(
    claims: &[crate::convert::claim_repin::HeldClaim],
    to_pid: u32,
) -> Option<String> {
    repin_failures(claims, to_pid)
}

/// The failure sentence when one or more claims could not move, so the
/// caller refuses while naming the key rather than reporting a clean
/// conversion over a claim the session no longer holds.
fn repin_failures(
    claims: &[crate::convert::claim_repin::HeldClaim],
    to_pid: u32,
) -> Option<String> {
    let failures =
        crate::convert::claim_repin::repin_all(claims, to_pid, &crate::convert::claim_repin::repin);
    if failures.is_empty() {
        return None;
    }
    Some(
        failures
            .iter()
            .map(|(key, error)| format!("{key} ({error})"))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// Run one blocking step on the blocking pool. The codex arm must await the
/// app-server handle, so it cannot sit on `spawn_blocking` wholesale the way
/// the claude arm does; every registry flock and sqlite call it makes hops
/// out individually instead of stalling the runtime thread.
async fn off_runtime<T, F>(what: &str, work: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| format!("the {what} was dropped before it ran"))
}

/// The Codex transaction: re-pin, stop the TUI, flip the row, resume the
/// rollout through the shared app-server.
///
/// The daemon is the new writer for a codex thread - the row owns no pid of
/// its own - so the claims move ONCE, to the daemon, before the old writer
/// stops. There is no second hop and no window with a live claim pinned to a
/// dead process.
async fn run_server_resume(ctx: &Arc<Ctx>, req: &Request, plan: &ConvertPlan) -> Response {
    let daemon_pid = std::process::id();
    let child_pid = plan.host.child_pid();
    let refuse = |detail: String| Response::err(req.id, ErrorCode::Internal, detail);

    let parked = off_runtime("claim park", move || {
        park_session_claims(child_pid, daemon_pid)
    })
    .await;
    match parked {
        Ok(Ok(_)) => {}
        Ok(Err(failure)) => {
            return refuse(format!(
                "convert {}: claims could not be re-pinned to the daemon, so the conversion \
                 never started and the pane is untouched: {failure}",
                plan.name
            ))
        }
        Err(dropped) => return refuse(format!("convert {}: {dropped}", plan.name)),
    }
    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({"name": plan.name, "strategy": plan.strategy, "phase": "claims-held"}),
    );

    let registry_path = ctx.home.registry_json();
    let read = {
        let registry_path = registry_path.clone();
        let name = plan.name.clone();
        off_runtime("row read", move || read_row(&registry_path, &name)).await
    };
    let Ok(Some(entry)) = read else {
        return refuse(format!(
            "convert {}: the row vanished between classification and the stop; nothing was moved",
            plan.name
        ));
    };
    let snapshot = crate::convert::server_resume::RowSnapshot::of(&entry);

    // Only ESRCH counts. A kill that was merely SENT leaves the TUI holding
    // the rollout, and the resume below would open a second reader on it.
    let stop = {
        let entry = entry.clone();
        match tokio::task::spawn_blocking(move || {
            crate::pane_stop::stop_pane_process_confirmed(&entry)
        })
        .await
        {
            Ok(stop) => stop,
            Err(_) => return refuse(format!("convert {}: the stop was dropped", plan.name)),
        }
    };
    if !stop.confirmed {
        return refuse(format!(
            "convert {}: the pane child {child_pid} could not be proven gone ({}), so the resume \
             never ran. The session is still on its pane.",
            plan.name, stop.detail
        ));
    }
    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({"name": plan.name, "strategy": plan.strategy, "phase": "pane-stopped"}),
    );

    let name = plan.name.clone();
    let flip = {
        let registry_path = registry_path.clone();
        off_runtime("row flip", move || {
            state::update_registry(&registry_path, move |registry| {
                if let Some(row) = registry.find_mut(&name) {
                    crate::convert::server_resume::to_codex_thread(row);
                }
            })
        })
        .await
    };
    match flip {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            return refuse(format!(
                "convert {}: the pane stopped but the row flip failed: {error}. Re-run the same \
                 command; it reclassifies from what is true.",
                plan.name
            ))
        }
        Err(dropped) => return refuse(format!("convert {}: {dropped}", plan.name)),
    }

    let reread = {
        let registry_path = registry_path.clone();
        let name = plan.name.clone();
        off_runtime("row re-read", move || read_row(&registry_path, &name)).await
    };
    let Ok(Some(flipped)) = reread else {
        return refuse(format!(
            "convert {}: the row vanished after the flip; the session is stopped",
            plan.name
        ));
    };
    match ensure_codex_thread_handle(ctx, &flipped).await {
        Ok(_handle) => {
            let _ = ctx.emitter.emit(
                "agent_convert_phase",
                &json!({"name": plan.name, "strategy": plan.strategy, "phase": "resumed"}),
            );
            let mut result = plan_json(plan, false);
            result["stop_detail"] = json!(stop.detail);
            Response::ok(req.id, result)
        }
        Err(resume_error) => {
            // Put the row back on its pane shape. The pane itself is gone,
            // so the receipt names the one command that brings it back: a
            // row restored to a shape with no process is the state a later
            // `fno agents resume` knows how to repair.
            let name = plan.name.clone();
            let restore = snapshot.clone();
            let registry_path = registry_path.clone();
            let restore_error = off_runtime("rollback", move || {
                state::update_registry(&registry_path, move |registry| {
                    if let Some(row) = registry.find_mut(&name) {
                        restore.restore(row);
                        row.status = crate::AgentStatus::Exited;
                    }
                })
                .err()
                .map(|error| error.to_string())
            })
            .await
            .unwrap_or_else(|dropped| Some(dropped));
            let _ = ctx.emitter.emit(
                "agent_convert_phase",
                &json!({
                    "name": plan.name,
                    "strategy": plan.strategy,
                    "phase": "rolled-back",
                    "error": resume_error,
                }),
            );
            match restore_error {
                None => refuse(format!(
                    "convert {}: the pane stopped and the resume refused ({resume_error}). The \
                     row is back on its pane shape and reads exited. Bring the session back \
                     with: fno agents resume {}",
                    plan.name, plan.name
                )),
                Some(restore_error) => refuse(format!(
                    "convert {}: the resume refused ({resume_error}) AND the rollback failed \
                     ({restore_error}). The session is stopped. Bring it back with: fno agents \
                     resume {}",
                    plan.name, plan.name
                )),
            }
        }
    }
}

/// `--allow-new-id`, as the client sends it. Absent reads false, which is the
/// safe answer: a conversion that silently changed the session id would move
/// every address the operator has for the session.
fn allow_new_id(req: &Request) -> bool {
    req.params
        .get("allow_new_id")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// How long to wait for the relaunched session to appear on the roster.
/// `claude --bg` returns as soon as it has forked, and the row lands a beat
/// later, so a single read would report a roster that has not caught up.
const ROSTER_SETTLE: Duration = Duration::from_secs(20);
/// How long the relaunch itself may take to return. `claude --bg` forks and
/// exits, so this bounds a hang, not the session.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);
const ROSTER_POLL: Duration = Duration::from_millis(500);

/// The claude transaction: re-pin to the daemon, stop the pane, relaunch the
/// session in claude's own background lane, read the id back, flip the row.
///
/// The order is forced by one measured fact in `claude --help`: with
/// `--resume`, `--bg` "continues that session under the same ID, OR STARTS A
/// COPY AND SAYS SO when the session is already running". A relaunch over a
/// live pane therefore forks the conversation. So the pane is proven gone
/// first, and the id is read back rather than assumed.
///
/// Claims move in two hops, never one. The pane child dies mid-transaction,
/// so a claim pinned to it would read stale in the window before the new
/// writer exists. They go to the DAEMON pid first, which outlives every
/// outcome here, and only reach the new writer once the roster proves it.
fn run_client_resume(ctx: &Ctx, req: &Request, plan: &ConvertPlan, allow_new_id: bool) -> Response {
    let daemon_pid = std::process::id();
    let child_pid = plan.host.child_pid();
    let refuse = |detail: String| Response::err(req.id, ErrorCode::Internal, detail);
    let registry_path = ctx.home.registry_json();

    let Some(entry) = read_row(&registry_path, &plan.name) else {
        return refuse(format!(
            "convert {}: the row vanished between classification and the relaunch; nothing was \
             moved",
            plan.name
        ));
    };
    // Read ONCE, before anything moves. A crown granted mid-transaction must
    // not change the verdict on a session that was already relaunched.
    let crowned = crate::convert::client_resume::row_is_crowned(&entry);
    let session_id = match entry.harness_session_id.clone() {
        Some(id) if !id.is_empty() => id,
        _ => {
            return refuse(format!(
                "convert {}: the row names no harness session id, so there is nothing to resume",
                plan.name
            ))
        }
    };
    let snapshot = crate::convert::server_resume::RowSnapshot::of(&entry);
    // The live writer's own pins, before the writer stops.
    let carried = crate::convert::client_resume::carried_flags(
        &crate::census::process_argv(child_pid).unwrap_or_default(),
    );

    let parked = match park_session_claims(child_pid, daemon_pid) {
        Ok(parked) => parked,
        Err(failure) => {
            return refuse(format!(
                "convert {}: claims could not be re-pinned to the daemon, so the conversion \
                 never started and the pane is untouched: {failure}",
                plan.name
            ))
        }
    };
    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({"name": plan.name, "strategy": plan.strategy, "phase": "claims-held"}),
    );

    let before: Vec<String> = roster_short_ids();

    let stop = crate::pane_stop::stop_pane_process_confirmed(&entry);
    if !stop.confirmed {
        return refuse(format!(
            "convert {}: the pane child {child_pid} could not be proven gone ({}), so the \
             relaunch never ran. A relaunch over a live session forks it rather than continuing \
             it. The session is still on its pane, and its claims are held by the daemon.",
            plan.name, stop.detail
        ));
    }
    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({"name": plan.name, "strategy": plan.strategy, "phase": "pane-stopped"}),
    );

    let argv = crate::convert::client_resume::resume_argv(&session_id, &carried);
    // A misconfigured account root is NOT "the ambient root". Relaunching
    // there runs the session on the wrong account, bills the wrong lane,
    // and then reports "never appeared on the roster" - which names the
    // symptom and hides the cause.
    let config_dir = match crate::claude_roster::removal_config_dir(
        &crate::claude_roster::read_all_agents_union(),
        &snapshot.short_id,
        entry.launch_account.as_deref(),
    ) {
        Ok(dir) => dir,
        Err(error) => {
            return refuse(rolled_back(
                ctx,
                &registry_path,
                plan,
                &snapshot,
                &format!(
                    "the claude account root for this session could not be resolved ({error}), \
                     so the relaunch never ran rather than landing on the wrong account"
                ),
            ))
        }
    };
    if let Err(error) = launch_background_claude(&entry.cwd, &argv, config_dir.as_deref()) {
        return refuse(rolled_back(
            ctx,
            &registry_path,
            plan,
            &snapshot,
            &format!("the relaunch could not start ({error})"),
        ));
    }

    let row = match settle_roster(&before, &session_id) {
        Some(row) => row,
        None => {
            return refuse(rolled_back(
                ctx,
                &registry_path,
                plan,
                &snapshot,
                "the relaunched session never appeared on the claude roster, or more than one \
                 new session did and none could be proven to be this one",
            ))
        }
    };

    let verdict = crate::convert::client_resume::id_verdict(
        &session_id,
        row.session_id.as_deref(),
        allow_new_id,
        crowned,
    );
    let new_id = match &verdict {
        crate::convert::client_resume::IdVerdict::Kept => session_id.clone(),
        crate::convert::client_resume::IdVerdict::AcceptedNew { new, .. } => new.clone(),
        crate::convert::client_resume::IdVerdict::RollBack(reason) => {
            // Stop what the relaunch started before restoring the row. A
            // rollback that leaves the new session running is two live
            // readers on one conversation.
            let stopped = stop_background_claude(&row.short_id, config_dir.as_deref());
            let detail = match stopped {
                Ok(()) => reason.clone(),
                Err(error) => format!(
                    "{reason}. The session it started ({}) could not be stopped ({error}); stop \
                     it with: fno agents stop {}",
                    row.short_id, row.short_id
                ),
            };
            return refuse(rolled_back(ctx, &registry_path, plan, &snapshot, &detail));
        }
    };

    // The new writer is proven live, so the claims take their second hop.
    // A failure here is reported rather than rolled back: the claims are
    // still on the daemon, which is live, so nothing reads stale (AC5-ERR).
    let claim_failure = row
        .pid
        .and_then(|writer_pid| repin_session_claims(&parked, writer_pid));

    let name = plan.name.clone();
    let short_id = row.short_id.clone();
    let writer_pid = row.pid;
    let flip_id = new_id.clone();
    if let Err(error) = state::update_registry(&registry_path, move |registry| {
        if let Some(row) = registry.find_mut(&name) {
            crate::convert::client_resume::to_claude_thread(row, &short_id, writer_pid, &flip_id);
        }
    }) {
        return refuse(format!(
            "convert {}: the session is live in the background as {} but the row flip failed: \
             {error}. Re-run the same command; it reclassifies from what is true.",
            plan.name, row.short_id
        ));
    }

    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({
            "name": plan.name,
            "strategy": plan.strategy,
            "phase": "flipped",
            "short_id": row.short_id,
            "session_id": new_id,
        }),
    );

    if let Some(failure) = claim_failure {
        return refuse(format!(
            "convert {}: the session converted and reads live as {}, but claims could not be \
             re-pinned to the new writer: {failure}. They are still held by the daemon, so \
             nothing reads stale.",
            plan.name, row.short_id
        ));
    }

    let mut result = plan_json(plan, false);
    result["short_id"] = json!(row.short_id);
    result["session_id"] = json!(new_id);
    result["writer_pid"] = json!(row.pid);
    result["stop_detail"] = json!(stop.detail);
    if let crate::convert::client_resume::IdVerdict::AcceptedNew { old, new } = &verdict {
        result["id_changed"] = json!(true);
        result["previous_session_id"] = json!(old);
        result["receipt"] = json!(format!(
            "{}\n  NOTE: the session id changed from {old} to {new}; --allow-new-id authorized \
             it.\n",
            plan.receipt()
        ));
    }
    Response::ok(req.id, result)
}

/// Put the row back on its pane shape and say so. The pane process is gone
/// either way, so the row reads exited and the receipt names the one command
/// that brings the session back.
fn rolled_back(
    ctx: &Ctx,
    registry_path: &std::path::Path,
    plan: &ConvertPlan,
    snapshot: &crate::convert::server_resume::RowSnapshot,
    reason: &str,
) -> String {
    let name = plan.name.clone();
    let restore = snapshot.clone();
    let restore_error = state::update_registry(registry_path, move |registry| {
        if let Some(row) = registry.find_mut(&name) {
            restore.restore(row);
            row.status = crate::AgentStatus::Exited;
        }
    })
    .err()
    .map(|error| error.to_string());
    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({
            "name": plan.name,
            "strategy": plan.strategy,
            "phase": "rolled-back",
            "error": reason,
        }),
    );
    match restore_error {
        None => format!(
            "convert {}: {reason}. The row is back on its pane shape and reads exited. Bring the \
             session back with: fno agents resume {}",
            plan.name, plan.name
        ),
        Some(restore_error) => format!(
            "convert {}: {reason} AND the rollback failed ({restore_error}). The session is \
             stopped. Bring it back with: fno agents resume {}",
            plan.name, plan.name
        ),
    }
}

/// The short ids the roster already knows. A roster that cannot be read
/// answers EMPTY, which makes every row look new and so makes the relaunch
/// identifiable only by its own session id - strictly the safer error.
fn roster_short_ids() -> Vec<String> {
    match crate::claude_roster::read_all_agents_union() {
        crate::claude_roster::ClaudeAgentsSnapshot::Known { rows, .. } => {
            rows.into_iter().map(|row| row.short_id).collect()
        }
        crate::claude_roster::ClaudeAgentsSnapshot::Unknown { .. } => Vec::new(),
    }
}

/// Poll the roster until the relaunched session is identifiable, or the
/// settle window closes. Silence at the end is never read as success.
fn settle_roster(
    before: &[String],
    session_id: &str,
) -> Option<crate::claude_roster::ClaudeAgentRow> {
    let deadline = std::time::Instant::now() + ROSTER_SETTLE;
    loop {
        let snapshot = crate::claude_roster::read_all_agents_union();
        let rows = match &snapshot {
            crate::claude_roster::ClaudeAgentsSnapshot::Known { rows, .. }
            | crate::claude_roster::ClaudeAgentsSnapshot::Unknown { rows, .. } => rows.as_slice(),
        };
        // A row whose session id has not landed yet is not an answer. Taking
        // it would hand `id_verdict` a None and roll back a conversion that
        // in fact kept its id, stopping a session that was fine.
        if let Some(row) = crate::convert::client_resume::relaunched_row(before, rows, session_id)
            .filter(|row| row.session_id.as_deref().is_some_and(|id| !id.is_empty()))
        {
            return Some(row.clone());
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(ROSTER_POLL);
    }
}

/// Start `claude --bg --resume ...` detached and wait for it to return. It
/// returns as soon as the background session has forked, so this is seconds,
/// not the life of the session.
fn launch_background_claude(
    cwd: &str,
    argv: &[String],
    config_dir: Option<&std::path::Path>,
) -> Result<(), String> {
    let mut command = std::process::Command::new("claude");
    command
        .args(argv)
        .current_dir(cwd)
        // The daemon's stdin is not this launch's stdin. Inheriting it lets
        // a prompt block forever while the agent lock is held and the pane
        // child is already dead.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = config_dir {
        command.env("CLAUDE_CONFIG_DIR", dir);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("claude --bg failed to start: {error}"))?;
    // Bounded like every other wait on this path. `--bg` returns as soon as
    // it has forked, so this deadline is slack, not a race.
    let deadline = std::time::Instant::now() + LAUNCH_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let stderr = child
                    .wait_with_output()
                    .ok()
                    .map(|output| String::from_utf8_lossy(&output.stderr).trim().to_string())
                    .unwrap_or_default();
                return Err(format!("claude --bg exited {status}: {stderr}"));
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "claude --bg did not return within {}s and was stopped",
                    LAUNCH_TIMEOUT.as_secs()
                ));
            }
            Err(error) => return Err(format!("claude --bg could not be waited on: {error}")),
        }
    }
}

/// Stop the background session a rollback is undoing, through the ONE
/// bounded stop every lane uses. A rollback that left it running would put
/// two readers on one conversation.
///
/// It runs on its own thread with its own runtime: this arm is already on
/// the blocking pool, where awaiting the ambient runtime panics.
fn stop_background_claude(
    short_id: &str,
    config_dir: Option<&std::path::Path>,
) -> Result<(), String> {
    let short = short_id.to_string();
    let dir = config_dir.map(std::path::Path::to_path_buf);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("the stop could not build a runtime: {error}"))?;
        match runtime.block_on(crate::lifecycle_child::bounded_claude_stop_in(
            &short,
            Duration::from_secs(15),
            dir.as_deref(),
        )) {
            Ok(Ok(output)) if output.status.success() => Ok(()),
            Ok(Ok(output)) => Err(format!(
                "the stop exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Ok(Err(error)) => Err(format!("the stop could not start: {error}")),
            Err(_) => Err("the stop did not answer within 15s".to_string()),
        }
    })
    .join()
    .unwrap_or_else(|_| Err("the stop thread panicked".to_string()))
}

fn read_row(registry_path: &std::path::Path, name: &str) -> Option<RegistryEntry> {
    state::load_registry(registry_path)
        .ok()?
        .entries
        .into_iter()
        .find(|entry| entry.name == name)
}

fn prepare(ctx: &Ctx, req: &Request) -> Prepared {
    let answer = |response: Response| Prepared::Answer(Box::new(response));
    let Some(name) = req.params.get("name").and_then(Value::as_str) else {
        return answer(Response::err(
            req.id,
            ErrorCode::InvalidParams,
            "convert needs a name",
        ));
    };
    let dry_run = req
        .params
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // A dry run takes the lock too. It reports what the conversion WOULD do,
    // and a plan read while another writer is mid-move describes a world
    // that no longer exists by the time the operator reads it.
    let lock = match crate::agent_lock::AgentLock::acquire(&ctx.home, name, CONVERT_LOCK_TIMEOUT) {
        Ok(lock) => lock,
        Err(()) => {
            return answer(Response::err(
                req.id,
                ErrorCode::Internal,
                format!(
                    "convert {name}: another writer holds the agent lock{}",
                    crate::agent_lock::holder_note(&ctx.home, name)
                ),
            ))
        }
    };

    let entries = match state::load_registry(&ctx.home.registry_json()) {
        Ok(registry) => registry.entries,
        Err(error) => {
            return answer(Response::err(
                req.id,
                ErrorCode::Internal,
                format!("convert {name}: registry read failed: {error}"),
            ))
        }
    };
    let Some(entry) = entries.iter().find(|entry| entry.name == name) else {
        return answer(Response::err(
            req.id,
            ErrorCode::InvalidParams,
            format!("convert {name}: no such agent"),
        ));
    };

    let contract = match crate::harness_capabilities::HarnessContract::packaged()
        .and_then(|contract| contract.conversion(entry.harness_name()))
    {
        Ok(contract) => contract,
        Err(error) => {
            return answer(Response::err(
                req.id,
                ErrorCode::Internal,
                format!("convert {name}: capability contract unreadable: {error}"),
            ))
        }
    };

    let plan = match crate::convert::classify(entry, &contract, &read_panes(entry), &read_keepers())
    {
        Ok(plan) => plan,
        Err(refusal) => return answer(refusal_response(req.id, refusal)),
    };

    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({
            "name": plan.name,
            "strategy": plan.strategy,
            "phase": if dry_run { "classified-dry-run" } else { "classified" },
        }),
    );

    if dry_run {
        return answer(Response::ok(req.id, plan_json(&plan, true)));
    }
    Prepared::Act(lock, Box::new(plan))
}

/// The keeper-rebind transaction: hand the socket off, re-read it, flip the
/// row. The child never stops, so there is no claim to move and no window
/// in which the session has no writer.
///
/// The failure shape that matters is a hand-off that lands and a flip that
/// does not. That leaves the keeper at the thread socket with the row still
/// reading `pane` - recoverable, because a re-run reclassifies from what is
/// actually true, finds the keeper at the thread path, and completes the
/// flip. So this never writes a journal: the world is the journal.
fn run_keeper_rebind(ctx: &Ctx, req: &Request, plan: &ConvertPlan) -> Response {
    let state_root = ctx
        .home
        .root()
        .parent()
        .unwrap_or_else(|| ctx.home.root())
        .to_path_buf();
    let target = match &plan.host {
        // Already handed off: the keeper's own socket is the truth, and
        // re-deriving one would probe a path nothing answers if the naming
        // rule ever changes.
        crate::convert::ConvertHost::HandedOffKeeper { keeper_socket, .. } => {
            std::path::PathBuf::from(keeper_socket)
        }
        _ => crate::convert::keeper_rebind::thread_socket_path(&state_root, &plan.name),
    };

    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({"name": plan.name, "strategy": plan.strategy, "phase": "hand-off"}),
    );
    if let Err(error) = crate::convert::keeper_rebind::hand_off(
        plan,
        &target,
        &crate::convert::keeper_rebind::run_mux_hand_off,
    ) {
        // The server refuses before it touches the layout, so the pane is
        // still seated and still served. Nothing to roll back.
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!(
                "convert {}: hand-off refused, pane untouched: {error}",
                plan.name
            ),
        );
    }

    let identify = match probe_keeper_socket(&target, Duration::from_millis(2000)) {
        KeeperProbe::Answered(reply) => reply,
        KeeperProbe::NoListener => {
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!(
                    "convert {}: the socket moved to {} but nothing answers behind it; the row \
                     is untouched. Read `fno mux pane keeper list` before retrying.",
                    plan.name,
                    target.display()
                ),
            )
        }
        KeeperProbe::Silent => {
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!(
                    "convert {}: the keeper at {} accepted the probe and stayed silent. Silence \
                     never proves death, so the row is left as it was rather than flipped onto a \
                     keeper that cannot answer for itself.",
                    plan.name,
                    target.display()
                ),
            )
        }
    };
    let outcome = match crate::convert::keeper_rebind::verify_moved(
        plan,
        &identify,
        &target.to_string_lossy(),
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            return Response::err(
                req.id,
                ErrorCode::Internal,
                format!("convert {}: {error}; the row is untouched", plan.name),
            )
        }
    };

    let name = plan.name.clone();
    let flip = outcome.clone();
    if let Err(error) = state::update_registry(&ctx.home.registry_json(), move |registry| {
        if let Some(row) = registry.find_mut(&name) {
            crate::convert::keeper_rebind::flipped_row(row, &flip);
        }
    }) {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!(
                "convert {}: the keeper is live at {} but the row flip failed: {error}. Re-run \
                 the same command; it reclassifies from what is true and completes the flip.",
                plan.name,
                target.display()
            ),
        );
    }

    let _ = ctx.emitter.emit(
        "agent_convert_phase",
        &json!({
            "name": plan.name,
            "strategy": plan.strategy,
            "phase": "flipped",
            "socket": outcome.socket,
            "child_pid": outcome.child_pid,
        }),
    );
    let mut result = plan_json(plan, false);
    result["socket"] = json!(outcome.socket);
    result["keeper_pid"] = json!(outcome.keeper_pid);
    result["child_pid"] = json!(outcome.child_pid);
    Response::ok(req.id, result)
}

fn refusal_response(id: u64, refusal: ConvertRefusal) -> Response {
    match refusal {
        // Idempotent: the caller asked for a state the world is in, so this
        // is a success with a receipt, not an error.
        ConvertRefusal::AlreadyAThread { .. } => Response::ok(
            id,
            json!({"outcome": "already-a-thread", "detail": refusal.message()}),
        ),
        ConvertRefusal::Refused(_) => {
            Response::err(id, ErrorCode::InvalidParams, refusal.message())
        }
    }
}

fn plan_json(plan: &ConvertPlan, dry_run: bool) -> Value {
    let pane = plan.host.pane();
    json!({
        "outcome": if dry_run { "dry-run" } else { "converted" },
        "name": plan.name,
        "harness": plan.harness,
        "strategy": plan.strategy,
        "preserves_id": plan.preserves_id,
        "session_id": plan.session_id,
        "pane": pane.map(|(session, pane_id)| json!({"session": session, "pane_id": pane_id})),
        "child_pid": plan.host.child_pid(),
        "steps": plan.steps,
        "receipt": plan.receipt(),
    })
}

/// Live panes, read through the same listing the pane stop uses. Scoped to
/// the row's recorded mux session when it has one, and unscoped otherwise:
/// a reseated row has no mux ref and is still found by child pid.
fn read_panes(entry: &RegistryEntry) -> Vec<PaneRead> {
    crate::pane_stop::pane_list_via_fno(entry.mux.as_ref().map(|mux| mux.session.as_str()))
        .into_iter()
        .map(|pane| PaneRead {
            session: pane.session,
            pane_id: pane.pane_id,
            child_pid: pane.child_pid,
        })
        .collect()
}

/// The keeper sockets, read through `fno mux pane keeper list --json`. An
/// unreadable listing answers EMPTY, which makes a keeper-lane conversion
/// refuse with its relaunch remedy rather than proceed as though the child
/// had no keeper and stop a process that did not need stopping.
fn read_keepers() -> Vec<KeeperSighting> {
    let Ok(run) = crate::pane_stop::run_fno(&["mux", "pane", "keeper", "list", "--json"]) else {
        return Vec::new();
    };
    if !run.ok {
        return Vec::new();
    }
    let Ok(rows) = serde_json::from_slice::<Value>(&run.stdout) else {
        return Vec::new();
    };
    KeeperSighting::from_json(&rows)
}

//! The codex thread resume-and-recovery lane, moved shrink-only out of the
//! config hub's Rust sibling (this file carried 8.7k lines against a 5k
//! budget). One resume seam every caller goes through, plus the startup
//! recovery pass that re-hosts candidates.

use super::*;

/// A Codex thread row startup recovery may auto-resume: it needs a full
/// durable identity AND a status that was non-terminal when the daemon died.
/// `handle_stop` marks a stopped thread `Exited`; resurrecting that row on the
/// next daemon start would silently undo `fno agents stop`.
pub(super) fn codex_thread_recovery_candidate(entry: &RegistryEntry) -> bool {
    codex_thread_resume_identity(entry).ok().flatten().is_some() && is_non_terminal(entry.status)
}

pub(super) async fn ensure_codex_thread_handle(
    ctx: &Ctx,
    entry: &RegistryEntry,
) -> Result<CodexThreadHandle, String> {
    if let Some(handle) = ctx.codex_threads.lock().await.get(&entry.name).cloned() {
        return Ok(handle);
    }
    let Some((session_id, cwd)) = codex_thread_resume_identity(entry)? else {
        return Err(format!("agent '{}' is not a Codex thread", entry.name));
    };
    // A resumed thread with no cwd reads alive yet can never run a turn: the
    // app-server accepts the resume, so without this guard the daemon makes
    // its own liveness evidence for a dead row.
    if !cwd.is_dir() {
        return Err(format!(
            "codex thread '{}' cwd {} no longer exists; resume refused",
            entry.name,
            cwd.display()
        ));
    }
    // Connect OUTSIDE the lock. The resume now ensures the shared daemon
    // (which can take seconds to boot) and completes a network handshake, and
    // `codex_threads` is the map every other codex ask, stop and retask goes
    // through. Holding it across that await let one slow connect stall every
    // other codex thread on the machine, including the recovery loop.
    let carry = crate::codex_thread::parse_harness_args(&entry.harness_args).map_err(|reason| {
        format!(
            "codex thread '{}' stored harness_args refuse to re-parse: {reason}",
            entry.name
        )
    })?;
    let bounded = !crate::codex_posture::entry_posture_is_full_access(entry);
    // AC3-EDGE: read the spawn-time record BEFORE the resume runs, so the
    // write-back after it can never overwrite the only durable copy of the
    // grant with a narrowed result. What the resume actually re-applied is
    // read off the driver and unioned below.
    let recorded_roots = entry.granted_writable_roots.clone();
    // The row's recorded posture, rebuilt typed: the v35 requested mode when
    // it still resolves, else the recorded posture name with the lane's
    // historical `never` approval. A pre-v19 row with neither reads bounded.
    let posture = crate::codex_posture::CodexPosture::from_record(
        entry.requested_permission_mode.as_deref(),
        entry.sandbox_posture.as_deref(),
    );
    let config = carry.config;
    let driver = crate::codex_thread::CodexThread::resume_with_state_dirs(
        cwd,
        &session_id,
        entry.model.as_deref(),
        &posture,
        entry.effort.as_deref(),
        &recorded_roots,
        Some(&config),
    )
    .await
    .map_err(|error| format!("codex thread '{}' resume refused: {error}", entry.name))?;
    // The event narrows to its true meaning: a row that CARRIED nothing to
    // restore. A row with recorded roots got them re-applied (state_dirs
    // above), and a full-access thread needs no grant, so neither fires.
    // An event that fires on the healthy path is telemetry an operator
    // learns to ignore.
    if bounded && recorded_roots.is_empty() {
        let _ = ctx.emitter.emit(
            "codex_thread_resumed_without_state_grant",
            &json!({"name": entry.name, "lane": "thread", "session_id": session_id}),
        );
    }
    // Persist what THIS resume actually resolved beside the record it was
    // built from. Read from the driver BEFORE `into_actor` consumes it; the
    // actor exposes neither field.
    let resolved_sandbox = driver.resolved_sandbox_posture().to_string();
    // AC3-EDGE: the record survives the resume. The spawn-time roots are
    // unioned back in (recorded order first, dedup), so a narrowed resume
    // cannot erase the only durable copy of the grant and a second resume
    // can still restore them.
    let mut granted_writable_roots = driver.granted_writable_roots().to_vec();
    for root in &recorded_roots {
        if !granted_writable_roots
            .iter()
            .any(|existing| existing == root)
        {
            granted_writable_roots.push(root.clone());
        }
    }
    // v35 backfill: a pre-v35 row carries no requested mode; the posture the
    // resume rebuilt is the best record of it. Never overwrites a recorded
    // string.
    let requested_permission_mode = entry
        .requested_permission_mode
        .clone()
        .or_else(|| Some(posture.requested.clone()).filter(|r| !r.is_empty()));
    let resumed_name = entry.name.clone();
    let turn_policy_source = driver.turn_policy_source().to_string();
    let _ = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
        if let Some(row) = registry.find_mut(&resumed_name) {
            row.resolved_sandbox = Some(resolved_sandbox);
            row.granted_writable_roots = granted_writable_roots;
            row.requested_permission_mode = requested_permission_mode;
            row.turn_policy_source = Some(turn_policy_source);
        }
    })
    .await;
    let mut threads = ctx.codex_threads.lock().await;
    // A concurrent caller may have won the race while we were connecting.
    // Theirs is already published, so keep it and drop ours: dropping a
    // driver closes one connection to the shared daemon and ends no thread.
    if let Some(handle) = threads.get(&entry.name).cloned() {
        return Ok(handle);
    }
    // The resumed actor's report seq starts ABOVE the row's current
    // seq, so its first write clears the gate instead of dying under the
    // previous incarnation's seq. The counter itself lives on the callback,
    // one per thread start/resume, never on the row.
    let first_seq = entry
        .inside_leg
        .as_ref()
        .map(|report| report.seq + 1)
        .unwrap_or(1);
    let handle = Arc::new(driver.into_actor(
        codex_thread_on_done(&ctx.emitter, ctx.home.registry_json(), &entry.name),
        codex_thread_on_status(
            &ctx.emitter,
            ctx.home.registry_json(),
            &entry.name,
            &session_id,
            first_seq,
            ctx.opts.notify_on_blocked,
            ctx.opts.notify_on_done,
        ),
    ));
    threads.insert(entry.name.clone(), Arc::clone(&handle));
    Ok(handle)
}

/// Reconnect to Codex threads after daemon startup. Recovery first selects
/// rows by durable identity; this asynchronous pass reopens the shared-daemon
/// connections without delaying the supervisor's accept loop. The threads
/// themselves never stopped: the shared app-server daemon kept them.
pub(super) fn schedule_codex_thread_recovery(ctx: Arc<Ctx>) {
    tokio::spawn(async move {
        recover_codex_threads(&ctx).await;
    });
}

/// The recovery pass body, split from the scheduler so a test can await it
/// (the spawned task is fire-and-forget). Resume-or-settle: a candidate that
/// resumes goes Live; one that fails is stamped Orphaned (AC15), never left
/// reading Live forever.
pub(super) async fn recover_codex_threads(ctx: &Ctx) {
    {
        let registry = match load_registry_offloaded(ctx.home.registry_json()).await {
            Ok(registry) => registry,
            Err(error) => {
                let _ = ctx.emitter.emit(
                    "daemon_recovery_error",
                    &json!({"op": "resume_codex_thread_registry", "error": error.to_string()}),
                );
                return;
            }
        };
        for entry in registry.entries {
            if !codex_thread_recovery_candidate(&entry) {
                continue;
            }
            match ensure_codex_thread_handle(&ctx, &entry).await {
                Ok(_handle) => {
                    let name = entry.name.clone();
                    let _ = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
                        if let Some(entry) = registry.find_mut(&name) {
                            // Same reason as `build_codex_thread_entry`: the
                            // thread owns no process, so its liveness cannot
                            // be a pid. Clear any stale one a pre-shared-daemon
                            // row still carries.
                            entry.pid = None;
                            entry.pid_start_time = None;
                            entry.status = AgentStatus::Live;
                        }
                    })
                    .await;
                }
                Err(error) => {
                    let _ = ctx.emitter.emit(
                        "daemon_recovery_error",
                        &json!({"op": "resume_codex_thread", "name": entry.name, "error": error}),
                    );
                    // A failed resume leaves the row readable Live forever
                    // unless it is settled here: Orphaned, because the
                    // rollout on disk is still the durable object a later
                    // resume (or a human) can pick up. Only a row that is
                    // still non-terminal is stamped - never overwrite a
                    // terminal status a concurrent stop just wrote.
                    let recover_name = entry.name.clone();
                    let _ = update_registry_offloaded(ctx.home.registry_json(), move |registry| {
                        if let Some(entry) = registry.find_mut(&recover_name) {
                            if is_non_terminal(entry.status) {
                                entry.status = AgentStatus::Orphaned;
                            }
                        }
                    })
                    .await;
                }
            }
        }
    }
}

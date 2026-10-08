use super::*;

pub(super) async fn serve(
    listener: std::os::unix::net::UnixListener,
    socket: &Path,
    session_name: String,
    pane_children: PaneChildRoster,
    mut signal_rx: mpsc::Receiver<CoreMsg>,
    shutdown_complete: Arc<AtomicBool>,
    owner: Option<OwnerLease>,
) -> i32 {
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("fno mux: listener setup failed: {e}");
        return 1;
    }
    let listener = match tokio::net::UnixListener::from_std(listener) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fno mux: listener setup failed: {e}");
            return 1;
        }
    };

    // One shared pane-tagged output channel + one exit channel for all PTY
    // reader threads. Squads (and their first panes) are born from attaches;
    // nothing is spawned upfront.
    let (out_tx, mut out_rx) = mpsc::channel::<(u64, PaneChunk)>(256);
    let (exit_tx, mut exit_rx) = mpsc::channel::<u64>(64);
    let (core_tx, mut core_rx) = mpsc::channel::<CoreMsg>(256);
    // Attached-client count for the periodic readers: Core owns the
    // sender; each reader holds a receiver as its work gate + 0->1 wakeup.
    let (client_count_tx, client_count_rx) = watch::channel(0usize);
    let persisted_pane_floor = crate::squad_store::load().next_pane_id;
    let initial_agents = read_guard_agents().await;

    let mut core = Core {
        session: Session::default(),
        panes: HashMap::new(),
        pane_watch: HashMap::new(),
        pane_stats: Arc::new(RwLock::new(HashMap::new())),
        pane_stats_emit_failures: Arc::new(AtomicU64::new(0)),
        pane_children,
        clients: Vec::new(),
        next_pane_id: pane_id_floor(
            persisted_pane_floor,
            initial_agents.as_deref().unwrap_or(&[]),
        ),
        next_squad_id: 1,
        tab_areas: HashMap::new(),
        session_name,
        shells: shell_candidates(std::env::var_os("SHELL").as_deref()),
        out_tx,
        exit_tx,
        self_tx: core_tx.clone(),
        agents: Vec::new(),
        agents_read_ok: false,
        journal: crate::spawn_journal::JournalCache::load(),
        launch_desk: Default::default(),
        branch_by_cwd: HashMap::new(),
        tail_by_session: HashMap::new(),
        ctx_by_session: HashMap::new(),
        truth_by_name: HashMap::new(),
        truth_seq: 0,
        backlog: Vec::new(),
        backlog_lanes: Vec::new(),
        backlog_stale: false,
        backlog_holders: HashMap::new(),
        backlog_pr: HashMap::new(),
        backlog_driver: HashMap::new(),
        claim_eligible: HashSet::new(),
        claims: HashMap::new(),
        touch_last_emit: HashMap::new(),
        wheel_gate: HashMap::new(),
        touch_emit_failures: Arc::new(AtomicU64::new(0)),
        started_at: crate::server_stats::stamp_now(),
        client_count: client_count_tx,
        seen: HashSet::new(),
        attached: HashMap::new(),
        worker_pane: HashMap::new(),
        worker_session_pane: HashMap::new(),
        held_workers: HashMap::new(),
        detached_panes: HashMap::new(),
        diff_pane: None,
        portals: BTreeMap::new(),
        portal_noticed: false,
        squad_members: HashMap::new(),
        template_specs: HashMap::new(),
        pending_template_restores: Vec::new(),
        external_lifecycle: Vec::new(),
        persist_degraded_notified: false,
        shared_identity_notified: HashSet::new(),
        restored: false,
        restore_pending: false,
        store_generations: HashMap::new(),
        pre_restore_squads: HashSet::new(),
        topology_dirty: false,
        last_topology_flush: None,
        reentry_verdict: None,
        staged_resume_argv: None,
        batch_plans: HashMap::new(),
        pending_thread_reply: None,
        keeper_adopted: Vec::new(),
        shell_rc_dirs: HashMap::new(),
        portal_session_guards: BTreeMap::new(),
    };

    // The off-loop registry reader (4a-G2): a 1s interval task stats/reads
    // BOTH the fno-agents registry AND claude's daemon roster on the
    // blocking pool, unions them into the agent row set (TTL aging + roster
    // liveness upgrade + foreign rows included), and sends it to the core only
    // when the MERGED set changed. Each file is behind its own mtime+len gate,
    // so a roster-only change publishes and an idle tick reads nothing. The
    // render path never touches either file (AC2-UI; the origin freeze class),
    // and staleness stays bounded by this one interval.
    {
        let core_tx = core_tx.clone();
        let reg_path = agents_view::registry_path();
        let roster_path = agents_view::roster_path();
        let mut count_rx = client_count_rx.clone();
        tokio::spawn(async move {
            let mut state = agents_view::ReaderState::default();
            // Carried across ticks so the tail pass can run even when
            // the row set did not move: the uuid set to look up, and the last
            // map pushed, so an unchanged result stays off the wire.
            let mut last_uuids: Vec<(String, Option<String>)> = Vec::new();
            let mut last_tails: HashMap<String, String> = HashMap::new();
            let mut last_ctx: HashMap<String, String> = HashMap::new();
            // Shared so the path cache survives across blocking-pool passes.
            let tail_reader = std::sync::Arc::new(std::sync::Mutex::new(
                crate::transcript_tail::TailReader::new(),
            ));
            let mut last_truth = Instant::now();
            // Shared with the detached probe tasks: clear means no probe is
            // in flight. Held by [`TruthProbeLatch`], which clears it on drop.
            let truth_in_flight = Arc::new(AtomicBool::new(false));
            // Logged once per outage, never per tick: the fallback is a
            // fact about the environment, and a fleet with no daemon must
            // not pay one line per second for it. The recovery clears it.
            let mut fallback_logged = false;
            // Same once-only discipline for the wedged-probe skip below.
            let mut latch_wedge_logged = false;
            // (v48) Launch order for AgentTruth probes, so an out-of-order
            // completion cannot clobber a fresher result (see CoreMsg::AgentTruth).
            let mut truth_probe_seq: u64 = 0;
            // The models.dev catalog refresh: once at start, then hourly (a
            // stat per hour when fresh). Pricing never depends on a user who
            // never opens the composer; a failed fetch keeps the old cache
            // and the next hour retries. The refresh itself re-stats and
            // spawns only on a stale cache, so the steady state is one stat.
            let mut catalog_every = tokio::time::interval(Duration::from_secs(3600));
            catalog_every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Stat + conditional read of one file behind an mtime+len gate,
            // both off the core loop. Returns (fresh stamp, raw-if-changed).
            async fn scan(
                path: std::path::PathBuf,
                cached: Option<(std::time::SystemTime, u64)>,
            ) -> (Option<(std::time::SystemTime, u64)>, Option<String>) {
                let stat_path = path.clone();
                let stamp = tokio::task::spawn_blocking(move || {
                    std::fs::metadata(&stat_path)
                        .ok()
                        .map(|m| (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()))
                })
                .await
                .ok()
                .flatten();
                // A fence directory's stat never moves; its rows live in graph.db,
                // so it is read every tick.
                let raw = if stamp != cached || path.is_dir() {
                    tokio::task::spawn_blocking(move || {
                        crate::registry_read::registry_text(&path).ok()
                    })
                    .await
                    .ok()
                    .flatten()
                } else {
                    None
                };
                (stamp, raw)
            }
            loop {
                // Gate the registry+roster read on an attached client.
                // The `changed()` arm IS the 0->1 kick: an attach wakes the
                // parked reader at once so the first overlay is fresh (AC3-FR).
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = catalog_every.tick() => {
                        crate::model_catalog::refresh_if_stale(&crate::model_catalog::state_dir());
                    }
                    res = count_rx.changed() => {
                        if res.is_err() {
                            return; // Core dropped; server shutting down
                        }
                    }
                }
                if *count_rx.borrow() == 0 {
                    continue; // no viewer -> skip both file reads entirely
                }
                // (v48) Reachability evidence, one `fno agents list --json`
                // process for the whole fleet on a slow sub-interval. Each
                // probe runs as its own task so a slow CLI start never stalls
                // the 1s registry tick; a failed probe sends nothing and the
                // last good map stands. The latch skips a tick whose
                // predecessor still runs, so a probe slower than the interval
                // stacks no second interpreter. Ages are measured at probe
                // time, so between probes a row's displayed age lags by at
                // most this interval - invisible next to the 600s threshold
                // it feeds.
                if last_truth.elapsed() >= TRUTH_PROBE_EVERY {
                    match TruthProbeLatch::begin(&truth_in_flight) {
                        Some(latch) => {
                            last_truth = Instant::now();
                            truth_probe_seq += 1;
                            let seq = truth_probe_seq;
                            let tx = core_tx.clone();
                            tokio::spawn(async move {
                                let probe = tokio::task::spawn_blocking(probe_truth_map)
                                    .await
                                    .ok()
                                    .flatten();
                                drop(latch);
                                if let Some(map) = probe {
                                    let _ = tx.send(CoreMsg::AgentTruth { map, seq }).await;
                                }
                            });
                        }
                        None => {
                            // A probe still running a full interval after it
                            // started is wedged, not slow. The old code healed
                            // that by stacking a new probe; the latch cannot,
                            // so say so once instead of silently freezing
                            // every row's age at the last good reading.
                            if !latch_wedge_logged {
                                latch_wedge_logged = true;
                                eprintln!(
                                    "fno mux: a fleet truth probe has run over {TRUTH_PROBE_EVERY:?}; \
                                     skipping probes until it exits"
                                );
                            }
                        }
                    }
                }
                // The registry leg subscribes to the daemon
                // when its socket answers (AC12): rows are SERVED, the stamp
                // domain is the same (mtime, len) the file scan gated with, and
                // an unchanged answer costs one stat server-side and one small
                // frame here - no file read on either side. When nothing
                // answers, the file scan below stands (AC13, the supported
                // no-daemon shape), logged once.
                let (reg_stamp, reg_raw) = match agents_view::watch_registry(
                    state.reg_stamp(),
                    &agents_view::supervisor_sock_path(),
                )
                .await
                {
                    // Every tick retries the daemon, so a miss at spawn (a
                    // daemon restarting beside the mux, a 2s round under
                    // load) heals by itself. Log both edges, or the log
                    // reads degraded for the server's whole life.
                    Ok(answer) => {
                        if fallback_logged {
                            fallback_logged = false;
                            eprintln!("fno mux: agent daemon answering again; rows are served");
                        }
                        answer
                    }
                    Err(error) => {
                        if !fallback_logged {
                            fallback_logged = true;
                            eprintln!(
                                "fno mux: no agent daemon answering at {} ({error}); reading the registry file directly (degraded fallback, retried every tick)",
                                agents_view::supervisor_sock_path().display()
                            );
                            // Ruling d-e096c669: the mux starts fno's own
                            // pieces; a vendor tab is never what unblocks it.
                            // `fno-agents status` lazy-starts the daemon
                            // (client::call runs ensure_daemon). Once per
                            // fallback edge, off the loop, bounded, output
                            // dropped.
                            let bin = crate::digest_overlay::fno_agents_bin();
                            tokio::spawn(async move {
                                let mut command = crate::process_admission::tokio_command(&bin);
                                command.args(["status"]).stdin(std::process::Stdio::null());
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(10),
                                    crate::process_admission::tokio_output(&mut command),
                                )
                                .await
                                {
                                    Ok(Ok(_)) => {
                                        eprintln!("fno mux: started the agent daemon")
                                    }
                                    Ok(Err(e)) => {
                                        eprintln!("fno mux: agent daemon start failed: {e}")
                                    }
                                    Err(_) => eprintln!(
                                        "fno mux: agent daemon start failed: timed out after 10s"
                                    ),
                                }
                            });
                        }
                        scan(reg_path.clone(), state.reg_stamp()).await
                    }
                };
                let (roster_stamp, roster_raw) =
                    scan(roster_path.clone(), state.roster_stamp()).await;
                // Each registered isolated account's roster.json, folded
                // into the union tagged by account (managed accounts share
                // ~/.claude and add no dir). The config re-read is tiny and
                // gated on an attached viewer; each roster read is stamp-gated
                // per dir by `isolated_stamp`, so only a changed dir re-reads.
                let iso_paths = tokio::task::spawn_blocking(agents_view::isolated_roster_paths)
                    .await
                    .unwrap_or_default();
                let mut isolated = Vec::with_capacity(iso_paths.len());
                for (account, path) in iso_paths {
                    let (stamp, raw) = scan(path, state.isolated_stamp(&account)).await;
                    isolated.push(agents_view::IsolatedRead {
                        account,
                        stamp,
                        raw,
                    });
                }
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let changed = state.tick(
                    reg_stamp,
                    move || reg_raw,
                    roster_stamp,
                    move || roster_raw,
                    isolated,
                    now,
                );
                if let Some(rows) = &changed {
                    // The tail key is the row's transcript identity: the claude
                    // uuid where one exists, else the harness session id (the
                    // rollout uuid a codex row carries). log_path rides beside
                    // it - a row naming its own transcript is read directly.
                    last_uuids = rows
                        .iter()
                        .filter_map(|r| {
                            let uuid = r
                                .claude_session_uuid
                                .clone()
                                .or_else(|| r.harness_session_id.clone())?;
                            Some((uuid, r.log_path.clone()))
                        })
                        .collect();
                }
                // Tails resolve on EVERY tick (a transcript grows
                // with no registry change); TailReader re-reads known paths
                // each tick and paces only the discovery walk, which at 1Hz
                // was most of this loop's CPU.
                let uuids = last_uuids.clone();
                let reader = tail_reader.clone();
                let (tails, ctx) = tokio::task::spawn_blocking(move || {
                    // Poison-recover rather than expect: a panic in one pass
                    // must not blank the column on every later tick (the
                    // cache is a HashMap, safe to reuse mid-poison).
                    reader
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .tails_and_ctx(&uuids)
                })
                .await
                .unwrap_or_default();
                if let Some(rows) = changed {
                    // (US4) Resolve the git branch per UNIQUE row cwd,
                    // off the core loop, on the blocking pool - bounded file
                    // reads only, per-cwd degradation on failure (AC1-FR). This
                    // rides the change-gated emit: a branch only moves when the
                    // row set does, so the reads stay off idle ticks.
                    let cwds: Vec<String> = {
                        let mut seen = std::collections::HashSet::new();
                        rows.iter()
                            .map(|r| r.cwd.clone())
                            .filter(|c| !c.is_empty() && seen.insert(c.clone()))
                            .collect()
                    };
                    let branches = tokio::task::spawn_blocking(move || {
                        cwds.into_iter()
                            .filter_map(|c| {
                                agents_view::resolve_branch(std::path::Path::new(&c))
                                    .map(|b| (c, b))
                            })
                            .collect::<HashMap<String, String>>()
                    })
                    .await
                    .unwrap_or_default();
                    last_tails = tails.clone();
                    last_ctx = ctx.clone();
                    if core_tx
                        .send(CoreMsg::AgentRows {
                            rows,
                            branches,
                            tails,
                            ctx,
                            read_ok: state.read_ok(),
                        })
                        .await
                        .is_err()
                    {
                        return; // core loop gone; the server is shutting down
                    }
                } else if tails != last_tails || ctx != last_ctx {
                    // Rows unchanged but somebody said something: push the tails
                    // alone rather than forcing a whole row set through.
                    last_tails = tails.clone();
                    last_ctx = ctx.clone();
                    if core_tx
                        .send(CoreMsg::AgentTails { tails, ctx })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
    }

    crate::board_reader::spawn(core_tx.clone(), client_count_rx.clone());

    // The per-pane counter cadence: a fixed 30s tick telling the core to
    // snapshot and emit. Delay (not Burst) on a missed tick - counters are
    // monotonic totals, so one late sample costs nothing a difference needs.
    {
        let core_tx = core_tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PANE_STATS_CADENCE);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if core_tx.send(CoreMsg::PaneStatsTick).await.is_err() {
                    return; // Core dropped; server shutting down
                }
            }
        });
    }

    // The squad-key cache, shared by the per-connection handshake tasks. The
    // blocking git resolution runs there (spawn_blocking), NEVER on the core
    // loop - a hung git may delay ONE attach by the 2s timeout, but every
    // pane and peer keeps streaming (the drive-freeze class).
    let resolver = Arc::new(Mutex::new(Resolver::default()));

    // Accept loop: handshake each connection off the core loop's back.
    let accept_core_tx = core_tx.clone();
    let accept_stats = core.pane_stats.clone();
    let conns_alive = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let accept_conns = conns_alive.clone();
    tokio::spawn(async move {
        let mut next_id: u64 = 1;
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let id = next_id;
                    next_id += 1;
                    // Peer pid names WHICH client process this is (the e2e
                    // harness logs its children's pids for the join).
                    e2e_log(format_args!(
                        "conn {id} accepted (peer pid {:?})",
                        stream.peer_cred().ok().and_then(|c| c.pid())
                    ));
                    let conn_core_tx = accept_core_tx.clone();
                    let conn_resolver = resolver.clone();
                    let conn_stats = accept_stats.clone();
                    // Count from the accept itself, not the task's first poll:
                    // a scheduler-starved newborn task would otherwise leave
                    // the reaper a mid-verb window with conns_alive == 0.
                    let alive = ConnAlive::new(&accept_conns);
                    tokio::spawn(async move {
                        let _alive = alive;
                        handle_client(stream, conn_core_tx, conn_resolver, id, conn_stats).await;
                    });
                }
                Err(e) => {
                    eprintln!("fno mux: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });

    eprintln!("fno mux: serving {}", socket.display());

    // Re-adopt surviving keeper panes BEFORE any attach can restore: this
    // runs synchronously before the loop drains its first CoreMsg, so an
    // early attach's restore sees adopted panes as already-live members.
    core.keeper_readopt();

    // Shared ownerless servers persist across client detach. E2E servers and
    // owner-bound sandbox servers use the same graceful idle shutdown path;
    // sandbox pane output cannot extend the deadline after the last client
    // leaves, while a script-style E2E session can still re-arm on output.
    let idle_exit_e2e = std::env::var_os("FNO_E2E").is_some();
    let idle_exit_enabled = idle_exit_e2e || owner.is_some();
    let idle_grace = Duration::from_millis(
        std::env::var("FNO_IDLE_EXIT_GRACE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60_000),
    );
    let mut idle_count_rx = client_count_rx.clone();
    let mut idle_deadline = tokio::time::Instant::now() + idle_grace;
    let mut ever_attached = false;
    let mut pane_reap_tick = tokio::time::interval(Duration::from_secs(1));
    pane_reap_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut owner_check_tick = tokio::time::interval(Duration::from_secs(1));
    owner_check_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval`'s first tick is immediate; consume it so the first scan lands
    // after one interval instead of duplicating startup's known-live state.
    pane_reap_tick.tick().await;

    // Build-drift retirement: the watch stats its own executable
    // off-loop every 5th tick and retires through Flow::Shutdown only on a
    // drifted verdict at a fully quiet tick. The machinery lives in
    // server/drift_retire.rs.
    let mut drift_watch = drift_retire::RetireWatch::new();

    // diagnostics: which panes' output the CORE LOOP has seen. Pairs
    // with the pty reader thread's own first-chunk line to split "shell never
    // spoke" from "core loop never drained it".
    let mut e2e_first_out: HashSet<u64> = HashSet::new();
    // Explicit e2e-only fault seam: after a real pane registration, park the
    // core-loop thread forever so SIGTERM cannot be handled by this loop.
    let core_wedge_e2e = std::env::var_os("FNO_E2E_CORE_WEDGE").is_some();
    let mut core_wedge_armed = false;

    // The word this server died of, set by every Flow::Shutdown break before
    // breaking. Unassigned on purpose: a future break without a cause fails
    // to compile, so the log and the events feed can never disagree. The
    // tail prints it once and record_exit carries it into the event.
    let cause;

    let flow = loop {
        tokio::select! {
            chunk = out_rx.recv() => {
                // out_tx lives in Core, so recv never yields None.
                let Some((pid, item)) = chunk else { cause = "channel-closed"; break Flow::Shutdown };
                drain_pty_output(
                    &mut core,
                    &mut out_rx,
                    Some((pid, item)),
                    &mut e2e_first_out,
                );
                // Pane output is a liveness signal: re-arm the idle
                // reaper so a working client-less script session never reaps.
                if idle_exit_enabled
                    && (idle_exit_e2e || !ever_attached || *idle_count_rx.borrow() > 0)
                {
                    idle_deadline = tokio::time::Instant::now() + idle_grace;
                }
            }
            exited = exit_rx.recv() => {
                let Some(pid) = exited else { cause = "channel-closed"; break Flow::Shutdown };
                e2e_log(format_args!("pane {pid} child exited"));
                // The reader sends this only after enqueuing every output
                // chunk. Drain that channel before removing the pane so final
                // bytes cannot lose a select race between separate receivers.
                if drain_pty_output(&mut core, &mut out_rx, None, &mut e2e_first_out)
                    && idle_exit_enabled
                    && (idle_exit_e2e || !ever_attached || *idle_count_rx.borrow() > 0)
                {
                    idle_deadline = tokio::time::Instant::now() + idle_grace;
                }
                // A worker that died on its own (churn) tombstones its member
                // BEFORE the reap clears the mapping (AC4-EDGE).
                let ctx = core.member_ctx(pid);
                core.reconcile_member_close(ctx, true);
                if core.close_viewer_died(pid, "viewer exited") == Flow::Shutdown {
                    cause = "last-pane-gone";
                    break Flow::Shutdown;
                }
            }
            _ = pane_reap_tick.tick() => {
                // Deferred repaint requests ride the same 1s pass.
                core.fire_due_nudges();
                core.reconcile_grid_sizes();
                core.follow_portal_viewer_titles();
                // Snapshot first: reader completion guarantees all output for
                // these panes was enqueued before this point. Drain it, then
                // close exactly the snapshot even if another reader finishes
                // concurrently.
                let dead = core.dead_children_ready_to_reap();
                if !dead.is_empty()
                    && drain_pty_output(&mut core, &mut out_rx, None, &mut e2e_first_out)
                    && idle_exit_enabled
                    && (idle_exit_e2e || !ever_attached || *idle_count_rx.borrow() > 0)
                {
                    idle_deadline = tokio::time::Instant::now() + idle_grace;
                }
                if core.reap_dead_children(dead) == Flow::Shutdown {
                    cause = "last-dead-pane-reaped";
                    break Flow::Shutdown;
                }
                // Drift retirement: a drifted verdict at a fully
                // quiet tick (no panes, clients, or connections) retires the
                // server so the next attach spawns the installed build. The
                // stat runs off-loop in server/drift_retire.rs.
                if let Some((running, on_disk)) = drift_watch.tick(|| {
                    core.panes.is_empty()
                        && *core.client_count.borrow() == 0
                        && conns_alive.load(Ordering::Acquire) == 0
                }) {
                    eprintln!(
                        "fno mux: stale-build retire: on-disk binary changed ({} -> {}); \
                         no panes, clients, or connections; retiring so the next \
                         attach spawns the installed build",
                        running.path.display(),
                        on_disk.path.display()
                    );
                    cause = "stale-build";
                    break Flow::Shutdown;
                }
            }
            msg = core_rx.recv() => {
                // core_tx lives in the accept loop, so recv never yields None.
                let Some(msg) = msg else { cause = "channel-closed"; break Flow::Shutdown };
                // Coalesce resize storms PER CLIENT: only each client's
                // final geometry hits its viewed tab's clamp (AC1-FR). Other
                // messages drained here run after, in arrival order.
                if let CoreMsg::Resize { id, rows, cols } = msg {
                    let mut last: HashMap<u64, (u16, u16)> = HashMap::new();
                    last.insert(id, (rows, cols));
                    let mut order = vec![id];
                    let mut pending = Vec::new();
                    while let Ok(m) = core_rx.try_recv() {
                        match m {
                            CoreMsg::Resize { id, rows, cols } => {
                                if last.insert(id, (rows, cols)).is_none() {
                                    order.push(id);
                                }
                            }
                            other => pending.push(other),
                        }
                    }
                    let mut flow = Flow::Continue;
                    for id in order {
                        let (rows, cols) = last[&id];
                        flow = core.handle(CoreMsg::Resize { id, rows, cols });
                        if flow == Flow::Shutdown { break; }
                    }
                    for m in pending {
                        if flow == Flow::Shutdown { break; }
                        flow = core.handle(m);
                    }
                    if flow == Flow::Shutdown {
                        cause = "kill";
                        break Flow::Shutdown;
                    }
                } else if let CoreMsg::Mouse { id, pane, event } = msg {
                    // Wheel-scroll coalescing (mirrors the resize-storm coalescer
                    // above): fold a contiguous run of interpreted wheel ticks on
                    // one pane into ONE broadcast. Each tick is applied IN ORDER so
                    // vt.scroll's per-tick clamp is preserved (algebraic netting
                    // would cancel a clamped tick and lose a reversal at a
                    // boundary); only the intermediate frames are skipped, so a
                    // reversal queued behind in-flight opposite ticks lands in one
                    // frame instead of rubber-banding through every offset. A
                    // non-scroll event (passthrough/select) or passive sender stops
                    // the fold, so ordering and read-only gating stay unchanged.
                    if core.is_passive(id) {
                        if core.handle(CoreMsg::Mouse { id, pane, event }) == Flow::Shutdown {
                            cause = "kill";
                            break Flow::Shutdown;
                        }
                    } else if let Some(d0) = core.scroll_delta(pane, &event) {
                        let before = core.scroll_offset(pane) as i32;
                        core.scroll_tick(pane, d0);
                        let mut trailer = None;
                        while let Ok(m) = core_rx.try_recv() {
                            if let CoreMsg::Mouse { id: mid, pane: mpane, event: mev } = &m {
                                if *mpane == pane && !core.is_passive(*mid) {
                                    if let Some(d) = core.scroll_delta(pane, mev) {
                                        core.scroll_tick(pane, d);
                                        continue;
                                    }
                                }
                            }
                            trailer = Some(m);
                            break;
                        }
                        // Cap the fold's net move to one viewport: a fast trackpad
                        // flick drops many ticks in a single drain and would
                        // otherwise jump hundreds of lines at once ("too fast").
                        // The in-order clamp above is intact; this only bounds the
                        // aggregate, so a lone wheel notch (well under a screen)
                        // passes through untouched.
                        let after = core.scroll_offset(pane) as i32;
                        let cap = (core.pane_rows(pane) as i32).max(MOUSE_WHEEL_LINES);
                        let bounded = bounded_scroll_target(before, after, cap);
                        if bounded != after {
                            core.scroll_tick(pane, bounded - after);
                        }
                        if bounded != before {
                            core.broadcast_pane(pane);
                        }
                        if let Some(m) = trailer {
                            if core.handle(m) == Flow::Shutdown {
                                cause = "kill";
                                break Flow::Shutdown;
                            }
                        }
                    } else if core.handle(CoreMsg::Mouse { id, pane, event }) == Flow::Shutdown {
                        cause = "kill";
                        break Flow::Shutdown;
                    }
                } else if core.handle(msg) == Flow::Shutdown {
                    cause = "kill";
                    break Flow::Shutdown;
                }
                if core_wedge_e2e && !core_wedge_armed && !core.panes.is_empty() {
                    core_wedge_armed = true;
                    e2e_log(format_args!("core wedge armed"));
                    std::thread::park();
                }
            }
            signal = signal_rx.recv() => {
                let Some(signal) = signal else { cause = "channel-closed"; break Flow::Shutdown };
                if core.handle(signal) == Flow::Shutdown {
                    cause = "signal";
                    break Flow::Shutdown;
                }
            }
            // Attach and final-detach edges both start a bounded owner-sandbox
            // idle window. Shared ownerless servers leave this branch disabled.
            res = idle_count_rx.changed(), if idle_exit_enabled => {
                if res.is_ok() {
                    ever_attached |= *idle_count_rx.borrow() > 0;
                    idle_deadline = tokio::time::Instant::now() + idle_grace;
                }
            }
            _ = owner_check_tick.tick(), if owner.is_some() => {
                if owner.as_ref().is_some_and(|lease| !lease.alive()) {
                    let session = owner.as_ref().map(|lease| lease.session.as_str()).unwrap_or("unknown");
                    eprintln!("fno mux: sandbox owner {session} is gone; shutting down");
                    cause = "owner-gone";
                    break Flow::Shutdown;
                }
            }
            // On grace with no client, shut down owner-bound sandboxes and
            // explicit test servers through Flow::Shutdown. Replicate the
            // CoreMsg::Kill teardown (kill every pane PTY + Flow::Shutdown so
            // SocketGuard unlinks); NEVER std::process::exit, which would
            // orphan the pane shells and leak the socket file.
            _ = tokio::time::sleep_until(idle_deadline), if idle_exit_enabled => {
                if *idle_count_rx.borrow() == 0
                    && conns_alive.load(std::sync::atomic::Ordering::Acquire) == 0
                {
                    eprintln!("fno mux: idle-exit: no client for grace window");
                    cause = "idle-exit";
                    break Flow::Shutdown;
                }
                idle_deadline = tokio::time::Instant::now() + idle_grace;
            }
        }
        // Loop-tail choke point: the out_rx/exit_rx arms mutate
        // `clients` via the dead-client sweeps (broadcast_pane /
        // sync_focused_modes / close_pane) without a `handle()` call, so the
        // handle-tail publish alone would leave the count stale on exactly
        // the orphan path the readers gate on.
        core.publish_client_count();
        ever_attached |= !core.clients.is_empty();
    };
    if flow == Flow::Shutdown {
        eprintln!(
            "fno mux: shutting down ({cause}, {} panes)",
            core.panes.len()
        );
        // Capture only from a safe restore state and current store generation.
        core.record_exit(cause);
        core.kill_all_panes();
        core.bye_all("session ended");
        // Give writer tasks a beat to flush the Byes; a lost Bye reads as
        // "session ended (server closed)" client-side, so this is best-effort.
        tokio::time::sleep(BYE_FLUSH).await;
    }
    shutdown_complete.store(true, Ordering::Release);
    0
}

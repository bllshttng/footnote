//! The periodic fleet arms, one driver: which arms run on each tick, and on
//! which home. Extracted verbatim from `run()`'s idle tick (daemon.rs is
//! shrink-only) so the sandbox guard can gate the whole set at one call site:
//! a daemon on a sandbox home builds no `FleetArms` at all, because every arm
//! here resolves its targets from the real cwd, the real graph, the mux, `ps`
//! and the `fno` porcelain - none from the daemon's own home.

use super::*;

/// Arm state used only by the tick arms. The idle-probe locals
/// (`idle_probe_in_flight`, `idle_probe_verdict`, `drift_flag`) stay in
/// `run()`: they serve the idle-exit tail, not the fleet.
pub(super) struct FleetArms {
    // Screen-manifest scrape gate: a slow mux stalls only its own sweep.
    scrape_in_flight: Arc<std::sync::atomic::AtomicBool>,
    // Terminal-stop gate: a large marker set never serializes inline.
    terminal_stop_in_flight: Arc<std::sync::atomic::AtomicBool>,
    worktree_sweep_in_flight: Arc<std::sync::atomic::AtomicBool>,
    // Orphaned-test-binary reap gate: shells ps + a kill, off the core loop.
    orphan_sweep_in_flight: Arc<std::sync::atomic::AtomicBool>,
    last_orphan_sweep: Instant,
    liveness_sweep_in_flight: Arc<std::sync::atomic::AtomicBool>,
    last_liveness_sweep: Instant,
    // The periodic arms: each module owns its cadence, gate and memory.
    machine_watch: crate::machine_watch::Arm,
    merge_close: crate::merge_close::Arm,
    crown_ledger: crate::king_ledger::Arm,
    fleet_page: crate::fleet_page::Arm,
    arm_watch: crate::arm_watch::Arm,
    provider_cap: crate::provider_cap_verbs::Arm,
    slot_cutover: crate::slot_cutover::Arm,
    attention: crate::attention_arm::Arm,
    burn_watch: crate::burn_watch::Arm,
    // Retirement-sweep cadence: the throttle stamp beside the gate,
    // plus the next interval cell the sweep body hands back (the idle-probe
    // verdict pattern), so the tick reads a mutex instead of config files.
    last_gc_sweep: Instant,
    retire_interval_next: Arc<crate::gc::RetireIntervalCell>,
    // Dead-row GC gate: its dormant check shells out to the truth
    // probe, so it gets the same one-in-flight discipline as its neighbors.
    gc_in_flight: Arc<std::sync::atomic::AtomicBool>,
    // Stale-question reconcile: same one-in-flight discipline as the sweeps
    // beside it. The verb dedupes on outcome identity, so an extra run is a
    // no-op; the gate exists so a slow fleet probe never stacks.
    stale_sweep_in_flight: Arc<std::sync::atomic::AtomicBool>,
    // Park sweep: same one-in-flight discipline. The verb is idempotent on a
    // store nobody touched, so an extra run is a no-op; the gate exists so a
    // slow head probe never stacks.
    park_sweep_in_flight: Arc<std::sync::atomic::AtomicBool>,
}

impl FleetArms {
    pub(super) fn new(opts: &DaemonOptions) -> Self {
        Self {
            scrape_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            terminal_stop_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worktree_sweep_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            orphan_sweep_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_orphan_sweep: Instant::now(),
            liveness_sweep_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_liveness_sweep: Instant::now(),
            machine_watch: crate::machine_watch::Arm::default(),
            merge_close: crate::merge_close::Arm::default(),
            crown_ledger: crate::king_ledger::Arm::default(),
            fleet_page: crate::fleet_page::Arm::new(opts.agents_config_cwd.clone()),
            arm_watch: crate::arm_watch::Arm::new(opts.agents_config_cwd.clone()),
            provider_cap: crate::provider_cap_verbs::Arm::new(opts.agents_config_cwd.clone()),
            slot_cutover: crate::slot_cutover::Arm::new(opts.agents_config_cwd.clone()),
            attention: crate::attention_arm::Arm::new(opts.agents_config_cwd.clone()),
            burn_watch: crate::burn_watch::Arm::default(),
            last_gc_sweep: Instant::now(),
            retire_interval_next: crate::gc::seed_retire_interval_cell(&opts.agents_config_cwd),
            gc_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stale_sweep_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            park_sweep_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// One idle tick's worth of fleet arms. Every arm keeps the loop's rule:
    /// nothing blocking runs INLINE, so each shells out behind
    /// `spawn_blocking` (or a task) plus its one-in-flight gate.
    pub(super) fn tick(&mut self, ctx: &Arc<Ctx>) {
        // Screen-manifest scrape sweep (the badge-lattice fallback
        // rung): subprocesses + file IO, so it runs off-loop under
        // spawn_blocking behind the one-in-flight gate.
        if !self
            .scrape_in_flight
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let flag = Arc::clone(&self.scrape_in_flight);
            let home = ctx.home.clone();
            let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
            let notify_on_blocked = ctx.opts.notify_on_blocked;
            tokio::task::spawn_blocking(move || {
                let _gate = SweepGate(flag);
                crate::scrape::scrape_sweep(&home, &emitter, notify_on_blocked);
            });
        }
        // Retirement sweep: a row leaves when its work is done (reverse
        // join) and its transcript is quiet past `agents.retire_grace_s`.
        // The dead crown sweep runs first. Throttled to
        // `agents.retire_interval_s`; off-loop.
        let retire_interval = crate::gc::retire_interval_snapshot(&self.retire_interval_next);
        crate::gc::maybe_retirement_sweep(
            &mut self.last_gc_sweep,
            &self.gc_in_flight,
            &self.retire_interval_next,
            ctx.home.clone(),
            ctx.opts.agents_config_cwd.clone(),
            ctx.home.events_jsonl(),
            retire_interval,
            || crate::gc::mux_tab_sweep(false, false),
            crate::gc::production_roster_sweep,
            crate::gc::production_crown_sweep,
        );
        // Worktree sweep + merge reaper: the sweep backstops
        // what the reaper cannot reach; the reaper is the merge-triggered
        // consumer of `merge_cleanup_requested` (60s floor). Both
        // off-loop; grace and stop order live in merge_reap.rs.
        if !self
            .worktree_sweep_in_flight
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let flag = Arc::clone(&self.worktree_sweep_in_flight);
            let home = ctx.home.clone();
            let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
            let grace_cwd = ctx.opts.agents_config_cwd.clone();
            tokio::task::spawn_blocking(move || {
                let _gate = SweepGate(flag);
                let roots = worktree_sweep::registry_repo_roots(&home);
                let now = now_epoch_secs();
                worktree_sweep::worktree_sweep(
                    &home,
                    &emitter,
                    now,
                    &roots,
                    &|root| {
                        // A pending merge-cleanup request is the standing
                        // order: the pass applies while one waits.
                        crate::merge_reap::merge_cleanup_requested(&home, root).into()
                    },
                    &|root, apply| {
                        let mut cmd = std::process::Command::new("fno");
                        cmd.current_dir(root)
                            .env("FNO_AGENTS_HOME", home.root())
                            .args(["agents", "workspace", "worktree", "cleanup", "--merged"]);
                        if apply {
                            cmd.arg("--apply");
                        }
                        match cmd.output() {
                            Ok(output) => worktree_sweep::WorktreeSweepOutput {
                                exit_code: output.status.code(),
                                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                            },
                            Err(error) => worktree_sweep::WorktreeSweepOutput {
                                exit_code: None,
                                stdout: String::new(),
                                stderr: error.to_string(),
                            },
                        }
                    },
                );
                let grace_secs = crate::agents_config::retire_grace_secs(&grace_cwd) as i64;
                crate::merge_reap::consume_merge_cleanup_requests(
                    &home, &roots, &emitter, grace_secs,
                );
                // Daily janitor; gate and receipt in reclaim.rs.
                crate::reclaim::maybe_run_daily(&home);
            });
        }
        crate::orphan_reap::maybe_sweep(
            &mut self.last_orphan_sweep,
            &self.orphan_sweep_in_flight,
            ctx.home.events_jsonl(),
        );
        crate::machine_watch::maybe_tick(&self.machine_watch, ctx.home.clone());
        crate::merge_close::maybe_tick(&self.merge_close, ctx.home.clone());
        crate::king_ledger::maybe_tick(&self.crown_ledger, ctx.home.clone());
        crate::fleet_page::maybe_tick(&self.fleet_page, ctx.home.clone());
        crate::arm_watch::maybe_tick(&self.arm_watch, ctx.home.clone());
        crate::provider_cap_verbs::maybe_tick(&self.provider_cap, ctx.home.clone());
        crate::slot_cutover::maybe_tick(&self.slot_cutover, ctx.home.clone());
        crate::attention_arm::maybe_tick(&self.attention, ctx.home.clone());
        crate::burn_watch::maybe_tick(&self.burn_watch, ctx.home.clone());
        // Serve-only liveness tick: the served pair is the sweep's measurement,
        // refreshed every SERVED_LIVENESS_CADENCE; off-loop, one-in-flight.
        let codex_threads_for_liveness = Arc::clone(&ctx.codex_threads);
        crate::liveness_sweep::maybe_sweep(
            &mut self.last_liveness_sweep,
            &self.liveness_sweep_in_flight,
            ctx.home.clone(),
            ctx.home.events_jsonl(),
            Arc::new(move |entry: &RegistryEntry| {
                match codex_threads_for_liveness.try_lock() {
                    Ok(guard) => guard.contains_key(&entry.name),
                    // An actor is mid insert/remove: hosted, so a race
                    // can never settle a thread the map is about to name.
                    Err(_) => true,
                }
            }),
        );
        // Terminal-stop sweep: exit fire-and-forget `claude --bg`
        // workers finalize marked terminal, so a shipped bg /target frees
        // its slot instead of parking at an idle prompt forever. Spawned
        // off the select arm behind a one-in-flight gate (mirrors the
        // scrape sweep) so N serialized `claude stop`s never starve
        // accept()/SIGTERM. Cheap when there are no markers.
        if !self
            .terminal_stop_in_flight
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let flag = Arc::clone(&self.terminal_stop_in_flight);
            let home = ctx.home.clone();
            let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
            tokio::spawn(async move {
                let _gate = SweepGate(flag);
                terminal_stop_sweep(&home, &emitter).await;
            });
        }
        // Stale-question reconcile, the arm above `stale_sweep`: its
        // doc comment there covers the shape.
        if !self
            .stale_sweep_in_flight
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let flag = Arc::clone(&self.stale_sweep_in_flight);
            let home = ctx.home.clone();
            let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
            tokio::task::spawn_blocking(move || {
                let _gate = SweepGate(flag);
                stale_sweep(&home, &emitter, now_epoch_secs(), &|| {
                    std::process::Command::new("fno")
                        .args(["agents", "stale-escalate", "--json"])
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                });
            });
        }
        // Park sweep, the arm beside `stale_sweep`: same doc comment
        // there covers the one-in-flight shape. The run closure walks
        // every repo root the registry knows, so parked rows of other
        // repos are un-parked from THEIR checkout (the head probe resolves PR numbers against the repo).
        if !self
            .park_sweep_in_flight
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            let flag = Arc::clone(&self.park_sweep_in_flight);
            let home = ctx.home.clone();
            let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
            tokio::task::spawn_blocking(move || {
                let _gate = SweepGate(flag);
                park_sweep(&home, &emitter, now_epoch_secs(), &|| {
                    sweeps::sweep_all_roots(&home)
                });
            });
        }
        crate::question_sweep::daemon_tick(&ctx.home, now_epoch_secs());
    }
}

//! `fno-agents-daemon` entrypoint (Wave 3). Argv parsing -> `daemon::run`.
//!
//! Usage:
//! ```text
//! fno-agents-daemon            # start (foreground); lazy-exits when idle
//! fno-agents-daemon --once     # run recovery + serve until idle/SIGTERM
//! ```
//! The client lazy-starts this detached on first need; running it directly is
//! for debugging and for the Python wrapper's explicit `daemon` sub-mode.

use fno_agents::daemon::{run, DaemonOptions};
use fno_agents::paths::AgentsHome;
use std::time::Duration;

fn main() {
    // A failed daemon must surface a non-zero exit and a clear stderr line; it
    // must never panic silently (Silent-Failure-Hunter posture).
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fno-agents-daemon: cannot build tokio runtime: {e}");
            std::process::exit(1);
        }
    };

    let home = AgentsHome::from_env();
    let mut opts = DaemonOptions::default();
    // Allow an idle-exit override (seconds) via env for tests / tuning.
    if let Ok(s) = std::env::var("FNO_AGENTS_IDLE_EXIT_SECS") {
        if let Ok(secs) = s.parse::<u64>() {
            opts.idle_exit = Duration::from_secs(secs);
        }
    }
    // Dead-row GC grace window (x-b1aa): resolve config.agents.dead_row_grace
    // (env FNO_AGENTS_DEAD_ROW_GRACE_SECS > FNO_CONFIG > project > global >
    // default 1h). The daemon's cwd is where it was lazy-started; a global
    // ~/.fno knob is read via the global fallback regardless.
    let grace_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    opts.dead_row_grace =
        Duration::from_secs(fno_agents::agents_config::dead_row_grace_secs(&grace_cwd));
    // Badge -> OS notification knobs (x-dd84): config.mux.notify_on_blocked
    // (default ON) / notify_on_done (default OFF), read from the same cwd.
    opts.notify_on_blocked = fno_agents::agents_config::notify_on_blocked_enabled(&grace_cwd);
    opts.notify_on_done = fno_agents::agents_config::notify_on_done_enabled(&grace_cwd);
    // Opt out of the startup reconcile sweep for the fastest cold start
    // (Architecture B, plan ab-70faa65b). Any non-empty value disables it.
    if std::env::var("FNO_AGENTS_NO_STARTUP_RECONCILE")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        opts.reconcile_on_start = false;
    }

    match rt.block_on(run(home, opts)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("fno-agents-daemon: {e}");
            std::process::exit(1);
        }
    }
}

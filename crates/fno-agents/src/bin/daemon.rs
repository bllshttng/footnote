//! `fno-agents-daemon` entrypoint (Wave 3). Argv parsing -> `daemon::run`.
//!
//! Usage:
//! ```text
//! fno-agents-daemon                    # start (foreground); lazy-exits when idle
//! fno-agents-daemon --home <dir>       # name the home in argv; must agree with
//!                                      # FNO_AGENTS_HOME or the daemon refuses
//! fno-agents-daemon --once             # run recovery + serve until idle/SIGTERM
//! ```
//! The client lazy-starts this detached on first need; running it directly is
//! for debugging and for the Python wrapper's explicit `daemon` sub-mode.

use fno_agents::daemon::{run, DaemonOptions};
use fno_agents::paths::AgentsHome;
use std::time::Duration;

fn main() {
    // `version [--json]`: report the baked-in build rev so `fno update` can
    // verify this bin is the SAME build as its triad siblings, not just present.
    // Execs cheaply and returns without touching a running daemon or the runtime.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("version" | "-V" | "--version")
    ) {
        fno_agents::version::print_version(args.iter().any(|a| a == "--json"));
        return;
    }

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

    // `--home <path>` (x-cd31): names the home in argv so a stray daemon can be
    // attributed in ps. The env stays the resolution source; a --home that
    // DISAGREES with the env is a daemon about to serve a home nobody expects,
    // so refuse it loudly rather than silently prefer one. No --home keeps the
    // env-only behavior (a daemon started by hand for debugging).
    if let Some(home_arg) = parse_home_arg(&args) {
        let env_home = AgentsHome::from_env();
        let same = |a: &std::path::Path, b: &std::path::Path| {
            std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf())
                == std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf())
        };
        if !same(std::path::Path::new(&home_arg), env_home.root()) {
            eprintln!(
                "fno-agents-daemon: --home {} disagrees with FNO_AGENTS_HOME {}; refusing to start",
                home_arg,
                env_home.root().display()
            );
            std::process::exit(2);
        }
    }

    let home = AgentsHome::from_env();
    let mut opts = DaemonOptions::default();
    // Allow an idle-exit override (seconds) via env for tests / tuning.
    if let Ok(s) = std::env::var("FNO_AGENTS_IDLE_EXIT_SECS") {
        if let Ok(secs) = s.parse::<u64>() {
            opts.idle_exit = Duration::from_secs(secs);
        }
    }
    // Dead-row GC grace window (x-b1aa, per-harness since x-9de7 task 6):
    // resolve config.agents.dead_row_grace.<harness> (env
    // FNO_AGENTS_DEAD_ROW_GRACE_SECS > FNO_CONFIG > project > global >
    // default 1h) once per row, at sweep time -- the idle tick reads this cwd,
    // not a pre-resolved Duration. The daemon's cwd is where it was
    // lazy-started; a global ~/.fno knob is read via the global fallback
    // regardless.
    opts.dead_row_grace_cwd =
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Badge -> OS notification knobs (x-dd84): config.mux.notify_on_blocked
    // (default ON) / notify_on_done (default OFF), read from the same cwd.
    opts.notify_on_blocked =
        fno_agents::agents_config::notify_on_blocked_enabled(&opts.dead_row_grace_cwd);
    opts.notify_on_done =
        fno_agents::agents_config::notify_on_done_enabled(&opts.dead_row_grace_cwd);
    // Opt out of the startup reconcile sweep for the fastest cold start
    // (Architecture B, plan ab-70faa65b). Any non-empty value disables it.
    if std::env::var("FNO_AGENTS_NO_STARTUP_RECONCILE")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        opts.reconcile_on_start = false;
    }

    let outcome = rt.block_on(run(home, opts));

    // Bound the wind-down instead of letting `rt` drop at the end of `main`.
    // Dropping a runtime WAITS for every already-started `spawn_blocking` task,
    // and the sweeps are exactly that: a gc sweep over a large roster shells one
    // child per row and runs for minutes. So a plain drop kept the process alive
    // long after SIGTERM had been received and `daemon_exited {"clean": true}`
    // had been written -- an event log claiming an exit that had not happened,
    // which is a worse signal for the watchdog than a slow exit. The sweeps own
    // nothing that must survive: each is a read plus an advisory-locked write
    // that either landed or did not, and the next daemon redoes it.
    rt.shutdown_timeout(std::time::Duration::from_secs(5));

    if let Err(e) = outcome {
        eprintln!("fno-agents-daemon: {e}");
        std::process::exit(1);
    }
}

/// The value of `--home <path>` (or `--home=<path>`) from the daemon's argv,
/// or `None` when absent. Other argv is not ours to judge (the `version`
/// subcommand is matched earlier; anything else is ignored as today).
fn parse_home_arg(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--home" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--home=") {
            return Some(v.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_home_arg_takes_both_spellings() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        assert_eq!(parse_home_arg(&s(&["--home", "/a"])), Some("/a".into()));
        assert_eq!(parse_home_arg(&s(&["--home=/b"])), Some("/b".into()));
        assert_eq!(parse_home_arg(&s(&["--once", "--home", "/a"])), Some("/a".into()));
        // Absent, or present with no value.
        assert_eq!(parse_home_arg(&s(&[])), None);
        assert_eq!(parse_home_arg(&s(&["--once"])), None);
        assert_eq!(parse_home_arg(&s(&["--home"])), None);
    }
}

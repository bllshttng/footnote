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
    // `version [--json]`: report the baked-in build rev so `fno doctor update` can
    // verify this bin is the SAME build as its triad siblings, not just present.
    // Execs cheaply and returns without touching a running daemon or the runtime.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("version" | "-V" | "--version")
    ) {
        fno_agents::version::print_version(fno_agents::json_output::requested(&args));
        return;
    }

    // Test harnesses may launch children with SIGTERM blocked. The daemon owns
    // its SIGTERM listener below, so do not inherit a mask that makes graceful
    // shutdown permanently pending.
    #[cfg(unix)]
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }

    // Shed a leaked anti-recursion pin before the runtime starts any other
    // thread: env mutation is only sound while the process is still
    // single-threaded, and both the tokio runtime below and the test-owner
    // watchdog spawn threads that may read this same var concurrently.
    if leaked_dispatch_pin(std::env::var_os("FNO_AGENTS_RUNTIME").as_deref()) {
        std::env::remove_var("FNO_AGENTS_RUNTIME");
    }

    // Same single-threaded rule: fill the build-dir env before the runtime
    // below spawns threads, so the heal lane's `fno doctor update` and every
    // other child inherits it even when the daemon's parent passed no value.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    fno_agents::cargo_build_dirs::fill_build_dir_env(&cwd);

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

    // `--home <path>`: names the home in argv so a stray daemon can be
    // attributed in ps. The env stays the resolution source; a --home that
    // DISAGREES with the env is a daemon about to serve a home nobody expects,
    // so refuse it loudly rather than silently prefer one. No --home keeps the
    // env-only behavior (a daemon started by hand for debugging).
    if let Some(home_arg) = parse_home_arg(&args) {
        let env_home = AgentsHome::from_env();
        if !fno_agents::paths::same_path(std::path::Path::new(&home_arg), env_home.root()) {
            eprintln!(
                "fno-agents-daemon: --home {} disagrees with FNO_AGENTS_HOME {}; refusing to start",
                home_arg,
                env_home.root().display()
            );
            std::process::exit(2);
        }
    }

    if let Some((owner_pid, owner_birth)) = fno_agents::test_run::declared_owner_from_env() {
        if !fno_agents::test_run::owner_alive(owner_pid, owner_birth) {
            eprintln!(
                "fno-agents-daemon: test owner pid={owner_pid} birth={owner_birth} is not alive; refusing to start (unset FNO_TEST_OWNER_PID outside a test run)"
            );
            std::process::exit(3);
        }
        let armed = fno_agents::test_run::spawn_owner_watchdog(
            owner_pid,
            owner_birth,
            "fno-daemon-test-owner",
            move || {
                eprintln!(
                    "fno-agents-daemon: test_owner_reaped owner_pid={owner_pid} owner_birth={owner_birth}"
                );
                // SAFETY: SIGTERM to self enters the existing graceful shutdown arm.
                unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
            },
        );
        if !armed {
            eprintln!("fno-agents-daemon: refusing to start without an owner watchdog");
            std::process::exit(3);
        }
    }

    let mut home = AgentsHome::from_env();
    let mut opts = DaemonOptions::default();
    // Allow an idle-exit override (seconds) via env for tests / tuning.
    if let Ok(s) = std::env::var("FNO_AGENTS_IDLE_EXIT_SECS") {
        if let Ok(secs) = s.parse::<u64>() {
            opts.idle_exit = Duration::from_secs(secs);
        }
    }
    // Retirement sweep config: resolve config.agents.retire_grace_s
    // and reap-receipt retention (env > FNO_CONFIG > project > global >
    // defaults) at sweep time -- the idle tick reads opts.agents_config_cwd
    // (the anchor pinned below), not a pre-resolved Duration. A
    // global ~/.fno knob is read via the global fallback regardless.
    // Pin the daemon's cwd for life, before any child can inherit it. The
    // daemon keeps whoever lazy-started it as its cwd; when that dir is a
    // worktree the merge reaper later deletes it, and every child spawned
    // without an explicit cwd then fails: roster reads exit 1 and
    // reconcile-style runs hit FileNotFoundError in os.getcwd, each cured
    // only by a manual restart. The canonical checkout is never reaped, so
    // anchor there; outside any repo, the agents home.
    let launch_dir = std::env::current_dir().ok();
    // A relative FNO_AGENTS_HOME resolves against the launch cwd. Absolutize
    // it BEFORE the chdir below, or the daemon would bind its socket under
    // the anchor while the client still waits under the launch dir.
    home = AgentsHome::at(absolutize_home(launch_dir.as_deref(), home.root()));
    let _ = home.ensure_root();
    let anchor = daemon_anchor(launch_dir.as_deref(), home.root());
    match std::env::set_current_dir(&anchor) {
        Ok(()) => opts.agents_config_cwd = anchor,
        Err(e) => {
            eprintln!("fno-agents-daemon: cannot enter {}: {e}", anchor.display());
        }
    }
    // Badge -> OS notification knobs: config.mux.notify_on_blocked
    // (default ON) / notify_on_done (default OFF), read from the same cwd.
    opts.notify_on_blocked =
        fno_agents::agents_config::notify_on_blocked_enabled(&opts.agents_config_cwd);
    opts.notify_on_done =
        fno_agents::agents_config::notify_on_done_enabled(&opts.agents_config_cwd);
    // Opt out of the startup reconcile sweep for the fastest cold start
    // (Architecture B, plan). Any non-empty value disables it.
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

/// `FNO_AGENTS_RUNTIME=python` is a per-child anti-recursion pin. A daemon
/// lazy-started by a pinned child would keep it for life, and every arm child
/// would then refuse the verbs that have no Python leg (`rm` exits 127).
fn leaked_dispatch_pin(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v.to_string_lossy().trim().eq_ignore_ascii_case("python"))
}

/// The dir the daemon runs in for life. A worktree gets reaped under a live
/// daemon; its canonical checkout does not. Outside a repo, or when the
/// launch dir cannot be read, the agents home is the anchor.
fn daemon_anchor(launch: Option<&std::path::Path>, home: &std::path::Path) -> std::path::PathBuf {
    if let Some(root) = launch.and_then(fno_agents::paths::canonical_repo_root) {
        return root;
    }
    if home.is_absolute() {
        home.to_path_buf()
    } else {
        launch.unwrap_or(std::path::Path::new(".")).to_path_buf()
    }
}

/// Absolute form of the agents home, resolved against the launch cwd. A
/// relative home must be pinned before the daemon enters its anchor, or
/// every later home path would resolve against the anchor instead and split
/// daemon and client onto two different homes.
fn absolutize_home(launch: Option<&std::path::Path>, home: &std::path::Path) -> std::path::PathBuf {
    if home.is_absolute() {
        return home.to_path_buf();
    }
    launch.unwrap_or(std::path::Path::new(".")).join(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaked_dispatch_pin_matches_python_case_and_whitespace_insensitively() {
        let some = |v: &str| Some(std::ffi::OsString::from(v));
        assert!(leaked_dispatch_pin(some("python").as_deref()));
        assert!(leaked_dispatch_pin(some(" Python ").as_deref()));
        assert!(!leaked_dispatch_pin(some("rust").as_deref()));
        assert!(!leaked_dispatch_pin(some("auto").as_deref()));
        assert!(!leaked_dispatch_pin(some("").as_deref()));
        assert!(!leaked_dispatch_pin(None));
    }

    #[test]
    fn parse_home_arg_takes_both_spellings() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        assert_eq!(parse_home_arg(&s(&["--home", "/a"])), Some("/a".into()));
        assert_eq!(parse_home_arg(&s(&["--home=/b"])), Some("/b".into()));
        assert_eq!(
            parse_home_arg(&s(&["--once", "--home", "/a"])),
            Some("/a".into())
        );
        // Absent, or present with no value.
        assert_eq!(parse_home_arg(&s(&[])), None);
        assert_eq!(parse_home_arg(&s(&["--once"])), None);
        assert_eq!(parse_home_arg(&s(&["--home"])), None);
    }

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-daemon-anchor-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} in {dir:?} did not run: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} in {dir:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn daemon_anchor_resolves_the_canonical_root_from_a_linked_worktree() {
        if !git_available() {
            return;
        }
        let base = tmp_dir("wt");
        let main = base.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@t"]);
        git(&main, &["config", "user.name", "t"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let linked = base.join("wt");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                linked.to_str().unwrap(),
                "-b",
                "feat",
            ],
        );
        let home = base.join("home");
        assert_eq!(
            daemon_anchor(Some(&linked), &home),
            std::fs::canonicalize(&main).unwrap()
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn daemon_anchor_falls_back_to_the_home_outside_a_repo() {
        let base = tmp_dir("nogit");
        std::fs::create_dir_all(&base).unwrap();
        let home = base.join("home");
        assert_eq!(daemon_anchor(Some(&base), &home), home);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn daemon_anchor_falls_back_to_the_home_when_launch_is_unknown() {
        let home = tmp_dir("unknown-home");
        assert_eq!(daemon_anchor(None, &home), home);
    }

    #[test]
    fn absolutize_home_keeps_an_absolute_home_unchanged() {
        let home = tmp_dir("abs");
        let launch = tmp_dir("launch-abs");
        assert_eq!(absolutize_home(Some(&launch), &home), home);
        assert_eq!(absolutize_home(None, &home), home);
    }

    #[test]
    fn absolutize_home_joins_a_relative_home_onto_the_launch_dir() {
        let launch = tmp_dir("launch-rel");
        let home = std::path::Path::new("relhome");
        assert_eq!(absolutize_home(Some(&launch), home), launch.join("relhome"));
        // No readable launch dir: the relative form survives and the anchor
        // keeps the launch cwd instead.
        assert_eq!(
            absolutize_home(None, home),
            std::path::Path::new(".").join("relhome")
        );
    }
}

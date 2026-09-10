//! One native process-group owner for a single `fno doctor test` run.
//!
//! Admits under a machine-wide `test:suite` claim, spawns the given argv as
//! the leader of a fresh session/process group, and ALWAYS attempts to
//! terminate that whole group before this process exits - on a normal exit,
//! not only on timeout or interruption. The defect this closes:
//! `wait_or_kill_group` in `cli/src/fno/test_runner.py` only killed the
//! group on `TimeoutExpired` or an exception, so cargo's own clean exit left
//! an orphaned `deps/` test binary running in the same group (one such binary
//! held 225 of the machine's 228 zombies for 3h32m after cargo itself had
//! already exited).
//!
//! `killpg` targets every process sharing the group's pgid regardless of
//! parent/child lineage, which is why this reaches a grandchild the leader
//! never `wait()`s for - the shape a plain `wait()`-based reaper cannot see.
//!
//! Internal dispatch only (`fno-agents test-run`, matched in `bin/client.rs`
//! next to `probe-run`) - never a public `fno` verb.
//! `cli/src/fno/test_runner.py` is the thin Python adapter that shells to
//! this binary.

use std::io::Write as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SUITE_CLAIM_KEY: &str = "test:suite";
/// Grace window for a SIGTERM to land before escalating to SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(3);
/// Poll interval while waiting on a held admission claim or the child.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The signal number this OWNER received, or 0 (none). A plain Python
/// `Popen` wrapper no longer isolates this process into its own group (that
/// job moved to the child's `setsid()`), so a terminal Ctrl-C reaches this
/// owner directly - without a handler, the default disposition would kill it
/// before its own cleanup ever ran. The handler does the one thing
/// async-signal-safe code may do: store an integer.
static RECEIVED_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn record_signal(sig: libc::c_int) {
    RECEIVED_SIGNAL.store(sig, std::sync::atomic::Ordering::SeqCst);
}

fn install_signal_handlers() {
    // SAFETY: `signal()` with a plain extern "C" fn pointer and a real
    // signal number is the documented, single-threaded-at-startup use.
    unsafe {
        libc::signal(libc::SIGINT, record_signal as *const () as usize);
        libc::signal(libc::SIGTERM, record_signal as *const () as usize);
    }
}

fn received_signal() -> Option<i32> {
    let sig = RECEIVED_SIGNAL.load(std::sync::atomic::Ordering::SeqCst);
    (sig != 0).then_some(sig)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A fresh id per run so a receipt line can be matched back to one specific
/// incarnation, never a bare (reusable) pid.
fn new_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{:x}", std::process::id(), nanos)
}

fn emit(run_id: &str, kind: &str, fields: &[(&str, String)]) {
    let mut line = format!("test_run: {kind} run={run_id} ts={}", now_secs());
    for (k, v) in fields {
        line.push(' ');
        line.push_str(k);
        line.push('=');
        line.push_str(v);
    }
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[derive(Debug, PartialEq)]
struct Options {
    timeout: Duration,
    claims_root: Option<PathBuf>,
    argv: Vec<String>,
}

/// `[--timeout SECS] [--claims-root PATH] -- ARGV...`. The separator is
/// mandatory: a flag-shaped argv token (e.g. a test binary's own `--nocapture`)
/// must never be mistaken for one of ours.
fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut timeout_secs: u64 = 1800;
    let mut claims_root: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--timeout" => {
                let v = args.get(i + 1).ok_or("--timeout needs a value")?;
                timeout_secs = v
                    .parse()
                    .map_err(|_| format!("--timeout: not a number: {v}"))?;
                i += 2;
            }
            "--claims-root" => {
                let v = args.get(i + 1).ok_or("--claims-root needs a value")?;
                claims_root = Some(PathBuf::from(v));
                i += 2;
            }
            "--" => {
                return Ok(Options {
                    timeout: Duration::from_secs(timeout_secs),
                    claims_root,
                    argv: args[i + 1..].to_vec(),
                });
            }
            other => return Err(format!("unrecognized argument before `--`: {other}")),
        }
    }
    Err("missing `--` argv separator".to_string())
}

/// A nested `fno doctor test` invocation (one test spawning another suite)
/// inherits the outer admission instead of deadlocking on its own claim -
/// but only when the declared owner is a REAL, currently-live process with
/// the matching birth identity. A stale or foreign env value (inherited from
/// an unrelated ancestor shell) is never trusted; that case re-acquires a
/// fresh claim like a top-level run.
fn nested_owner() -> Option<(u32, u64)> {
    owner_from_env()
}

/// The declared test-run owner from `FNO_TEST_OWNER_PID`/`FNO_TEST_OWNER_BIRTH`,
/// verified against the LIVE process (never trusted on the name alone) - the
/// one parser test-owned keeper lanes share (pane_keeper.rs, graph_keeper.rs)
/// so a malformed or foreign token reads the same way everywhere.
pub fn owner_from_env() -> Option<(u32, u64)> {
    let pid: u32 = std::env::var("FNO_TEST_OWNER_PID").ok()?.parse().ok()?;
    let birth: u64 = std::env::var("FNO_TEST_OWNER_BIRTH").ok()?.parse().ok()?;
    owner_alive(pid, birth).then_some((pid, birth))
}

/// Whether `pid` is still the SAME incarnation that was born at `birth` - a
/// recycled pid with a different birth timestamp answers `false`.
pub fn owner_alive(pid: u32, birth: u64) -> bool {
    crate::daemon::process_start_time(pid) == Some(birth)
}

/// Block until the claim is ours or `deadline` passes. A contender spawns
/// ZERO workers while waiting: the loop returns before any `Command` is
/// built.
fn acquire_suite_claim(
    run_id: &str,
    holder: &str,
    root: Option<&Path>,
    deadline: Instant,
) -> Result<(), i32> {
    loop {
        let opts = crate::claims::AcquireOpts {
            pid: Some(std::process::id()),
            ttl_ms: Some(3_600_000),
            reason: Some("test-run".to_string()),
            root: root.map(PathBuf::from),
            ..Default::default()
        };
        match crate::claims::acquire(SUITE_CLAIM_KEY, holder, opts) {
            crate::claims::AcquireOutcome::Acquired(_) => return Ok(()),
            crate::claims::AcquireOutcome::HeldByOther {
                holder: h,
                pid,
                host,
            } => {
                if Instant::now() >= deadline {
                    emit(
                        run_id,
                        "suite_wait_timeout",
                        &[("holder", h), ("pid", format!("{pid:?}")), ("host", host)],
                    );
                    return Err(124);
                }
                emit(
                    run_id,
                    "suite_waiting",
                    &[("holder", h), ("pid", format!("{pid:?}")), ("host", host)],
                );
                std::thread::sleep(POLL_INTERVAL.max(Duration::from_millis(500)));
            }
            crate::claims::AcquireOutcome::Error(e) => {
                eprintln!("fno-agents test-run: claim error: {e}");
                return Err(2);
            }
        }
    }
}

/// Spawn `argv` as the leader of a brand-new session (so it, and everything
/// it forks, carries a pgid this process never shares - a signal aimed at
/// the group can never boomerang onto the owner itself). The owner identity
/// rides the child's env so nested runners and test-owned keepers can find
/// it without a second IPC channel.
fn spawn_group(argv: &[String], owner_pid: u32, owner_birth: u64) -> std::io::Result<Child> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.env("FNO_TEST_OWNER_PID", owner_pid.to_string());
    cmd.env("FNO_TEST_OWNER_BIRTH", owner_birth.to_string());
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    // SAFETY: setsid() is async-signal-safe and takes no arguments; called
    // here it runs in the child after fork, before exec, single-threaded.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()
}

fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Why [`wait_bounded`] stopped without a real exit status.
enum Unfinished {
    TimedOut,
    Signalled(i32),
}

/// Poll until the child exits, `deadline` passes, or this OWNER itself
/// receives SIGINT/SIGTERM. `Ok` carries the real exit code (a `wait()`, not
/// a guess); `Err` means the child is still running and must be cleaned up.
fn wait_bounded(child: &mut Child, deadline: Instant) -> Result<i32, Unfinished> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(exit_code_of(status)),
            Ok(None) => {
                if let Some(sig) = received_signal() {
                    return Err(Unfinished::Signalled(sig));
                }
                if Instant::now() >= deadline {
                    return Err(Unfinished::TimedOut);
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => return Err(Unfinished::TimedOut),
        }
    }
}

/// SIGTERM the whole group, give it [`TERM_GRACE`] to leave, then SIGKILL.
/// Returns whether the group is confirmed EMPTY afterward - `killpg(pgid, 0)`
/// answers that for every member by pgid, not only processes this owner
/// itself `wait()`s for, which is exactly what a grandchild leaked by a
/// leader's own clean exit needs.
fn cleanup_group(pgid: i32) -> bool {
    // SAFETY: pgid is this run's own child pid, and that child called
    // setsid() before exec, so pgid == its own pid: the signal never reaches
    // a process outside processes THIS run spawned.
    unsafe { libc::killpg(pgid, libc::SIGTERM) };
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        if unsafe { libc::killpg(pgid, 0) } != 0 {
            return true; // ESRCH: nothing left in the group
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if unsafe { libc::killpg(pgid, libc::SIGKILL) } != 0 {
        return true; // already gone between the last poll and this call
    }
    std::thread::sleep(Duration::from_millis(200));
    unsafe { libc::killpg(pgid, 0) != 0 }
}

pub fn run_test_run(args: &[String]) -> i32 {
    install_signal_handlers();
    let opts = match parse_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("fno-agents test-run: {e}");
            return 2;
        }
    };
    if opts.argv.is_empty() {
        eprintln!("fno-agents test-run: empty argv after `--`");
        return 2;
    }

    let run_id = new_run_id();
    let holder = format!("test-run:{}:{}", std::process::id(), now_secs());
    let claims_root = opts
        .claims_root
        .clone()
        .or_else(crate::claims::global_claims_root);

    // A nested invocation inherits the outer admission rather than
    // re-acquiring: it never touches the outer claim and never multiplies
    // the outer concurrency budget, because it never becomes a NEW claim
    // holder at all.
    let nested = nested_owner();
    let mut claimed = false;
    if nested.is_none() {
        let deadline = Instant::now() + opts.timeout;
        if let Err(code) = acquire_suite_claim(&run_id, &holder, claims_root.as_deref(), deadline) {
            return code;
        }
        claimed = true;
    }

    let (owner_pid, owner_birth) = nested.unwrap_or_else(|| {
        let pid = std::process::id();
        (pid, crate::daemon::process_start_time(pid).unwrap_or(0))
    });

    let mut child = match spawn_group(&opts.argv, owner_pid, owner_birth) {
        Ok(c) => c,
        Err(e) => {
            emit(
                &run_id,
                "suite_cleanup_failed",
                &[("stage", "spawn".to_string()), ("error", e.to_string())],
            );
            if claimed {
                let _ =
                    crate::claims::release(SUITE_CLAIM_KEY, &holder, claims_root.as_deref(), None);
            }
            return 2;
        }
    };
    let child_pid = child.id() as i32;
    let child_birth = crate::daemon::process_start_time(child.id()).unwrap_or(0);
    emit(
        &run_id,
        "suite_started",
        &[
            ("pid", child_pid.to_string()),
            ("birth", child_birth.to_string()),
        ],
    );

    let run_deadline = Instant::now() + opts.timeout;
    let wait_result = wait_bounded(&mut child, run_deadline);

    // ALWAYS attempted, success or failure or timeout or signal - this line
    // is the fix: ownership of cleanup does not depend on which of those
    // paths brought us here.
    let cleaned = cleanup_group(child_pid);
    // `wait()` after a reaped `try_wait()` just replays the cached status;
    // after a force-kill it is the real reap of the (by-now-dead) leader.
    let _ = child.wait();

    emit(
        &run_id,
        if cleaned {
            "suite_cleanup_complete"
        } else {
            "suite_cleanup_failed"
        },
        &[("pid", child_pid.to_string())],
    );

    if claimed {
        let _ = crate::claims::release(SUITE_CLAIM_KEY, &holder, claims_root.as_deref(), None);
        emit(&run_id, "suite_released", &[]);
    }

    let exit_code = match wait_result {
        Ok(code) => code,
        Err(Unfinished::TimedOut) => {
            eprintln!(
                "fno-agents test-run: TIMEOUT after {}s; process group killed",
                opts.timeout.as_secs()
            );
            return 124;
        }
        Err(Unfinished::Signalled(sig)) => {
            eprintln!("fno-agents test-run: received signal {sig}; process group killed");
            // POSIX convention (128 + signal number), matching a shell's own
            // report of a signal-terminated command.
            return 128 + sig;
        }
    };
    if !cleaned {
        // Cleanup failure is nonzero even after test success: a green suite
        // that leaked its group must not read as done.
        return if exit_code == 0 { 1 } else { exit_code };
    }
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timeout_and_claims_root_before_the_separator() {
        let args: Vec<String> = [
            "--timeout",
            "60",
            "--claims-root",
            "/tmp/x",
            "--",
            "sleep",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let opts = parse_args(&args).unwrap();
        assert_eq!(opts.timeout, Duration::from_secs(60));
        assert_eq!(opts.claims_root, Some(PathBuf::from("/tmp/x")));
        assert_eq!(opts.argv, vec!["sleep".to_string(), "1".to_string()]);
    }

    #[test]
    fn defaults_timeout_when_omitted() {
        let args: Vec<String> = ["--", "sleep", "1"].iter().map(|s| s.to_string()).collect();
        let opts = parse_args(&args).unwrap();
        assert_eq!(opts.timeout, Duration::from_secs(1800));
    }

    #[test]
    fn a_flag_shaped_binary_arg_after_the_separator_is_never_consumed() {
        let args: Vec<String> = ["--", "cargo", "test", "--", "--nocapture"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let opts = parse_args(&args).unwrap();
        assert_eq!(
            opts.argv,
            vec![
                "cargo".to_string(),
                "test".to_string(),
                "--".to_string(),
                "--nocapture".to_string()
            ]
        );
    }

    #[test]
    fn missing_separator_refuses() {
        let args: Vec<String> = ["--timeout", "60"].iter().map(|s| s.to_string()).collect();
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn nested_owner_rejects_a_foreign_or_stale_token() {
        // A pid this test did not spawn (pid 1 on any unix box) will never
        // match a birth value this process invents, so the token is refused
        // rather than trusted.
        std::env::set_var("FNO_TEST_OWNER_PID", "1");
        std::env::set_var("FNO_TEST_OWNER_BIRTH", "424242424242");
        let result = nested_owner();
        std::env::remove_var("FNO_TEST_OWNER_PID");
        std::env::remove_var("FNO_TEST_OWNER_BIRTH");
        assert_eq!(result, None);
    }

    #[test]
    fn nested_owner_accepts_a_live_matching_self() {
        let pid = std::process::id();
        let birth = crate::daemon::process_start_time(pid).unwrap_or(0);
        std::env::set_var("FNO_TEST_OWNER_PID", pid.to_string());
        std::env::set_var("FNO_TEST_OWNER_BIRTH", birth.to_string());
        let result = nested_owner();
        std::env::remove_var("FNO_TEST_OWNER_PID");
        std::env::remove_var("FNO_TEST_OWNER_BIRTH");
        assert_eq!(result, Some((pid, birth)));
    }
}

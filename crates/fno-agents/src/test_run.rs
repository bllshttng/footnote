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
const BUILD_CLAIM_KEY: &str = "build:cargo";
/// How often a held build or a queued suite run repeats its waiting line on
/// stderr.
const BUILD_HOLD_NOTICE: Duration = Duration::from_secs(30);
/// How often a held build scans for a cargo nested under the holder, and for
/// whether the holder is still compiling.
const NESTED_SCAN_INTERVAL: Duration = Duration::from_secs(5);
/// How long a build-admit waiter lets the holder cargo run no compile before
/// it takes the slot. Compilation is what needs one-at-a-time; a test run
/// does not.
const BUILD_IDLE_TAKEOVER: Duration = Duration::from_secs(30);
/// Grace window for a SIGTERM to land before escalating to SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(3);
/// Poll interval while waiting on a held admission claim or the child.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// True when `argv` is a cargo test run that selects no narrower target
/// than a whole crate: no test-name filter, no `--test`/`--bin`/`--example`/
/// `--bench`/`--doc`, no nextest filterset. `--lib` alone is whole: it runs
/// every unit test. Any other program reads false. An unknown flag's value
/// reads as a filter, so a doubt errs toward targeted.
pub(crate) fn cargo_test_selects_whole_suite(argv: &[String]) -> bool {
    let is_cargo = argv
        .first()
        .is_some_and(|p| Path::new(p).file_name().is_some_and(|n| n == "cargo"));
    if !is_cargo {
        return false;
    }
    let mut i = 1;
    while i < argv.len() && argv[i].starts_with('+') {
        i += 1;
    }
    let rest_start = match argv.get(i).map(String::as_str) {
        Some("test") | Some("t") => i + 1,
        Some("nextest") => match argv.get(i + 1).map(String::as_str) {
            Some("run") | Some("r") => i + 2,
            _ => return false,
        },
        _ => return false,
    };
    // Flags whose presence means the selection is narrower than a crate.
    const TARGETED: &[&str] = &[
        "--test",
        "--bin",
        "--example",
        "--bench",
        "--doc",
        "-E",
        "--filterset",
        "--filter-expr",
    ];
    // Flags whose NEXT token is a value, so a positional filter is never
    // mistaken for one. Only the exact spelling consumes; `--target=foo`
    // carries its own value.
    const VALUE_FLAGS: &[&str] = &[
        "--manifest-path",
        "-p",
        "--package",
        "--exclude",
        "-F",
        "--features",
        "-j",
        "--jobs",
        "--target",
        "--target-dir",
        "--profile",
        "--cargo-profile",
        "--color",
        "--config",
        "-Z",
        "--message-format",
        "--test-threads",
        "--partition",
        "--retries",
    ];
    let split = argv[rest_start..]
        .iter()
        .position(|t| t == "--")
        .map(|p| rest_start + p);
    let (cargo_args, libtest_args): (&[String], &[String]) = match split {
        Some(p) => (&argv[rest_start..p], &argv[p + 1..]),
        None => (&argv[rest_start..], &[]),
    };
    let mut skip = false;
    for tok in cargo_args {
        if skip {
            skip = false;
            continue;
        }
        if TARGETED.contains(&tok.as_str())
            || TARGETED
                .iter()
                .any(|f| tok.strip_prefix(f).is_some_and(|r| r.starts_with('=')))
        {
            return false;
        }
        if VALUE_FLAGS.contains(&tok.as_str()) {
            skip = true;
            continue;
        }
        if !tok.starts_with('-') {
            return false;
        }
    }
    // Past `--` the argv is libtest/nextest's own: positional tokens are
    // filters, and these flags take a value.
    const LIBTEST_VALUES: &[&str] = &[
        "--test-threads",
        "--skip",
        "--format",
        "--color",
        "-Z",
        "--logfile",
        "--shuffle-seed",
    ];
    let mut skip = false;
    for tok in libtest_args {
        if skip {
            skip = false;
            continue;
        }
        if LIBTEST_VALUES.contains(&tok.as_str()) {
            skip = true;
            continue;
        }
        if !tok.starts_with('-') {
            return false;
        }
    }
    true
}

/// The claim key naming the one checkout that gets the next test or build
/// slot: set with `fno agents claim acquire test:priority --holder
/// worktree:<checkout> --ttl 30m --pid-unavailable -R "<why>"`.
pub const PRIORITY_KEY: &str = "test:priority";

/// The priority lane a live `test:priority` claim names: that checkout's
/// canonical worktree path, and the claim's expiry.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PriorityLane {
    worktree: PathBuf,
    expires_at: i64,
}

/// The priority lane, when a live `test:priority` claim names one. Only a
/// Live or Suspect claim with an `expires_at` still in the future and a
/// `worktree:` holder counts: a `--pid-unavailable` claim reads Suspect, so
/// its TTL is its liveness, and a claim with no TTL never names a lane. The
/// holder path is canonicalized, with the raw path as fallback.
fn priority_lane(root: Option<&Path>) -> Option<PriorityLane> {
    let (state, rec) = crate::claims::status(PRIORITY_KEY, root);
    let rec = rec?;
    if !matches!(
        state,
        crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
    ) {
        return None;
    }
    let expires_at = rec.expires_at?;
    if expires_at <= crate::claims::now_ms() {
        return None;
    }
    let raw = rec.holder.strip_prefix("worktree:")?;
    let worktree = std::fs::canonicalize(raw).unwrap_or_else(|_| PathBuf::from(raw));
    Some(PriorityLane {
        worktree,
        expires_at,
    })
}

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

/// The declared test-run owner from `FNO_TEST_OWNER_PID`/`FNO_TEST_OWNER_BIRTH`.
/// This only parses the identity; callers that need liveness must use
/// `owner_from_env` or `owner_alive` separately.
pub fn declared_owner_from_env() -> Option<(u32, u64)> {
    let pid: u32 = std::env::var("FNO_TEST_OWNER_PID").ok()?.parse().ok()?;
    let birth: u64 = std::env::var("FNO_TEST_OWNER_BIRTH").ok()?.parse().ok()?;
    Some((pid, birth))
}

/// The declared test-run owner, verified against the LIVE process (never
/// trusted on the name alone). Nested test runs use this form so a stale token
/// re-acquires the suite claim instead of inheriting ownership.
pub fn owner_from_env() -> Option<(u32, u64)> {
    let owner = declared_owner_from_env()?;
    owner_alive(owner.0, owner.1).then_some(owner)
}

/// The current process identity in the environment shape inherited by every
/// test-spawned daemon or client.
pub fn self_owner_env() -> [(&'static str, String); 2] {
    let pid = std::process::id();
    let birth = crate::daemon::process_start_time(pid)
        .expect("the current test process must have a readable birth time");
    [
        ("FNO_TEST_OWNER_PID", pid.to_string()),
        ("FNO_TEST_OWNER_BIRTH", birth.to_string()),
    ]
}

/// Whether `pid` is still the SAME incarnation that was born at `birth` - a
/// recycled pid with a different birth timestamp answers `false`.
pub fn owner_alive(pid: u32, birth: u64) -> bool {
    crate::daemon::process_start_time(pid) == Some(birth)
}

/// Spawn a background thread that polls a declared test-run owner's liveness
/// at least every 250ms and calls `on_death` once, then exits - the one
/// shape both keeper families (`pane_keeper.rs`, `graph_keeper.rs`) use to
/// bind their lifetime to that owner instead of each hand-rolling its own
/// poll. The daemon is the third consumer alongside the pane and graph
/// keepers. Returns `false` when the thread could not spawn; a caller that
/// cannot serve unwatched must refuse to start.
pub fn spawn_owner_watchdog(
    owner_pid: u32,
    owner_birth: u64,
    thread_name: &str,
    on_death: impl FnOnce() + Send + 'static,
) -> bool {
    let spawn = std::thread::Builder::new()
        .name(thread_name.to_string())
        .spawn(move || loop {
            if !owner_alive(owner_pid, owner_birth) {
                on_death();
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        });
    match spawn {
        Ok(_) => true,
        Err(e) => {
            eprintln!(
                "fno-agents: owner watchdog thread spawn failed owner_pid={owner_pid}: {e}; the caller runs unwatched"
            );
            false
        }
    }
}

/// What a blocking claim wait does after one refused poll.
enum OnHeld {
    Wait,
    /// Proceed without the claim (a nested build under the holder).
    Admit,
    Stop(i32),
}

/// A waiter's lane at one admission door, best first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lane {
    Priority,
    Normal,
    Full,
}

impl Lane {
    /// The name the waiting line prints (`normal`, never the dir spelling).
    fn name(self) -> &'static str {
        match self {
            Lane::Priority => "priority",
            Lane::Normal => "normal",
            Lane::Full => "full",
        }
    }

    fn dir_suffix(self) -> &'static str {
        match self {
            Lane::Priority => "priority",
            Lane::Normal => "queue",
            Lane::Full => "full",
        }
    }

    /// The lanes better than this one, best first: a waiter tries only
    /// while every one of these is empty.
    fn better(self) -> &'static [Lane] {
        match self {
            Lane::Priority => &[],
            Lane::Normal => &[Lane::Priority],
            Lane::Full => &[Lane::Priority, Lane::Normal],
        }
    }
}

/// What a held poll hands its caller: this waiter's own lane, and the better
/// lane it is yielding to, when one holds live tickets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Wait {
    lane: Lane,
    yielding_to: Option<Lane>,
}

/// The live or suspect holders of `keys`: `(holder, pid, host)` per key.
/// Every branch that skips the acquire attempt passes these rows, so the
/// ancestor-admit and nested-cargo checks keep running while a better lane
/// gates the attempt - that read is the deadlock guard.
fn holder_rows(keys: &[String], root: Option<&Path>) -> Vec<(String, Option<i32>, String)> {
    keys.iter()
        .filter_map(|key| {
            let (state, rec) = crate::claims::status(key, root);
            if matches!(
                state,
                crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
            ) {
                Some((
                    rec.as_ref().map_or(String::new(), |r| r.holder.clone()),
                    rec.as_ref().and_then(|r| r.pid),
                    rec.map(|r| r.host).unwrap_or_default(),
                ))
            } else {
                None
            }
        })
        .collect()
}

/// Block until one of `keys` is ours, or `on_held` admits or stops. Each
/// poll tries the keys in order and returns on the first acquire; when every
/// key refuses, `on_held` sees every refused `(holder, pid, host)` row and
/// this waiter's [`Wait`] readout. A contender spawns ZERO workers while
/// waiting: the loop returns before any `Command` is built.
///
/// Lanes: each door orders its waiters in up to three claim-queue dirs
/// beside the first key's lockfile, best first ([`Lane`]). A waiter holds
/// one ticket in its own lane's dir and may attempt the acquire only when
/// every better lane is empty and it sits in the front `keys.len()` positions
/// of its own lane - the run slots pass `cap` keys, which is a counted
/// semaphore, and a front-only rule would serialise those slots down to one.
/// An empty better lane reserves nothing. A ticket whose lane dir cannot be
/// read degrades to the old unordered poll rather than refusing a run that
/// can still make progress.
fn acquire_claim_blocking(
    keys: &[String],
    holder: &str,
    opts: impl Fn(usize) -> crate::claims::AcquireOpts,
    mut lane_of: impl FnMut() -> Lane,
    mut on_held: impl FnMut(&[(String, Option<i32>, String)], Option<(usize, usize)>, Wait) -> OnHeld,
) -> Result<(), i32> {
    let admit_width = keys.len();
    let opts0 = opts(0);
    let lock_path = crate::claims::claim_path(&keys[0], opts0.root.as_deref()).ok();
    let dir_for = |lane: Lane| {
        lock_path
            .as_deref()
            .map(|p| crate::claim_queue::lane_dir_for(p, lane.dir_suffix()))
    };
    let mut lane = lane_of();
    let mut dir = dir_for(lane);
    let mut ticket = dir
        .as_deref()
        .and_then(|dir| match crate::claim_queue::enter(dir) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "test_run: claim queue enter failed on {}: {e}; waiting unordered. \
                         remedy: check the directory's permissions or free disk space",
                    dir.display()
                );
                None
            }
        });
    let result = 'wait: loop {
        // The lane may change while this waiter sits queued: a whole-suite
        // waiter holds `full` at the suite door until its budget is spent,
        // then joins `queue` at the back. Re-read the lane every poll and
        // re-seat the ticket when it moved.
        let current = lane_of();
        if current != lane {
            if let Some(t) = ticket.take() {
                crate::claim_queue::leave(t);
            }
            lane = current;
            dir = dir_for(lane);
            ticket = match dir.as_deref().map(crate::claim_queue::enter) {
                Some(Ok(t)) => Some(t),
                Some(Err(e)) => {
                    eprintln!(
                        "test_run: claim queue re-enter failed on {}: {e}; waiting unordered",
                        dir.as_deref()
                            .map_or(String::new(), |d| d.display().to_string())
                    );
                    None
                }
                None => None,
            };
        }
        let mut pos_out: Option<(usize, usize)> = None;
        if let (Some(dir), Some(t)) = (dir.as_deref(), ticket.as_ref()) {
            match crate::claim_queue::position(t) {
                Ok(pos) => {
                    // A live ticket in any better lane gates the attempt:
                    // a waiter tries only while every better lane is empty.
                    let mut yielding: Option<Lane> = None;
                    for better in lane.better() {
                        if let Some(p) = lock_path.as_deref() {
                            let dir = crate::claim_queue::lane_dir_for(p, better.dir_suffix());
                            if crate::claim_queue::depth(&dir) > 0 {
                                yielding = Some(*better);
                                break;
                            }
                        }
                    }
                    if yielding.is_some() || pos.index >= admit_width {
                        match on_held(
                            &holder_rows(keys, opts0.root.as_deref()),
                            Some((pos.index, pos.total)),
                            Wait {
                                lane,
                                yielding_to: yielding,
                            },
                        ) {
                            OnHeld::Wait => {
                                std::thread::sleep(POLL_INTERVAL.max(Duration::from_millis(500)))
                            }
                            OnHeld::Admit => break 'wait Ok(()),
                            OnHeld::Stop(code) => break 'wait Err(code),
                        }
                        continue;
                    }
                    pos_out = Some((pos.index, pos.total));
                }
                Err(e) => {
                    // The ticket was reaped or removed under us. Re-enqueue
                    // at the back once, printing one line for the event -
                    // never one line per poll. Only a failed re-enter waits
                    // unordered (the old behaviour).
                    eprintln!("test_run: claim queue ticket {e}; re-enqueueing at the back");
                    match crate::claim_queue::enter(dir) {
                        Ok(fresh) => {
                            // Sleep before the next gate read so the fresh
                            // back-of-queue ticket cannot snipe the lock in
                            // the gap before the front waiter's next poll.
                            ticket = Some(fresh);
                            std::thread::sleep(POLL_INTERVAL.max(Duration::from_millis(500)));
                            continue 'wait;
                        }
                        Err(e2) => {
                            eprintln!(
                                "test_run: claim queue re-enter failed on {}: {e2}; waiting unordered",
                                dir.display()
                            );
                        }
                    }
                }
            }
        }
        let mut held: Vec<(String, Option<i32>, String)> = Vec::with_capacity(keys.len());
        for (i, key) in keys.iter().enumerate() {
            match crate::claims::acquire(key, holder, opts(i)) {
                crate::claims::AcquireOutcome::Acquired(_) => break 'wait Ok(()),
                crate::claims::AcquireOutcome::HeldByOther {
                    holder: h,
                    pid,
                    host,
                } => held.push((h, pid, host)),
                crate::claims::AcquireOutcome::Error(e) => {
                    eprintln!("fno-agents test-run: claim error: {e}");
                    break 'wait Err(2);
                }
            }
        }
        match on_held(
            &held,
            pos_out,
            Wait {
                lane,
                yielding_to: None,
            },
        ) {
            OnHeld::Wait => std::thread::sleep(POLL_INTERVAL.max(Duration::from_millis(500))),
            OnHeld::Admit => break 'wait Ok(()),
            OnHeld::Stop(code) => break 'wait Err(code),
        }
    };
    if let Some(t) = ticket {
        crate::claim_queue::leave(t);
    }
    result
}

/// The fields every suite-wait receipt carries, read fresh from the same
/// claims root the acquire polls: a waiter behind the queue front sees no
/// `HeldByOther` row, so the read is `claims::status`, not the acquire's
/// poll result. The claim key, the holder's identity and liveness, the queue
/// position when one is active, and the wait so far.
fn suite_wait_fields(
    root: Option<&Path>,
    pos: Option<(usize, usize)>,
    started: Instant,
    wait: Wait,
    prio: Option<&Path>,
) -> Vec<(&'static str, String)> {
    let (state, rec) = crate::claims::status(SUITE_CLAIM_KEY, root);
    let mut fields: Vec<(&'static str, String)> = vec![
        ("claim", SUITE_CLAIM_KEY.to_string()),
        (
            "holder",
            rec.as_ref().map_or("-".to_string(), |r| r.holder.clone()),
        ),
        (
            "pid",
            rec.as_ref()
                .and_then(|r| r.pid)
                .map_or("-".to_string(), |p| p.to_string()),
        ),
        (
            "host",
            match rec.as_ref() {
                Some(r) => r.host.clone(),
                None => "-".to_string(),
            },
        ),
        ("holder_state", state.as_str().to_string()),
        (
            "holder_left_s",
            match rec.as_ref().and_then(|r| r.expires_at) {
                Some(expires_at) => ((expires_at - crate::claims::now_ms()) / 1000).to_string(),
                None => "-".to_string(),
            },
        ),
    ];
    if let Some((index, total)) = pos {
        fields.push(("position", index.to_string()));
        fields.push(("queued", total.to_string()));
    }
    if wait.yielding_to.is_some() {
        fields.push((
            "yielding_to",
            wait.yielding_to
                .map_or(String::new(), |l| l.name().to_string()),
        ));
    }
    fields.push(("lane", wait.lane.name().to_string()));
    if let Some(p) = prio {
        fields.push(("priority", p.display().to_string()));
    }
    fields.push(("waited_s", started.elapsed().as_secs().to_string()));
    fields
}

fn acquire_suite_claim(
    run_id: &str,
    holder: &str,
    root: Option<&Path>,
    worktree: Option<&Path>,
    whole: bool,
    budget: Duration,
) -> Result<(), i32> {
    let opts = |_: usize| crate::claims::AcquireOpts {
        pid: Some(std::process::id()),
        // The recorded pid is this owner, which lives for the whole run:
        // stamp holder-process so the classifier reads the pid's verdict and
        // a SIGKILLed holder's slot frees inside the TTL instead of refusing
        // every waiter until expiry. The TTL is the holder's own run budget,
        // so expires_at is that deadline; a live holder-process pid keeps
        // the lease Live past it while the run is still going.
        pid_provenance: Some(crate::claims::HOLDER_PROCESS.to_string()),
        // expires_at is the holder's own run deadline, clamped into the
        // range every claim must carry (a sub-60s budget reads the 60s
        // floor; a live holder-process pid keeps the lease past it anyway).
        ttl_ms: Some(
            (budget.as_millis() as i64).clamp(crate::claims::MIN_TTL_MS, crate::claims::MAX_TTL_MS),
        ),
        reason: Some("test-run".to_string()),
        root: root.map(PathBuf::from),
        ..Default::default()
    };
    let started = Instant::now();
    let mut last_notice: Option<Instant> = None;
    let mut last_wait: Option<Wait> = None;
    let mut whole_notice = false;
    // The wait has no timer, like `poll_held`: it ends on admission or a
    // signal. `--timeout` bounds only the admitted run.
    acquire_claim_blocking(
        &[SUITE_CLAIM_KEY.to_string()],
        holder,
        opts,
        || {
            if let Some(w) = worktree {
                if priority_lane(root).is_some_and(|p| p.worktree.as_path() == w) {
                    return Lane::Priority;
                }
            }
            if whole && started.elapsed() < budget {
                Lane::Full
            } else {
                Lane::Normal
            }
        },
        |_rows, pos, wait| {
            if let Some(sig) = received_signal() {
                let prio = priority_lane(root);
                let mut fields = suite_wait_fields(
                    root,
                    pos,
                    started,
                    wait,
                    prio.as_ref().map(|p| p.worktree.as_path()),
                );
                fields.push(("signal", sig.to_string()));
                emit(run_id, "suite_wait_interrupted", &fields);
                return OnHeld::Stop(128 + sig);
            }
            let changed = last_wait != Some(wait);
            if changed || last_notice.is_none_or(|t| t.elapsed() >= BUILD_HOLD_NOTICE) {
                last_wait = Some(wait);
                last_notice = Some(Instant::now());
                let prio = priority_lane(root);
                let fields = suite_wait_fields(
                    root,
                    pos,
                    started,
                    wait,
                    prio.as_ref().map(|p| p.worktree.as_path()),
                );
                emit(run_id, "suite_waiting", &fields);
                if wait.lane == Lane::Full && !whole_notice {
                    whole_notice = true;
                    eprintln!(
                        "test_run: this run selects a whole crate suite, so it waits while \
                         targeted runs are queued, for at most {}s, then queues in arrival \
                         order. CI runs every suite on every PR.",
                        budget.as_secs()
                    );
                }
            }
            OnHeld::Wait
        },
    )
}

/// `test-run build-admit --cargo-pid PID --worktree PATH`: the rustc wrapper
/// calls this before every compile. One cargo per machine holds
/// `build:cargo`; the claim carries the cargo pid and no TTL, so it frees
/// itself the moment that cargo exits. A TTL would keep a dead cargo's claim
/// Suspect, and so refused, until the TTL ran out. Compilation is what needs
/// one-at-a-time, so a waiter also takes the claim from a holder cargo that
/// has run no compile process for [`BUILD_IDLE_TAKEOVER`] - a cargo in its
/// test phase, or a `cargo run` program, walls nothing for long.
fn run_build_admit(args: &[String]) -> i32 {
    install_signal_handlers();
    let (cargo_pid, worktree) = match parse_cargo_admit_args(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("fno-agents test-run build-admit: {e}");
            return 2;
        }
    };
    let worktree = std::fs::canonicalize(&worktree).unwrap_or(worktree);
    let holder = format!("cargo:{}:{cargo_pid}", worktree.display());

    // Cargo calls the wrapper once per crate. Once admitted, a read answers
    // every later call without a claim write or an audit event.
    if let (crate::claims::ClaimState::Live, Some(rec)) =
        crate::claims::status(BUILD_CLAIM_KEY, None)
    {
        if rec.holder == holder {
            return 0;
        }
    }

    // Lock order: a run slot first, then build:cargo, so no cargo ever waits
    // on build:cargo while it holds no slot.
    if let Err(code) = admit_run_slot(cargo_pid, &worktree) {
        return code;
    }

    let mut wait = CargoWait::new(cargo_pid, &worktree);
    let mut idle = HolderIdle::new();
    // The reason is decided inside the wait (a takeover) but read by opts,
    // so it travels through a RefCell the two closures share.
    let takeover_reason = std::cell::RefCell::new(None::<String>);

    let opts = |_: usize| crate::claims::AcquireOpts {
        pid: Some(cargo_pid),
        reason: Some(
            takeover_reason
                .borrow()
                .clone()
                .unwrap_or_else(|| "cargo build".to_string()),
        ),
        events_dir: Some(worktree.clone()),
        ..Default::default()
    };
    let lane_of = || {
        if priority_lane(None).is_some_and(|p| p.worktree == worktree) {
            Lane::Priority
        } else {
            Lane::Normal
        }
    };
    let result = acquire_claim_blocking(
        &[BUILD_CLAIM_KEY.to_string()],
        &holder,
        opts,
        lane_of,
        |rows, _, w| {
            let mut scan = |table: &[crate::census::ProcRow],
                            parent: &ParentMap|
             -> Option<OnHeld> {
                let (h, pid, _) = rows.first()?;
                let holder_pid = (*pid).filter(|p| *p > 0)?;
                // A holder that has stopped compiling keeps the slot for no
                // one. Feed the idle clock on the scan poll_held already makes;
                // when the window is out, release the holder's claim by its
                // exact holder string (a holder mismatch is a silent no-op,
                // which is how two waiters racing stay safe) and let the next
                // poll acquire.
                let compiling = holder_compiling_map(parent, table, holder_pid as u32)?;
                // A takeover reason names the holder it displaced. When the
                // claim passes to a different holder, the guard resets, so this
                // waiter can still take over the new holder when it idles.
                if idle.holder().is_some_and(|seen| seen != h.as_str()) {
                    *takeover_reason.borrow_mut() = None;
                }
                let idle_for = idle.observe(h, compiling, Instant::now());
                if idle_for >= build_idle_window() && takeover_reason.borrow().is_none() {
                    let waited = idle_for.as_secs();
                    eprintln!(
                    "cargo admission: taking over; {h} (pid {holder_pid}) ran no compile for {waited}s"
                );
                    let _ = crate::claims::release(BUILD_CLAIM_KEY, h, None, Some(&worktree));
                    *takeover_reason.borrow_mut() = Some(format!(
                        "cargo build; took over from {h}, no compile for {waited}s"
                    ));
                }
                None
            };
            wait.poll_held(rows, None, Some(&mut scan), w)
        },
    );
    wait.clear_marker();
    match result {
        Ok(()) => 0,
        Err(code) => code,
    }
}

/// `test-run run-admit --cargo-pid PID --worktree PATH`: the cargo target
/// runner calls this before every test binary and doctest. The cargo holds
/// one of `test.max_cargo_runs` machine-wide run slots (`test:cargo-run:<i>`),
/// keyed to its pid with no TTL, so the slot frees when the cargo exits. The
/// status pass writes nothing: a slot already naming this holder, or one
/// whose pid is this cargo or an ancestor (a nested cargo, a doctest's
/// rustdoc), admits at once without a second claim.
fn admit_run_slot(cargo_pid: u32, worktree: &Path) -> Result<(), i32> {
    install_signal_handlers();
    let worktree = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    let holder = format!("cargo:{}:{cargo_pid}", worktree.display());
    let cap = crate::agents_config::max_cargo_runs(&worktree) as usize;
    let keys: Vec<String> = (0..cap).map(|i| format!("test:cargo-run:{i}")).collect();

    for key in &keys {
        if let (crate::claims::ClaimState::Live, Some(rec)) = crate::claims::status(key, None) {
            let ancestor = rec
                .pid
                .is_some_and(|p| p > 0 && is_self_or_ancestor(p as u32, cargo_pid));
            if rec.holder == holder || ancestor {
                return Ok(());
            }
        }
    }

    let mut wait = CargoWait::new(cargo_pid, &worktree);
    let started = wait.started;
    let opts = |i: usize| crate::claims::AcquireOpts {
        pid: Some(cargo_pid),
        reason: Some(format!(
            "cargo run slot {i} of {cap}, waited {}s",
            started.elapsed().as_secs()
        )),
        events_dir: Some(worktree.clone()),
        ..Default::default()
    };
    let lane_of = || {
        if priority_lane(None).is_some_and(|p| p.worktree == worktree) {
            Lane::Priority
        } else {
            Lane::Normal
        }
    };
    let result = acquire_claim_blocking(&keys, &holder, opts, lane_of, |rows, _, w| {
        wait.poll_held(rows, Some((cap, "cargo run slots")), None, w)
    });
    wait.clear_marker();
    result
}

/// Parse and wait for a `run-admit` ask. Exits with the admission's code;
/// the wrapper execs the test binary only on 0.
fn run_run_admit(args: &[String]) -> i32 {
    let (cargo_pid, worktree) = match parse_cargo_admit_args(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("fno-agents test-run run-admit: {e}");
            return 2;
        }
    };
    match admit_run_slot(cargo_pid, &worktree) {
        Ok(()) => 0,
        Err(code) => code,
    }
}

/// The shared wait state of a cargo admission waiter: which cargo it stands
/// for, its waiter marker, and the notice and scan throttles. Both doors
/// poll through [`Self::poll_held`], so a verdict on a holder is decided
/// once, not per door.
struct CargoWait {
    cargo_pid: u32,
    worktree: PathBuf,
    marker: Option<PathBuf>,
    marked: bool,
    last_notice: Option<Instant>,
    last_scan: Option<Instant>,
    last_readout: Option<(Lane, Option<Lane>)>,
    started: Instant,
}

impl CargoWait {
    fn new(cargo_pid: u32, worktree: &Path) -> Self {
        let marker = crate::claims::build_waiters_dir().map(|dir| {
            dir.join(format!(
                "{}.json",
                crate::claims::encode_key(&worktree.to_string_lossy())
            ))
        });
        Self {
            cargo_pid,
            worktree: worktree.to_path_buf(),
            marker,
            marked: false,
            last_notice: None,
            last_scan: None,
            last_readout: None,
            started: Instant::now(),
        }
    }

    /// One poll of a held admission: the wait policy both cargo doors share.
    /// Admit when a holder pid is this cargo or an ancestor; every
    /// [`NESTED_SCAN_INTERVAL`], admit when a holder runs a nested cargo, and
    /// hand `scan_hook` the same process-table read (the build door's idle
    /// takeover rides it); stop with `128 + signal` on SIGINT or SIGTERM;
    /// write the waiter marker once; name every holder at most every
    /// [`BUILD_HOLD_NOTICE`]. `slot_context` is `Some((cap, label))` for the
    /// run-slot pool, `None` for the build claim.
    fn poll_held(
        &mut self,
        rows: &[(String, Option<i32>, String)],
        slot_context: Option<(usize, &'static str)>,
        scan_hook: Option<&mut dyn FnMut(&[crate::census::ProcRow], &ParentMap) -> Option<OnHeld>>,
        w: Wait,
    ) -> OnHeld {
        for (_, pid, _) in rows {
            if pid.is_some_and(|p| p > 0 && is_self_or_ancestor(p as u32, self.cargo_pid)) {
                return OnHeld::Admit;
            }
        }
        if self
            .last_scan
            .is_none_or(|t| t.elapsed() >= NESTED_SCAN_INTERVAL)
        {
            self.last_scan = Some(Instant::now());
            let table = crate::census::process_table().0;
            let parent = parent_map(&table);
            for (_, pid, _) in rows {
                if pid.is_some_and(|p| {
                    p > 0 && runs_under_map(&parent, &table, p as u32, is_cargo_row)
                }) {
                    return OnHeld::Admit;
                }
            }
            if let Some(hook) = scan_hook {
                if let Some(verdict) = hook(&table, &parent) {
                    return verdict;
                }
            }
        }
        if let Some(sig) = received_signal() {
            return OnHeld::Stop(128 + sig);
        }
        if !self.marked {
            if let (Some(path), Some((holder, _, _))) = (&self.marker, rows.first()) {
                write_waiter_marker(path, self.cargo_pid, &self.worktree, holder);
            }
            self.marked = true;
        }
        let readout = (w.lane, w.yielding_to);
        let changed = self.last_readout != Some(readout);
        if changed
            || self
                .last_notice
                .is_none_or(|t| t.elapsed() >= BUILD_HOLD_NOTICE)
        {
            self.last_readout = Some(readout);
            let held = rows
                .iter()
                .map(|(h, pid, _)| {
                    format!(
                        "{h} (pid {})",
                        pid.map_or("?".to_string(), |p| p.to_string())
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let context = slot_context
                .map(|(cap, label)| format!("{} of {cap} {label} held by ", rows.len()))
                .unwrap_or_default();
            let prefix = match (w.lane, w.yielding_to) {
                (Lane::Priority, _) => "holding (priority lane); ".to_string(),
                (_, Some(Lane::Priority)) => {
                    let lane = priority_lane(None);
                    let secs = lane.as_ref().map_or(0, |l| {
                        ((l.expires_at - crate::claims::now_ms()) / 1000).max(0)
                    });
                    format!(
                        "yielding to priority worktree {} for {}s more; ",
                        lane.as_ref()
                            .map_or(String::new(), |l| l.worktree.display().to_string()),
                        secs
                    )
                }
                _ => "holding; ".to_string(),
            };
            eprintln!(
                "cargo admission: {prefix}{context}{held}; waited {}s",
                self.started.elapsed().as_secs()
            );
            // The priority lever, taught once per wait: a person (or a
            // king) can give this checkout the next slot with one acquire.
            if self.last_notice.is_none() {
                eprintln!(
                    "cargo admission: to give {wt} the next slot (user or king): \
                     fno agents claim acquire test:priority --holder worktree:{wt} \
                     --ttl 30m --pid-unavailable -R \"<why>\"",
                    wt = self.worktree.display()
                )
            }
            self.last_notice = Some(Instant::now());
        }
        OnHeld::Wait
    }

    fn clear_marker(&mut self) {
        if self.marked {
            if let Some(path) = &self.marker {
                let _ = std::fs::remove_file(path);
            }
            self.marked = false;
        }
    }
}

fn parse_cargo_admit_args(args: &[String]) -> Result<(u32, PathBuf), String> {
    let mut cargo_pid = None;
    let mut worktree = None;
    let mut i = 0;
    while i < args.len() {
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{} needs a value", args[i]))?;
        match args[i].as_str() {
            "--cargo-pid" => {
                cargo_pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("--cargo-pid: not a pid: {value}"))?,
                )
            }
            "--worktree" => worktree = Some(PathBuf::from(value)),
            other => return Err(format!("unrecognized argument: {other}")),
        }
        i += 2;
    }
    Ok((
        cargo_pid.ok_or("--cargo-pid is required")?,
        worktree.ok_or("--worktree is required")?,
    ))
}

fn write_waiter_marker(path: &Path, cargo_pid: u32, worktree: &Path, holder: &str) {
    let body = serde_json::json!({
        "pid": std::process::id(),
        "cargo_pid": cargo_pid,
        "worktree": worktree,
        "holder": holder,
        "since_ms": crate::claims::now_ms(),
    });
    let Some(dir) = path.parent() else { return };
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, body.to_string()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The stop hook's read: `Some` when a live `build-admit` is holding a cargo
/// build for the checkout that holds `cwd`. The walk stops at the first
/// `.git`, so a worktree nested under another checkout never reads that
/// checkout's hold. The marker is a courtesy signal, never a gate, so an
/// unreadable one reads as no hold.
pub fn build_hold_message(cwd: &Path) -> Option<String> {
    build_hold_message_in(&crate::claims::build_waiters_dir()?, cwd)
}

fn build_hold_message_in(dir: &Path, cwd: &Path) -> Option<String> {
    let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    for path in start.ancestors() {
        if let Some(message) = live_waiter_hold(dir, path) {
            return Some(message);
        }
        if path.join(".git").exists() {
            break;
        }
    }
    None
}

fn live_waiter_hold(dir: &Path, checkout: &Path) -> Option<String> {
    let marker = dir.join(format!(
        "{}.json",
        crate::claims::encode_key(&checkout.to_string_lossy())
    ));
    let raw = std::fs::read_to_string(&marker).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let pid = value["pid"].as_u64().unwrap_or(0) as i32;
    let since_ms = value["since_ms"].as_i64().unwrap_or(i64::MAX);
    let alive = pid > 0
        && match crate::claims::probe_pid(pid) {
            crate::claims::PidProbe::Created(create_ms) => create_ms <= since_ms,
            crate::claims::PidProbe::Refused => true,
            crate::claims::PidProbe::Absent => false,
        };
    if !alive {
        let _ = std::fs::remove_file(&marker);
        return None;
    }
    let holder = value["holder"].as_str().unwrap_or("another cargo");
    Some(format!(
        "held for cargo build admission: {holder} is building"
    ))
}

/// The pid->ppid map over one process-table read, shared by every predicate
/// that read feeds.
type ParentMap = std::collections::HashMap<u32, u32>;

pub(crate) fn parent_map(rows: &[crate::census::ProcRow]) -> ParentMap {
    rows.iter().map(|row| (row.pid, row.ppid)).collect()
}

/// True when some process other than the holder itself, matching `pred`,
/// sits anywhere under `holder_pid` in the process table. The walk follows
/// each matching row's ppid chain, at most 64 hops.
fn runs_under_map(
    parent: &ParentMap,
    rows: &[crate::census::ProcRow],
    holder_pid: u32,
    pred: impl Fn(&crate::census::ProcRow) -> bool,
) -> bool {
    rows.iter()
        .filter(|row| row.pid != holder_pid)
        .filter(|row| pred(row))
        .any(|row| {
            let mut current = row.ppid;
            for _ in 0..64 {
                if current == holder_pid {
                    return true;
                }
                match parent.get(&current) {
                    Some(&next) if current > 1 => current = next,
                    _ => return false,
                }
            }
            false
        })
}

/// Test-shape wrappers over the map-taking walks. Production callers build
/// the parent map once per scan and route through the `_map` forms.
#[cfg(test)]
fn runs_under(
    rows: &[crate::census::ProcRow],
    holder_pid: u32,
    pred: impl Fn(&crate::census::ProcRow) -> bool,
) -> bool {
    runs_under_map(&parent_map(rows), rows, holder_pid, pred)
}

/// A row whose program is cargo.
pub(crate) fn is_cargo_row(row: &crate::census::ProcRow) -> bool {
    let argv0 = row.command.split_whitespace().next().unwrap_or("");
    Path::new(argv0)
        .file_name()
        .is_some_and(|name| name == "cargo")
}

/// True when a `cargo` process other than `holder_pid` runs under it.
#[cfg(test)]
fn runs_nested_cargo(rows: &[crate::census::ProcRow], holder_pid: u32) -> bool {
    runs_under(rows, holder_pid, is_cargo_row)
}

/// A compile process: a token of the argv naming `rustc` (a bare rustc, an
/// sccache client's target, or this wrapper's argument), or an argv0 that is
/// a build-script binary. The holder's own argv never counts: the walk below
/// excludes the holder row itself.
pub(crate) fn is_compile(row: &crate::census::ProcRow) -> bool {
    let argv0 = row.command.split_whitespace().next().unwrap_or("");
    if Path::new(argv0)
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with("build-script-"))
    {
        return true;
    }
    row.command.split_whitespace().any(|token| {
        Path::new(token)
            .file_name()
            .is_some_and(|name| name == "rustc")
    })
}

/// Whether the holder cargo shows a compile process. `None` when the table
/// cannot see the holder at all, so an invisible holder is never read as
/// idle. `Some(false)` means the holder has no compile under it right now.
#[cfg(test)]
fn holder_compiling(rows: &[crate::census::ProcRow], holder_pid: u32) -> Option<bool> {
    holder_compiling_map(&parent_map(rows), rows, holder_pid)
}

fn holder_compiling_map(
    parent: &ParentMap,
    rows: &[crate::census::ProcRow],
    holder_pid: u32,
) -> Option<bool> {
    let visible = rows.iter().any(|row| row.pid == holder_pid);
    visible.then(|| runs_under_map(parent, rows, holder_pid, is_compile))
}

/// The idle window for a build-admit waiter. `FNO_TEST_BUILD_IDLE_SECS`
/// shrinks it so tests need not wait out the real 30 seconds.
fn build_idle_window() -> Duration {
    std::env::var("FNO_TEST_BUILD_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(BUILD_IDLE_TAKEOVER)
}

/// How long the current holder has shown no compile process. A holder change
/// or a compile resets the clock.
struct HolderIdle {
    holder: Option<String>,
    since: Instant,
}

impl HolderIdle {
    fn new() -> Self {
        Self {
            holder: None,
            since: Instant::now(),
        }
    }
    fn observe(&mut self, holder: &str, compiling: bool, now: Instant) -> Duration {
        if self.holder.as_deref() != Some(holder) || compiling {
            self.holder = Some(holder.to_string());
            self.since = now;
        }
        now - self.since
    }
    fn holder(&self) -> Option<&str> {
        self.holder.as_deref()
    }
}

/// True when `holder_pid` is `pid` or one of its process ancestors: a cargo
/// run under the holding cargo (a test that shells out to cargo) must not
/// wait on its own parent.
fn is_self_or_ancestor(holder_pid: u32, pid: u32) -> bool {
    let mut current = pid;
    for _ in 0..64 {
        if current == holder_pid {
            return true;
        }
        if current <= 1 {
            return false;
        }
        match parent_pid(current) {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn parent_pid(pid: u32) -> Option<u32> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a zeroed proc_bsdinfo of exactly `size` bytes.
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    (got == size).then_some(info.pbi_ppid)
}

#[cfg(not(target_os = "macos"))]
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parens; fields resume after the
    // last `)`: state, then ppid.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
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
    WaitFailed(std::io::Error),
}

/// Poll until the child exits, `deadline` passes, or this OWNER itself
/// receives SIGINT/SIGTERM. `Ok` carries the real exit code (a `wait()`, not
/// a guess); `Err` means the child is still running (or its status could not
/// be read) and must be cleaned up.
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
            Err(e) => return Err(Unfinished::WaitFailed(e)),
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

/// The durable fleet incident stop, read as a refusal for this run: `Some`
/// exit code with a `suite_refused` receipt when the fleet is stopped or the
/// incident state is unreadable, `None` when clear. `extra` fields ride the
/// receipt; the post-admission recheck names its wait there.
fn fleet_refusal(run_id: &str, extra: &[(&str, String)]) -> Option<i32> {
    match crate::fleet_incident::verdict() {
        crate::fleet_incident::Verdict::Clear(_) => None,
        crate::fleet_incident::Verdict::Stopped(record) => {
            let mut fields = vec![
                ("reason", "fleet-stop".to_string()),
                ("generation", record.generation.to_string()),
                ("incident", record.reason.clone()),
            ];
            fields.extend_from_slice(extra);
            emit(run_id, "suite_refused", &fields);
            Some(crate::fleet_incident::EXIT_CHECK_STOPPED)
        }
        crate::fleet_incident::Verdict::Unavailable(detail) => {
            let mut fields = vec![
                ("reason", "fleet-stop-unavailable".to_string()),
                ("detail", detail),
            ];
            fields.extend_from_slice(extra);
            emit(run_id, "suite_refused", &fields);
            Some(crate::fleet_incident::EXIT_CHECK_UNAVAILABLE)
        }
    }
}

pub fn run_test_run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("build-admit") => return run_build_admit(&args[1..]),
        Some("run-admit") => return run_run_admit(&args[1..]),
        _ => {}
    }
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

    // The durable fleet incident stop gates BEFORE nested-owner
    // detection, suite-claim acquisition, or any child process group. A suite
    // already running when the stop lands is untouched (this process was
    // admitted earlier); every LATER admission refuses here with a positive
    // suite_refused receipt naming the generation, so absence never reads as
    // evidence. An unreadable state fails closed with its own marker.
    let run_id = new_run_id();
    if let Some(code) = fleet_refusal(&run_id, &[]) {
        return code;
    }

    let holder = format!("test-run:{}:{}", std::process::id(), now_secs());
    let claims_root = opts
        .claims_root
        .clone()
        .or_else(crate::claims::global_claims_root);

    // --timeout bounds the argv's run, counted from admission, so a queued
    // run keeps its whole budget. The wait for the claim has no timer: it
    // ends on admission or a signal, the same policy the cargo doors run.
    let wait_started = Instant::now();
    // A nested invocation inherits the outer admission rather than
    // re-acquiring: it never touches the outer claim and never multiplies
    // the outer concurrency budget, because it never becomes a NEW claim
    // holder at all.
    // The priority lane names one checkout; `whole` routes this run's wait
    // through the `full` lane while targeted runs are queued.
    let worktree = std::env::current_dir()
        .ok()
        .map(|cwd| crate::paths::worktree_repo_root(&cwd));
    let whole = cargo_test_selects_whole_suite(&opts.argv);
    let nested = nested_owner();
    let mut claimed = false;
    if nested.is_none() {
        if let Err(code) = acquire_suite_claim(
            &run_id,
            &holder,
            claims_root.as_deref(),
            worktree.as_deref(),
            whole,
            opts.timeout,
        ) {
            return code;
        }
        claimed = true;
        // Recheck after the wait: a fleet stop that landed while this run
        // queued must not spawn mid-incident, and the wait has no timer of
        // its own to bound that window.
        if let Some(code) = fleet_refusal(
            &run_id,
            &[("waited_s", wait_started.elapsed().as_secs().to_string())],
        ) {
            let _ = crate::claims::release(SUITE_CLAIM_KEY, &holder, claims_root.as_deref(), None);
            return code;
        }
    }
    let admitted = Instant::now();
    let run_deadline = admitted + opts.timeout;

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
            let wait_secs = admitted.saturating_duration_since(wait_started).as_secs();
            eprintln!(
                "fno-agents test-run: TIMEOUT after {}s running the argv ({}s queued for the test:suite claim before that); process group killed",
                opts.timeout.as_secs(),
                wait_secs
            );
            eprintln!(
                "fno-agents test-run:   narrow the target, or raise the budget: FNO_TEST_TIMEOUT_SECONDS=<secs> fno doctor test ..."
            );
            return 124;
        }
        Err(Unfinished::Signalled(sig)) => {
            eprintln!("fno-agents test-run: received signal {sig}; process group killed");
            // POSIX convention (128 + signal number), matching a shell's own
            // report of a signal-terminated command.
            return 128 + sig;
        }
        Err(Unfinished::WaitFailed(e)) => {
            eprintln!("fno-agents test-run: wait() failed: {e}; process group killed");
            return 2;
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

    /// AC15-HP: a `test:priority` claim whose holder path is spelled through
    /// a symlink reads as the canonical checkout path.
    #[test]
    fn priority_lane_canonicalizes_the_holder_path() {
        let root = std::env::temp_dir().join(format!("fno-prio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let real = root.join("checkout");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let acquired = crate::claims::acquire(
            PRIORITY_KEY,
            &format!("worktree:{}", link.display()),
            crate::claims::AcquireOpts {
                ttl_ms: Some(600_000),
                pid_unavailable: true,
                reason: Some("probe".to_string()),
                root: Some(root.clone()),
                ..Default::default()
            },
        );
        assert!(matches!(
            acquired,
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        let lane = priority_lane(Some(&root)).expect("the claim names a lane");
        assert_eq!(
            lane.worktree,
            std::fs::canonicalize(&real).unwrap(),
            "the lane names the canonical checkout"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// AC16-EDGE: a claim with no TTL, a holder without the `worktree:`
    /// prefix, or a TTL already past names no lane.
    #[test]
    fn priority_lane_ignores_an_unusable_claim() {
        let root = std::env::temp_dir().join(format!("fno-prio-neg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let opts = |ttl: Option<i64>| crate::claims::AcquireOpts {
            ttl_ms: ttl,
            pid_unavailable: true,
            reason: Some("probe".to_string()),
            root: Some(root.clone()),
            ..Default::default()
        };
        let acquire_with =
            |holder: &str, ttl| crate::claims::acquire(PRIORITY_KEY, holder, opts(ttl));
        // No TTL: pid_unavailable legally requires a TTL, so this claim
        // carries the test process's own pid instead.
        let no_ttl = crate::claims::AcquireOpts {
            ttl_ms: None,
            pid: Some(std::process::id()),
            reason: Some("probe".to_string()),
            root: Some(root.clone()),
            ..Default::default()
        };
        assert!(matches!(
            crate::claims::acquire(PRIORITY_KEY, "worktree:/tmp/w1", no_ttl),
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        assert!(priority_lane(Some(&root)).is_none(), "no TTL, no lane");
        let _ = crate::claims::release(PRIORITY_KEY, "worktree:/tmp/w1", Some(&root), None);
        // No worktree: prefix.
        assert!(matches!(
            acquire_with("pane:somewhere", Some(600_000)),
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        assert!(
            priority_lane(Some(&root)).is_none(),
            "a holder without the prefix names no lane"
        );
        let _ = crate::claims::release(PRIORITY_KEY, "pane:somewhere", Some(&root), None);
        // TTL already past: the store refuses a sub-minimum TTL, so the
        // record's expiry is rewound by hand past the 60s floor.
        assert!(matches!(
            acquire_with("worktree:/tmp/w2", Some(crate::claims::MIN_TTL_MS)),
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        let lock = crate::claims::claim_path(PRIORITY_KEY, Some(&root)).unwrap();
        let text = std::fs::read_to_string(&lock).unwrap();
        let rewound: String = text
            .lines()
            .map(|line| {
                if line.starts_with("expires_at:") {
                    "expires_at: 0"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&lock, rewound).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            priority_lane(Some(&root)).is_none(),
            "an expired TTL names no lane"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// AC14-HP: the classifier reads a whole crate suite from an unfiltered
    /// cargo test argv, and a targeted run from any test-name filter, target
    /// flag or nextest filterset. A non-cargo program never reads whole.
    #[test]
    fn whole_suite_classifier_reads_the_selection() {
        let v = |parts: &[&str]| -> Vec<String> { parts.iter().map(|s| s.to_string()).collect() };
        for whole in [
            v(&[
                "cargo",
                "test",
                "-q",
                "--manifest-path",
                "/abs/crates/fno/Cargo.toml",
                "--",
                "--test-threads",
                "12",
            ]),
            v(&["cargo", "test", "--lib"]),
            v(&["cargo", "+1.94.1", "test", "-p", "fno-agents"]),
            v(&["cargo", "nextest", "run", "--test-threads", "4"]),
            v(&["cargo", "test"]),
        ] {
            assert!(
                cargo_test_selects_whole_suite(&whole),
                "must read whole: {whole:?}"
            );
        }
        for targeted in [
            v(&[
                "cargo",
                "test",
                "-q",
                "--manifest-path",
                "crates/fno/Cargo.toml",
                "some_test",
            ]),
            v(&["cargo", "test", "--test", "test_run_lifecycle"]),
            v(&["cargo", "test", "--lib", "--", "session_activity"]),
            v(&["cargo", "test", "--doc"]),
            v(&["cargo", "test", "--features=x", "name"]),
            v(&["cargo", "nextest", "run", "-E", "test(x)"]),
        ] {
            assert!(
                !cargo_test_selects_whole_suite(&targeted),
                "must read targeted: {targeted:?}"
            );
        }
        for other in [
            v(&["bash", "-c", "cargo test"]),
            v(&["python", "-m", "pytest"]),
            v(&["cargo", "build"]),
            v(&[]),
        ] {
            assert!(
                !cargo_test_selects_whole_suite(&other),
                "a non-cargo-test program must read false: {other:?}"
            );
        }
    }

    #[test]
    fn nested_owner_rejects_a_foreign_or_stale_token() {
        // The env pins are process-global: hold the shared env lock so a
        // sibling env test cannot clear these vars mid-read.
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pid = std::process::id();
        let birth = crate::daemon::process_start_time(pid).unwrap_or(0);
        std::env::set_var("FNO_TEST_OWNER_PID", pid.to_string());
        std::env::set_var("FNO_TEST_OWNER_BIRTH", birth.to_string());
        let result = nested_owner();
        std::env::remove_var("FNO_TEST_OWNER_PID");
        std::env::remove_var("FNO_TEST_OWNER_BIRTH");
        assert_eq!(result, Some((pid, birth)));
    }

    #[test]
    fn declared_owner_accepts_a_dead_well_formed_token_without_liveness() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FNO_TEST_OWNER_PID", "1");
        std::env::set_var("FNO_TEST_OWNER_BIRTH", "424242424242");
        let declared = declared_owner_from_env();
        let live = owner_from_env();
        std::env::remove_var("FNO_TEST_OWNER_PID");
        std::env::remove_var("FNO_TEST_OWNER_BIRTH");
        assert_eq!(declared, Some((1, 424242424242)));
        assert_eq!(live, None);
    }

    fn marker_for(dir: &Path, worktree: &Path, pid: u32) -> PathBuf {
        let marker = dir.join(format!(
            "{}.json",
            crate::claims::encode_key(&worktree.to_string_lossy())
        ));
        std::fs::create_dir_all(dir).unwrap();
        let body = serde_json::json!({
            "pid": pid,
            "cargo_pid": pid,
            "worktree": worktree,
            "holder": "cargo:/other:42",
            "since_ms": crate::claims::now_ms(),
        });
        std::fs::write(&marker, body.to_string()).unwrap();
        marker
    }

    #[test]
    fn a_live_waiter_marker_holds_every_path_under_its_worktree() {
        let td = tempfile::TempDir::new().unwrap();
        let worktree = std::fs::canonicalize(td.path()).unwrap();
        let nested = worktree.join("crates/fno-agents");
        std::fs::create_dir_all(&nested).unwrap();
        let dir = worktree.join("waiters");
        marker_for(&dir, &worktree, std::process::id());
        let message = build_hold_message_in(&dir, &nested).expect("a live waiter holds");
        assert!(message.contains("cargo:/other:42"), "{message}");
    }

    #[test]
    fn a_nested_checkout_never_reads_its_parent_checkouts_hold() {
        let td = tempfile::TempDir::new().unwrap();
        let outer = std::fs::canonicalize(td.path()).unwrap();
        let inner = outer.join(".claude/worktrees/x");
        std::fs::create_dir_all(inner.join(".git")).unwrap();
        let dir = outer.join("waiters");
        marker_for(&dir, &outer, std::process::id());
        assert_eq!(build_hold_message_in(&dir, &inner.join("crates")), None);
    }

    #[test]
    fn a_dead_waiter_marker_reads_no_hold_and_is_removed() {
        let td = tempfile::TempDir::new().unwrap();
        let worktree = std::fs::canonicalize(td.path()).unwrap();
        let dir = worktree.join("waiters");
        let mut child = Command::new("/usr/bin/true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let marker = marker_for(&dir, &worktree, dead);
        assert_eq!(build_hold_message_in(&dir, &worktree), None);
        assert!(!marker.exists(), "a dead waiter's marker must be removed");
    }

    #[test]
    fn a_process_is_its_own_ancestor_and_its_parent_is_one() {
        let me = std::process::id();
        assert!(is_self_or_ancestor(me, me));
        let parent = parent_pid(me).expect("this process has a parent");
        assert!(is_self_or_ancestor(parent, me));
        assert!(!is_self_or_ancestor(me, parent));
    }

    #[test]
    fn a_cargo_under_the_holder_is_found_through_intermediate_processes() {
        let row = |pid, ppid, command: &str| crate::census::ProcRow {
            pid,
            ppid,
            state: 'S',
            elapsed_s: 0,
            cpu_pct: 0.0,
            rss_kb: 0,
            command: command.to_string(),
        };
        let mut rows = vec![
            row(100, 1, "/Users/x/.cargo/bin/cargo test -p fno"),
            row(200, 100, "/tmp/deps/cross_door_property-abc"),
            row(300, 1, "cargo build"),
        ];
        assert!(
            !runs_nested_cargo(&rows, 100),
            "an unrelated cargo is not nested"
        );
        rows.push(row(400, 200, "cargo build --bin fno-agents"));
        assert!(runs_nested_cargo(&rows, 100));
    }

    #[test]
    fn build_admit_requires_both_flags() {
        let args: Vec<String> = ["--cargo-pid", "12"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_cargo_admit_args(&args).is_err());
        let args: Vec<String> = ["--cargo-pid", "12", "--worktree", "/tmp/x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_cargo_admit_args(&args),
            Ok((12, PathBuf::from("/tmp/x")))
        );
    }

    fn proc_row(pid: u32, ppid: u32, command: &str) -> crate::census::ProcRow {
        crate::census::test_proc_row(pid, ppid, command)
    }

    #[test]
    fn compile_processes_mark_a_holder_as_compiling() {
        for child in [
            "sccache /t/bin/rustc --crate-name a",
            "/t/bin/rustc --crate-name a",
            "bash /r/scripts/lib/cargo-rustc-wrapper.sh /t/bin/rustc --crate-name a",
            "/r/target/debug/build/ring-0a1b/build-script-build",
        ] {
            let rows = vec![proc_row(100, 1, "cargo check"), proc_row(101, 100, child)];
            assert_eq!(holder_compiling(&rows, 100), Some(true), "{child}");
        }
        // A grandchild counts too: rustc under an sccache client under cargo.
        let rows = vec![
            proc_row(100, 1, "cargo check"),
            proc_row(101, 100, "sccache"),
            proc_row(102, 101, "/t/bin/rustc --crate-name a"),
        ];
        assert_eq!(holder_compiling(&rows, 100), Some(true));
    }

    #[test]
    fn a_holder_running_only_a_test_binary_reads_idle() {
        let rows = vec![
            // The holder's own argv names rustc; it is not its own descendant.
            proc_row(100, 1, "cargo rustc -- -C x"),
            proc_row(101, 100, "/r/target/debug/deps/fno_agents-0123abcd"),
            // An unrelated cargo with a compile child changes nothing.
            proc_row(300, 1, "cargo check"),
            proc_row(301, 300, "/t/bin/rustc --crate-name b"),
        ];
        assert_eq!(holder_compiling(&rows, 100), Some(false));
    }

    #[test]
    fn a_holder_missing_from_the_table_gets_no_verdict() {
        let rows = vec![
            proc_row(200, 1, "cargo check"),
            proc_row(201, 200, "/t/bin/rustc --crate-name a"),
        ];
        assert_eq!(holder_compiling(&rows, 100), None);
    }

    #[test]
    fn the_idle_clock_restarts_on_a_new_holder_or_a_compile() {
        let mut clock = HolderIdle::new();
        let t0 = Instant::now();
        assert_eq!(clock.observe("cargo:/a:100", false, t0), Duration::ZERO);
        let t1 = t0 + Duration::from_secs(9);
        assert_eq!(
            clock.observe("cargo:/a:100", false, t1),
            Duration::from_secs(9)
        );
        // A compile resets the clock.
        let t2 = t1 + Duration::from_secs(4);
        assert_eq!(clock.observe("cargo:/a:100", true, t2), Duration::ZERO);
        let t3 = t2 + Duration::from_secs(2);
        assert_eq!(
            clock.observe("cargo:/a:100", false, t3),
            Duration::from_secs(2)
        );
        // A different holder restarts the watch.
        let t4 = t3 + Duration::from_secs(30);
        assert_eq!(clock.observe("cargo:/b:200", false, t4), Duration::ZERO);
    }

    /// AC3-EDGE: the wait receipt names a free claim with no holder when the
    /// root holds no suite claim, and names a live holder with its pid as
    /// digits when this process holds it.
    #[test]
    fn suite_wait_fields_names_free_and_live_holders() {
        let root = std::env::temp_dir().join(format!("fno-test-run-fields-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let fields = suite_wait_fields(
            Some(&root),
            Some((1, 2)),
            Instant::now(),
            Wait {
                lane: Lane::Normal,
                yielding_to: None,
            },
            None,
        );
        let get = |name: &str| {
            fields
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("claim"), "test:suite");
        assert_eq!(get("holder"), "-");
        assert_eq!(get("pid"), "-");
        assert_eq!(get("holder_state"), "free");
        assert_eq!(get("holder_left_s"), "-");
        assert_eq!(get("position"), "1");
        assert_eq!(get("queued"), "2");

        let holder = format!("test-run:{}:{}", std::process::id(), now_secs());
        let acquired = crate::claims::acquire(
            SUITE_CLAIM_KEY,
            &holder,
            crate::claims::AcquireOpts {
                pid: Some(std::process::id()),
                ttl_ms: Some(3_600_000),
                reason: Some("test-run".to_string()),
                root: Some(root.clone()),
                ..Default::default()
            },
        );
        assert!(matches!(
            acquired,
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        let fields = suite_wait_fields(
            Some(&root),
            None,
            Instant::now(),
            Wait {
                lane: Lane::Normal,
                yielding_to: None,
            },
            None,
        );
        let get = |name: &str| {
            fields
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("holder"), holder);
        assert_eq!(get("pid"), std::process::id().to_string());
        assert_eq!(get("host"), crate::claims::hostname());
        assert_eq!(get("holder_state"), "live");
        let left: i64 = get("holder_left_s").parse().unwrap();
        assert!(
            (3590..=3600).contains(&left),
            "a one-hour TTL reads 3590..=3600 seconds left, got {left}"
        );
        assert_eq!(get("position"), "");
        let _ = std::fs::remove_dir_all(&root);
    }
}

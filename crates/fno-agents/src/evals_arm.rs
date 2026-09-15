//! `evals-arm` (x-cf8f): the eval bank's scheduled writer, native.
//!
//! Two modes behind one verb. **Tick mode** (default) is the pr-watch tick's
//! evals leg: when the newest regression-tier run is older than
//! `evals.schedule_days` and the spawn gate admits, it launches **run mode**
//! detached (new session, stdio on /dev/null) and holds the
//! `evals:scheduled-run` claim across ticks, so the 480s tick budget never
//! bounds a 50-minute task-timeout sum. **Run mode** (`--run`) is the child:
//! it runs `fno doctor evals run --tier regression -y` as its own process
//! group, attributes the appended history rows by timestamp, journals
//! `evals_scheduled_run` (or `evals_stale` for plumbing failures), and always
//! releases the claim.
//!
//! Behavior spec is the deleted Python phase (`git show
//! 2cf4d6297^:cli/src/fno/pr_watch/_evals_phase.py`): graded fails are data,
//! plumbing failures are `evals_stale`, the journal is the notice dedup, and
//! the operator notice fires at most once per schedule window.
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use chrono::DateTime;
use chrono::Utc;
use serde_json::json;
use serde_json::Value;

use crate::agents_config;
use crate::claims;
use crate::events::EventEmitter;
use crate::paths::AgentsHome;
use crate::spawn_gate;

const CLAIM_KEY: &str = "evals:scheduled-run";
const EXIT_USAGE: i32 = 2;
const USAGE: &str = "usage: fno-agents evals-arm --history <jsonl> --events <jsonl> --fno-bin <path> --schedule-days N --stale-days N [--summary-json <json|null>] [--run --started <rfc3339>] [--run-timeout-s S] [--claims-root DIR]";

/// One JSON receipt line on stdout; the tick phase parses acted/skip_reason/detail.
fn receipt(acted: u8, skip_reason: Option<&str>, detail: &str) -> i32 {
    let line = json!({"acted": acted, "skip_reason": skip_reason, "detail": detail});
    println!("{}", serde_json::to_string(&line).unwrap_or_default());
    0
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

struct Opts {
    history: PathBuf,
    events: PathBuf,
    fno_bin: String,
    schedule_days: i64,
    stale_days: i64,
    summary_json: Option<String>,
    run: bool,
    started: Option<String>,
    run_timeout_s: u64,
    claims_root: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        history: PathBuf::new(),
        events: PathBuf::new(),
        fno_bin: String::new(),
        schedule_days: -1,
        stale_days: -1,
        summary_json: None,
        run: false,
        started: None,
        run_timeout_s: 3600,
        claims_root: None,
    };
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].clone();
        i += 1;
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => match args.get(i).cloned() {
                    Some(v) => {
                        i += 1;
                        Ok(v)
                    }
                    None => Err(format!("evals-arm: {what} needs a value")),
                },
            }
        };
        match name.as_str() {
            "--history" => o.history = PathBuf::from(value("--history")?),
            "--events" => o.events = PathBuf::from(value("--events")?),
            "--fno-bin" => o.fno_bin = value("--fno-bin")?,
            "--schedule-days" => {
                o.schedule_days = value("--schedule-days")?
                    .parse()
                    .map_err(|_| "evals-arm: --schedule-days needs an integer".to_string())?;
            }
            "--stale-days" => {
                o.stale_days = value("--stale-days")?
                    .parse()
                    .map_err(|_| "evals-arm: --stale-days needs an integer".to_string())?;
            }
            "--summary-json" => o.summary_json = Some(value("--summary-json")?),
            "--run" => o.run = true,
            "--started" => o.started = Some(value("--started")?),
            "--run-timeout-s" => {
                o.run_timeout_s = value("--run-timeout-s")?
                    .parse()
                    .map_err(|_| "evals-arm: --run-timeout-s needs an integer".to_string())?;
            }
            "--claims-root" => o.claims_root = Some(PathBuf::from(value("--claims-root")?)),
            _ => return Err(format!("evals-arm: unknown flag {name}")),
        }
    }
    if o.history.as_os_str().is_empty()
        || o.events.as_os_str().is_empty()
        || o.fno_bin.is_empty()
        || o.schedule_days < 0
        || o.stale_days < 0
    {
        return Err("evals-arm: --history, --events, --fno-bin, --schedule-days and --stale-days are required".into());
    }
    if o.run && o.started.is_none() {
        return Err("evals-arm: --run needs --started".into());
    }
    Ok(o)
}

/// The spawn-gate readings the due check gates on. Production reads the live
/// counters; tests inject them (`GateReading` in, no fs or `vm_stat` touched).
struct GateReading {
    slots: usize,
    max_live: u32,
    ram_gb: Option<f64>,
    min_free_gb: f64,
}

fn read_gate() -> GateReading {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut warnings: Vec<String> = Vec::new();
    let registry = AgentsHome::from_env().registry_json();
    GateReading {
        slots: spawn_gate::slot_count(&registry, &mut warnings),
        max_live: agents_config::max_live(&cwd),
        ram_gb: spawn_gate::available_ram_gb(),
        min_free_gb: agents_config::min_free_gb(&cwd),
    }
}

/// Newest prior `evals_stale` ts in the journal's last 500 lines, or None.
/// The journal IS the notice dedup: one operator notice per schedule window
/// needs no state file of its own.
fn last_stale_ts(events: &Path) -> Option<DateTime<Utc>> {
    let text = std::fs::read_to_string(events).ok()?;
    let newest = text
        .lines()
        .rev()
        .take(500)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|row| row.get("type").and_then(Value::as_str) == Some("evals_stale"))
        .filter_map(|row| {
            row.get("ts")
                .and_then(Value::as_str)
                .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
        })
        .max()?;
    Some(newest.with_timezone(&Utc))
}

/// Journal `evals_stale`, then notify at most once per schedule window, the
/// deleted phase's contract: the dedup ts is read BEFORE emitting (this very
/// row must not swallow the mail), the notice rides a successful emit, and a
/// notify failure never breaks the caller.
fn journal_stale(o: &Opts, age_days: Option<f64>, never_ran: bool, reason: &str, detail: &str) {
    let last = last_stale_ts(&o.events);
    let emitted = EventEmitter::new(&o.events, "daemon")
        .emit(
            "evals_stale",
            &json!({
                "reason": reason,
                "detail": truncate(detail, 200),
                "age_days": age_days,
                "never_ran": never_ran,
                "window_days": o.schedule_days,
            }),
        )
        .is_ok();
    if let Some(last) = last {
        if (Utc::now() - last).num_seconds() < o.schedule_days * 86_400 {
            return;
        }
    }
    if !emitted {
        // The journal is the rate bound: a notice without its receipt row is
        // unverifiable state. The next tick re-journals and notifies.
        return;
    }
    let _ = Command::new(&o.fno_bin)
        .args([
            "inbox",
            "notify",
            "fno evals stale",
            &format!(
                "{reason}: {}. Run: fno doctor evals run --tier regression -y",
                truncate(detail, 160)
            ),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The summary document the Python side passes inline (`--summary-json`):
/// `evals_health_summary()`'s JSON, or the literal `null` when it had no rows.
struct Summary {
    never_ran: bool,
    age_days: Option<f64>,
}

fn parse_summary(raw: Option<&str>) -> Summary {
    let value = match raw {
        None | Some("null") => Value::Null,
        Some(s) => serde_json::from_str(s).unwrap_or(Value::Null),
    };
    match value {
        Value::Null => Summary {
            never_ran: true,
            age_days: None,
        },
        v => Summary {
            never_ran: v.get("never_ran").and_then(Value::as_bool).unwrap_or(false),
            age_days: v.get("age_days").and_then(Value::as_f64),
        },
    }
}

/// Spawns the detached run-mode child and returns its pid; the seam tests
/// inject so no test needs the real binary to re-exec itself.
type Spawner<'a> = &'a dyn Fn(&[String]) -> Result<u32, String>;

fn self_exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "fno-agents".into())
}

/// The child argv, shared by the production spawner and the tests that
/// assert on it.
fn run_argv(o: &Opts, started_rfc3339: &str) -> Vec<String> {
    let mut argv = vec![
        self_exe(),
        "evals-arm".into(),
        "--run".into(),
        "--history".into(),
        o.history.to_string_lossy().into_owned(),
        "--events".into(),
        o.events.to_string_lossy().into_owned(),
        "--fno-bin".into(),
        o.fno_bin.clone(),
        "--schedule-days".into(),
        o.schedule_days.to_string(),
        "--stale-days".into(),
        o.stale_days.to_string(),
        "--run-timeout-s".into(),
        o.run_timeout_s.to_string(),
        "--started".into(),
        started_rfc3339.to_string(),
    ];
    if let Some(sj) = &o.summary_json {
        argv.push("--summary-json".into());
        argv.push(sj.clone());
    }
    if let Some(root) = &o.claims_root {
        argv.push("--claims-root".into());
        argv.push(root.to_string_lossy().into_owned());
    }
    argv
}

/// Tick mode: the pr-watch evals leg.
fn run_tick_mode(o: &Opts, gate: &dyn Fn() -> GateReading, spawner: Spawner<'_>) -> i32 {
    let claims_root = o.claims_root.clone().or_else(claims::global_claims_root);
    let summary = parse_summary(o.summary_json.as_deref());

    // 1. The cross-tick claim. Live holder: a run is in flight; Suspect/Stale:
    // the child died without a receipt - journal it, release, go again.
    match claims::status(CLAIM_KEY, claims_root.as_deref()) {
        (claims::ClaimState::Live, _) => {
            return receipt(0, Some("in_flight"), "a scheduled run holds the claim")
        }
        (claims::ClaimState::Corrupted, _) => {
            return receipt(
                0,
                Some("claim_corrupted"),
                "claim file unreadable; left for force-release",
            )
        }
        (state, Some(rec))
            if matches!(
                state,
                claims::ClaimState::Suspect | claims::ClaimState::Stale
            ) =>
        {
            let holder = rec.holder.clone();
            journal_stale(
                o,
                summary.age_days,
                summary.never_ran,
                "error",
                "scheduled run died without a receipt",
            );
            let _ = claims::release(CLAIM_KEY, &holder, claims_root.as_deref(), None);
        }
        _ => {}
    }

    // 2. The demand read: is the newest regression run older than the window?
    // No summary and never_ran are both DUE; an unreadable age neither
    // asserts staleness nor spends budget.
    let age_days = summary.age_days;
    if !summary.never_ran && age_days.is_none() {
        return receipt(
            0,
            Some("age_unknown"),
            "history carries no readable regression timestamp",
        );
    }
    if !summary.never_ran && age_days.map_or(false, |a| a <= o.schedule_days as f64) {
        return receipt(
            0,
            Some("fresh"),
            &format!(
                "age {:.0}d <= {}d window",
                age_days.unwrap_or(0.0),
                o.schedule_days
            ),
        );
    }
    let escalates = summary.never_ran || age_days.map_or(false, |a| a > 2.0 * o.stale_days as f64);

    // 3. The gate, read-only: the same counters a spawn would be refused on.
    let g = gate();
    let gate_refusal = if g.slots >= g.max_live as usize {
        Some(("fleet_full", "spawn gate refused: fleet_full".to_string()))
    } else {
        g.ram_gb.and_then(|ram| {
            (ram < g.min_free_gb).then(|| {
                (
                    "ram",
                    format!(
                        "spawn gate refused: ram {ram:.1}GB below min {:.1}GB",
                        g.min_free_gb
                    ),
                )
            })
        })
    };
    if let Some((skip, detail)) = gate_refusal {
        if escalates {
            journal_stale(o, age_days, summary.never_ran, "gate", &detail);
        }
        return receipt(0, Some(skip), &detail);
    }

    // 4. Detached launch, then the cross-tick claim on the child's pid.
    let started = Utc::now().to_rfc3339();
    let argv = run_argv(o, &started);
    match spawner(&argv) {
        Err(e) => {
            if escalates {
                journal_stale(
                    o,
                    age_days,
                    summary.never_ran,
                    "error",
                    &format!("could not launch run mode: {e}"),
                );
            }
            receipt(
                0,
                Some("spawn_failed"),
                &format!("could not launch run mode: {e}"),
            )
        }
        Ok(pid) => {
            let opts = claims::AcquireOpts {
                pid: Some(pid),
                pid_unavailable: false,
                ttl_ms: Some((o.run_timeout_s as i64 + 600) * 1000),
                reason: Some("evals scheduled regression run".into()),
                metadata: None,
                root: claims_root.clone(),
                events_dir: None,
            };
            let held = matches!(
                claims::acquire(CLAIM_KEY, &pid.to_string(), opts),
                claims::AcquireOutcome::Acquired(_)
            );
            let claim_note = if held { "" } else { " (claim held elsewhere)" };
            receipt(1, None, &format!("launched pid {pid}{claim_note}"))
        }
    }
}

/// The pid probe is only honest for same-machine holders; the claim carried
/// one pid, so a stale-free read means the child released or died - both are
/// "the claim is free" to the next tick (it re-arms either way).

// -- Run mode -----------------------------------------------------------------

/// Regression rows appended at or after `started`, attributed by each row's
/// own ts (a concurrent writer's rows carry their own ts and stay out of this
/// run's receipt). Variant: absent reads as baseline.
fn rows_since(history: &Path, started: DateTime<Utc>) -> Vec<Value> {
    let text = match std::fs::read_to_string(history) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|r| r.get("tier").and_then(Value::as_str) == Some("regression"))
        .filter(|r| match r.get("variant") {
            None | Some(Value::Null) => true,
            Some(Value::String(v)) => v == "baseline",
            Some(_) => false,
        })
        .filter(|r| {
            r.get("ts")
                .and_then(Value::as_str)
                .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
                .map_or(false, |dt| dt.with_timezone(&Utc) >= started)
        })
        .collect()
}

/// Drain a piped stderr on a reader thread so a chatty child can never fill
/// the pipe and deadlock the bounded wait.
fn spawn_stderr_drain(child: &mut std::process::Child) -> Option<std::thread::JoinHandle<Vec<u8>>> {
    child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    })
}

fn stderr_tail(buf: &[u8]) -> String {
    let text = String::from_utf8_lossy(buf);
    text.trim()
        .lines()
        .next_back()
        .map(|l| truncate(l, 200))
        .unwrap_or_default()
}

/// Set a setsid pre-exec hook so the child leads its own process group: a
/// signal aimed at the group can never boomerang onto this process.
fn setsid_hook(cmd: &mut Command) {
    // SAFETY: setsid() is async-signal-safe and takes no arguments; called
    // here it runs in the child after fork, before exec, single-threaded.
    unsafe {
        cmd.pre_exec(|| {
            unsafe { libc::setsid() };
            Ok(())
        })
    };
}

/// Spawn `argv` as the leader of a brand-new session (the `test_run` pattern).
fn spawn_group(argv: &[String]) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());
    setsid_hook(&mut cmd);
    cmd.spawn()
}

/// Run mode: the detached child. Runs the bank bounded by `--run-timeout-s`,
/// attributes rows, journals the outcome, always releases the claim.
fn run_run_mode(o: &Opts) -> i32 {
    let started = match o
        .started
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    {
        Some(dt) => dt.with_timezone(&Utc),
        None => return receipt(0, Some("started_invalid"), "--started must be RFC3339"),
    };
    let claims_root = o.claims_root.clone().or_else(claims::global_claims_root);
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(o.run_timeout_s);

    let mut cmd = Command::new(&o.fno_bin);
    cmd.args(["doctor", "evals", "run", "--tier", "regression", "-y"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    setsid_hook(&mut cmd);
    let mut run = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            journal_stale(
                o,
                None,
                false,
                "error",
                &format!("could not spawn {}: {e}", o.fno_bin),
            );
            let _ = claims::release(
                CLAIM_KEY,
                &std::process::id().to_string(),
                claims_root.as_deref(),
                None,
            );
            return receipt(0, Some("run_failed"), &format!("could not spawn: {e}"));
        }
    };
    let stderr_handle = spawn_stderr_drain(&mut run);
    let exit_code = loop {
        match run.try_wait() {
            Ok(Some(status)) => break Some(status.code().unwrap_or(-1)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(_) => break Some(-1),
        }
    };
    let duration_s = t0.elapsed().as_secs_f64();
    let stderr_buf = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();

    let outcome: Result<(usize, usize), (&str, String)> = match exit_code {
        None => {
            // The child is its own group leader (setsid), so -pid is its group.
            unsafe {
                libc::kill(-(run.id() as i32), libc::SIGKILL);
            }
            let _ = run.wait();
            Err((
                "timeout",
                format!("killed at the {}s run budget", o.run_timeout_s),
            ))
        }
        Some(code) if code != 0 => Err((
            "error",
            format!("exit {code}: {}", stderr_tail(&stderr_buf)),
        )),
        Some(_) => {
            let rows = rows_since(&o.history, started);
            if rows.is_empty() {
                Err((
                    "no_rows",
                    "run exited 0 but appended no history rows".into(),
                ))
            } else {
                let task_ids: std::collections::BTreeSet<String> = rows
                    .iter()
                    .filter_map(|r| r.get("task_id").and_then(Value::as_str).map(str::to_string))
                    .collect();
                let passes = rows
                    .iter()
                    .filter(|r| r.get("pass") == Some(&Value::Bool(true)))
                    .count();
                Ok((task_ids.len(), passes))
            }
        }
    };

    let _ = claims::release(
        CLAIM_KEY,
        &std::process::id().to_string(),
        claims_root.as_deref(),
        None,
    );
    match outcome {
        Ok((task_count, passes)) => {
            let summary = parse_summary(o.summary_json.as_deref());
            let _ = EventEmitter::new(&o.events, "daemon").emit(
                "evals_scheduled_run",
                &json!({
                    "task_count": task_count,
                    "passes": passes,
                    "duration_s": (duration_s * 1000.0).round() / 1000.0,
                    "age_days_before": summary.age_days,
                    "window_days": o.schedule_days,
                }),
            );
            receipt(
                1,
                None,
                &format!(
                    "{task_count} task(s), {passes}/{} pass, {duration_s:.0}s",
                    task_count
                ),
            )
        }
        Err((reason, detail)) => {
            journal_stale(o, None, false, reason, &detail);
            receipt(0, Some(reason), &detail)
        }
    }
}

/// `fno-agents evals-arm`: one JSON receipt line, exit 0; usage errors exit 2.
pub fn run_evals_arm(args: &[String]) -> i32 {
    let o = match parse_args(args) {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!("{USAGE}");
            return EXIT_USAGE;
        }
    };
    if o.run {
        run_run_mode(&o)
    } else {
        run_tick_mode(&o, &read_gate, &spawn_detached)
    }
}

/// The production spawner: the run-mode child in a new session, stdio null.
fn spawn_detached(argv: &[String]) -> Result<u32, String> {
    spawn_group(argv).map(|c| c.id()).map_err(|e| e.to_string())
}

// -- Tests --------------------------------------------------------------------

#[cfg(test)]
mod tests;

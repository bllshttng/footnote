//! Target driver: TargetQueue + the `loop run` CLI verb.
//!
//! ## Degenerate walk (one unit, re-dispatch until terminal event)
//!
//! A target session is a single unit of work: the active `target-state.md`
//! manifest identifies the session. The walk loop is degenerate:
//!
//! ```text
//! queue.next() -> Some(unit for session_id)
//! inner dispatch loop:
//!   run driver_invoke, wait
//!   if termination event in journal -> close unit, break
//!   else -> node_failed, re-dispatch
//! queue.next() -> None (closed) -> NoWork -> exit 0
//! ```
//!
//! The outer loop terminates with `NoWork` after the single unit is closed.
//! The CLI maps `NoWork` from a single-unit walk to exit 0 and reports the
//! unit's own termination reason as the headline (it is the news; the walk-level
//! `NoWork` is plumbing).
//!
//! ## Why TargetQueue::close() is inert
//!
//! The target session's own loop-check stop hook already emitted the
//! `termination` event before `close()` is called. The manifest is immutable
//! (invariant from ab-d0337fbc). Closing the backlog graph node and stamping the
//! plan belong to `reconcile` and `stamp-plan` respectively; calling them here
//! would duplicate work and couple the loop runtime to concerns it must not own.
//! `megawalk`'s Queue (group 2, ab-7303e5d7) IS where `fno backlog done` runs.

use crate::loop_dispatch::{
    driver_default_max, preflight, resolve_driver_binary, ShelloutDispatcher,
};
use crate::loop_runtime::{
    run_loop, CloseOutcome, Evidence, GlobalJournalPath, Journal, LoopBudget, LoopError,
    ProjectJournalPath, Queue, Unit,
};
use crate::loopcheck::TerminationReason;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

// ── SIGINT handler ────────────────────────────────────────────────────────────

pub(crate) static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);

// SAFETY: signal handler touches only an AtomicBool. No allocations, no locks.
extern "C" fn handle_sigint(_: libc::c_int) {
    SIGINT_RECEIVED.store(true, Ordering::SeqCst);
}

/// Install the SIGINT handler. The child process group receives SIGINT
/// naturally (foreground process group); after the child exits, `cancel()`
/// returns true and the loop terminates with Interrupted.
pub(crate) fn install_sigint_handler() {
    // SAFETY: handle_sigint is async-signal-safe (one atomic store).
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_sigint as *const () as libc::sighandler_t,
        );
    }
}

// ── TargetQueue ───────────────────────────────────────────────────────────────

/// Parsed fields from target-state.md frontmatter needed by the loop runtime.
struct TargetManifest {
    session_id: String,
    input: String,
    plan_path: String,
}

/// Parse the minimal frontmatter fields needed by TargetQueue.
/// Style mirrors loopcheck.rs `parse_manifest` but is a local copy per the spec:
/// "write your own tiny local parser - do NOT modify loopcheck.rs".
fn parse_target_manifest(content: &str) -> Option<TargetManifest> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }
    let after_first = &content[3..];
    let end = after_first.find("\n---")?;
    let body = &after_first[..end];

    let mut session_id = String::new();
    let mut input = String::new();
    let mut plan_path = String::new();

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            // Strip surrounding quotes from values.
            let v = v.trim().trim_matches(|c: char| c == '"' || c == '\'');
            match k {
                "session_id" => session_id = v.to_string(),
                "input" => input = v.to_string(),
                "plan_path" => plan_path = v.to_string(),
                _ => {}
            }
        }
    }

    if session_id.is_empty() {
        return None;
    }
    Some(TargetManifest {
        session_id,
        input,
        plan_path,
    })
}

/// A degenerate queue containing exactly one unit: the active target session.
///
/// `next()` returns the unit on the first call. After `close()` is called, or
/// after the unit has been returned, subsequent `next()` calls return `None`.
///
/// `close()` is intentionally inert: the session's own stop hook already
/// emitted the termination event; the manifest is immutable; graph-node closing
/// and plan-stamping belong to `reconcile` / `stamp-plan`, not the loop runtime.
/// megawalk's Queue (group 2, ab-7303e5d7) is where `fno backlog done` runs.
/// See module documentation for why `close()` is inert.
///
/// ## Why Option<Unit> (not Mutex) (F8)
///
/// Queue::next/close take `&mut self` so no Mutex is needed here. The walk
/// loop is single-threaded; interior mutability would add noise without benefit.
pub struct TargetQueue {
    unit: Option<Unit>,
}

impl TargetQueue {
    /// Read `.fno/target-state.md` from `repo_root` and construct the queue.
    pub fn from_manifest(repo_root: &Path) -> Result<Self, LoopError> {
        let manifest_path = repo_root.join(".fno").join("target-state.md");
        if !manifest_path.exists() {
            return Err(LoopError::Queue(format!(
                "No state file found at .fno/target-state.md - run /target first to initialize (looked in: {})",
                manifest_path.display()
            )));
        }
        let content = fs::read_to_string(&manifest_path).map_err(LoopError::Io)?;
        let manifest = parse_target_manifest(&content).ok_or_else(|| {
            LoopError::Queue(
                "No state file found at .fno/target-state.md - run /target first to initialize (manifest missing required fields)".to_string()
            )
        })?;

        let unit = Unit {
            id: manifest.session_id.clone(),
            title: manifest.input.clone(),
            session_key: manifest.session_id,
            plan_path: if manifest.plan_path.is_empty() {
                None
            } else {
                Some(manifest.plan_path)
            },
            extra_env: vec![],
        };
        Ok(Self { unit: Some(unit) })
    }
}

impl Queue for TargetQueue {
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        Ok(self.unit.take())
    }

    /// Inert close: see module doc for why this does nothing.
    ///
    /// The session's loop-check stop hook already emitted the termination event.
    /// The manifest is immutable (ab-d0337fbc invariant). Graph-node closing and
    /// plan-stamping belong to reconcile / stamp-plan, not the loop runtime.
    /// megawalk's Queue (group 2, ab-7303e5d7) is where `fno backlog done` runs.
    fn close(&mut self, _unit: &Unit, _evidence: &Evidence) -> Result<CloseOutcome, LoopError> {
        Ok(CloseOutcome::Closed)
    }
}

// ── exit-code mapping ─────────────────────────────────────────────────────────

/// Map a LoopOutcome walk reason to a process exit code.
///
/// DonePRGreen | DoneAdvisory | NoWork -> 0  (success)
/// Budget | NoProgress | Aborted       -> 1  (failed / budget)
/// Interrupted                         -> 130 (SIGINT convention)
///
/// For the degenerate single-unit walk, NoWork is reported after the unit closes
/// with a terminal reason. The headline exit code is derived from the unit's OWN
/// evidence reason, not the walk-level NoWork, so the caller sees the actual
/// outcome (DonePRGreen -> 0, Budget -> 1, etc.).
pub(crate) fn exit_code_for_reason(reason: &TerminationReason) -> i32 {
    match reason {
        TerminationReason::DonePRGreen
        | TerminationReason::DoneAdvisory
        | TerminationReason::DoneBatched
        // DoneAwaitingMerge: work complete, human-merge-gated past proven main
        // red - a clean stop like the other Done* terminals. The reason string
        // (not the exit code) is what a wrapper reads to distinguish it.
        | TerminationReason::DoneAwaitingMerge
        | TerminationReason::NoWork => 0,
        TerminationReason::Budget | TerminationReason::NoProgress | TerminationReason::Aborted => 1,
        TerminationReason::Interrupted => 130,
    }
}

// ── CLI verb ──────────────────────────────────────────────────────────────────

/// Entry point for `fno-agents loop run ...`.
///
/// Usage:
/// ```text
/// fno-agents loop run
///   --driver target
///   [--dispatcher claude-code|hermes|openclaw]
///   [--max-iterations N]
///   [--max-turns N]
///   [--budget N]
///   [--model NAME]
///   [--prompt-file PATH]
///   [--cli claude|opencode]
///   [--driver-lib-dir DIR]
///   [--cwd DIR]
/// ```
///
/// Exit codes:
/// - 0: DonePRGreen | DoneAdvisory | NoWork (unit terminated successfully)
/// - 1: Budget | NoProgress | Aborted (walk failed or hit ceiling)
/// - 2: usage error / internal error
/// - 77: driver binary missing from PATH (preflight failure)
/// - 130: Interrupted (SIGINT)
pub fn run_loop_verb(args: &[String]) -> i32 {
    match run_loop_verb_inner(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fno-agents loop: {e}");
            2
        }
    }
}

fn run_loop_verb_inner(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    // ── subcommand check ──────────────────────────────────────────────────────
    let subcommand = args.first().map(|s| s.as_str()).unwrap_or("");
    if subcommand != "run" {
        eprintln!("fno-agents loop: expected subcommand 'run', got '{subcommand}'");
        eprintln!("Usage: fno-agents loop run --driver <name> [options]");
        return Ok(2);
    }
    let args = &args[1..]; // skip "run"

    // ── flag parsing ──────────────────────────────────────────────────────────
    let mut driver: Option<String> = None;
    let mut dispatcher_name = "claude-code".to_string();
    let mut max_iterations: Option<u64> = None;
    let mut max_turns: u64 = 15;
    let mut budget_usd: f64 = 25.0;
    let mut model: Option<String> = None;
    let mut prompt_file: Option<String> = None;
    let mut cli_alias: Option<String> = None;
    let mut driver_lib_dir: Option<PathBuf> = None;
    let mut cwd: PathBuf = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // Megawalk-only flags.
    let mut project: Option<String> = None;
    let mut all = false;
    let mut allow_merge = false;
    let mut parallel_cap: Option<u64> = None;
    let mut max_units: Option<u64> = None;
    // Megawalk flags (group 3, ab-9fd662c6).
    let mut mission: Option<String> = None;
    let mut termination_key: Option<String> = None;

    // Helper: advance i and return the next argument, or emit a "missing value"
    // usage error (exit 2) if the flag is trailing with no following value.
    // Using a macro (not a closure) to allow `return Ok(2)` from the outer fn.
    macro_rules! require_value {
        ($flag:expr, $args:expr, $i:expr) => {{
            $i += 1;
            match $args.get($i) {
                Some(v) => v.as_str(),
                None => {
                    eprintln!("fno-agents loop run: {}: missing value", $flag);
                    return Ok(2);
                }
            }
        }};
    }

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--driver" => {
                driver = Some(require_value!("--driver", args, i).to_string());
            }
            "--dispatcher" => {
                dispatcher_name = require_value!("--dispatcher", args, i).to_string();
            }
            "--max-iterations" => {
                let v = require_value!("--max-iterations", args, i);
                max_iterations = Some(
                    v.parse::<u64>()
                        .map_err(|_| format!("--max-iterations: expected integer, got '{v}'"))?,
                );
            }
            "--max-turns" => {
                let v = require_value!("--max-turns", args, i);
                max_turns = v
                    .parse::<u64>()
                    .map_err(|_| format!("--max-turns: expected integer, got '{v}'"))?;
            }
            "--budget" => {
                let v = require_value!("--budget", args, i);
                let parsed = v
                    .parse::<f64>()
                    .map_err(|_| format!("--budget: expected number, got '{v}'"))?;
                // F3: reject zero/negative/NaN budget per plan Failure Mode.
                if !parsed.is_finite() || parsed <= 0.0 {
                    eprintln!(
                        "fno-agents loop run: --budget must be a positive number, got '{v}' ({parsed})"
                    );
                    return Ok(2);
                }
                budget_usd = parsed;
            }
            "--model" => {
                model = Some(require_value!("--model", args, i).to_string());
            }
            "--prompt-file" => {
                prompt_file = Some(require_value!("--prompt-file", args, i).to_string());
            }
            "--cli" => {
                cli_alias = Some(require_value!("--cli", args, i).to_string());
            }
            "--driver-lib-dir" => {
                driver_lib_dir = Some(PathBuf::from(require_value!("--driver-lib-dir", args, i)));
            }
            "--cwd" => {
                cwd = PathBuf::from(require_value!("--cwd", args, i));
            }
            "--project" => {
                project = Some(require_value!("--project", args, i).to_string());
            }
            "--all" => {
                all = true;
            }
            "--allow-merge" => {
                allow_merge = true;
            }
            "--parallel-cap" => {
                let v = require_value!("--parallel-cap", args, i);
                let parsed = v
                    .parse::<u64>()
                    .map_err(|_| format!("--parallel-cap: expected integer, got '{v}'"))?;
                parallel_cap = Some(crate::loop_megawalk::clamp_parallel_cap(parsed));
                if parsed > 1 {
                    eprintln!(
                        "fno-agents loop megawalk: --parallel-cap {parsed} accepted; \
                         execution remains SEQUENTIAL (collision-conservative default, \
                         group-2 serializes regardless of cap)"
                    );
                }
            }
            "--max-units" => {
                let v = require_value!("--max-units", args, i);
                let parsed = v
                    .parse::<u64>()
                    .map_err(|_| format!("--max-units: expected integer >= 1, got '{v}'"))?;
                if parsed == 0 {
                    eprintln!(
                        "fno-agents loop run: --max-units must be >= 1, got 0 \
                         (use --max-units 1 for the /megawalk once modifier)"
                    );
                    return Ok(2);
                }
                max_units = Some(parsed);
            }
            "--mission" => {
                mission = Some(require_value!("--mission", args, i).to_string());
            }
            "--termination-key" => {
                termination_key = Some(require_value!("--termination-key", args, i).to_string());
            }
            _ => {
                eprintln!("fno-agents loop run: unknown flag '{flag}'");
                return Ok(2);
            }
        }
        i += 1;
    }

    // ── driver validation ─────────────────────────────────────────────────────
    let driver = match driver.as_deref() {
        None => {
            eprintln!("fno-agents loop run: --driver is required");
            eprintln!("Usage: fno-agents loop run --driver <target|...> [options]");
            return Ok(2);
        }
        Some("megawalk") => {
            // Validate that --allow-merge is not passed with --driver target.
            // (allow_merge is megawalk-only; if somehow we get here with target
            //  the check below would fire.)
            return Ok(crate::loop_megawalk::run(
                &dispatcher_name,
                max_iterations,
                max_turns,
                budget_usd,
                model.as_deref(),
                prompt_file.as_deref(),
                cli_alias.as_deref(),
                driver_lib_dir,
                cwd,
                project,
                all,
                allow_merge,
                parallel_cap,
                max_units,
                mission,
                termination_key,
            ));
        }
        Some("target") => {
            // --allow-merge is megawalk-only; reject with clear message.
            if allow_merge {
                eprintln!(
                    "fno-agents loop run: --allow-merge is only valid with --driver megawalk"
                );
                return Ok(2);
            }
            // --max-units is megawalk-only; reject with clear message.
            if max_units.is_some() {
                eprintln!("fno-agents loop run: --max-units is only valid with --driver megawalk");
                return Ok(2);
            }
            // --mission / --termination-key are megawalk flags.
            if mission.is_some() {
                eprintln!("fno-agents loop run: --mission is only valid with --driver megawalk");
                return Ok(2);
            }
            if termination_key.is_some() {
                eprintln!(
                    "fno-agents loop run: --termination-key is only valid with --driver megawalk"
                );
                return Ok(2);
            }
            "target"
        }
        Some(other) => {
            eprintln!(
                "fno-agents loop run: unknown --driver '{other}'; \
                 supported: 'target', 'megawalk'"
            );
            return Ok(2);
        }
    };
    let _ = driver; // "target" confirmed

    // ── resolve driver-lib-dir ────────────────────────────────────────────────
    let lib_dir = match driver_lib_dir {
        Some(d) => d,
        None => {
            // Try FNO_DRIVER_LIB_DIR env, then <cwd>/scripts/lib.
            if let Ok(env_dir) = std::env::var("FNO_DRIVER_LIB_DIR") {
                PathBuf::from(env_dir)
            } else {
                let candidate = cwd.join("scripts").join("lib");
                if candidate.is_dir() {
                    candidate
                } else {
                    eprintln!(
                        "fno-agents loop run: cannot resolve driver lib directory. \
                         Pass --driver-lib-dir <path> (the abilities plugin's \
                         scripts/lib directory) or set FNO_DRIVER_LIB_DIR env."
                    );
                    return Ok(2);
                }
            }
        }
    };

    // ── preflight (all before any dispatch) ───────────────────────────────────
    // 1. Manifest exists (exit 1 on missing).
    let mut queue = match TargetQueue::from_manifest(&cwd) {
        Ok(q) => q,
        Err(e) => {
            eprintln!("fno-agents loop run: {e}");
            return Ok(1);
        }
    };

    // 2. Driver whitelist + lib file + binary (exit 77 on missing binary).
    // F2: pass cli_alias so preflight checks the same binary the dispatcher will use.
    let lib_path = match preflight(&dispatcher_name, &lib_dir, cli_alias.as_deref()) {
        Ok(p) => p,
        Err(LoopError::Dispatch(msg)) => {
            // Binary missing.
            eprintln!("fno-agents loop run: {msg}");
            return Ok(77);
        }
        Err(e) => {
            eprintln!("fno-agents loop run: {e}");
            return Ok(2);
        }
    };

    // ── resolve max_iterations ────────────────────────────────────────────────
    let max_iters = match max_iterations {
        Some(n) => n,
        None => match driver_default_max(&lib_path) {
            Ok(n) => n,
            Err(e) => {
                eprintln!(
                    "fno-agents loop run: could not query driver_default_max: {e}; \
                     pass --max-iterations explicitly"
                );
                return Ok(2);
            }
        },
    };

    // ── build static env for the dispatcher ──────────────────────────────────
    // Mirrors run-target-loop.sh:36-40.
    let abilities_dir = cwd.join(".fno");
    let output_file = abilities_dir.join("target-last-output.txt");
    let history_file = abilities_dir.join("target-history.txt");
    let signal_file = abilities_dir.join("target-promise.signal");

    let mut env: Vec<(String, String)> = vec![
        (
            "OUTPUT_FILE".to_string(),
            output_file.to_str().unwrap_or("").to_string(),
        ),
        (
            "HISTORY_FILE".to_string(),
            history_file.to_str().unwrap_or("").to_string(),
        ),
        (
            "SIGNAL_FILE".to_string(),
            signal_file.to_str().unwrap_or("").to_string(),
        ),
        ("MAX_TURNS".to_string(), max_turns.to_string()),
        ("BUDGET_USD".to_string(), format!("{budget_usd}")),
        (
            "CONTINUE_PROMPT".to_string(),
            "/target --resume".to_string(),
        ),
    ];

    if let Some(m) = &model {
        env.push(("MODEL_FLAG".to_string(), format!("--model {m}")));
    } else {
        env.push(("MODEL_FLAG".to_string(), String::new()));
    }

    if let Some(pf) = &prompt_file {
        env.push(("PROMPT_FILE".to_string(), pf.clone()));
    }

    if let Some(cli) = &cli_alias {
        env.push(("CLI".to_string(), cli.clone()));
    }

    // Pass FNO_CWD so driver stubs and real drivers can resolve paths.
    env.push((
        "FNO_CWD".to_string(),
        cwd.to_str().unwrap_or(".").to_string(),
    ));

    // ── SIGINT handler ────────────────────────────────────────────────────────
    install_sigint_handler();

    // ── build journal ─────────────────────────────────────────────────────────
    let project_events = abilities_dir.join("events.jsonl");
    let home_dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let global_events = home_dir.join(".fno").join("events.jsonl");
    let journal = Journal::new(
        ProjectJournalPath(project_events),
        GlobalJournalPath(global_events),
    );

    // ── peek at the first unit for the header (F6: no TOCTOU re-read) ────────
    // Read session_id/input from the already-constructed queue instead of
    // re-reading the manifest (which avoids the TOCTOU double-read and the
    // .unwrap().unwrap() panic path).
    let (session_id_display, input_display) = {
        // Peek without consuming: TargetQueue stores Option<Unit>, so we look
        // at the inner unit via as_ref without taking it.
        match queue.unit.as_ref() {
            Some(u) => (u.id.clone(), u.title.clone()),
            None => ("(none)".to_string(), "(none)".to_string()),
        }
    };

    // Print header. resolve_driver_binary now reflects the cli_alias (F2).
    let binary_name = resolve_driver_binary(&dispatcher_name, cli_alias.as_deref());
    println!("fno-agents loop run");
    println!("  driver:     target");
    println!("  dispatcher: {dispatcher_name} (binary: {binary_name})");
    println!("  session:    {session_id_display}");
    println!("  input:      {input_display}");
    println!("  iterations: {max_iters} max");
    println!("  budget:     ${budget_usd} USD");

    // ── build dispatcher ──────────────────────────────────────────────────────
    let dispatcher = ShelloutDispatcher::new(lib_path, env, cwd.clone());

    // ── build budget ──────────────────────────────────────────────────────────
    let budget = match LoopBudget::new(max_iters) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fno-agents loop run: {e}");
            return Ok(2);
        }
    };

    // ── cancel closure ────────────────────────────────────────────────────────
    let cancel_file = cwd.join(".fno").join(".target-cancelled");
    let cancel = move || SIGINT_RECEIVED.load(Ordering::SeqCst) || cancel_file.exists();

    // ── run the loop ──────────────────────────────────────────────────────────
    let outcome = match run_loop(&mut queue, &dispatcher, &budget, &journal, &cancel, None) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("fno-agents loop run: fatal loop error: {e}");
            return Ok(2);
        }
    };

    // ── report outcome ────────────────────────────────────────────────────────
    // For the degenerate single-unit walk, report the unit's evidence reason as
    // the headline; the walk-level NoWork is plumbing, not news.
    let (headline_reason, exit_code) = if let Some(unit_result) = outcome.units.first() {
        let r = &unit_result.evidence.reason;
        let code = exit_code_for_reason(r);
        (format!("{r:?}"), code)
    } else {
        // No units closed (Budget/Interrupted at walk level before close).
        let code = exit_code_for_reason(&outcome.reason);
        (format!("{:?}", outcome.reason), code)
    };

    println!(
        "loop: {} ({} iterations used)",
        headline_reason, outcome.iterations_used
    );

    // Emit a summary line for each unit.
    for unit_result in &outcome.units {
        println!(
            "  unit {}: {:?} ({:?})",
            unit_result.unit_id, unit_result.evidence.reason, unit_result.close
        );
    }

    Ok(exit_code)
}

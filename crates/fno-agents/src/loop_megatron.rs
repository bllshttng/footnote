//! Megatron driver: MegatronQueue + MegatronDispatcher + the
//! `loop run --driver megatron` verb glue (group 3 of ab-ed61946d,
//! node ab-9fd662c6).
//!
//! ## The recursion (collapse doc promise)
//!
//! `work(unit)` at the mission altitude IS `loop()` one altitude below:
//! MegatronQueue dequeues fleet PROJECTS (via `fno megatron next`), and the
//! dispatcher runs each project as a full megawalk - this same binary,
//! re-invoked with `--driver megawalk --cwd <project_path> --mission <id>
//! --termination-key <session_key>`. The child walk journals a `termination`
//! event keyed by the session key when it finishes (see
//! `loop_megawalk::emit_walk_termination`), which the UNCHANGED `run_loop`
//! runtime observes through `Journal::find_termination`'s global-mirror
//! fallback (the child runs in a different cwd). Zero runtime changes -
//! the fifth-driver test holds.
//!
//! ## What replaced the commander poll loop
//!
//! The Python commander (`cli/src/fno/megatron/loop.py`, deleted in
//! this group) POLLED `~/.fno/fleet/{slug}/completions/wave-N/*.json`
//! on a sleep cycle. Here the completion evidence is the child walk's
//! journaled termination event; `MegatronQueue::close` records the outcome
//! through `fno megatron complete` (which idempotently writes the same
//! completion JSON ledger the worker ship gates write - the FILES survive
//! as the mission record; the polling died).
//!
//! ## Verb seam (grilled 7 applied at the fleet altitude)
//!
//! The queue never reads manifests, mission state, or the fleet directory.
//! All mission logic (wave advancement, manifest sha guard, dispatch-on-
//! demand plan+intake, completion records) lives behind `fno megatron
//! next` / `fno megatron complete` in Python, where the well-tested
//! manifest/state/dispatch code already lives.

use crate::loop_megawalk::{abi_cmd, gen_session_key_with_infix, maybe_stale_hint, retry_etxtbsy};
use crate::loop_runtime::{
    run_loop, CloseOutcome, DispatchCtx, Dispatcher, Evidence, GlobalJournalPath, Journal,
    LoopBudget, LoopError, ProjectJournalPath, Queue, Session, Unit,
};
use crate::loopcheck::TerminationReason;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command};

// ── MegatronQueue ─────────────────────────────────────────────────────────────

/// Per-unit bookkeeping recorded at dequeue time so `close()` can name the
/// (project, wave) pair without parsing it back out of the unit id.
struct ProjectEntry {
    project: String,
    wave: u64,
}

/// A Queue over a fleet mission's projects. Shells `fno megatron next` /
/// `fno megatron complete`; never touches the fleet directory itself.
pub struct MegatronQueue {
    /// Path or name of the fno binary. `$FNO_BIN` env overrides for tests.
    abi_bin: String,
    /// Full mission id (`ab-XXXXXXXX`).
    mission_id: String,
    /// unit.id -> (project, wave) for close().
    active: HashMap<String, ProjectEntry>,
}

impl MegatronQueue {
    pub fn new(abi_bin: String, mission_id: String) -> Self {
        Self {
            abi_bin,
            mission_id,
            active: HashMap::new(),
        }
    }
}

impl Queue for MegatronQueue {
    /// Dequeue the next incomplete project of the mission.
    ///
    /// `fno megatron next <mission> --json` contract:
    /// - `null`                       -> mission complete -> Ok(None)
    /// - `{"pause": {"policy","detail"}}` -> Err(LoopError::Pause{policy, detail})
    ///   (run_loop maps this to walk_paused + NoProgress)
    /// - unit object                  -> Some(Unit)
    fn next(&mut self) -> Result<Option<Unit>, LoopError> {
        let out = retry_etxtbsy(|| {
            abi_cmd(&self.abi_bin)
                .args(["megatron", "next", &self.mission_id, "--json"])
                .output()
        })
        .map_err(|e| {
            LoopError::Queue(maybe_stale_hint(
                format!("fno megatron next: spawn failed: {e}"),
                &self.abi_bin,
            ))
        })?;

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(LoopError::Queue(maybe_stale_hint(
                format!("fno megatron next: exit {}: {stderr}", out.status),
                &self.abi_bin,
            )));
        }

        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if stdout == "null" || stdout.is_empty() {
            return Ok(None);
        }

        let v: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| {
            LoopError::Queue(maybe_stale_hint(
                format!("fno megatron next: JSON parse error: {e} (stdout: {stdout:?})"),
                &self.abi_bin,
            ))
        })?;

        // Pause shape: {"pause": {"policy": ..., "detail": ...}}
        if let Some(p) = v.get("pause") {
            let policy = p["policy"].as_str().unwrap_or("unknown");
            let detail = p["detail"].as_str().unwrap_or("");
            return Err(LoopError::Pause {
                policy: policy.to_string(),
                detail: detail.to_string(),
            });
        }

        let project = match v["project"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return Err(LoopError::Queue(maybe_stale_hint(
                    format!("fno megatron next: missing 'project' field in: {stdout:?}"),
                    &self.abi_bin,
                )));
            }
        };
        let wave = match v["wave"].as_u64() {
            Some(w) => w,
            None => {
                return Err(LoopError::Queue(maybe_stale_hint(
                    format!("fno megatron next: missing 'wave' field in: {stdout:?}"),
                    &self.abi_bin,
                )));
            }
        };
        // project_path is REQUIRED: without it there is no cwd to walk in.
        // A missing path means the project is not declared in settings
        // workspaces - fail loudly naming the project (Failure Mode: "a unit
        // whose plan_path is missing at dispatch time" analog).
        let project_path = match v["project_path"].as_str() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return Err(LoopError::Queue(format!(
                    "fno megatron next: project {project:?} has no project_path \
                     (not found in settings workspaces); cannot dispatch a walk"
                )));
            }
        };
        let title = v["title"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("Mission {} wave {wave} - {project}", self.mission_id));

        let session_key = gen_session_key_with_infix("mt");
        let unit_id = format!("{project}@wave-{wave}");

        self.active.insert(
            unit_id.clone(),
            ProjectEntry {
                project: project.clone(),
                wave,
            },
        );

        Ok(Some(Unit {
            id: unit_id,
            title,
            session_key,
            plan_path: None,
            // The dispatcher reads the project path from extra_env; the env
            // vars also reach the child walk's environment for observability.
            extra_env: vec![
                ("MEGATRON_PROJECT_PATH".to_string(), project_path),
                ("MEGATRON_PROJECT".to_string(), project),
                ("MEGATRON_WAVE".to_string(), wave.to_string()),
                ("MEGATRON_MISSION_ID".to_string(), self.mission_id.clone()),
            ],
        }))
    }

    /// Record the child walk's outcome against the mission.
    ///
    /// NoWork (walk drained) | DonePRGreen | DoneAdvisory -> `complete
    /// --outcome done` -> Closed. Anything else -> `complete --outcome
    /// failed` (the verb pauses the mission) -> Parked; the NEXT next() call
    /// returns the pause and run_loop terminates NoProgress.
    fn close(&mut self, unit: &Unit, evidence: &Evidence) -> Result<CloseOutcome, LoopError> {
        let (project, wave) = match self.active.remove(&unit.id) {
            Some(e) => (e.project, e.wave),
            None => {
                // Unit not dequeued by this queue instance (e.g. a caller
                // bypassing next()). Recover from the unit id shape - but
                // LOUDLY: a malformed wave must not silently become
                // `--wave 0` (a record no manifest matches; the mission
                // would stall instead of failing - sigma-review).
                match unit.id.split_once("@wave-") {
                    Some((p, w)) => {
                        let parsed = w.parse::<u64>().map_err(|_| {
                            LoopError::Queue(format!(
                                "megatron close: malformed unit id {:?} (wave is not an integer)",
                                unit.id
                            ))
                        })?;
                        (p.to_string(), parsed)
                    }
                    None => {
                        return Err(LoopError::Queue(format!(
                            "megatron close: unknown unit {:?} (no active entry)",
                            unit.id
                        )));
                    }
                }
            }
        };

        let done = matches!(
            evidence.reason,
            TerminationReason::NoWork
                | TerminationReason::DonePRGreen
                | TerminationReason::DoneAdvisory
        );
        let outcome_flag = if done { "done" } else { "failed" };
        let reason_str = format!("{:?}", evidence.reason);

        let out = retry_etxtbsy(|| {
            abi_cmd(&self.abi_bin)
                .args([
                    "megatron",
                    "complete",
                    &self.mission_id,
                    "--project",
                    &project,
                    "--wave",
                    &wave.to_string(),
                    "--outcome",
                    outcome_flag,
                    "--reason",
                    &reason_str,
                ])
                .output()
        })
        .map_err(|e| LoopError::Queue(format!("fno megatron complete: spawn failed: {e}")))?;

        if !out.status.success() {
            // Fail closed: an unrecordable close must not advance the walk.
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(LoopError::Queue(maybe_stale_hint(
                format!("fno megatron complete: exit {}: {stderr}", out.status),
                &self.abi_bin,
            )));
        }

        // The verb can REFUSE a done outcome: a drained child walk is not
        // proof the project's mission node is done (a prior child's live
        // claim hides the node from `backlog next` - codex P1). The verb
        // pauses the mission and answers {"result": "incomplete"}; map it to
        // Parked so the walk never records a false completion.
        if done {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) {
                if v["result"].as_str() == Some("incomplete") {
                    let detail = v["detail"].as_str().unwrap_or("project incomplete");
                    return Ok(CloseOutcome::Parked(detail.to_string()));
                }
            }
        }

        if done {
            Ok(CloseOutcome::Closed)
        } else {
            let detail = if evidence.message.is_empty() {
                format!("project walk terminated: {reason_str}")
            } else {
                format!(
                    "project walk terminated: {reason_str}: {}",
                    evidence.message
                )
            };
            Ok(CloseOutcome::Parked(detail))
        }
    }
}

// ── MegatronDispatcher ────────────────────────────────────────────────────────

/// A live child megawalk process.
pub struct MegatronSession {
    child: Child,
}

impl Session for MegatronSession {
    fn wait(&mut self) -> Result<i32, LoopError> {
        let status = self.child.wait().map_err(LoopError::Io)?;
        use std::os::unix::process::ExitStatusExt;
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    }
}

/// Dispatches each project unit as a megawalk one altitude down: spawns this
/// same binary with `loop run --driver megawalk --cwd <project_path>
/// --mission <id> --termination-key <session_key>`.
pub struct MegatronDispatcher {
    /// Path to the fno-agents binary to re-invoke. `current_exe()` in
    /// production; injectable for tests.
    fno_agents_bin: PathBuf,
    dispatcher_name: String,
    driver_lib_dir: PathBuf,
    mission_id: String,
    max_turns: u64,
    budget_usd: f64,
    model: Option<String>,
    cli_alias: Option<String>,
    allow_merge: bool,
}

impl MegatronDispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fno_agents_bin: PathBuf,
        dispatcher_name: String,
        driver_lib_dir: PathBuf,
        mission_id: String,
        max_turns: u64,
        budget_usd: f64,
        model: Option<String>,
        cli_alias: Option<String>,
        allow_merge: bool,
    ) -> Self {
        Self {
            fno_agents_bin,
            dispatcher_name,
            driver_lib_dir,
            mission_id,
            max_turns,
            budget_usd,
            model,
            cli_alias,
            allow_merge,
        }
    }
}

impl Dispatcher for MegatronDispatcher {
    fn run(&self, unit: &Unit, _ctx: &DispatchCtx) -> Result<Box<dyn Session>, LoopError> {
        let project_path = unit
            .extra_env
            .iter()
            .find(|(k, _)| k == "MEGATRON_PROJECT_PATH")
            .map(|(_, v)| v.clone())
            .ok_or_else(|| {
                LoopError::Dispatch(format!(
                    "megatron dispatch: unit {:?} carries no MEGATRON_PROJECT_PATH",
                    unit.id
                ))
            })?;

        let mut cmd = Command::new(&self.fno_agents_bin);
        cmd.args([
            "loop",
            "run",
            "--driver",
            "megawalk",
            "--cwd",
            &project_path,
            "--mission",
            &self.mission_id,
            "--termination-key",
            &unit.session_key,
            "--dispatcher",
            &self.dispatcher_name,
            "--max-turns",
            &self.max_turns.to_string(),
            "--budget",
            &self.budget_usd.to_string(),
        ]);
        cmd.args([
            "--driver-lib-dir",
            self.driver_lib_dir.to_str().ok_or_else(|| {
                LoopError::Dispatch("driver lib dir path is not valid UTF-8".to_string())
            })?,
        ]);
        if let Some(ref m) = self.model {
            cmd.args(["--model", m]);
        }
        if let Some(ref c) = self.cli_alias {
            cmd.args(["--cli", c]);
        }
        if self.allow_merge {
            cmd.arg("--allow-merge");
        }

        // Pass the unit env through so the child walk (and its target
        // sessions) can see which mission/project dispatched it.
        for (k, v) in &unit.extra_env {
            cmd.env(k, v);
        }

        // Inherit stdio so the child walk's progress lines stream through
        // the commander's terminal (AC2-UI analog at the fleet altitude).
        let child = retry_etxtbsy(|| cmd.spawn())
            .map_err(|e| LoopError::Dispatch(format!("spawn child megawalk: {e}")))?;

        Ok(Box::new(MegatronSession { child }))
    }
}

// ── fleet claim RAII guard ────────────────────────────────────────────────────

/// Releases the `fleet:<mission_id>` singleton claim on drop, so EVERY exit
/// path - early `?` returns (e.g. `current_exe()` failure), fatal loop
/// errors, and panics - releases the claim instead of leaking it for the
/// 24h TTL (gemini HIGH on PR #458: the manual release calls missed the
/// `?` propagation path between acquire and the first release site).
struct FleetClaimGuard {
    abi_bin: String,
    key: String,
    holder: String,
}

impl Drop for FleetClaimGuard {
    fn drop(&mut self) {
        let _ = abi_cmd(&self.abi_bin)
            .args(["claim", "release", &self.key, "--holder", &self.holder])
            .output();
    }
}

// ── verb glue: pub fn run() ───────────────────────────────────────────────────

/// Entry point for `fno-agents loop run --driver megatron --mission <id>`.
///
/// Exit codes (preserving the `fno megatron run` CLI contract):
/// - 0:   mission complete (NoWork)
/// - 1:   Budget / failure
/// - 2:   usage / configuration error
/// - 3:   another commander holds the fleet claim (CommanderAlreadyRunning)
/// - 4:   mission paused (NoProgress via pause policy)
/// - 77:  driver binary missing (preflight failure, child walks would fail)
/// - 130: Interrupted (SIGINT)
#[allow(clippy::too_many_arguments)]
pub fn run(
    dispatcher_name: &str,
    max_iterations: Option<u64>,
    max_turns: u64,
    budget_usd: f64,
    model: Option<&str>,
    cli_alias: Option<&str>,
    driver_lib_dir: Option<PathBuf>,
    cwd: PathBuf,
    allow_merge: bool,
    mission_id: &str,
) -> i32 {
    match run_inner(
        dispatcher_name,
        max_iterations,
        max_turns,
        budget_usd,
        model,
        cli_alias,
        driver_lib_dir,
        cwd,
        allow_merge,
        mission_id,
    ) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fno-agents loop megatron: {e}");
            2
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    dispatcher_name: &str,
    max_iterations: Option<u64>,
    max_turns: u64,
    budget_usd: f64,
    model: Option<&str>,
    cli_alias: Option<&str>,
    driver_lib_dir: Option<PathBuf>,
    cwd: PathBuf,
    allow_merge: bool,
    mission_id: &str,
) -> Result<i32, Box<dyn std::error::Error>> {
    use crate::loop_dispatch::{preflight, resolve_driver_binary};
    use crate::loop_target::{exit_code_for_reason, install_sigint_handler, SIGINT_RECEIVED};
    use std::sync::atomic::Ordering;

    // ── resolve driver-lib-dir (passed through to every child walk) ──────────
    let lib_dir = match driver_lib_dir {
        Some(d) => d,
        None => {
            if let Ok(env_dir) = std::env::var("FNO_DRIVER_LIB_DIR") {
                PathBuf::from(env_dir)
            } else {
                let candidate = cwd.join("scripts").join("lib");
                if candidate.is_dir() {
                    candidate
                } else {
                    eprintln!(
                        "fno-agents loop megatron: cannot resolve driver lib directory. \
                         Pass --driver-lib-dir <path> or set FNO_DRIVER_LIB_DIR env."
                    );
                    return Ok(2);
                }
            }
        }
    };

    // ── preflight: the CHILD walks need the driver binary; fail before claims ─
    if let Err(e) = preflight(dispatcher_name, &lib_dir, cli_alias) {
        match e {
            LoopError::Dispatch(msg) => {
                eprintln!("fno-agents loop megatron: {msg}");
                return Ok(77);
            }
            other => {
                eprintln!("fno-agents loop megatron: {other}");
                return Ok(2);
            }
        }
    }

    // ── acquire the fleet singleton claim ─────────────────────────────────────
    // Preserves the CommanderAlreadyRunning contract (old loop.py PR1 claim):
    // a second commander on the same mission exits 3, never racing dispatch.
    // TTL liveness (not PID): the claim subprocess's PID is short-lived, so
    // 24h TTL is the model. Assumption: no single mission run exceeds 24h
    // (each project walk is itself budget-capped); a marathon mission would
    // need a TTL refresh between iterations - conscious gap, not an accident.
    let abi_bin = std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".to_string());
    let fleet_key = format!("fleet:{mission_id}");
    let fleet_holder = format!("megatron-loop:{}", std::process::id());

    let claim_out = abi_cmd(&abi_bin)
        .args([
            "claim",
            "acquire",
            &fleet_key,
            "--holder",
            &fleet_holder,
            "--ttl",
            "24h",
            "--reason",
            "megatron commander singleton",
        ])
        .output();

    match claim_out {
        Ok(o) if !o.status.success() => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            eprintln!(
                "fno-agents loop megatron: another commander is already running \
                 mission {mission_id}: {stderr}"
            );
            return Ok(3);
        }
        Err(e) => {
            // Fail closed (unlike megawalk's best-effort walker claim): the
            // queue verbs shell the SAME fno binary, so a commander that
            // cannot spawn it could never make progress anyway - and a
            // claimless commander racing a healthy one would race wave
            // advancement and completion-record writes (sigma-review).
            eprintln!(
                "fno-agents loop megatron: cannot spawn '{abi_bin}' to acquire the fleet \
                 claim: {e}; refusing to run without the commander singleton"
            );
            return Ok(2);
        }
        Ok(_) => {}
    }

    // RAII: releases on every exit path from here on (early returns, `?`,
    // panics). Explicitly dropped after run_loop to preserve release timing.
    let claim_guard = FleetClaimGuard {
        abi_bin: abi_bin.clone(),
        key: fleet_key,
        holder: fleet_holder,
    };

    // ── SIGINT handler ────────────────────────────────────────────────────────
    install_sigint_handler();

    // ── journal (commander cwd project journal + global mirror) ───────────────
    let abilities_dir = cwd.join(".fno");
    let project_events = abilities_dir.join("events.jsonl");
    let home_dir = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let global_events = home_dir.join(".fno").join("events.jsonl");
    let journal = Journal::new(
        ProjectJournalPath(project_events),
        GlobalJournalPath(global_events),
    );

    // ── header ────────────────────────────────────────────────────────────────
    let binary_name = resolve_driver_binary(dispatcher_name, cli_alias);
    let max_iters = max_iterations.unwrap_or(DEFAULT_MISSION_ITERATIONS);
    println!("fno-agents loop megatron");
    println!("  driver:     megatron (projects walk via --driver megawalk)");
    println!("  dispatcher: {dispatcher_name} (binary: {binary_name})");
    println!("  mission:    {mission_id}");
    println!("  iterations: {max_iters} max");
    println!("  budget:     ${budget_usd} USD per project session");

    // ── queue + dispatcher ────────────────────────────────────────────────────
    let mut queue = MegatronQueue::new(abi_bin.clone(), mission_id.to_string());
    let self_bin = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dispatcher = MegatronDispatcher::new(
        self_bin,
        dispatcher_name.to_string(),
        lib_dir,
        mission_id.to_string(),
        max_turns,
        budget_usd,
        model.map(|s| s.to_string()),
        cli_alias.map(|s| s.to_string()),
        allow_merge,
    );

    // ── budget ────────────────────────────────────────────────────────────────
    let budget = match LoopBudget::new(max_iters) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fno-agents loop megatron: {e}");
            return Ok(2);
        }
    };

    // ── cancel closure ────────────────────────────────────────────────────────
    // Mission-level cancel (`fno megatron cancel`) propagates through the
    // verb seam: it flips status to cancelled, and the next `fno megatron
    // next` returns a terminal pause. The sentinel here covers the local
    // commander process only.
    let cancel_file = cwd.join(".fno").join(".target-cancelled");
    let cancel = move || SIGINT_RECEIVED.load(Ordering::SeqCst) || cancel_file.exists();

    // ── run the loop ──────────────────────────────────────────────────────────
    // A child walk that exits without journaling a termination event is
    // abnormal (the walk has its own internal re-dispatch); cap re-dispatch
    // attempts low so a crash-looping walk parks rather than burning budget.
    const PER_PROJECT_MAX_DISPATCHES: u64 = 3;
    let outcome = match run_loop(
        &mut queue,
        &dispatcher,
        &budget,
        &journal,
        &cancel,
        Some(PER_PROJECT_MAX_DISPATCHES),
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("fno-agents loop megatron: fatal loop error: {e}");
            return Ok(2);
        }
    };

    // Release before the final report so the singleton frees the moment the
    // walk is over (same timing as the previous manual release site).
    drop(claim_guard);

    // ── report + exit-code mapping ────────────────────────────────────────────
    // NoProgress here means a pause policy fired (mission paused / manifest
    // mutated / project failed); map to exit 4 per the megatron CLI contract.
    let exit_code = match outcome.reason {
        TerminationReason::NoProgress => 4,
        ref r => exit_code_for_reason(r),
    };
    println!(
        "megatron: {:?} ({} iterations used, {} project walks closed)",
        outcome.reason,
        outcome.iterations_used,
        outcome.units.len()
    );
    for unit_result in &outcome.units {
        println!(
            "  project {}: {:?} ({:?})",
            unit_result.unit_id, unit_result.evidence.reason, unit_result.close
        );
    }

    Ok(exit_code)
}

/// Default mission-level iteration ceiling when --max-iterations is absent.
/// Each project dispatch consumes one iteration; re-dispatch of a crashed
/// walk consumes more. 50 covers a large mission (waves x projects x retries)
/// while still bounding a runaway commander.
const DEFAULT_MISSION_ITERATIONS: u64 = 50;

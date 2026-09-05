//! What a family-1 worker is actually doing, read once per answer.
//!
//! `fno agents truth` is the shared reader: Python resolves the transcript, so
//! `state` and `observed_model` come from the SAME probe and the Rust list
//! emitter reports the identical reading the Python one does. A Rust port would
//! grow the second transcript reader that constraint exists to prevent.
//!
//! Moved out of `claude_ask` because it answers a different question. That file
//! is the `claude --bg` ask path; this one is "what is this session doing", and
//! its callers (`wait`, `needs`, `king_board`, the daemon's list rows) reach it
//! without going near an ask.
//!
//! Every spawn here goes through [`crate::single_flight`]. The daemon probes
//! every roster row on every sweep, from several processes at once, and the
//! duplicate children were what drove the load average to 319 against a ceiling
//! of 96. The retry stays INSIDE the flight: a crashed batch that retried
//! outside it would leave a joiner waiting on a claim nobody holds, and that
//! joiner would then start a second batch of its own.

use std::time::{Duration, Instant};

use crate::single_flight::{self, FlightKind};

/// One family-1 truth answer: the supervision state, plus the model the worker
/// is ACTUALLY answering as.
///
/// Both come from the SAME probe. `observed_model` is derived by the Python
/// resolver from the worker's own transcript, so the Rust list emitter reports
/// the identical reading the Python one does instead of growing a second
/// transcript reader that could drift from it.
#[derive(Debug, Clone)]
pub struct TruthProbe {
    pub state: String,
    /// The shared reachability verdict (`reachable` / `unreachable` /
    /// `unknown`), derived Python-side with the falsifiers applied. `None` on a
    /// truth build that predates the field.
    ///
    /// Prefer this over mapping [`Self::state`]: `state` is transcript activity
    /// alone, so a session whose process died minutes ago still reads
    /// `working` and renders live. That two-hour blind spot is the whole bug.
    pub reachability: Option<String>,
    /// The evidence [`Self::reachability`] was reached from (`transcript` /
    /// `process-gone` / `pane-gone` / `silent` / `no-evidence`), and how old that
    /// evidence is.
    ///
    /// Carried so the daemon's row can re-emit the whole triple. A verdict
    /// without its basis is the shape this module exists to retire: it renders
    /// as a bare word and a reader cannot tell a positive transcript reading
    /// from a fired falsifier.
    pub basis: Option<String>,
    pub last_activity_age_s: Option<f64>,
    /// The absolute ISO8601 stamp of the newest transcript activity, and the
    /// flattened text of the LAST turn (compact `[tool_use: name]` markers
    /// included, capped at 200 chars Python-side). Derived by the same probe as
    /// the age; `None` when the probe did not answer, which is never the same
    /// claim as "nothing happened".
    pub last_event_at: Option<String>,
    pub last_message: Option<String>,
    pub observed_model: serde_json::Value,
    /// The title the HARNESS carries for this session (claude's Ctrl+R
    /// agent-name record; codex/opencode's index title), read Python-side by
    /// the same probe so the list emitter never grows a second title reader.
    /// `None` = the harness carries none or the probe predates the field:
    /// absence renders as absence, never as the row's label.
    pub harness_title: Option<String>,
}

fn family1_truth_command(handle: &str) -> std::process::Command {
    let mut command = std::process::Command::new("fno");
    command
        .args(["agents", "truth", handle, "--json"])
        .env("FNO_AGENTS_RUNTIME", "python");
    command
}

pub fn family1_truth_probe(handle: &str) -> Option<TruthProbe> {
    // This probe runs once per tracked session, continuously, which makes it
    // the highest-frequency reader of the tree `uv tool install --reinstall`
    // (what `fno doctor update` runs) rewrites in place. A probe landing in that
    // window dies before `cmd_truth` can write anything, and spent a WARN line
    // on a failure that is over by the next sweep.
    //
    // So the crash shape specifically -- and only that shape -- buys one silent
    // retry. A refusal always carries its `{state, reason}` body, so `not-found`
    // and friends still answer on the first attempt and no dead registry row
    // pays for a second process. A genuinely broken probe crashes twice and
    // keeps its warning, which is the point: this tolerates a transient
    // failure, it does not hide a persistent one.
    //
    // Worth naming, because the Python half of this fix is built the other way:
    // that one FALSIFIES (is the module on disk right now?) before retrying,
    // while this one INFERS from the failure shape alone. There is no cheap
    // equivalent question to ask here -- "why did it exit 1" has no answer from
    // out here -- so a permanent fault matching the shape retries every sweep
    // rather than settling. The cost is bounded and visible: one extra
    // fast-failing spawn per affected row, and the second attempt always keeps
    // its WARN, so a stuck probe is loud rather than silent.
    family1_truth_latched(handle, Duration::from_secs(5))
}

/// One `fno agents truth <handle>` in flight per handle, machine-wide.
///
/// The daemon, `wait`, `needs` and `king_board` each probe the same rows from
/// their own processes. Every one of those was a Python cold start of 1 to 2.4
/// seconds, and a slow read made the next one overlap it. The retry rides
/// INSIDE the flight: retrying outside it would leave a joiner waiting on a
/// claim nobody holds, and that joiner would then spawn a second probe.
fn family1_truth_latched(handle: &str, timeout: Duration) -> Option<TruthProbe> {
    let key = single_flight::flight_key(&["agents", "truth", handle, "--json"]);
    let mut own: Option<TruthAttempt> = None;
    let flight = single_flight::run_or_join(&key, latch_ttl(), latch_join_budget(), || {
        let answer = family1_truth_answer(|| family1_truth_command(handle), timeout, handle);
        let shared = shareable(&answer.stdout);
        own = Some(answer);
        shared
    });
    match flight.kind {
        FlightKind::Spawn | FlightKind::Timeout => own.and_then(|a| a.probe),
        FlightKind::Cache | FlightKind::Join => flight
            .stdout
            .as_deref()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
            .as_ref()
            .and_then(parse_truth_payload),
    }
}

/// The stdout worth handing a joiner, or `None`.
///
/// Empty means the run measured nothing (it never spawned, timed out, or its
/// body rode a non-zero exit). Caching that would hand a joiner an answer whose
/// provenance it cannot see, and would hide the failure the per-handle fallback
/// exists to catch.
fn shareable(stdout: &[u8]) -> Option<Vec<u8>> {
    (!stdout.is_empty()).then(|| stdout.to_vec())
}

/// The latch knobs, resolved from `config.agents.*` against the current dir.
fn latch_ttl() -> Duration {
    crate::agents_config::single_flight_ttl(&current_dir())
}

fn latch_join_budget() -> Duration {
    crate::agents_config::single_flight_join_budget(&current_dir())
}

fn current_dir() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

/// Probe family-1 truth within a caller-supplied total budget.
///
/// The retrying reader may make two attempts when the first process crashes.
/// Split the budget across those attempts so a waiter's outer deadline remains
/// authoritative even when the truth command is unavailable.
pub fn family1_truth_probe_with_timeout(handle: &str, timeout: Duration) -> Option<TruthProbe> {
    family1_truth_latched(handle, timeout / 2)
}

/// [`family1_truth_probe`] with the command built per attempt, so a test can
/// count the attempts a given failure shape actually costs. A `Command` cannot
/// be reused after a spawn, which is why this takes a factory.
///
/// Returns the whole attempt, not just the probe: the latch needs the bytes
/// the answering run produced so a joiner is handed the same ones.
fn family1_truth_answer(
    mut command_for_attempt: impl FnMut() -> std::process::Command,
    timeout: Duration,
    handle: &str,
) -> TruthAttempt {
    let first = family1_truth_attempt(command_for_attempt(), timeout, handle, false);
    if !first.crashed {
        return first;
    }
    family1_truth_attempt(command_for_attempt(), timeout, handle, true)
}

/// The transcript state, LOWERED to `"unreachable"` when the shared verdict
/// affirmatively falsified the row.
///
/// `resume` and the attach pointer read this and match on `working | watching |
/// your-move` to decide "is live". Transcript state alone cannot see a dead
/// process, so a session whose process died forty minutes ago still reads
/// `working` and resume wakes/attaches to nothing. Passing the falsified case through
/// as a state neither arm matches drops both callers into their inconclusive
/// branch (refuse / no pointer), which is the safe answer.
///
/// The override is MONOTONE, matching `fno.agents.reachability`: it only ever
/// lowers a would-be-live reading, never raises `done`/`stalled` toward live and
/// never invents a verdict when the probe did not carry one.
pub fn family1_truth_state(handle: &str) -> Option<String> {
    let probe = family1_truth_probe(handle)?;
    Some(lower_state_with_verdict(&probe.state, probe.reachability.as_deref()).to_string())
}

fn lower_state_with_verdict<'a>(state: &'a str, reachability: Option<&str>) -> &'a str {
    if reachability == Some("unreachable") && matches!(state, "working" | "watching" | "your-move")
    {
        return "unreachable";
    }
    state
}

/// Variant of [`family1_truth_state`] for the resume smart verb. The shared
/// lowering renders a gone process as `"unreachable"` (matches no arm, so resume
/// refuses it as inconclusive - the safe answer for the mail path). Resume wants
/// the opposite for a worker the verdict confirms is DEAD: relaunch it. So an
/// `unreachable` verdict whose `basis` is the process being gone (`pane-gone` /
/// `process-gone`) lowers to `"stalled"` and resume's `done | stalled` relaunch
/// arm fires. A `silent` / `no-evidence` unreachable stays `"unreachable"`
/// (inconclusive): the process may still be alive, and relaunching would open a
/// second writer on one transcript.
///
/// Separate from [`family1_truth_state`] because routing a gone worker to
/// relaunch is a resume-specific call; the mail path's orphan-reason reader
/// (`family1_orphan_reason`) shares the probe but must not change with it.
pub fn family1_truth_state_for_resume(handle: &str) -> Option<String> {
    let probe = family1_truth_probe(handle)?;
    Some(
        lower_state_for_resume(
            &probe.state,
            probe.reachability.as_deref(),
            probe.basis.as_deref(),
        )
        .to_string(),
    )
}

/// [`lower_state_with_verdict`] plus one rule: a live-seeming state the verdict
/// falsified with evidence the PROCESS is gone (not merely silent) is dead, so
/// resume relaunches it. Falls through to the shared lowering for every other
/// case, so reachable-working stays live and a pre-verdict probe build is
/// unchanged.
fn lower_state_for_resume<'a>(
    state: &'a str,
    reachability: Option<&str>,
    basis: Option<&str>,
) -> &'a str {
    if reachability == Some("unreachable")
        && matches!(basis, Some("pane-gone") | Some("process-gone"))
        && matches!(state, "working" | "watching" | "your-move")
    {
        return "stalled";
    }
    lower_state_with_verdict(state, reachability)
}

/// Diagnostic for a failed family-1 truth probe. truth writes its refusal
/// JSON ({state,reason}) to stdout on a non-zero exit, so the reason is read
/// off stdout, falling back to stderr only when stdout is not the expected JSON.
fn family1_truth_failure_detail(stdout: &[u8], stderr: &str) -> String {
    let reason = serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .and_then(|value| value.get("reason")?.as_str().map(str::to_owned));
    reason.unwrap_or_else(|| stderr.trim().to_owned())
}

/// truth's answer for a handle it has no transcript for. Not a malfunction:
/// family-1 is CLAUDE transcript truth, so an opencode/codex handle can never
/// resolve, and a reaped claude session no longer does. The volume caller is
/// `daemon::handle_list`, which probes EVERY registry row on every
/// `fno agents list` with no gate at all - one dead row there produced a warn
/// line per sweep and buried the other failures below that DO mean something is
/// broken. A resolver crash is deliberately NOT this string
/// (`session_truth.py` reports `resolver-error`) so it survives the filter.
///
/// The tradeoff this accepts: on the `resume`/`attach` paths in `client_verbs`
/// the probe is gated on a dead socket rather than a missing session, so a
/// not-found there can also mean the registry and the transcript store
/// disagree. That case loses its warn line. It is diagnostics-only - the
/// verdict is `None` either way - and the gate would have to distinguish
/// "locate_session missed" from "found but socket dead" to say more.
const TRUTH_NOT_FOUND: &str = "not-found";

/// Whether a non-zero truth exit is worth a warning. Routine "gone" is not;
/// anything else is a probe or transcript malfunction the operator needs.
fn truth_failure_is_routine(detail: &str) -> bool {
    detail.trim() == TRUTH_NOT_FOUND
}

/// One probe run's outcome, split just far enough to answer "is this worth a
/// single silent retry?".
struct TruthAttempt {
    probe: Option<TruthProbe>,
    /// The child's stdout, kept so [`single_flight`] can hand the SAME bytes to
    /// a caller that joined this flight instead of starting its own.
    stdout: Vec<u8>,
    /// The probe never got far enough to measure anything: it failed to spawn
    /// at all, or exited non-zero with NO parseable body on stdout, meaning the
    /// process died before `cmd_truth` wrote its verdict. Both are what a
    /// `uv tool install --reinstall` looks like from out here - it replaces the
    /// console script and the package tree together. A refusal writes
    /// `{state, reason}` first, so `not-found` never sets this.
    crashed: bool,
}

impl TruthAttempt {
    /// An answer, whatever it is, that a second run could only repeat.
    fn answered(probe: Option<TruthProbe>, stdout: Vec<u8>) -> Self {
        Self {
            probe,
            stdout,
            crashed: false,
        }
    }
}

/// The outcome of running one truth subprocess to a deadline, before any
/// decoding. Shared by the single-handle attempt and the batch one so the
/// spawn, the poll loop, and the WARN wording have exactly one implementation.
#[derive(Debug)]
enum BoundedRun {
    /// Never started. `uv tool install --reinstall` replaces the `fno` console
    /// script itself, not just the package tree, so that window shows up out
    /// here as a bare ENOENT on spawn - the same "measured nothing" shape as an
    /// import crash, and it buys the same single retry.
    SpawnFailed,
    /// Started, produced nothing usable (timed out, or the wait/collect
    /// failed). Already warned; a second run would only repeat it.
    NoOutput,
    Output(std::process::Output),
}

fn run_truth_subprocess(
    mut command: std::process::Command,
    timeout: Duration,
    label: &str,
    warn_on_spawn_failure: bool,
) -> BoundedRun {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if warn_on_spawn_failure {
                eprintln!("WARN: family-1 truth probe for {label} failed to start: {error}");
            }
            return BoundedRun::SpawnFailed;
        }
    };
    // Drain both pipes for the WHOLE run, on their own threads. Waiting for
    // exit before reading deadlocks against a child that fills the pipe
    // buffer: it blocks in write() and can never reach exit, so `try_wait`
    // never reports one and the deadline kills a process that was healthy.
    // Batching is what put this in reach. One handle's answer is ~535 bytes
    // measured against the live roster, so a single probe had a hundredfold
    // margin under the 64 KiB buffer, while N handles cross it near 120 rows
    // -- and an 88-row roster is already on the record in `daemon.rs`, with
    // `last_message` free text able to inflate any one entry.
    //
    // The readers hand their bytes back over a channel rather than a join
    // handle, because a JOIN here is its own unbounded wait: a GRANDCHILD
    // inherits these pipe fds, so `read_to_end` does not end when the child
    // does. `sh -c "sleep 5"` under a 50ms bound proves it -- joining waited
    // the full five seconds and blew the bound this function exists to keep.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = out_pipe.as_mut() {
            let _ = std::io::Read::read_to_end(pipe, &mut buf);
        }
        let _ = out_tx.send(buf);
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = err_pipe.as_mut() {
            let _ = std::io::Read::read_to_end(pipe, &mut buf);
        }
        let _ = err_tx.send(buf);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("WARN: family-1 truth probe for {label} timed out");
                break None;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("WARN: family-1 truth probe for {label} wait failed: {error}");
                break None;
            }
        }
    };
    // A run that blew its bound is over NOW. Waiting on the readers here is
    // what broke the bound in the first place, and there is no answer to
    // collect anyway; the threads end when the fds finally close.
    let Some(status) = status else {
        return BoundedRun::NoOutput;
    };
    // The child exited on its own, so its bytes are already written and the
    // readers are done or nearly so. Still bounded, for the same grandchild
    // reason: an inherited fd can hold these pipes open past the child's own
    // exit, and this function may never wait without a limit.
    const DRAIN_GRACE: Duration = Duration::from_secs(2);
    let stdout = match out_rx.recv_timeout(DRAIN_GRACE) {
        Ok(buf) => buf,
        Err(_) => {
            eprintln!("WARN: family-1 truth probe for {label} left its output undrained");
            return BoundedRun::NoOutput;
        }
    };
    // stderr is diagnostic only, so a slow one degrades to empty rather than
    // discarding an answer stdout already delivered.
    let stderr = err_rx.recv_timeout(DRAIN_GRACE).unwrap_or_default();
    BoundedRun::Output(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn family1_truth_attempt(
    command: std::process::Command,
    timeout: Duration,
    handle: &str,
    warn_on_crash: bool,
) -> TruthAttempt {
    let output = match run_truth_subprocess(command, timeout, handle, warn_on_crash) {
        BoundedRun::SpawnFailed => {
            return TruthAttempt {
                probe: None,
                stdout: Vec::new(),
                crashed: true,
            }
        }
        BoundedRun::NoOutput => return TruthAttempt::answered(None, Vec::new()),
        BoundedRun::Output(output) => output,
    };
    let parsed = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
    if !output.status.success() {
        // No parseable body at all means the process never reached the code
        // that writes one, so this run measured nothing about the session -
        // unlike a refusal, which is a real answer. Only that shape is worth a
        // retry, and only that shape holds its WARN back for it.
        let crashed = parsed.is_none();
        // The warn-on-malfunction decision stays exactly as before, keyed off
        // `reason` regardless of what follows: a resolver crash still needs
        // its WARN even though its body is about to be salvaged below.
        let detail =
            family1_truth_failure_detail(&output.stdout, &String::from_utf8_lossy(&output.stderr));
        if !truth_failure_is_routine(&detail) && (warn_on_crash || !crashed) {
            eprintln!(
                "WARN: family-1 truth probe for {handle} exited {}: {}",
                output.status, detail
            );
        }
        // truth writes its full, already-computed verdict to stdout BEFORE
        // deciding the exit code (`cmd_truth` in cli.py), so exit 13
        // (unresolvable handle) still carries a real `{state, reachability,
        // basis, ...}` body - typically `state: "unknown"` with a genuine
        // `reachability`/`basis` pair, not a bare refusal. Discarding it here
        // used to turn a correct "unmeasured" answer into `None`, which the
        // next reader is free to interpret as death (x-9de7). Salvage it, but
        // only when it is NOT a live-seeming state: a failed probe run has no
        // standing to assert liveness, so this is monotone-lowering only,
        // exactly like `lower_state_with_verdict` above - it can report
        // done/stalled/unknown, never invent "still working".
        let probe = parsed
            .as_ref()
            .and_then(parse_truth_payload)
            .filter(|p| matches!(p.state.as_str(), "done" | "stalled" | "unknown"));
        // A salvaged body is a real answer for THIS caller and a bad one to
        // share: it rode a non-zero exit, and a joiner reading it back from the
        // record could not tell that. Empty stdout is what the latch treats as
        // nothing worth caching.
        return TruthAttempt {
            probe,
            stdout: Vec::new(),
            crashed,
        };
    }
    let probe = match parsed.as_ref().and_then(parse_truth_payload) {
        Some(probe) => Some(probe),
        None => {
            eprintln!("WARN: family-1 truth probe for {handle} returned malformed output");
            None
        }
    };
    TruthAttempt::answered(probe, output.stdout)
}

/// Build a [`TruthProbe`] from a parsed truth JSON body and its already-read
/// `state`. Shared by the success path and the x-9de7 non-zero-exit salvage
/// path so the field extraction has exactly one implementation.
fn build_truth_probe(parsed: Option<&serde_json::Value>, state: &str) -> TruthProbe {
    TruthProbe {
        state: state.to_owned(),
        // The shared reachability verdict, derived Python-side with the
        // falsifiers applied. Absent on a truth build that predates the
        // field, and callers then fall back to mapping `state` — which
        // is transcript activity only, so it cannot see a dead process.
        reachability: parsed
            .and_then(|value| value.get("reachability")?.as_str().map(str::to_owned)),
        basis: parsed.and_then(|value| value.get("basis")?.as_str().map(str::to_owned)),
        last_activity_age_s: parsed.and_then(|value| value.get("last_activity_age_s")?.as_f64()),
        last_event_at: parsed
            .and_then(|value| value.get("last_event_at")?.as_str().map(str::to_owned)),
        last_message: parsed
            .and_then(|value| value.get("last_message")?.as_str().map(str::to_owned)),
        // Absent on a truth build that predates the field: null rather
        // than a fabricated variant, so a stale `fno` reads as "this
        // probe did not answer" instead of asserting no transcript.
        observed_model: parsed
            .and_then(|value| value.get("observed_model").cloned())
            .unwrap_or(serde_json::Value::Null),
        harness_title: parsed
            .and_then(|value| value.get("harness_title")?.as_str().map(str::to_owned)),
    }
}

/// Decode ONE truth payload into a [`TruthProbe`]: the `{state, reachability,
/// basis, ...}` object `_truth_payload` writes, whether it arrived alone or as
/// one value of a `--handles` batch.
///
/// `None` when `state` is absent or is not one of the six the verb emits, which
/// is the malformed-output case both entry points already refuse.
///
/// The single decoder is the point. Two of them is how a batch reading and a
/// single reading of the same transcript start disagreeing about the same row.
fn parse_truth_payload(value: &serde_json::Value) -> Option<TruthProbe> {
    let state = value.get("state")?.as_str()?;
    match state {
        "done" | "watching" | "your-move" | "working" | "stalled" | "unknown" => {
            Some(build_truth_probe(Some(value), state))
        }
        _ => None,
    }
}

/// The batch spelling of [`family1_truth_command`]: N handles, ONE interpreter.
///
/// Batch mode always exits 0 Python-side (see `cmd_truth`'s `--handles` help),
/// so the exit code carries no per-handle verdict here and the keyed object is
/// the whole answer. A handle the batch could not resolve is present with
/// `state: "unknown"` and its own `reason`.
fn family1_truth_batch_command(handles: &[String]) -> std::process::Command {
    let mut command = std::process::Command::new("fno");
    command
        .args(["agents", "truth", "--handles", &handles.join(","), "--json"])
        .env("FNO_AGENTS_RUNTIME", "python");
    command
}

/// [`family1_truth_probe`] for many handles at once: one child, one cold start.
///
/// The daemon probes every roster row on every sweep. Measured on the
/// operator's box: 0.83 ms of real work per handle behind 780 ms of Python
/// interpreter startup, a 940-to-1 ratio, so 24 rows cost 18.7 s as
/// subprocesses and 19.9 ms in one process.
///
/// This is a BATCH and not a Rust reimplementation on purpose. [`TruthProbe`]'s
/// own doc states the constraint: `state` and `observed_model` come from the
/// SAME reader, so the Rust list emitter reports the identical reading the
/// Python one does. A Rust port would grow the second transcript reader that
/// constraint exists to prevent; batching removes the cost and keeps one
/// reader.
///
/// Bounded by [`family1_truth_batch_timeout`], and buying the same
/// one-silent-retry-on-crash the single probe documents. The retry now re-costs
/// one cold start for the whole batch rather than one per row. An EMPTY slice
/// returns an empty map without spawning anything.
///
/// A batch that FAILS after its retry falls back to one probe per handle. That
/// costs exactly what this function exists to delete, and it is still right,
/// because the alternative is a total outage of the truth column. The trigger
/// is not hypothetical: an `fno` on PATH that predates `--handles` exits 2 on
/// the unknown option, and every worktree carries its own binary, so a
/// half-deployed tree is the ORDINARY state right after this lands. Without the
/// fallback every list row renders null reachability and `no-transcript`, the
/// dormant gate can never reach the positive `done` reading an eviction needs,
/// and `fno agents needs` reports no refused workers - all three at once, until
/// someone runs `fno update`.
///
/// The fallback is keyed on a FAILURE, never on an empty answer. A batch that
/// ran and legitimately resolved nothing returns an empty map and spends no
/// second round; only a batch that never answered escalates. Reading "no
/// answers" as "the batch broke" would re-spawn N processes every sweep over a
/// roster where nothing resolves.
pub fn family1_truth_probe_many(
    handles: &[String],
) -> std::collections::HashMap<String, TruthProbe> {
    // `--handles` is comma-separated, so a handle CARRYING a comma cannot be
    // put on the wire: the reader would split it into two handles that match
    // no row, and that row would go unanswered on every list, silently and
    // forever. It takes the single-handle path instead, where it rides its own
    // argv element and no splitting happens. The canonical namer sanitizes to
    // [a-z0-9-] and cannot produce one, but that guard sits on ONE of the
    // paths that write a row's name, and this seam is where the assumption
    // actually lives.
    let (batchable, unrepresentable): (Vec<String>, Vec<String>) =
        handles.iter().cloned().partition(|h| !h.contains(','));
    let mut probes = family1_truth_probe_batchable(&batchable);
    for handle in unrepresentable {
        if let Some(probe) = family1_truth_probe(&handle) {
            probes.insert(handle, probe);
        }
    }
    probes
}

fn family1_truth_probe_batchable(
    handles: &[String],
) -> std::collections::HashMap<String, TruthProbe> {
    match family1_truth_batch_latched(handles) {
        Some(probes) => probes,
        None => {
            eprintln!(
                "WARN: family-1 truth batch of {} handles failed twice; \
                 falling back to one probe per handle. Either this `fno` \
                 predates `--handles` (run `fno update`), or no `fno` is on \
                 PATH right now (a `uv tool install --reinstall` window).",
                handles.len()
            );
            handles
                .iter()
                .filter_map(|handle| Some((handle.clone(), family1_truth_probe(handle)?)))
                .collect()
        }
    }
}

/// One `fno agents truth --handles` in flight per handle SET, machine-wide.
///
/// This is the measured fan-out: seven concurrent children from five parents
/// inside fifteen seconds, several carrying the same list. The key normalizes
/// the list, so `a,b` and `b,a` join one flight while two different lists stay
/// two. The retry rides inside the flight, so a joiner receives the retried
/// answer rather than starting a second batch.
fn family1_truth_batch_latched(
    handles: &[String],
) -> Option<std::collections::HashMap<String, TruthProbe>> {
    if handles.is_empty() {
        return Some(std::collections::HashMap::new());
    }
    let timeout = family1_truth_batch_timeout(handles.len());
    let key = single_flight::flight_key(&["agents", "truth", "--handles", &handles.join(",")]);
    let mut own: Option<Option<TruthBatchAttempt>> = None;
    let flight = single_flight::run_or_join(&key, latch_ttl(), latch_join_budget(), || {
        let answer = family1_truth_batch_answer(handles, family1_truth_batch_command, timeout);
        let shared = answer.as_ref().and_then(|a| shareable(&a.stdout));
        own = Some(answer);
        shared
    });
    match flight.kind {
        FlightKind::Spawn | FlightKind::Timeout => own.flatten().map(|a| a.probes),
        FlightKind::Cache | FlightKind::Join => flight.stdout.as_deref().map(decode_truth_batch),
    }
}

/// The batch's wall-clock bound, scaled to the work asked for.
///
/// The single probe's flat 5 s is a budget for ONE handle. Handing a batch of N
/// the same budget is the defect this function exists to prevent: measured
/// against the live roster, one handle's resolve costs 90-160 ms of real work
/// (a registry read plus a transcript-store walk), so twelve real handles took
/// 5.7 s and a flat 5 s killed the child mid-flight. The whole page then came
/// back EMPTY - every row rendering null reachability and `no-transcript` - and
/// the dormant gate stopped evicting, because its verdict needs a positive
/// `done` reading it could no longer get. A batch that times out is strictly
/// worse than the per-row probes it replaced, which each had their own 5 s.
///
/// 300 ms per handle is roughly double the measured cost, so an ordinary sweep
/// finishes well inside it. The ceiling keeps one pathological transcript from
/// wedging a sweep for minutes; the daemon runs this in `spawn_blocking`, off
/// the select arm, so a long batch cannot starve `accept()` the way the inline
/// probes once did.
fn family1_truth_batch_timeout(handles: usize) -> Duration {
    const BASE: Duration = Duration::from_secs(5);
    const PER_HANDLE: Duration = Duration::from_millis(300);
    const CEILING: Duration = Duration::from_secs(60);
    std::cmp::min(BASE + PER_HANDLE * handles as u32, CEILING)
}

/// [`family1_truth_probe_many`] with the command built per attempt, so a test
/// can count the spawns a given failure shape actually costs - and assert that
/// an empty slice costs none. Mirrors [`family1_truth_answer`].
/// `None` means the batch never answered, even after its retry - the signal
/// [`family1_truth_probe_many`] falls back on. `Some(map)` is a real answer,
/// including `Some(empty)` for a batch that ran and resolved nothing.
fn family1_truth_batch_answer(
    handles: &[String],
    mut command_for_attempt: impl FnMut(&[String]) -> std::process::Command,
    timeout: Duration,
) -> Option<TruthBatchAttempt> {
    if handles.is_empty() {
        return Some(TruthBatchAttempt {
            probes: std::collections::HashMap::new(),
            stdout: Vec::new(),
            crashed: false,
        });
    }
    // Warnings name the batch, not a row: no single handle owns the failure.
    let label = format!("a batch of {} handles", handles.len());
    let first = family1_truth_batch_attempt(command_for_attempt(handles), timeout, &label, false);
    if !first.crashed {
        return Some(first);
    }
    let second = family1_truth_batch_attempt(command_for_attempt(handles), timeout, &label, true);
    if second.crashed {
        return None;
    }
    Some(second)
}

/// One batch run's outcome. `crashed` carries exactly the meaning
/// [`TruthAttempt::crashed`] does: the run measured nothing, so a second is
/// worth one try.
struct TruthBatchAttempt {
    probes: std::collections::HashMap<String, TruthProbe>,
    /// The child's stdout, kept so a joiner is handed the SAME bytes.
    stdout: Vec<u8>,
    crashed: bool,
}

/// Decode a `--handles` body into per-handle probes.
///
/// The ONE decoder for a batch answer, whether the bytes came from this
/// process's child or from the record another process wrote. Two of them is how
/// a joined reading and a spawned reading of the same transcript start
/// disagreeing about the same row.
fn decode_truth_batch(stdout: &[u8]) -> std::collections::HashMap<String, TruthProbe> {
    serde_json::from_slice::<serde_json::Value>(stdout)
        .ok()
        .as_ref()
        .and_then(|value| value.as_object().cloned())
        .map(|object| {
            object
                .iter()
                .filter_map(|(handle, value)| Some((handle.clone(), parse_truth_payload(value)?)))
                .collect()
        })
        .unwrap_or_default()
}

fn family1_truth_batch_attempt(
    command: std::process::Command,
    timeout: Duration,
    label: &str,
    warn_on_crash: bool,
) -> TruthBatchAttempt {
    let empty = || std::collections::HashMap::new();
    let output = match run_truth_subprocess(command, timeout, label, warn_on_crash) {
        BoundedRun::SpawnFailed => {
            return TruthBatchAttempt {
                probes: empty(),
                stdout: Vec::new(),
                crashed: true,
            }
        }
        // A timeout is an ANSWER, not a crash, so it buys no retry - the same
        // rule the single probe follows. Deliberate: with a bound that scales
        // to the handle count, a timeout means a genuine malfunction rather
        // than an underfunded batch, and retrying would double a wait already
        // measured in tens of seconds. The bound is the fix for a slow batch;
        // a retry never was.
        //
        // This one flag also withholds the per-handle FALLBACK, which is a
        // second decision and is meant here too. A batch slow enough to blow a
        // bound that already scales at 300ms a handle will not be rescued by N
        // probes each carrying its own 5s: that trades one long wait for a
        // longer one, on a box already struggling. The outage a fallback
        // exists to prevent is a batch that CANNOT answer, not one that is
        // merely slow. A timeout nulls this page and the next sweep tries
        // again in seconds.
        BoundedRun::NoOutput => {
            return TruthBatchAttempt {
                probes: empty(),
                stdout: Vec::new(),
                crashed: false,
            }
        }
        BoundedRun::Output(output) => output,
    };
    let parsed = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
    match parsed.as_ref().and_then(|value| value.as_object()) {
        // The keyed object IS the answer. The exit code is deliberately not
        // consulted: batch mode always exits 0, so it carries nothing a reader
        // could act on, and every per-handle verdict rides its own entry.
        Some(_) => TruthBatchAttempt {
            probes: decode_truth_batch(&output.stdout),
            stdout: output.stdout.clone(),
            crashed: false,
        },
        // No keyed object and a non-zero exit: the process died before writing
        // one, or this `fno` predates `--handles` and refused the usage. Both
        // measured nothing, so both buy the one retry.
        None if !output.status.success() => {
            let detail = family1_truth_failure_detail(
                &output.stdout,
                &String::from_utf8_lossy(&output.stderr),
            );
            if warn_on_crash && !truth_failure_is_routine(&detail) {
                eprintln!(
                    "WARN: family-1 truth probe for {label} exited {}: {}",
                    output.status, detail
                );
            }
            TruthBatchAttempt {
                probes: empty(),
                stdout: Vec::new(),
                crashed: true,
            }
        }
        // Exited clean and wrote something that is not a keyed object. A real
        // answer, just an unusable one; a retry would repeat it.
        None => {
            eprintln!("WARN: family-1 truth probe for {label} returned malformed output");
            TruthBatchAttempt {
                probes: empty(),
                stdout: Vec::new(),
                crashed: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shell command, built fresh per attempt so the retry path can spawn it
    /// twice. `-c` body only; the probe supplies nothing else.
    fn sh(script: &'static str) -> std::process::Command {
        let mut command = std::process::Command::new("sh");
        command.args(["-c", script]);
        command
    }

    /// `resume` matches on the STATE, so the verdict has to reach it through
    /// that channel or the falsifier is decorative on this path: a session whose
    /// process died forty minutes ago still reads `working` and resume prints
    /// "is live" and wakes/attaches at nothing.
    #[test]
    fn an_unreachable_verdict_lowers_a_would_be_live_state() {
        assert_eq!(
            lower_state_with_verdict("working", Some("unreachable")),
            "unreachable"
        );
        // Monotone: a verdict never raises, and never rewrites a terminal state
        // (resume's relaunch arm keys on `done`/`stalled` and must keep working).
        assert_eq!(
            lower_state_with_verdict("done", Some("unreachable")),
            "done"
        );
        assert_eq!(
            lower_state_with_verdict("stalled", Some("unknown")),
            "stalled"
        );
        // No verdict on the wire (a truth build that predates the field) leaves
        // the pre-existing mapping exactly as it was.
        assert_eq!(lower_state_with_verdict("working", None), "working");
        assert_eq!(
            lower_state_with_verdict("working", Some("reachable")),
            "working"
        );
    }

    #[test]
    fn resume_lowering_treats_a_gone_process_as_dead() {
        // x-b84f: the resume variant lowers a live-seeming state the verdict
        // falsified with PROCESS-gone evidence to "stalled", so the relaunch arm
        // fires for a pane-gone worker instead of the inconclusive refusal.
        assert_eq!(
            lower_state_for_resume("working", Some("unreachable"), Some("pane-gone")),
            "stalled"
        );
        assert_eq!(
            lower_state_for_resume("working", Some("unreachable"), Some("process-gone")),
            "stalled"
        );
        // A silent / no-evidence unreachable is NOT affirmatively dead: the
        // process may still be alive, so it stays "unreachable" (resume's
        // inconclusive refusal, never a relaunch that would double-write).
        assert_eq!(
            lower_state_for_resume("working", Some("unreachable"), Some("silent")),
            "unreachable"
        );
        assert_eq!(
            lower_state_for_resume("working", Some("unreachable"), None),
            "unreachable"
        );
        // Monotone: a terminal state is never rewritten, and the shared lowering
        // still owns the reachable / no-verdict cases unchanged.
        assert_eq!(
            lower_state_for_resume("done", Some("unreachable"), Some("pane-gone")),
            "done"
        );
        assert_eq!(lower_state_for_resume("working", None, None), "working");
        assert_eq!(
            lower_state_for_resume("working", Some("reachable"), Some("transcript")),
            "working"
        );
    }

    // A spawn must NEVER hand bg_create a `None` timeout: the create wait then
    // falls into the unbounded `rx.recv()` arm and a `claude --bg` that holds
    // its inherited stdout/stderr pipe fds open hangs the dispatcher forever
    // (the motivating incident). spawn_create_timeout guarantees a bound.
    /// A single probe attempt with warnings on, which is what every assertion
    /// below is about. The retry that production wraps around this is not
    /// hidden by it: it lives in `family1_truth_answer` and has its own
    /// attempt-counting tests further down.
    fn family1_truth_probe_with_command(
        command: std::process::Command,
        timeout: Duration,
        handle: &str,
    ) -> Option<TruthProbe> {
        family1_truth_attempt(command, timeout, handle, true).probe
    }

    #[test]
    fn crashing_truth_probe_is_retried_exactly_once() {
        // The reinstall window: the process dies before `cmd_truth` writes a
        // body, so the run measured nothing and is worth one more try. The
        // second attempt answering is what production needs; the WARN the first
        // attempt withheld belongs to a failure that no longer exists.
        let attempts = std::cell::Cell::new(0);
        let probe = family1_truth_answer(
            || {
                attempts.set(attempts.get() + 1);
                match attempts.get() {
                    1 => sh("echo 'ModuleNotFoundError: fno.agents.session_truth' >&2; exit 1"),
                    _ => sh("printf '{\"state\":\"working\"}'"),
                }
            },
            Duration::from_secs(5),
            "h1",
        ).probe;
        assert_eq!(attempts.get(), 2, "a crash must buy exactly one retry");
        assert_eq!(probe.expect("retry answers").state, "working");
    }

    #[test]
    fn crashing_twice_still_answers_none_and_stops() {
        // Tolerating a transient failure must not become tolerating a broken
        // one: two attempts, then the answer stands.
        let attempts = std::cell::Cell::new(0);
        let probe = family1_truth_answer(
            || {
                attempts.set(attempts.get() + 1);
                sh("echo boom >&2; exit 1")
            },
            Duration::from_secs(5),
            "h1",
        ).probe;
        assert_eq!(attempts.get(), 2, "never more than one retry");
        assert!(probe.is_none());
    }

    #[test]
    fn a_probe_that_cannot_spawn_is_retried_too() {
        // The reinstall replaces the `fno` console script as well as the tree
        // behind it, so the window is just as likely to show up as ENOENT on
        // spawn as it is as an import crash. Guarding only the second reaches
        // one of two shapes.
        let attempts = std::cell::Cell::new(0);
        let probe = family1_truth_answer(
            || {
                attempts.set(attempts.get() + 1);
                match attempts.get() {
                    1 => std::process::Command::new("fno-no-such-binary-reinstall-window"),
                    _ => sh("printf '{\"state\":\"working\"}'"),
                }
            },
            Duration::from_secs(5),
            "h1",
        ).probe;
        assert_eq!(attempts.get(), 2, "a failed spawn must buy one retry");
        assert_eq!(probe.expect("retry answers").state, "working");
    }

    #[test]
    fn a_refusal_with_a_body_is_never_retried() {
        // `not-found` is the volume case - `handle_list` probes every registry
        // row - and it is a real answer, not a crash. Retrying it would double
        // the process count of every sweep for nothing.
        let attempts = std::cell::Cell::new(0);
        let probe = family1_truth_answer(
            || {
                attempts.set(attempts.get() + 1);
                sh("printf '{\"state\":\"unknown\",\"reason\":\"not-found\"}'; exit 13")
            },
            Duration::from_secs(5),
            "h1",
        ).probe;
        assert_eq!(attempts.get(), 1, "a refusal carries a body: no retry");
        assert_eq!(probe.expect("salvaged body").state, "unknown");
    }

    /// The state half of a probe, for the tests that only pin the state enum.
    fn probe_state(
        command: std::process::Command,
        timeout: Duration,
        handle: &str,
    ) -> Option<String> {
        // Lowered by the verdict, exactly as `family1_truth_state` does. A raw
        // `.map(|p| p.state)` here would exercise a path production no longer
        // takes, and every state assertion below would pin the pre-verdict
        // reading forever.
        family1_truth_probe_with_command(command, timeout, handle)
            .map(|p| lower_state_with_verdict(&p.state, p.reachability.as_deref()).to_string())
    }

    #[test]
    fn family1_truth_probe_carries_the_observed_model() {
        // One probe answers both halves: the list emitter must not run a second
        // shellout, and must not grow a second transcript reader that could
        // report a different model than the truth verb does for the same worker.
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "printf '{\"state\":\"working\",\"observed_model\":\
             {\"kind\":\"observed\",\"model\":\"glm-5.2\",\"samples\":300}}'",
        ]);
        let probe = family1_truth_probe_with_command(cmd, Duration::from_secs(1), "h1")
            .expect("probe answers");
        assert_eq!(probe.state, "working");
        assert_eq!(probe.observed_model["model"], "glm-5.2");

        // A truth build that predates the field yields null, never a fabricated
        // variant: "the probe did not answer" is not "there is no transcript".
        let mut old = std::process::Command::new("sh");
        old.args(["-c", "printf '{\"state\":\"working\"}'"]);
        let stale = family1_truth_probe_with_command(old, Duration::from_secs(1), "h1")
            .expect("probe answers");
        assert!(stale.observed_model.is_null());
    }

    /// The whole reachability triple comes off the wire, not just the verdict.
    ///
    /// The daemon row re-emits all three, and a parser that lifted only the
    /// verdict would leave `basis` and `last_activity_age_s` permanently null
    /// there -- a key that is always null being the same lie as a missing one.
    /// `last_activity_age_s` crosses as a JSON integer from Python, so the
    /// float parse has to accept one.
    #[test]
    fn family1_truth_probe_carries_the_reachability_triple() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "printf '{\"state\":\"working\",\"reachability\":\"unreachable\",\
             \"basis\":\"pane-gone\",\"last_activity_age_s\":143255}'",
        ]);
        let probe = family1_truth_probe_with_command(cmd, Duration::from_secs(1), "h1")
            .expect("probe answers");
        assert_eq!(probe.reachability.as_deref(), Some("unreachable"));
        assert_eq!(probe.basis.as_deref(), Some("pane-gone"));
        assert_eq!(probe.last_activity_age_s, Some(143255.0));

        // A truth build too old to emit them reads as absent, never as a
        // fabricated `no-evidence` -- which is a VERDICT, not a missing field.
        let mut old = std::process::Command::new("sh");
        old.args(["-c", "printf '{\"state\":\"working\"}'"]);
        let stale = family1_truth_probe_with_command(old, Duration::from_secs(1), "h1")
            .expect("probe answers");
        assert!(stale.reachability.is_none());
        assert!(stale.basis.is_none());
        assert!(stale.last_activity_age_s.is_none());
    }

    /// The absolute stamp and the LAST-turn text cross the wire beside the
    /// triple: the list row's EVENT AGE column and LAST MESSAGE text
    /// come from this probe, and a parser that dropped either would leave it
    /// permanently null on the daemon path - a key that is always null being
    /// the same lie as a missing one.
    #[test]
    fn family1_truth_probe_carries_the_last_event_pair() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "printf '{\"state\":\"working\",\"last_event_at\":\
             \"2026-08-15T17:00:00+00:00\",\"last_message\":\
             \"Still growing (101 lines)\"}'",
        ]);
        let probe = family1_truth_probe_with_command(cmd, Duration::from_secs(1), "h1")
            .expect("probe answers");
        assert_eq!(
            probe.last_event_at.as_deref(),
            Some("2026-08-15T17:00:00+00:00")
        );
        assert_eq!(
            probe.last_message.as_deref(),
            Some("Still growing (101 lines)")
        );

        // A truth build that predates the pair reads as absent - never as a
        // fabricated fresh stamp or empty message.
        let mut old = std::process::Command::new("sh");
        old.args(["-c", "printf '{\"state\":\"working\"}'"]);
        let stale = family1_truth_probe_with_command(old, Duration::from_secs(1), "h1")
            .expect("probe answers");
        assert!(stale.last_event_at.is_none());
        assert!(stale.last_message.is_none());
    }

    #[test]
    fn family1_truth_subprocess_is_bounded_and_validated() {
        let mut valid = std::process::Command::new("sh");
        valid.args(["-c", "printf '{\"state\":\"watching\"}'"]);
        assert_eq!(
            probe_state(valid, Duration::from_secs(1), "h1").as_deref(),
            Some("watching")
        );

        let mut invalid = std::process::Command::new("sh");
        invalid.args(["-c", "printf '{\"state\":\"invented\"}'"]);
        assert_eq!(probe_state(invalid, Duration::from_secs(1), "h1"), None);

        let mut hung = std::process::Command::new("sh");
        hung.args(["-c", "sleep 5"]);
        let started = Instant::now();
        assert_eq!(probe_state(hung, Duration::from_millis(50), "h1"), None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_batch_answer_larger_than_the_pipe_buffer_still_comes_back_whole() {
        // The reader used to wait for exit before reading a word, which
        // deadlocks against a child blocked in write() once its answer passes
        // the pipe buffer -- 64 KiB, measured on the machine this was written
        // on. The child then cannot exit, the deadline kills a healthy
        // process, and the batch reports NoOutput. That maps to `crashed:
        // false`, so it buys no retry and no per-handle fallback either: every
        // row on the page renders null reachability at once.
        //
        // Sized well past the buffer on purpose. One handle's answer measured
        // ~535 bytes against the live roster, so this stands in for a roster
        // of a few hundred rows -- and for a smaller one carrying a single fat
        // `last_message`.
        const PAYLOAD: usize = 400 * 1024;
        let mut fat = std::process::Command::new("sh");
        fat.args(["-c", &format!("head -c {PAYLOAD} /dev/zero | tr '\\0' 'x'")]);
        let started = Instant::now();
        let run = run_truth_subprocess(fat, Duration::from_secs(20), "a fat batch", false);
        match run {
            BoundedRun::Output(output) => {
                assert_eq!(
                    output.stdout.len(),
                    PAYLOAD,
                    "the whole answer must survive, not just the first bufferful"
                );
                assert!(output.status.success(), "the child exited on its own");
            }
            other => panic!("a large answer must not read as a failed run: {other:?}"),
        }
        // Bounded well under the deadline: proves it returned because the
        // child finished, not because the timeout fired.
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_grandchild_holding_the_pipe_open_cannot_outrun_the_bound() {
        // The regression that the drain itself introduced, pinned so it cannot
        // come back. The child exits AT ONCE while a grandchild keeps the
        // inherited stdout open for 30s. Reading to end therefore does not end
        // when the child does, so joining the reader waited on the grandchild
        // and blew the bound. This must return on the drain grace instead.
        //
        // Written with an explicit background grandchild rather than a plain
        // `sleep`, because a shell may exec a single command in place, which
        // leaves no grandchild at all and quietly passes on one platform while
        // failing on another. That is exactly how this reached CI.
        let mut orphan = std::process::Command::new("sh");
        orphan.args(["-c", "sleep 30 & exit 0"]);
        let started = Instant::now();
        let _ = run_truth_subprocess(orphan, Duration::from_millis(200), "an orphan", false);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the call waited on a grandchild instead of its own bound"
        );
    }

    #[test]
    fn a_child_that_never_exits_is_still_bounded_after_the_drain() {
        // The drain must not cost the bound: a child holding its pipes open
        // forever still has to be killed on the deadline.
        let mut hung = std::process::Command::new("sh");
        hung.args(["-c", "sleep 30"]);
        let started = Instant::now();
        let run = run_truth_subprocess(hung, Duration::from_millis(100), "a hung batch", false);
        assert!(matches!(run, BoundedRun::NoOutput));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn only_a_routine_not_found_is_silenced() {
        // The quiet/loud split, pinned directly rather than inferred from a
        // return value both branches share.
        assert!(truth_failure_is_routine("not-found"));
        assert!(truth_failure_is_routine("  not-found\n"));
        // Everything else still warns: these mean something is actually broken.
        assert!(!truth_failure_is_routine("transcript-unreadable"));
        // A crashing resolver reports its own reason precisely so this
        // suppression cannot swallow it (cli/src/fno/agents/session_truth.py).
        assert!(!truth_failure_is_routine("resolver-error"));
        assert!(!truth_failure_is_routine("ambiguous"));
        assert!(!truth_failure_is_routine(""));
        assert!(!truth_failure_is_routine("not-found-ish"));
    }

    #[test]
    fn family1_truth_nonzero_exit_still_salvages_the_unknown_verdict() {
        // x-9de7: truth writes its full computed verdict to stdout BEFORE
        // deciding the exit code, so exit 13 with `state: "unknown"` is a
        // correct "unmeasured" answer, not a bare refusal - discarding it
        // used to become `reachability: null, basis: null`, an absence the
        // next reader was free to read as death. Both a routine not-found and
        // a genuine resolver-error now resolve identically to "unknown": the
        // warn-vs-quiet split (pinned by the predicate test above) governs
        // stderr noise only, never whether the verdict itself is kept.
        let mut not_found = std::process::Command::new("sh");
        not_found.args([
            "-c",
            "printf '{\"state\":\"unknown\",\"reason\":\"not-found\"}'; exit 13",
        ]);
        assert_eq!(
            probe_state(not_found, Duration::from_secs(1), "ses_1d9e"),
            Some("unknown".to_string())
        );

        let mut broken = std::process::Command::new("sh");
        broken.args([
            "-c",
            "printf '{\"state\":\"unknown\",\"reason\":\"resolver-error\"}'; exit 13",
        ]);
        assert_eq!(
            probe_state(broken, Duration::from_secs(1), "abcd1234"),
            Some("unknown".to_string())
        );
    }

    #[test]
    fn family1_truth_nonzero_exit_carries_the_real_reachability_and_basis() {
        // The realistic shape from the plan's own repro (cx-x-e14b): a row
        // with no corroborating identity surface resolves `state: "unknown"`
        // with a genuinely computed `reachability`/`basis` pair, still on
        // exit 13. The salvage must carry those through, not just the state.
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "printf '{\"state\":\"unknown\",\"reason\":\"not-found\",\
             \"reachability\":\"unreachable\",\"basis\":\"process-gone\"}'; exit 13",
        ]);
        let probe = family1_truth_probe_with_command(cmd, Duration::from_secs(1), "cx-x-e14b")
            .expect("a real computed verdict on exit 13 must not be discarded");
        assert_eq!(probe.state, "unknown");
        assert_eq!(probe.reachability.as_deref(), Some("unreachable"));
        assert_eq!(probe.basis.as_deref(), Some("process-gone"));
    }

    #[test]
    fn family1_truth_nonzero_exit_never_salvages_a_live_looking_state() {
        // Monotone-lowering safety net: today's real `cmd_truth` never pairs a
        // non-zero exit with a live state, but a failed probe run has no
        // standing to assert liveness either way, so a live-looking state
        // must still fall through to None rather than being trusted.
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "printf '{\"state\":\"working\"}'; exit 13"]);
        assert_eq!(
            family1_truth_probe_with_command(cmd, Duration::from_secs(1), "h1").map(|p| p.state),
            None,
            "a non-zero exit must never assert a live state"
        );
    }

    // -----------------------------------------------------------------
    // family1_truth_probe_many: N handles, ONE cold start (x-0d93)
    // -----------------------------------------------------------------

    #[test]
    fn family1_truth_batch_decodes_every_handle_from_one_spawn() {
        // The whole win: 24 rows used to cost 24 interpreter starts. One spawn
        // must come back with every row's full payload, triple included -- a
        // batch that dropped fields would render a poorer list row than the
        // per-row path it replaces.
        let spawns = std::cell::Cell::new(0);
        let probes = family1_truth_batch_answer(
            &["h1".to_string(), "h2".to_string()],
            |handles| {
                spawns.set(spawns.get() + 1);
                assert_eq!(handles.len(), 2);
                sh(
                    "printf '{\"h1\":{\"state\":\"working\",\"reachability\":\"reachable\",\
                    \"basis\":\"transcript\",\"last_activity_age_s\":12.5,\
                    \"observed_model\":{\"kind\":\"observed\",\"model\":\"glm-5.3\"}},\
                    \"h2\":{\"state\":\"done\",\"reachability\":\"reachable\",\
                    \"basis\":\"transcript\"}}'",
                )
            },
            Duration::from_secs(5),
        ).map(|a| a.probes)
        .expect("the batch answered");
        assert_eq!(spawns.get(), 1, "one batch, one process");
        assert_eq!(probes.len(), 2);
        let h1 = &probes["h1"];
        assert_eq!(h1.state, "working");
        assert_eq!(h1.reachability.as_deref(), Some("reachable"));
        assert_eq!(h1.basis.as_deref(), Some("transcript"));
        assert_eq!(h1.last_activity_age_s, Some(12.5));
        assert_eq!(h1.observed_model["model"], "glm-5.3");
        assert_eq!(probes["h2"].state, "done");
    }

    #[test]
    fn family1_truth_batch_on_an_empty_slice_spawns_nothing() {
        // A sweep with nothing to escalate must cost NO subprocess. A factory
        // that panics is the only way to assert an absence of spawns without
        // reading one.
        let probes = family1_truth_batch_answer(
            &[],
            |_| panic!("an empty batch must never spawn"),
            Duration::from_secs(5),
        ).map(|a| a.probes)
        .expect("an empty batch is an answer, not a failure");
        assert!(probes.is_empty());
    }

    #[test]
    fn family1_truth_batch_crash_is_retried_exactly_once_for_the_whole_batch() {
        // The reinstall window costs the batch ONE extra cold start, not one
        // per row -- the retry rule the single probe documents, priced per
        // batch.
        let attempts = std::cell::Cell::new(0);
        let probes = family1_truth_batch_answer(
            &["h1".to_string()],
            |_| {
                attempts.set(attempts.get() + 1);
                match attempts.get() {
                    1 => sh("echo 'ModuleNotFoundError: fno.agents.cli' >&2; exit 1"),
                    _ => sh("printf '{\"h1\":{\"state\":\"working\"}}'"),
                }
            },
            Duration::from_secs(5),
        ).map(|a| a.probes)
        .expect("the retry answered");
        assert_eq!(attempts.get(), 2, "a crash must buy exactly one retry");
        assert_eq!(probes["h1"].state, "working");

        // And it stops there: a persistently broken probe is loud, not looping.
        let attempts = std::cell::Cell::new(0);
        let probes = family1_truth_batch_answer(
            &["h1".to_string()],
            |_| {
                attempts.set(attempts.get() + 1);
                sh("exit 1")
            },
            Duration::from_secs(5),
        ).map(|a| a.probes);
        assert_eq!(attempts.get(), 2);
        assert!(
            probes.is_none(),
            "two crashes is a FAILURE, distinguishable from an empty answer"
        );
    }

    #[test]
    fn family1_truth_batch_answers_only_for_the_handles_the_batch_resolved() {
        // A handle the batch could not answer for is simply absent from the
        // map, which every caller already treats as `None` -- the same reading
        // the per-row path gave when its probe returned nothing.
        let probes = family1_truth_batch_answer(
            &["h1".to_string(), "gone".to_string()],
            |_| sh("printf '{\"h1\":{\"state\":\"working\"},\"gone\":{\"state\":\"nonsense\"}}'"),
            Duration::from_secs(5),
        ).map(|a| a.probes)
        .expect("the batch answered");
        assert_eq!(probes.len(), 1);
        assert!(!probes.contains_key("gone"));
    }

    #[test]
    fn family1_truth_batch_timeout_scales_with_the_handles_asked_for() {
        // The bug this pins: a batch of N handed the SINGLE handle's 5 s budget.
        // Measured on the live roster, twelve real handles took 5.7 s, so a flat
        // 5 s killed the child and the whole page came back empty - every row
        // rendering null reachability, and the dormant gate unable to get the
        // positive `done` reading an eviction needs. A timing-out batch is
        // strictly worse than the per-row probes it replaced, which each had
        // their own 5 s.
        let one = family1_truth_batch_timeout(1);
        let twelve = family1_truth_batch_timeout(12);
        let forty = family1_truth_batch_timeout(40);

        assert!(
            one >= Duration::from_secs(5),
            "never below the single budget"
        );
        assert!(twelve > one, "more handles must buy more time");
        assert!(forty > twelve);

        // Real measurements this must clear, with headroom: 12 handles in 1.6 s
        // and the full 31-row roster in 3.9 s after the resolver hoists.
        assert!(
            twelve >= Duration::from_secs(8),
            "12 handles took 1.6s measured"
        );
        assert!(
            forty >= Duration::from_secs(15),
            "31 handles took 3.9s measured"
        );

        // Capped, so one pathological transcript cannot wedge a sweep for
        // minutes. The daemon runs this off the select arm, so the ceiling is
        // about bounding a stuck probe, never about protecting `accept()`.
        assert_eq!(
            family1_truth_batch_timeout(10_000),
            Duration::from_secs(60),
            "the bound is capped, not unbounded"
        );
        // Saturates rather than wrapping or panicking. Checked by running the
        // arithmetic, not by reasoning about it: an earlier draft asserted
        // `Duration * u32` panicked here and added a clamp to prevent it.
        // Neither was true. 300 ms times u32::MAX is about 1.29e9 seconds, four
        // orders under Duration::MAX, and `min(..., CEILING)` already bounds
        // the result. The clamp was a second copy of a cap one line above it.
        assert_eq!(
            family1_truth_batch_timeout(usize::MAX),
            Duration::from_secs(60),
            "an absurd handle count saturates at the ceiling"
        );
    }

    #[test]
    fn family1_truth_batch_distinguishes_a_failure_from_an_empty_answer() {
        // The discriminator the per-handle fallback hangs off. An `fno` older
        // than `--handles` exits 2 on the unknown option with empty stdout -
        // the ordinary state right after this lands, since every worktree
        // carries its own binary. That must read as a FAILURE.
        let old_fno = family1_truth_batch_answer(
            &["h1".to_string()],
            |_| sh("echo 'Error: No such option: --handles' >&2; exit 2"),
            Duration::from_secs(5),
        ).map(|a| a.probes);
        assert!(old_fno.is_none(), "an fno too old to batch is a failure");

        // A batch that RAN and resolved nothing is an answer. Reading this as a
        // failure would re-spawn one process per handle every sweep over a
        // roster where nothing resolves - the exact cost this PR removes.
        let answered_nothing = family1_truth_batch_answer(
            &["h1".to_string()],
            |_| sh("printf '{}'"),
            Duration::from_secs(5),
        ).map(|a| a.probes);
        assert_eq!(
            answered_nothing
                .expect("an empty object is an answer")
                .len(),
            0
        );
    }

    #[test]
    fn family1_truth_batch_command_asks_for_the_keyed_object() {
        // Pins the wire call itself. The Python side keys `--handles` and emits
        // the object only under `--json`; a batch command missing either would
        // silently fall back to a single-handle read of a comma-joined string.
        let command = family1_truth_batch_command(&["a".to_string(), "b".to_string()]);
        let argv: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, ["agents", "truth", "--handles", "a,b", "--json"]);
    }

    #[test]
    fn family1_truth_failure_detail_prefers_stdout_reason() {
        // truth writes {state,reason} to stdout on a refusal (exit 13); stderr
        // holds the verify banner and is empty, so the reason is read off stdout.
        let detail =
            family1_truth_failure_detail(br#"{"state":"unknown","reason":"not-found"}"#, "");
        assert_eq!(detail, "not-found");
    }

    #[test]
    fn family1_truth_failure_detail_falls_back_to_stderr() {
        // A non-JSON stdout (e.g. a crashed probe) falls back to the stderr tail.
        let detail = family1_truth_failure_detail(b"not json", "  banner  ");
        assert_eq!(detail, "banner");
    }

}

//! How a resume wake delivers, and when its receipt may say live.
//!
//! Split out of `client_verbs.rs` (x-6ac3): that file is over the 5,000-line
//! budget and shrink-only, and the wake-delivery + respawn-confirm question
//! outgrew living beside the argv parsers. Callers keep thin call sites in
//! `client_verbs.rs`; the delivery and confirmation logic lives here.

use crate::claude_ask::{read_state_json, ClaudeHome};
use crate::client_verbs::{append_agents_event, trace_events_path};
use crate::daemon::PaneProbe;
use crate::paths::AgentsHome;
use crate::state;
use crate::truth_probe::family1_truth_state;
use serde_json::Value;
use std::path::Path;

/// Acquire the `session:<uuid>` single-writer claim for an interactive dead-row
/// resume, anchored to THIS process. `exec` keeps the pid, so the claim is held
/// by the resumed claude and self-releases when the operator quits (no explicit
/// release). Two racing resumers both probe dead, but only one wins this atomic
/// claim; the loser gets `Err` and refuses instead of opening a second writer on
/// one transcript - the residual double-writer window the liveness probe alone
/// cannot close. `root` is `None` in prod (session: keys route to
/// `$FNO_CLAIMS_ROOT`/`$HOME`); tests inject a temp root.
/// How long the session single-writer claim guards a mux-pane relaunch.
/// The launching process exits once the pane is up, so the claim cannot ride
/// the holder pid the way the in-terminal exec's does (a PID-only claim goes
/// Stale the moment that pid dies, so a second resumer would steal it before
/// the resumed claude is probe-live). This TTL keeps the claim Live across
/// that launch-to-probe-live window; once claude is probe-live the truth probe
/// (not this claim) stops a second relaunch. Picked wide against slow startup;
/// after it expires, a crashed worker can be re-resumed rather than blocked.
pub(crate) const MUX_RESUME_CLAIM_TTL_MS: u64 = 120_000;

pub(crate) fn acquire_resume_session_claim(
    uuid: &str,
    root: Option<&Path>,
    ttl_ms: Option<u64>,
) -> Result<(), (i32, String)> {
    acquire_named_session_claim(&format!("session:{uuid}"), uuid, root, ttl_ms)
}

/// Used directly by the dead-row `claude --resume` relaunch (keyed
/// `session:{uuid}`). The live-row headless wake uses the matching
/// `resume-attach:{short_id}` key too, but acquires it Python-side
/// (`resume_cli.py`'s `_resume_claude_wake`, gated on skip-eligibility) --
/// this Rust arm delegates the wake itself and does not call this function
/// for that key. Two different key prefixes by design: a live wake and a
/// dead relaunch are mutually exclusive outcomes of one truth-state read,
/// never racing each other for the same row, but two concurrent resumes
/// both landing on the SAME arm for the same row do race -- each key only
/// needs to guard against its own arm's double-writer.
pub(crate) fn acquire_named_session_claim(
    key: &str,
    label: &str,
    root: Option<&Path>,
    ttl_ms: Option<u64>,
) -> Result<(), (i32, String)> {
    use crate::claims::{acquire, AcquireOpts, AcquireOutcome};
    let holder = format!("resume:{}", std::process::id());
    let opts = AcquireOpts {
        root: root.map(Path::to_path_buf),
        reason: Some("interactive resume single-writer".to_string()),
        ttl_ms: ttl_ms.map(|t| t as i64),
        ..Default::default()
    };
    match acquire(key, &holder, opts) {
        AcquireOutcome::Acquired(_) => Ok(()),
        AcquireOutcome::HeldByOther { holder, pid, host } => Err((
            11,
            format!(
                "fno agents resume: session {label} is held live by another writer \
                 ({holder}, pid={}, host={host}); not opening a second writer on one transcript.",
                // The Python twins print the bare pid / `None`, never `Some(n)`.
                pid.map(|p| p.to_string())
                    .unwrap_or_else(|| "None".to_string())
            ),
        )),
        AcquireOutcome::Error(e) => Err((
            12,
            format!("fno agents resume: could not claim session {label}: {e}"),
        )),
    }
}

/// Default injected wake text when the caller passes no `--message`. Matches
/// the Python wake lane's `_DEFAULT_WAKE_MESSAGE` so the two runtimes stay in
/// parity.
const RESUME_WAKE_MESSAGE: &str = "continue";

/// Deliver a resume wake to a codex thread row over the app-server daemon
/// (x-6ac3). Exit 0 is a positive receipt: the daemon accepted the turn. An
/// `Err` maps to exit 16 carrying the reason token (`no-daemon`, `io-error`),
/// never an exec that may have done nothing under a captured stdin.
pub(crate) fn run_codex_thread_delivery(
    name: &str,
    session_id: &str,
    message: Option<&str>,
    cwd: &str,
    home: &AgentsHome,
) -> i32 {
    let text = message.unwrap_or(RESUME_WAKE_MESSAGE).to_string();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fno agents resume: codex daemon delivery failed: {e}");
            return 16;
        }
    };
    let session_id = session_id.to_string();
    let result = runtime.block_on(async {
        crate::codex_inject::deliver_via_codex_daemon(&session_id, &text).await
    });
    match result {
        Ok(()) => {
            append_agents_event(
                &trace_events_path(home),
                "agent_resumed",
                &[
                    ("name", Value::String(name.to_string())),
                    ("provider", Value::String("codex".to_string())),
                    ("session_id", Value::String(session_id)),
                    ("cwd", Value::String(cwd.to_string())),
                ],
            );
            eprintln!("delivered to {name} over the codex daemon");
            0
        }
        Err(reason) => {
            eprintln!("fno agents resume: not delivered: {reason}");
            16
        }
    }
}

/// How the wake route talks to the mux for its viewport attach. Shells out to
/// `fno` in production; tests feed the pane id and the lock screen in memory.
pub(crate) trait ViewportIo {
    fn launch(&self, argv: &[String]) -> std::io::Result<String>;
    fn screen(&self, server: &str, pane: &str) -> String;
    fn nap(&self, ms: u64);
}

/// The production mux transport.
pub(crate) struct ShellViewportIo;

impl ViewportIo for ShellViewportIo {
    fn launch(&self, argv: &[String]) -> std::io::Result<String> {
        // `fno mux pane run` prints the new pane id alone on stdout.
        let out = std::process::Command::new("fno").args(argv).output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "pane run exited {}",
                out.status
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn screen(&self, server: &str, pane: &str) -> String {
        std::process::Command::new("fno")
            .args([
                "mux", "pane", "read", "--server", server, "--lines", "40", pane,
            ])
            .output()
            .map(|o| {
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )
            })
            .unwrap_or_default()
            .to_lowercase()
    }

    fn nap(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// The codex resume arm for rows whose thread is reachable over the app-server
/// (x-4a68): a thread row delivers directly, and a pane row whose pane is dead
/// but whose thread is loaded in the app-server gets the message over
/// `turn/start` and attaches a `--remote unix://` viewport. `None` means this
/// route does not answer and the caller's claim / pane / exec paths run
/// unchanged - a live pane, a dead pane whose thread is not loaded, and any
/// unmeasurable verdict all fall through.
#[allow(clippy::too_many_arguments)]
pub(crate) fn codex_resume_route(
    name: &str,
    entry: &Value,
    session_id: &str,
    message: Option<&str>,
    cwd: &str,
    row_name: &str,
    identity: &[String],
    home: &AgentsHome,
    probe: &dyn Fn(&str, u64) -> PaneProbe,
    loaded: &dyn Fn() -> Result<Vec<String>, &'static str>,
    io: &dyn ViewportIo,
) -> Option<i32> {
    if entry.get("harness").and_then(Value::as_str) != Some("codex") {
        return None;
    }
    // A thread row delivers over the daemon, never a terminal exec (x-6ac3):
    // `codex resume <id>` needs a tty and a headless caller has none.
    if entry.get("substrate").and_then(Value::as_str) == Some("thread") {
        return Some(run_codex_thread_delivery(
            name, session_id, message, cwd, home,
        ));
    }
    let mux = entry.get("mux").and_then(|m| {
        Some(state::MuxRef {
            session: m.get("session")?.as_str()?.to_string(),
            pane_id: m.get("pane_id")?.as_u64()?,
        })
    });
    let mut route = None;
    if mux.is_some() {
        let verdict =
            crate::lane_heal::heal_dead_pane_binding(home, session_id, false, probe, loaded);
        if verdict.verdict == "dead-pane-loaded" {
            route = Some(wake_loaded_thread(
                name,
                session_id,
                message,
                cwd,
                row_name,
                identity,
                home,
                &mux.unwrap(),
                io,
                probe,
                loaded,
            ));
        }
    }
    route
}

/// The message goes over the daemon (carrying the writable-roots grant from
/// [`crate::codex_inject`]); then the viewport attaches through the declared
/// `interactive_attach` form - `codex resume <id> --remote unix://` behind the
/// daemon `pre_exec`, with NO `-c` token, because a client `-c` grant both
/// fails under `--remote` (pane 2205) and is redundant once the turn carries
/// the policy (pane 2206, measured 2026-09-14).
#[allow(clippy::too_many_arguments)]
fn wake_loaded_thread(
    name: &str,
    session_id: &str,
    message: Option<&str>,
    cwd: &str,
    row_name: &str,
    identity: &[String],
    home: &AgentsHome,
    mux: &state::MuxRef,
    io: &dyn ViewportIo,
    probe: &dyn Fn(&str, u64) -> PaneProbe,
    loaded: &dyn Fn() -> Result<Vec<String>, &'static str>,
) -> i32 {
    let code = run_codex_thread_delivery(name, session_id, message, cwd, home);
    if code != 0 {
        return code;
    }
    let argv = match crate::harness_capabilities::render_session_argv_with_ids(
        "codex",
        "interactive_attach",
        Some(session_id),
        None,
    ) {
        Ok(argv) => argv,
        Err(err) => {
            eprintln!(
                "fno agents resume: {name}: delivered over the codex daemon, \
                     but the viewport attach form is unavailable ({err}); the row \
                     stays bound to the thread lane"
            );
            return finish_on_the_thread_lane(home, session_id, probe, loaded);
        }
    };
    let run_argv =
        crate::pane_relaunch::mux_pane_run_argv(&mux.session, cwd, &argv, identity, Some(row_name));
    let pane = match io.launch(&run_argv) {
        Ok(out) => out,
        Err(err) => {
            eprintln!(
                "fno agents resume: {name}: delivered over the codex daemon, but \
                 the viewport pane launch failed ({err}); the row rebinds to the \
                 thread lane so sends still land"
            );
            return finish_on_the_thread_lane(home, session_id, probe, loaded);
        }
    };
    if pane.parse::<u64>().is_err() {
        eprintln!(
            "fno agents resume: {name}: delivered over the codex daemon, but the \
             viewport launch printed {pane:?}, not a pane id; the row rebinds to \
             the thread lane so sends still land"
        );
        return finish_on_the_thread_lane(home, session_id, probe, loaded);
    }
    // Poll the new pane for the app-server lock screen within a 5s budget.
    for _ in 0..10 {
        io.nap(500);
        if io
            .screen(&mux.session, &pane)
            .contains("open in another app")
        {
            println!(
                "fno agents resume: {name}: the message was delivered, but codex \
                 says the viewport is locked by another app (pane {pane}); the \
                 row rebinds to the thread lane so sends still land"
            );
            return finish_on_the_thread_lane(home, session_id, probe, loaded);
        }
    }
    // Rebind the row to the pane it made, only while the dead ref still
    // stands - a concurrent resume wins and this call stands down.
    let mut rebound = false;
    let _ =
        state::update_registry(&home.registry_json(), |r| {
            let Some(target) = r.entries.iter_mut().find(|e| {
                e.name == row_name && e.harness_session_id.as_deref() == Some(session_id)
            }) else {
                return;
            };
            let Some(current) = target.mux.as_ref() else {
                return;
            };
            if current.session != mux.session || current.pane_id != mux.pane_id {
                return;
            }
            target.mux = Some(state::MuxRef {
                session: mux.session.clone(),
                pane_id: pane.parse::<u64>().unwrap_or(mux.pane_id),
            });
            target.pid = None;
            target.pid_start_time = None;
            rebound = true;
        });
    if !rebound {
        eprintln!(
            "fno agents resume: {name}: delivered over the codex daemon, but the \
             row changed under this resume; it rebinds to the thread lane"
        );
        return finish_on_the_thread_lane(home, session_id, probe, loaded);
    }
    append_agents_event(
        &trace_events_path(home),
        "agent_resumed",
        &[
            ("name", Value::String(name.to_string())),
            ("provider", Value::String("codex".to_string())),
            ("session_id", Value::String(session_id.to_string())),
            ("cwd", Value::String(cwd.to_string())),
        ],
    );
    println!(
        "fno agents resume: {name} delivered over the codex daemon; viewport \
         pane {pane} attached with --remote unix://"
    );
    0
}

/// The production wake route: the real pane probe, the real loaded-thread
/// read, the real mux shell-out. The seams in [`codex_resume_route`] stay for
/// tests (x-4a68).
pub(crate) fn codex_resume_wake_route(
    name: &str,
    entry: &Value,
    session_id: &str,
    message: Option<&str>,
    cwd: &str,
    row_name: &str,
    identity: &[String],
    home: &AgentsHome,
) -> Option<i32> {
    let loaded = || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "io-error")?;
        let threads = rt.block_on(crate::codex_inject::discover_loaded_threads())?;
        Ok(threads.into_iter().map(|t| t.session_id).collect())
    };
    codex_resume_route(
        name,
        entry,
        session_id,
        message,
        cwd,
        row_name,
        identity,
        home,
        &crate::daemon::run_mux_pane_probe,
        &loaded,
        &ShellViewportIo,
    )
}

/// The delivered-but-not-rebound floor: rebind the row to the thread lane so
/// later sends still land, and exit 0 - the message WAS delivered.
fn finish_on_the_thread_lane(
    home: &AgentsHome,
    session_id: &str,
    probe: &dyn Fn(&str, u64) -> PaneProbe,
    loaded: &dyn Fn() -> Result<Vec<String>, &'static str>,
) -> i32 {
    let verdict = crate::lane_heal::heal_dead_pane_binding(home, session_id, true, probe, loaded);
    if verdict.verdict != "rebound-thread" {
        eprintln!(
            "fno agents resume: thread-lane rebind read {} ({:?}); the row is \
             left as it stands",
            verdict.verdict, verdict.reason
        );
    }
    0
}

/// Run a respawn plan as a child and confirm it actually revived the row.
///
/// `claude respawn` exits as soon as the job is relaunched, so this verb must
/// NOT exec it (the exec convention the other arms use): the operator's shell
/// would come back with nothing to show. And exit 0 is not proof - the
/// receipt-can-lie shape `fno agents rm` already shipped once. The
/// confirmation is the positive marker: `jobs/<short>/state.json` re-read
/// after the respawn with an `updated_at` newer than the pre-respawn read.
/// The state WORD is not evidence - the wake lane's confirm primitive exists
/// because `working -> working` read the same for a landed and an unlanded
/// message.
/// The copy short id inside the measured copy notice: `started a copy as
/// <short>.` None when the output does not carry one.
fn copy_short_from_notice(text: &str) -> Option<String> {
    const MARKER: &str = "started a copy as ";
    let idx = text.find(MARKER)? + MARKER.len();
    let short: String = text[idx..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    (short.len() == 8).then_some(short)
}

pub(crate) fn run_and_confirm_respawn(
    plan: &crate::reentry::ReentryPlan,
    name: &str,
    verb: &str,
    event_kind: &str,
    home: &AgentsHome,
) -> i32 {
    run_and_confirm_respawn_with_truth(
        plan,
        name,
        verb,
        event_kind,
        home,
        ClaudeHome::from_env(),
        family1_truth_state,
        std::thread::sleep,
    )
}

pub(crate) fn run_and_confirm_respawn_with_truth<F, S>(
    plan: &crate::reentry::ReentryPlan,
    name: &str,
    verb: &str,
    event_kind: &str,
    home: &AgentsHome,
    claude_home: ClaudeHome,
    truth_fn: F,
    sleep_fn: S,
) -> i32
where
    F: Fn(&str) -> Option<String>,
    S: Fn(std::time::Duration),
{
    let jobs_dir = claude_home.jobs_dir_for(&plan.short_id);
    let bg_resume = plan.mechanism == "bg-resume";
    // A bg resume relaunches a session whose job dir is typically GONE, so
    // its confirmation is the state file appearing after the launch, not a
    // stamp advancing. The respawn arm keeps the advance proof.
    let before_updated_at = if bg_resume {
        None
    } else {
        read_state_json(&jobs_dir).ok().and_then(|s| s.updated_at)
    };

    let mut command = std::process::Command::new(&plan.argv[0]);
    command.args(&plan.argv[1..]).current_dir(&plan.cwd);
    // Identity first, so a plan env entry can still override it: the
    // resumed serving process inherits none of the env the original spawn
    // carried (measured: FNO_AGENT_SELF absent from the resumed process),
    // so this stamp is the only carrier of the fno name.
    crate::claims::stamp_command_env(
        &mut command,
        Some(&plan.name),
        "claude",
        Some(&plan.session_id),
    );
    if let Some(node) = plan.node.as_deref().filter(|n| !n.is_empty()) {
        command.env("FNO_NODE", node);
    }
    for (key, value) in &plan.env {
        command.env(key, value);
    }
    if bg_resume {
        let out = match command.output() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("fno agents {verb}: failed to run {}: {e}", plan.argv[0]);
                return 1;
            }
        };
        if !out.status.success() {
            eprintln!(
                "fno agents {verb}: {} for {name} exited {}",
                plan.argv.join(" "),
                out.status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            );
            return 1;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let combined = format!("{stdout}{stderr}");
        // A live session answers the bg resume with a COPY under a NEW id
        // (measured notice: `started a copy as <short>.`); a plain
        // `backgrounded` line naming a DIFFERENT job id is the same fact.
        let observed = crate::claude_ask::parse_short_id(&combined).ok();
        let copy_short = copy_short_from_notice(&combined).or(observed);
        let copy = combined.contains("started a copy as ")
            || copy_short
                .as_deref()
                .is_some_and(|s| s != plan.short_id);
        if copy {
            if let Some(short) = copy_short.as_deref().filter(|s| *s != plan.short_id.as_str()) {
                let mut stop = std::process::Command::new("claude");
                stop.args(["stop", short]).current_dir(&plan.cwd);
                for (key, value) in &plan.env {
                    stop.env(key, value);
                }
                let _ = stop.status();
            }
            eprintln!(
                "fno agents {verb}: refused: session {} is already running, so the \
                 relaunch started a COPY instead of continuing it and the copy was \
                 stopped. Reach the live original with `fno agents attach {name}`.",
                plan.session_id
            );
            return crate::reentry::REENTRY_REFUSED_EXIT;
        }
    } else {
        let status = match command.status() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("fno agents {verb}: failed to run {}: {e}", plan.argv[0]);
                return 1;
            }
        };
        if !status.success() {
            eprintln!(
                "fno agents {verb}: {} for {name} exited {}",
                plan.argv.join(" "),
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            );
            return 1;
        }
    }

    let confirmed = if bg_resume {
        read_state_json(&jobs_dir).is_ok()
    } else {
        match (before_updated_at, read_state_json(&jobs_dir)) {
        (Some(before), Ok(s)) => s.updated_at.as_deref().is_some_and(|a| a > before.as_str()),
        // No readable BEFORE stamp (the file the resolver just proved exists
        // did not parse): an AFTER read carrying any stamp is the evidence
        // left, and it is still content, never an exit code.
        (None, Ok(s)) => s.updated_at.is_some(),
            (_, Err(_)) => false,
        }
    };
    if !confirmed {
        eprintln!(
            "fno agents {verb}: respawn for {name} reported success but {} did not \
             advance; the row is NOT confirmed back in agent view. Check \
             `claude agents` before retrying.",
            jobs_dir.join("state.json").display()
        );
        return 16;
    }

    // x-6ac3: `updated_at` advancing proves the job relaunched, never that
    // the worker is answering - the same receipt-can-lie shape the node
    // recorded (`is live again` printed, then truth read stalled). The
    // receipt may say live only when the truth probe agrees, within a
    // bounded window: a relaunched worker takes seconds to reach its first
    // live state.
    const TRUTH_POLLS: u32 = 10;
    const TRUTH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
    let is_live = |state: &str| matches!(state, "working" | "watching" | "your-move");
    let mut last_state = "unknown".to_string();
    let mut live = false;
    for attempt in 0..TRUTH_POLLS {
        if attempt > 0 {
            sleep_fn(TRUTH_INTERVAL);
        }
        if let Some(state) = truth_fn(&plan.session_id) {
            if is_live(&state) {
                live = true;
                last_state = state;
                break;
            }
            last_state = state;
        }
    }
    if !live {
        eprintln!(
            "fno agents {verb}: respawned {name}, but truth reads \
             {last_state}; not confirmed live."
        );
        return 16;
    }

    // The row still reads dead/exited while its session is demonstrably
    // live again. Flip it under the registry lock, matching on name AND
    // session id; the binding fields stay untouched so mail and attach keep
    // resolving the same session.
    let flip = crate::state::update_registry(&home.registry_json(), |reg| {
        let mut flipped = false;
        for entry in reg.entries.iter_mut() {
            if entry.name == plan.name
                && entry.harness_session_id.as_deref() == Some(plan.session_id.as_str())
            {
                entry.status = crate::AgentStatus::Live;
                entry.exited_at = None;
                flipped = true;
            }
        }
        flipped
    });
    match flip {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "fno agents {verb}: {name} is live, but no registry row carries \
                 session {}; the row was removed while the relaunch ran.",
                plan.session_id
            );
            return 16;
        }
        Err(e) => {
            eprintln!("fno agents {verb}: registry flip failed: {e}");
            return 16;
        }
    }

    append_agents_event(
        &trace_events_path(home),
        event_kind,
        &[
            ("name", Value::String(name.to_string())),
            ("provider", Value::String("claude".to_string())),
            ("session_id", Value::String(plan.session_id.clone())),
            ("cwd", Value::String(plan.cwd.clone())),
        ],
    );
    eprintln!(
        "{name} is live again under {} (same session id).",
        plan.session_id
    );
    eprintln!("`fno agents attach {name}` to drop in.");
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_thread_resume_delivers_over_the_daemon_and_exits_0() {
        let _guard = crate::path_test_guard();
        // FakeDaemon::start tokio::spawns its server, so it needs a reactor.
        // Multi-thread keeps the serve task running while the delivery
        // helper block_on's its own current-thread runtime below.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let daemon = rt.block_on(async {
            crate::codex_fake_daemon::FakeDaemon::start(crate::codex_fake_daemon::Behavior::quick())
        });
        let dir = std::env::temp_dir().join(format!("fno-cv-codex-deliver-{}", std::process::id()));
        let home = AgentsHome::at(dir.clone());
        let code = run_codex_thread_delivery("w1", "thread-abc", Some("continue"), "/tmp/x", &home);
        assert_eq!(code, 0);
        let params = daemon
            .first_params("turn/start")
            .expect("turn/start must have run");
        assert_eq!(params["threadId"], "thread-abc");
        assert_eq!(params["input"][0]["text"], "continue");
        drop(daemon);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn codex_thread_resume_without_a_daemon_refuses_16() {
        // No exec arm exists in the delivery helper: it never constructs a
        // Command, so no `codex resume` process can start from this lane.
        let _guard = crate::path_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let saved = std::env::var_os("CODEX_HOME");
        std::env::set_var("CODEX_HOME", temp.path());
        let home = AgentsHome::at(temp.path().join("agents-home"));
        let code = run_codex_thread_delivery("w1", "thread-abc", None, "/tmp/x", &home);
        match &saved {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
        assert_eq!(code, 16);
    }

    #[test]
    fn respawn_receipt_reads_live_only_when_truth_reads_live() {
        // ClaudeHome is injected (not read off HOME) so the test is hermetic
        // against concurrent tests mutating HOME.
        let temp = tempfile::tempdir().unwrap();
        let claude_home = crate::claude_ask::ClaudeHome::at(temp.path());
        let jobs = claude_home.jobs_dir_for("abcd1234");
        std::fs::create_dir_all(&jobs).unwrap();
        let state = jobs.join("state.json");
        std::fs::write(
            &state,
            r#"{"state":"idle","updatedAt":"2026-09-13T00:00:00Z"}"#,
        )
        .unwrap();
        let plan = crate::reentry::ReentryPlan {
            resolved: true,
            transition: "resume".into(),
            mechanism: "respawn".into(),
            name: "w1".into(),
            fno_id: None,
            node: None,
            session_id: "sess-uuid".into(),
            short_id: "abcd1234".into(),
            launch_account: "default".into(),
            claude_config_dir: None,
            route_settings_path: None,
            cwd: temp.path().display().to_string(),
            substrate: "bg".into(),
            mux: None,
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "printf '%s' '{{\"state\":\"working\",\"updatedAt\":\"2026-09-13T00:01:00Z\"}}' > \"{}\"",
                    state.display()
                ),
            ],
            env: Default::default(),
        };
        let home = AgentsHome::at(temp.path().join("agents-home"));
        seed_exited_row(&home, "w1", "sess-uuid");
        let code = run_and_confirm_respawn_with_truth(
            &plan,
            "w1",
            "resume",
            "agent_resumed",
            &home,
            claude_home.clone(),
            |handle| {
                assert_eq!(handle, "sess-uuid");
                Some("working".to_string())
            },
            |_| {},
        );
        assert_eq!(code, 0);
        let reg = crate::state::load_registry(&home.registry_json()).unwrap();
        let row = reg.entries.iter().find(|e| e.name == "w1").unwrap();
        assert_eq!(row.status, crate::AgentStatus::Live);
        assert_eq!(row.harness_session_id.as_deref(), Some("sess-uuid"));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn respawn_receipt_refuses_when_truth_never_reads_live() {
        let temp = tempfile::tempdir().unwrap();
        let claude_home = crate::claude_ask::ClaudeHome::at(temp.path());
        let jobs = claude_home.jobs_dir_for("abcd1234");
        std::fs::create_dir_all(&jobs).unwrap();
        let state = jobs.join("state.json");
        std::fs::write(
            &state,
            r#"{"state":"idle","updatedAt":"2026-09-13T00:00:00Z"}"#,
        )
        .unwrap();
        let plan = crate::reentry::ReentryPlan {
            resolved: true,
            transition: "resume".into(),
            mechanism: "respawn".into(),
            name: "w1".into(),
            fno_id: None,
            node: None,
            session_id: "sess-uuid".into(),
            short_id: "abcd1234".into(),
            launch_account: "default".into(),
            claude_config_dir: None,
            route_settings_path: None,
            cwd: temp.path().display().to_string(),
            substrate: "bg".into(),
            mux: None,
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "printf '%s' '{{\"state\":\"working\",\"updatedAt\":\"2026-09-13T00:01:00Z\"}}' > \"{}\"",
                    state.display()
                ),
            ],
            env: Default::default(),
        };
        let home = AgentsHome::at(temp.path().join("agents-home"));
        let code = run_and_confirm_respawn_with_truth(
            &plan,
            "w1",
            "resume",
            "agent_resumed",
            &home,
            claude_home,
            |_| Some("stalled".to_string()),
            |_| {}, // no-op sleep: the window must not cost wall clock in tests
        );
        assert_eq!(code, 16);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    /// A registry home with one exited row: the flip target. `fno_id` is set
    /// so the tests can prove the flip leaves the binding fields untouched.
    fn seed_exited_row(home: &AgentsHome, name: &str, session_id: &str) {
        crate::state::update_registry(&home.registry_json(), |reg| {
            reg.entries.push(crate::state::RegistryEntry {
                name: name.to_string(),
                harness: Some("claude".to_string()),
                harness_session_id: Some(session_id.to_string()),
                fno_id: Some("fid-keep".to_string()),
                status: crate::AgentStatus::Exited,
                exited_at: Some("2026-09-14T00:00:00Z".to_string()),
                ..Default::default()
            });
        })
        .unwrap();
    }

    #[test]
    fn bg_resume_confirms_a_returning_job_and_flips_the_row_live() {
        // The fake launch echoes the plain `backgrounded` line and recreates
        // the job state the resolver could not find. Exit 0 requires the
        // state file to appear and the truth probe to read live.
        let temp = tempfile::tempdir().unwrap();
        let claude_home = crate::claude_ask::ClaudeHome::at(temp.path());
        let jobs = claude_home.jobs_dir_for("abcd1234");
        let plan = crate::reentry::ReentryPlan {
            resolved: true,
            transition: "resume".into(),
            mechanism: "bg-resume".into(),
            name: "w1".into(),
            fno_id: None,
            node: None,
            session_id: "sess-uuid".into(),
            short_id: "abcd1234".into(),
            launch_account: "default".into(),
            claude_config_dir: None,
            route_settings_path: None,
            cwd: temp.path().display().to_string(),
            substrate: "bg".into(),
            mux: None,
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "echo 'backgrounded · abcd1234 · w1'; \
                     mkdir -p '{jobs}' && printf '%s' \
                     '{{\"state\":\"working\",\"updatedAt\":\"2026-09-15T00:00:00Z\"}}' \
                     > '{jobs}/state.json'",
                    jobs = jobs.display()
                ),
            ],
            env: Default::default(),
        };
        let home = AgentsHome::at(temp.path().join("agents-home"));
        seed_exited_row(&home, "w1", "sess-uuid");
        let code = run_and_confirm_respawn_with_truth(
            &plan,
            "w1",
            "resume",
            "agent_resumed",
            &home,
            claude_home.clone(),
            |_| Some("working".to_string()),
            |_| {},
        );
        assert_eq!(code, 0);
        let reg = crate::state::load_registry(&home.registry_json()).unwrap();
        let row = reg.entries.iter().find(|e| e.name == "w1").unwrap();
        assert_eq!(row.status, crate::AgentStatus::Live);
        assert_eq!(row.fno_id.as_deref(), Some("fid-keep"));
        assert_eq!(row.harness_session_id.as_deref(), Some("sess-uuid"));
        assert!(row.exited_at.is_none());
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[test]
    fn bg_resume_refuses_a_copy_stops_it_and_never_touches_the_registry() {
        // A live session answers the bg resume with the measured copy notice.
        // The launch must refuse at 3, stop the COPY (never the original),
        // and leave the registry byte-identical.
        let _guard = crate::path_test_guard();
        let temp = tempfile::tempdir().unwrap();
        let claude_home = crate::claude_ask::ClaudeHome::at(temp.path());
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let stop_log = temp.path().join("stop.log");
        std::fs::write(
            bin.join("claude"),
            format!("#!/bin/sh\necho \"$*\" >> '{}'\n", stop_log.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                bin.join("claude"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", crate::path_with(&bin));

        let plan = crate::reentry::ReentryPlan {
            resolved: true,
            transition: "resume".into(),
            mechanism: "bg-resume".into(),
            name: "w1".into(),
            fno_id: None,
            node: None,
            session_id: "sess-uuid".into(),
            short_id: "abcd1234".into(),
            launch_account: "default".into(),
            claude_config_dir: None,
            route_settings_path: None,
            cwd: temp.path().display().to_string(),
            substrate: "bg".into(),
            mux: None,
            argv: vec![
                "sh".into(),
                "-c".into(),
                "echo 'note: session abcd1234 is already running in the background, \
                 so this started a copy as 660e758a.'; \
                 echo 'backgrounded · 660e758a (idle - send a prompt to start)'"
                    .to_string(),
            ],
            env: Default::default(),
        };
        let home = AgentsHome::at(temp.path().join("agents-home"));
        seed_exited_row(&home, "w1", "sess-uuid");
        let reg_before = std::fs::read(home.registry_json()).unwrap();
        let code = run_and_confirm_respawn_with_truth(
            &plan,
            "w1",
            "resume",
            "agent_resumed",
            &home,
            claude_home,
            |handle| {
                // The copy refusal fires before any truth read.
                unreachable!("truth probe must not run on the copy path: {handle}");
            },
            |_| {},
        );
        match &old_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        assert_eq!(code, crate::reentry::REENTRY_REFUSED_EXIT);
        let stops = std::fs::read_to_string(&stop_log).unwrap();
        assert!(stops.contains("stop 660e758a"), "{stops}");
        let reg_after = std::fs::read(home.registry_json()).unwrap();
        assert_eq!(reg_before, reg_after);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    // ---- x-4a68 wake-route fixtures -------------------------------------

    struct ScriptedViewport {
        launched: std::sync::Mutex<Vec<Vec<String>>>,
        screen: std::sync::Mutex<String>,
        fail_launch: bool,
    }

    impl ScriptedViewport {
        fn new(screen_text: &str) -> Self {
            Self {
                launched: std::sync::Mutex::new(Vec::new()),
                screen: std::sync::Mutex::new(screen_text.to_string()),
                fail_launch: false,
            }
        }
    }

    impl ViewportIo for ScriptedViewport {
        fn launch(&self, argv: &[String]) -> std::io::Result<String> {
            self.launched.lock().unwrap().push(argv.to_vec());
            if self.fail_launch {
                return Err(std::io::Error::other("pane run refused"));
            }
            Ok("2301".to_string())
        }
        fn screen(&self, _s: &str, _p: &str) -> String {
            self.screen.lock().unwrap().clone()
        }
        fn nap(&self, _ms: u64) {}
    }

    fn codex_pane_entry_json() -> Value {
        serde_json::json!({
            "harness": "codex",
            "substrate": "pane",
            "mux": {"session": "main", "pane_id": 2179}
        })
    }

    fn push_codex_pane_row(home: &AgentsHome, name: &str, session: &str) {
        let mut row = state::RegistryEntry {
            name: name.to_string(),
            ..Default::default()
        };
        row.harness = Some("codex".to_string());
        row.harness_session_id = Some(session.to_string());
        row.mux = Some(state::MuxRef {
            session: "main".to_string(),
            pane_id: 2179,
        });
        row.substrate = Some("pane".to_string());
        state::update_registry(&home.registry_json(), |r| r.entries.push(row)).unwrap();
    }

    fn read_row(home: &AgentsHome, session: &str) -> Option<state::RegistryEntry> {
        state::load_registry(&home.registry_json())
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.harness_session_id.as_deref() == Some(session))
    }

    fn tmp_home(tag: &str) -> AgentsHome {
        let dir = std::env::temp_dir().join(format!("fno-wake-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        AgentsHome::at(dir)
    }

    #[test]
    fn a_loaded_thread_wakes_over_the_daemon_and_attaches_a_remote_viewport() {
        let _guard = crate::path_test_guard();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let daemon = rt.block_on(async {
            crate::codex_fake_daemon::FakeDaemon::start(crate::codex_fake_daemon::Behavior::quick())
        });
        let home = tmp_home("wake-hp");
        push_codex_pane_row(&home, "w1", "sess-1");
        let entry = codex_pane_entry_json();
        let io = ScriptedViewport::new("idle codex tui");
        let absent = |_s: &str, _p: u64| PaneProbe::Absent;
        let loaded_ok = || Ok(vec!["sess-1".to_string()]);
        let code = codex_resume_route(
            "w1",
            &entry,
            "sess-1",
            Some("continue"),
            "/tmp/x",
            "w1",
            &[],
            &home,
            &absent,
            &loaded_ok,
            &io,
        )
        .expect("the wake route must answer");
        assert_eq!(code, 0);
        let params = daemon
            .first_params("turn/start")
            .expect("turn/start must have run");
        assert_eq!(params["threadId"], "sess-1");
        assert_eq!(params["input"][0]["text"], "continue");
        let launched = io.launched.lock().unwrap();
        assert_eq!(launched.len(), 1, "one viewport launch");
        let joined = launched[0].join(" ");
        assert!(joined.contains("resume"), "attach form resumes: {joined}");
        assert!(joined.contains("--remote"), "attach is --remote: {joined}");
        assert!(
            joined.contains("unix://"),
            "attach targets the daemon: {joined}"
        );
        assert!(
            !joined.contains("'-c'"),
            "no client -c grant rides (the fence's sh -c is not one): {joined}"
        );
        let row = read_row(&home, "sess-1").unwrap();
        let mux = row.mux.expect("the row rebinds to the new pane");
        assert_eq!(mux.pane_id, 2301);
        assert!(row.pid.is_none());
        drop(daemon);
        std::fs::remove_dir_all(&home.registry_json().parent().unwrap()).ok();
    }

    #[test]
    fn a_live_pane_a_missing_thread_and_an_unreadable_list_never_wake() {
        let _guard = crate::path_test_guard();
        let home = tmp_home("wake-err");
        push_codex_pane_row(&home, "w1", "sess-1");
        let entry = codex_pane_entry_json();
        let io = ScriptedViewport::new("idle");
        let present = |_s: &str, _p: u64| PaneProbe::Present;
        let absent = |_s: &str, _p: u64| PaneProbe::Absent;
        let loaded_ok = || Ok(vec!["other".to_string()]);
        let loaded_err = || Err("io-error");
        assert!(codex_resume_route(
            "w1",
            &entry,
            "sess-1",
            None,
            "/tmp/x",
            "w1",
            &[],
            &home,
            &present,
            &loaded_ok,
            &io,
        )
        .is_none());
        assert!(codex_resume_route(
            "w1",
            &entry,
            "sess-1",
            None,
            "/tmp/x",
            "w1",
            &[],
            &home,
            &absent,
            &loaded_ok,
            &io,
        )
        .is_none());
        assert!(codex_resume_route(
            "w1",
            &entry,
            "sess-1",
            None,
            "/p",
            "w1",
            &[],
            &home,
            &absent,
            &loaded_err,
            &io,
        )
        .is_none());
        assert!(io.launched.lock().unwrap().is_empty());
        assert!(read_row(&home, "sess-1").unwrap().mux.is_some());
        std::fs::remove_dir_all(&home.registry_json().parent().unwrap()).ok();
    }

    #[test]
    fn the_lock_screen_delivers_then_rebinds_to_the_thread_lane() {
        let _guard = crate::path_test_guard();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let daemon = rt.block_on(async {
            crate::codex_fake_daemon::FakeDaemon::start(crate::codex_fake_daemon::Behavior::quick())
        });
        let home = tmp_home("wake-lock");
        push_codex_pane_row(&home, "w1", "sess-1");
        let entry = codex_pane_entry_json();
        let io = ScriptedViewport::new("This conversation is open in another app");
        let absent = |_s: &str, _p: u64| PaneProbe::Absent;
        let loaded_ok = || Ok(vec!["sess-1".to_string()]);
        let code = codex_resume_route(
            "w1",
            &entry,
            "sess-1",
            Some("go"),
            "/tmp/x",
            "w1",
            &[],
            &home,
            &absent,
            &loaded_ok,
            &io,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(daemon.first_params("turn/start").is_some());
        let row = read_row(&home, "sess-1").unwrap();
        assert!(row.mux.is_none());
        assert_eq!(row.substrate.as_deref(), Some("thread"));
        drop(daemon);
        std::fs::remove_dir_all(&home.registry_json().parent().unwrap()).ok();
    }

    #[test]
    fn a_failed_viewport_launch_still_delivers_and_rebinds() {
        let _guard = crate::path_test_guard();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let daemon = rt.block_on(async {
            crate::codex_fake_daemon::FakeDaemon::start(crate::codex_fake_daemon::Behavior::quick())
        });
        let home = tmp_home("wake-launchfail");
        push_codex_pane_row(&home, "w1", "sess-1");
        let entry = codex_pane_entry_json();
        let mut io = ScriptedViewport::new("idle");
        io.fail_launch = true;
        let absent = |_s: &str, _p: u64| PaneProbe::Absent;
        let loaded_ok = || Ok(vec!["sess-1".to_string()]);
        let code = codex_resume_route(
            "w1",
            &entry,
            "sess-1",
            Some("go"),
            "/tmp/x",
            "w1",
            &[],
            &home,
            &absent,
            &loaded_ok,
            &io,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert!(daemon.first_params("turn/start").is_some());
        let row = read_row(&home, "sess-1").unwrap();
        assert!(row.mux.is_none());
        drop(daemon);
        std::fs::remove_dir_all(&home.registry_json().parent().unwrap()).ok();
    }
}

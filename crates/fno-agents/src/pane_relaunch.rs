//! How a pane relaunch carries its identity : the `mux pane run`
//! argv builder and the `env(1)` assignment run that names the relaunched
//! worker. Extracted from client_verbs (over the file budget, shrink-only):
//! the code the identity change touched moved here with the change.

/// The `env(1)` assignment run that carries a row's mesh identity into a
/// relaunched pane: the same pairs `_mesh_env_wrapper` writes at spawn
/// (mux_spawn.py), identity tokens ONLY. The account and model ride
/// `--settings <path>` / the job's saved launch, so no value from inside a
/// route file can reach a printed command (re-pins #830 AC5). Err when a
/// token cannot ride an assignment: spawn validates names at mint, but the
/// resume path reads them back from the registry, so the wrap re-validates
/// and the caller refuses the relaunch rather than emit a shape
/// `agent_self_from_argv` cannot parse.
pub(crate) fn mesh_identity_assignments(
    name: &str,
    harness: &str,
    node: Option<&str>,
) -> Result<Vec<String>, String> {
    // An empty harness or node is OPTIONAL provenance (a degenerate row
    // can carry neither field) and is omitted, not written as an empty
    // assignment; an empty NAME is the one hard error - the wrapper exists
    // to carry it. `node` is the backlog node id (ReentryPlan::node /
    // RegistryEntry::node) -- a distinct axis from fno_id, the thread/session
    // identity. A caller that passes fno_id here stamps a session id into
    // FNO_NODE, which is what happened at both call sites before this fix.
    let mut pairs: Vec<(&str, &str)> = vec![("FNO_AGENT_SELF", name)];
    if !harness.is_empty() {
        pairs.push(("FNO_AGENT_HARNESS", harness));
    }
    if let Some(id) = node.filter(|id| !id.is_empty()) {
        pairs.push(("FNO_NODE", id));
    }
    for (key, value) in &pairs {
        if value.is_empty() || value.contains('=') || value.contains('\n') {
            return Err(format!(
                "row identity token {key}={value:?} cannot ride an env(1) assignment"
            ));
        }
    }
    Ok(pairs.iter().map(|(k, v)| format!("{k}={v}")).collect())
}

/// The row name a relaunched pane may carry as `--worker`, or `None` when the
/// name cannot ride the flag: the mux server validates it with the same
/// registry charset (`[A-Za-z0-9._-]`, <= 64 chars) and refuses the WHOLE
/// `pane run` on a bad token, so omitting the token keeps the relaunch alive
/// (unjoined) instead of failing it. Mirrors `squad_store::valid_worker_name`,
/// which lives across the crate boundary fno-agents cannot link.
fn worker_token(name: &str) -> Option<&str> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    ok.then_some(name)
}

/// Build the `mux pane run` argv (everything after the `fno` binary) that
/// relaunches `claude_argv` on a new pane in `session` at `cwd`. The `--` fence
/// keeps a `--resume <uuid>` (or any flag-shaped inner arg) out of the mux
/// parser, so the resumed command is transported verbatim - the one-verb form
/// of the manual `fno mux pane run 'cd <wt> && exec claude --resume <uuid>'`
/// recovery recipe (D3). `identity` rides as an `env(1)` assignment
/// run INSIDE the fence (W1): the server's `agent_self_from_argv`
/// reads exactly this shape to title the pane, and the same assignments set
/// the env the session-start restamp keys on - without it a relaunched pane
/// comes back anonymous, titled from the command basename. `worker` rides as
/// `--worker` BEFORE the fence, matching what spawn passes at
/// `mux_spawn.py`: the server records the pane as a squad member joined to
/// that row, so the relaunch survives a mux restart as an idle resumable row.
/// Callers keep any `which_on_path` check on the UNWRAPPED harness argv; the
/// wrap happens here.
pub(crate) fn mux_pane_run_argv(
    session: &str,
    cwd: &str,
    claude_argv: &[String],
    identity: &[String],
    worker: Option<&str>,
) -> Vec<String> {
    let mut v: Vec<String> = vec![
        "mux".into(),
        "pane".into(),
        "run".into(),
        "--server".into(),
        session.into(),
        "--cwd".into(),
        cwd.into(),
    ];
    if let Some(name) = worker.and_then(worker_token) {
        v.push("--worker".into());
        v.push(name.into());
    }
    v.push("--".into());
    if !identity.is_empty() {
        v.push("env".into());
        v.extend(identity.iter().cloned());
    }
    v.extend(claude_argv.iter().cloned());
    v
}

use std::path::Path;
use std::time::{Duration, Instant};

use crate::client_verbs::shlex_quote;
use crate::pane_stop::{pane_list_via_fno, PaneSighting};
use crate::scrape::mux_pane_read;

// (crates/fno `EXIT_CONTROL_UNANSWERED`). Duplicated here rather than
// imported: this crate does not depend on `fno`, and `fno mux pane run` is
// invoked as a subprocess, not a library call.
const MUX_CONTROL_UNANSWERED: i32 = 20;

/// Map a failed `fno mux pane run` (the relauncher's subprocess) to its
/// stderr message. Kept pure so the exit-code split is mechanically testable
/// apart from the launch itself.
pub(crate) fn mux_pane_run_failure_message(
    verb: &str,
    name: &str,
    session: &str,
    status: std::process::ExitStatus,
) -> String {
    if status.code() == Some(MUX_CONTROL_UNANSWERED) {
        return format!(
            "fno agents {verb}: the mux never answered the run for {name}; a pane \
             MAY have started. Check `fno mux pane ls --session {session}` before \
             retrying."
        );
    }
    format!(
        "fno agents {verb}: mux pane run for {name} exited {} (no pane started)",
        status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string())
    )
}

/// How long a relaunched pane gets to prove the worker stayed up (mirrors
/// `_BINDING_WINDOW_S` in mux_spawn.py), and how often it is polled
/// (`_BINDING_POLL_S`). Both fit inside the watchdog's 180s resume timeout.
const PANE_PROOF_WINDOW: Duration = Duration::from_secs(8);
const PANE_PROOF_POLL: Duration = Duration::from_millis(750);

/// The verdict of one pane-launch proof. The asymmetry is load-bearing: a
/// false `Died` tells the operator a live worker is dead; a false `Unproven`
/// costs one retry. Ambiguity therefore never promotes to `Died`.
#[derive(Debug)]
pub(crate) enum PaneProof {
    Live { child_pid: u32 },
    Died { tail: String },
    Unproven { tail: String, why: String },
}

/// The four reads a proof may make, injected so the ranking is testable with
/// no mux and no wall clock. A `None` answer means "could not run" - evidence
/// of nothing, never of death.
pub(crate) struct PaneProbes<'a> {
    /// `fno mux pane wait <id> --server <s> --timeout 0` exit code.
    pub wait_exit: &'a dyn Fn() -> Option<i32>,
    /// `pane ls --json`; `None` = unreadable.
    pub listing: &'a dyn Fn() -> Option<Vec<PaneSighting>>,
    /// `pane read <id> --json` text.
    pub read_tail: &'a dyn Fn() -> Option<String>,
    /// Whether a pid is alive.
    pub pid_alive: &'a dyn Fn(u32) -> bool,
}

pub(crate) fn prove_pane_worker(
    pane_id: u64,
    window: Duration,
    poll: Duration,
    probes: &PaneProbes,
    sleep: &dyn Fn(Duration),
) -> PaneProof {
    let started = Instant::now();
    let mut tail = String::new();
    loop {
        // Every tick reads the tail first and keeps the newest NON-EMPTY read:
        // a TUI that has not painted yet reads empty, and the mux drops a
        // dead pane's buffer at reap, so the tail must be captured before
        // that. Keep the last 20 lines - the receipt shows context, not
        // scrollback.
        if let Some(t) = (probes.read_tail)() {
            if !t.trim().is_empty() {
                tail = last_lines(&t, 20);
            }
        }
        match (probes.wait_exit)() {
            // EXIT_WAIT_EXITED: the pane's child is gone - death, observed
            // directly.
            Some(12) => return PaneProof::Died { tail },
            // Settled-quiet and wait-timeout both say the pane survived
            // the tick.
            Some(0) | Some(11) => {}
            // Anything else (error, unreachable server, probe refused) is not
            // evidence; ask the listing.
            _ => match (probes.listing)() {
                // Absent from a NON-EMPTY listing is death. An empty or
                // unreadable listing proves nothing - an unreachable session
                // also answers empty - so it marks the tick unknown.
                Some(listing) if !listing.is_empty() => {
                    if !listing.iter().any(|p| p.pane_id == pane_id) {
                        return PaneProof::Died { tail };
                    }
                }
                _ => {}
            },
        }
        // The first pass always made one full look, so a pane that was
        // already dead reads Died above, never Unproven; the window only
        // bounds how long a slow starter is waited for.
        if started.elapsed() >= window {
            break;
        }
        sleep(poll);
    }
    // Window over with no death seen: one final listing decides.
    match (probes.listing)() {
        Some(listing) if !listing.is_empty() => match listing.iter().find(|p| p.pane_id == pane_id)
        {
            Some(sighting) => match sighting.child_pid {
                Some(pid) if (probes.pid_alive)(pid) => PaneProof::Live { child_pid: pid },
                _ => PaneProof::Unproven {
                    tail,
                    why: format!("pane {pane_id} is listed but its child pid is missing or dead"),
                },
            },
            None => PaneProof::Died { tail },
        },
        _ => PaneProof::Unproven {
            tail,
            why: "the mux did not answer".to_string(),
        },
    }
}

fn last_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max);
    lines[start..].join("\n")
}

/// The Died receipt: what the pane showed, then the attended escape hatch.
fn pane_death_receipt(
    verb: &str,
    row_name: &str,
    pane_id: u64,
    window_secs: u64,
    tail: &str,
) -> String {
    let shown = if tail.trim().is_empty() {
        "(the pane exited before any output was captured)".to_string()
    } else {
        tail.to_string()
    };
    format!(
        "fno agents {verb}: {row_name} relaunched on pane {pane_id}, but the worker exited \
         within {window_secs}s. Last pane output:\n{shown}\nRun `fno agents {verb} {row_name} \
         --print-command` and run it attended to read the full error."
    )
}

/// Relaunch a worker on its mux pane and PROVE it stayed up before this verb
/// claims success. `pane run` answers `PaneSpawned` the moment the PTY child
/// starts, so its exit 0 proves a pane was created, never that the worker
/// lived - the lie the 2026-09-13 specimen shipped. Both the resume and the
/// recover pane arms launch through here.
///
/// Returns 0 only on a proof of life (with the row rebound to the new pane),
/// 1 on a launch that never produced a pane, 16 on a death or an unproven
/// launch (matching the two existing not-confirmed receipts on this verb).
pub(crate) fn relaunch_on_pane(
    verb: &str,
    row_name: &str,
    harness: &str,
    session_id: &str,
    cwd: &str,
    session: &str,
    pane_argv: &[String],
    env: &[(String, String)],
    expected_mux: Option<&crate::state::MuxRef>,
    events: (&str, &str),
    home: &crate::paths::AgentsHome,
) -> i32 {
    relaunch_on_pane_with(
        PANE_PROOF_WINDOW,
        PANE_PROOF_POLL,
        verb,
        row_name,
        harness,
        session_id,
        cwd,
        session,
        pane_argv,
        env,
        expected_mux,
        events,
        home,
        || crate::resume_gate::admit_revival(home, verb, row_name, std::path::Path::new(cwd)),
    )
}

/// [`relaunch_on_pane`] with the proof window and poll injected, so tests run
/// a zero window instead of waiting out the real one.
pub(crate) fn relaunch_on_pane_with<A>(
    window: Duration,
    poll: Duration,
    verb: &str,
    row_name: &str,
    harness: &str,
    session_id: &str,
    cwd: &str,
    session: &str,
    pane_argv: &[String],
    env: &[(String, String)],
    expected_mux: Option<&crate::state::MuxRef>,
    events: (&str, &str),
    home: &crate::paths::AgentsHome,
    admit: A,
) -> i32
where
    A: FnOnce() -> Result<crate::spawn_gate::GateGuard, i32>,
{
    // Hold admission through the pane proof so the slot count cannot miss
    // the row before it becomes visible.
    let _admission = match admit() {
        Ok(guard) => guard,
        Err(code) => {
            crate::resume_gate::release_revival_claims(session_id);
            return code;
        }
    };
    // Launch. stdin null so a pane run that reads stdin cannot stall against
    // the caller's terminal; stdout piped (the pane id); stderr inherited.
    let mut command = std::process::Command::new("fno");
    command
        .args(pane_argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    for (key, value) in env {
        command.env(key, value);
    }
    let output = match command.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("fno agents {verb}: failed to launch {row_name} on a mux pane: {e}");
            return 1;
        }
    };
    if !output.status.success() {
        // A non-zero launch emits nothing: no pane, nothing happened.
        eprintln!(
            "{}",
            mux_pane_run_failure_message(verb, row_name, session, output.status)
        );
        return 1;
    }
    let pane_id = pane_id_from_stdout(&String::from_utf8_lossy(&output.stdout));
    let events_path = crate::client_verbs::trace_events_path(home);
    let Some(pane_id) = pane_id else {
        // A zero exit with no pane id on stdout: the launch cannot be tied to
        // a pane, so the worker cannot be proven. Keep the claim (a pane may
        // still exist); the TTL bounds the window a second writer waits.
        append_launch_event(
            &events_path,
            events.1,
            row_name,
            harness,
            session_id,
            cwd,
            None,
            Some("unproven"),
            Some("pane run printed no pane id"),
        );
        eprintln!(
            "fno agents {verb}: launched for {row_name}, but could not prove the worker \
             live: pane run printed no pane id. Check `fno mux pane ls --server {session}` \
             before retrying."
        );
        return 16;
    };

    let probes = PaneProbes {
        wait_exit: &|| pane_wait_exit(session, pane_id),
        listing: &|| Some(pane_list_via_fno(Some(session))),
        read_tail: &|| mux_pane_read(std::ffi::OsStr::new("fno"), session, pane_id),
        pid_alive: &|pid| crate::daemon::process_start_time(pid).is_some(),
    };
    let proof = prove_pane_worker(pane_id, window, poll, &probes, &|d| std::thread::sleep(d));
    match proof {
        PaneProof::Live { child_pid } => {
            let rebind = crate::state::update_registry(
                &home.registry_json(),
                |reg| -> Result<Option<u64>, String> {
                    // Compare-and-set on the mux ref read BEFORE the launch,
                    // the same guard the scrape sweep's WriteDisposition
                    // applies: a concurrent re-home is never overwritten.
                    let row = reg
                        .entries
                        .iter_mut()
                        .find(|r| r.name == row_name)
                        .ok_or_else(|| format!("row {row_name} is no longer in the registry"))?;
                    if row.mux.as_ref() != expected_mux {
                        return Err(format!(
                            "the row now names pane {} while the relaunch ran",
                            row.mux.as_ref().map(|m| m.pane_id).unwrap_or(0)
                        ));
                    }
                    row.mux = Some(crate::state::MuxRef {
                        session: session.to_string(),
                        pane_id,
                    });
                    row.pid = Some(child_pid);
                    row.pid_start_time = crate::daemon::process_start_time(child_pid);
                    row.status = crate::AgentStatus::Live;
                    Ok(expected_mux.map(|m| m.pane_id))
                },
            );
            // The worker IS live either way, so the resume event is honest in
            // both branches; only the rebind receipt differs.
            append_launch_event(
                &events_path,
                events.0,
                row_name,
                harness,
                session_id,
                cwd,
                Some(pane_id),
                None,
                None,
            );
            match rebind {
                Ok(Ok(_)) => {
                    println!(
                        "fno agents {verb}: {row_name} is live on pane {pane_id} in mux \
                         session {session} (pid {child_pid})"
                    );
                    0
                }
                Ok(Err(reason)) => {
                    let old = expected_mux
                        .map(|m| m.pane_id.to_string())
                        .unwrap_or_else(|| "none".to_string());
                    eprintln!(
                        "fno agents {verb}: worker live on pane {pane_id}, but the row still \
                         names pane {old}: {reason}"
                    );
                    16
                }
                Err(e) => {
                    let old = expected_mux
                        .map(|m| m.pane_id.to_string())
                        .unwrap_or_else(|| "none".to_string());
                    eprintln!(
                        "fno agents {verb}: worker live on pane {pane_id}, but the row still \
                         names pane {old}: {e}"
                    );
                    16
                }
            }
        }
        PaneProof::Died { tail } => {
            // Leave the row untouched: it already names a dead pane, and the
            // reconcile sweep owns liveness.
            append_launch_event(
                &events_path,
                events.1,
                row_name,
                harness,
                session_id,
                cwd,
                Some(pane_id),
                Some("pane-exited"),
                None,
            );
            // Release the session claim so a retry after the operator fixes
            // the cause is not refused for the TTL window against a pane that
            // no longer exists.
            let _ = crate::claims::release(
                &format!("session:{session_id}"),
                &format!("resume:{}", std::process::id()),
                None,
                None,
            );
            eprintln!(
                "{}",
                pane_death_receipt(verb, row_name, pane_id, window.as_secs(), &tail)
            );
            16
        }
        PaneProof::Unproven { tail, why } => {
            // Keep the claim: the pane may still be live and the TTL guards
            // against a second writer.
            append_launch_event(
                &events_path,
                events.1,
                row_name,
                harness,
                session_id,
                cwd,
                Some(pane_id),
                Some("unproven"),
                Some(&why),
            );
            eprintln!(
                "fno agents {verb}: launched pane {pane_id} for {row_name}, but could not \
                 prove the worker live: {why}. Check `fno mux pane ls --server {session}` \
                 before retrying."
            );
            // The tail is evidence the operator cannot get later if the
            // pane dies right after this, so show it when it has content.
            if !tail.trim().is_empty() {
                eprintln!("Last pane output:\n{tail}");
            }
            16
        }
    }
}

fn append_launch_event(
    events_path: &Path,
    kind: &str,
    row_name: &str,
    harness: &str,
    session_id: &str,
    cwd: &str,
    pane_id: Option<u64>,
    reason: Option<&str>,
    why: Option<&str>,
) {
    let mut fields: Vec<(&str, serde_json::Value)> = vec![
        ("name", serde_json::Value::String(row_name.to_string())),
        ("provider", serde_json::Value::String(harness.to_string())),
        (
            "session_id",
            serde_json::Value::String(session_id.to_string()),
        ),
        ("cwd", serde_json::Value::String(cwd.to_string())),
    ];
    if let Some(id) = pane_id {
        fields.push(("pane_id", serde_json::Value::from(id)));
    }
    if let Some(reason) = reason {
        fields.push(("reason", serde_json::Value::String(reason.to_string())));
    }
    if let Some(why) = why {
        fields.push(("why", serde_json::Value::String(why.to_string())));
    }
    crate::client_verbs::append_agents_event(events_path, kind, &fields);
}

/// Exit code of `fno mux pane wait <pane_id> --server <session> --timeout 0`;
/// `None` when the wait could not run or died by signal - both mean "no
/// evidence".
fn pane_wait_exit(session: &str, pane_id: u64) -> Option<i32> {
    std::process::Command::new("fno")
        .args([
            "mux",
            "pane",
            "wait",
            &pane_id.to_string(),
            "--server",
            session,
            "--timeout",
            "0",
        ])
        .stdin(std::process::Stdio::null())
        .status()
        .ok()
        .and_then(|s| s.code())
}

/// `pane run` prints exactly one bare u64 line on stdout (the pane id).
fn pane_id_from_stdout(stdout: &str) -> Option<u64> {
    let text = stdout.trim();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    text.parse::<u64>().ok()
}

/// Provider-specific resume argv, mirroring Python `_build_resume_argv`.
/// Returns `None` for an unsupported provider AND for an unreadable capability
/// contract, but the caller only ever sees the second kind through a narrow
/// door. `interactive_resume_supported` also reads the packaged contract and
/// `unwrap_or(false)`s a failure, so an unreadable contract refuses as "not
/// supported" before this function runs. What actually reaches the caller's
/// "resume contract is invalid" message is a contract that LOADS and declares
/// the form, then fails to render it: a malformed token template.
///
/// The grant and the directory pin ride ONE `cwd` here, which is what the CLI
/// verb lane wants (it validates the cwd exists before launching). The mux
/// gesture needs them SPLIT: the grant follows the directory the worker will
/// actually get, while `--cd` must not pin a fallback directory (AC3-GONE),
/// so it calls [`build_resume_argv_split`] directly.
pub(crate) fn build_resume_argv(
    provider: &str,
    session_id: &str,
    cwd: Option<&str>,
) -> Option<Vec<String>> {
    let cwd = cwd.filter(|c| !c.is_empty());
    build_resume_argv_split(provider, session_id, cwd, cwd.is_some())
}

/// The grant/pin split behind [`build_resume_argv`]: `grant_cwd`
/// decides the codex writable-roots grant (None/empty = no grant), `pin_cd`
/// decides `--cd` independently. The mux gesture grants the directory the
/// worker will actually get and pins it only when it is the row's own
/// recorded cwd - pinning a fallback ($HOME, the squad canonical cwd) raises
/// codex's folder-trust screen, an unattended hang (AC3-GONE).
pub(crate) fn build_resume_argv_split(
    provider: &str,
    session_id: &str,
    grant_cwd: Option<&str>,
    pin_cd: bool,
) -> Option<Vec<String>> {
    let argv = build_resume_argv_tokens_split(provider, session_id, grant_cwd, pin_cd)?;
    crate::harness_capabilities::compose_pre_exec(provider, "interactive_resume", argv).ok()
}

/// Raw resume command tokens before the lane's declared `pre_exec` wrapper.
/// Print-command output uses these tokens because Python prints the paste-ready
/// harness command and does not launch the pre-exec daemon itself.
pub(crate) fn build_resume_argv_tokens_split(
    provider: &str,
    session_id: &str,
    grant_cwd: Option<&str>,
    pin_cd: bool,
) -> Option<Vec<String>> {
    // The declared form is the whole identity: cursor-agent's interactive_resume
    // tokens already end in --trust, and a second one is a duplicated flag,
    // never a stronger one. Python's builder renders the same form with no
    // cursor arm, so runtimes stay byte-identical by rendering and nothing else.
    // Raw render, splice, compose last: the composed `sh -c` script would
    // put a spliced grant OUTSIDE the codex command it must precede.
    let mut argv = crate::harness_capabilities::render_session_argv_raw(
        provider,
        "interactive_resume",
        Some(session_id),
    )
    .ok()?;
    // codex's bounded sandbox re-resolves from config on `resume`, so the git +
    // plan grants ride as `-c` tokens spliced right after the `codex` binary
    // token. (`codex resume` does accept --add-dir; `codex exec resume` is the
    // lane that does not. `-c` is kept because one grant builder serves both.)
    if provider == "codex" {
        // The grant follows the directory the worker will actually get; `--cd`
        // rides separately. An empty grant_cwd is absent for both, which is
        // what Python's `if cwd` does and the parity test pins (AC4-EDGE).
        if let Some(cwd) = grant_cwd.filter(|c| !c.is_empty()) {
            let grant = crate::provider::codex_writable_config_args(Path::new(cwd));
            let grant_len = grant.len();
            if !grant.is_empty() {
                argv.splice(1..1, grant);
            }
            // Without --cd, codex asks session-directory vs current-directory
            // and defaults to the SESSION directory: the canonical checkout
            // recorded at spawn, not the worktree the row works in. Unattended
            // that prompt is a hang. Attended it is a wrong default a human
            // must catch.
            //
            // Conditional, per codex's own docs: the prompt appears only when
            // the process cwd differs from the session's saved directory. The
            // config key `tui.resume_cwd` answers it globally, and --cd
            // outranks that. This lane wants --cd because it is per
            // invocation and names the directory outright.
            //
            // Spliced BEFORE the subcommand, beside the grant, which is the
            // only global-before-subcommand precedent in this tree. The spawn
            // lanes are not it: they spell the flag `-C`, after `exec` in the
            // headless lane and on a bare `codex` in the pane lane. Both
            // positions parse on codex 0.149.1, so this is a choice about
            // where a reader expects a global, not a fix.
            //
            // NO permission bypass rides here, deliberately. A registry row
            // records no sandbox posture, so this lane cannot tell a bounded
            // worker from a yolo one, and an unconditional bypass would resume
            // every bounded worker with approvals off. See the Python twin.
            // Right after the grant, so the token order matches the Python
            // twin exactly. `test_rust_verb_parity` compares the two argvs
            // element for element, so "both are globals" is not enough here.
            // The split caller (the mux gesture) omits `--cd` when it passes
            // `pin_cd == false`: the worker lands on a fallback directory and
            // codex's own session-directory offer is the one a human can take.
            if pin_cd {
                let at = (1 + grant_len).min(argv.len());
                argv.splice(at..at, ["--cd".to_string(), cwd.to_string()]);
            }
        }
    }
    Some(argv)
}

/// The env(1) assignment tokens for one env pair set, prefixed ahead of the
/// argv - the shape the mux verdict prefix prints and both resume print arms
/// (claude's canonical plan, codex's masked route env) reuse.
pub(crate) fn env_prefixed(env: &[(String, String)], argv: &[String]) -> Vec<String> {
    let mut prefixed: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    prefixed.extend(argv.iter().cloned());
    prefixed
}

/// The `--print-command` tail: the pane form when the row has a mux ref, else
/// the in-terminal exec form. Inspection only - shell-quoted paths and ids;
/// any key-masked env already rides `argv` as tokens.
pub(crate) fn print_relaunch_command(
    session: Option<&str>,
    cwd: &str,
    argv: &[String],
    identity: &[String],
    worker: &str,
) {
    if let Some(session) = session {
        let pane = mux_pane_run_argv(session, cwd, argv, identity, Some(worker));
        let quoted = pane
            .iter()
            .map(|a| shlex_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        println!("fno {quoted}");
    } else {
        let quoted = argv
            .iter()
            .map(|a| shlex_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        println!("cd {} && exec {}", shlex_quote(cwd), quoted);
    }
}

/// The `fno-agents resume-argv` verb: render one harness's
/// interactive-resume argv through the ONE builder the CLI verb lane uses,
/// so the mux gesture consumes the same argv instead of re-deriving the
/// declared form and losing the codex writable-roots grant. `--cwd` supplies
/// the grant (and the `--cd` value); `--cd` pins the directory separately,
/// so a fallback directory can be granted without being pinned (AC3-GONE).
/// `--json` prints `{"argv":[...]}`. A harness the capability table does not
/// name (or one whose declared form cannot render) exits 1: the mux gesture
/// treats any failure as the fail-open signal and renders the declared form
/// itself, never a second argv builder.
pub fn run_resume_argv(rest: &[String]) -> i32 {
    let mut positional: Vec<&str> = Vec::new();
    let mut cwd: Option<String> = None;
    let mut pin_cd = false;
    let mut json = false;
    let mut it = rest.iter();
    while let Some(tok) = it.next() {
        match tok.as_str() {
            "--cwd" => match it.next() {
                Some(v) => cwd = Some(v.to_string()),
                None => {
                    eprintln!("resume-argv: --cwd needs a path");
                    return 2;
                }
            },
            "--cd" => pin_cd = true,
            "--json" | "-J" => json = true,
            t if t.starts_with('-') => {
                eprintln!("resume-argv: unknown flag {t}");
                return 2;
            }
            t => positional.push(t),
        }
    }
    if positional.len() != 2 {
        eprintln!(
            "usage: fno-agents resume-argv <harness> <session-id> [--cwd <path>] [--cd] [--json|-J]"
        );
        return 2;
    }
    let harness = positional[0];
    let session_id = positional[1];
    // this verb's stdout is a recipe the mux gesture pastes into a
    // pane, and a pane spawn has no secret-free channel for a route's key
    // (the claude verdict prefix puts `env K=V` on the argv, visible in ps).
    // A routed codex row therefore refuses here by name instead of printing
    // the unrouted recipe, and the caller (and the operator) are pointed at
    // the door that restores the route. No row, or an unrouted row, changes
    // nothing.
    if harness == "codex" {
        if let Some(home) = crate::paths::AgentsHome::from_env_opt() {
            let entries = match crate::client_verbs::read_registry_entries(&home.registry_json()) {
                Ok(e) => e,
                Err(_) => Vec::new(),
            };
            let row = entries.iter().find(|e| {
                e.get("harness_session_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(session_id)
            });
            if let Some(row) = row {
                let name = row
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(session_id);
                let identity = crate::codex_route::row_route_identity(
                    row.get("harness").and_then(serde_json::Value::as_str),
                    row.get("route_provider_id")
                        .and_then(serde_json::Value::as_str),
                    row.get("model_name").and_then(serde_json::Value::as_str),
                );
                let refusal = match identity {
                    Err(reason) => Some(format!(
                        "{name} was launched on a codex route and it cannot be carried \
                         through this door ({reason}); resume it with \
                         `fno agents resume {name}`, which restores the route"
                    )),
                    Ok(Some((provider, _))) => Some(format!(
                        "{name} runs on codex route {provider}; resume it with \
                         `fno agents resume {name}`, which restores the route"
                    )),
                    Ok(None) => None,
                };
                if let Some(line) = refusal {
                    eprintln!("resume-argv: {line}");
                    return crate::reentry::REENTRY_REFUSED_EXIT;
                }
            }
        }
    }
    match build_resume_argv_split(harness, session_id, cwd.as_deref(), pin_cd) {
        Some(argv) => {
            if json {
                println!("{}", serde_json::json!({ "argv": argv }).to_string());
            } else {
                let quoted = argv
                    .iter()
                    .map(|a| shlex_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("{quoted}");
            }
            0
        }
        None => {
            eprintln!("resume-argv: harness {harness} declares no renderable resume form");
            1
        }
    }
}
#[cfg(test)]
mod tests {
    use super::{
        last_lines, mesh_identity_assignments, mux_pane_run_argv, pane_death_receipt,
        prove_pane_worker, worker_token, PaneProbes, PaneProof,
    };
    use crate::pane_stop::PaneSighting;
    use std::time::Duration;

    #[test]
    fn resume_argv_accepts_both_json_spellings() {
        let sid = "test-resume-session".to_string();
        // -J reaches the JSON branch (0), not the unknown-flag refusal (2).
        assert_eq!(
            super::run_resume_argv(&["claude".into(), sid.clone(), "-J".into()]),
            0
        );
        assert_eq!(
            super::run_resume_argv(&["claude".into(), sid, "--json".into()]),
            0
        );
        assert_eq!(super::run_resume_argv(&["claude".into(), "-J".into()]), 2);
    }

    /// Per-probe staged scripts: each probe keeps its OWN counter, so script
    /// slot N is the Nth time that probe is read. The tail and wait read
    /// once per tick; the listing only fires on a non-evidence wait result
    /// and once at the window end. `None` = the probe could not run.
    fn staged_probes(
        tails: Vec<Option<String>>,
        waits: Vec<Option<i32>>,
        listings: Vec<Option<Vec<PaneSighting>>>,
        pid_alive: bool,
    ) -> PaneProbes<'static> {
        let t_tick = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let w_tick = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let l_tick = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let leak = Box::leak(Box::new((tails, waits, listings)));
        let (tails, waits, listings) = (&leak.0, &leak.1, &leak.2);
        PaneProbes {
            read_tail: Box::leak(Box::new(move || {
                let mut t = t_tick.lock().unwrap();
                let answer = tails.get(*t).and_then(|x| x.clone());
                *t = t.saturating_add(1);
                answer
            })),
            wait_exit: Box::leak(Box::new(move || {
                let mut t = w_tick.lock().unwrap();
                let answer = waits.get(*t).copied().flatten();
                *t = t.saturating_add(1);
                answer
            })),
            listing: Box::leak(Box::new(move || {
                let mut t = l_tick.lock().unwrap();
                let answer = listings.get(*t).and_then(|x| x.clone());
                *t = t.saturating_add(1);
                answer
            })),
            pid_alive: Box::leak(Box::new(move |_| pid_alive)),
        }
    }

    fn sighting(pane_id: u64, child_pid: Option<u32>) -> PaneSighting {
        PaneSighting {
            session: "main".to_string(),
            pane_id,
            child_pid,
        }
    }

    fn no_sleep(_: Duration) {}

    // ---- prove_pane_worker (AC1-AC4) -----------------------------------

    #[test]
    fn prove_pane_worker_live_pane_with_live_child_is_live() {
        // AC1-HP: the pane stays in a non-empty listing with a child pid the
        // liveness probe accepts, so the window ends Live with that pid.
        let probes = staged_probes(
            vec![Some("codex 5.0".into()), Some(String::new())],
            vec![Some(11)],
            vec![Some(vec![sighting(4242, Some(7))])],
            true,
        );
        let proof = prove_pane_worker(4242, Duration::ZERO, Duration::ZERO, &probes, &no_sleep);
        match proof {
            PaneProof::Live { child_pid } => assert_eq!(child_pid, 7),
            other => panic!("expected Live, got {other:?}"),
        }
    }

    #[test]
    fn prove_pane_worker_wait_exit_12_is_death_with_the_kept_tail() {
        // AC2-ERR: the first tick keeps a non-empty tail read; the second
        // tick's wait exit 12 (pane's child exited) is death, carrying the
        // captured tail in the receipt.
        let probes = staged_probes(
            vec![
                Some("Error: model provider not found".into()),
                Some(String::new()),
            ],
            vec![Some(11), Some(12)],
            vec![None, None],
            false,
        );
        let proof = prove_pane_worker(
            4242,
            Duration::from_secs(30),
            Duration::ZERO,
            &probes,
            &no_sleep,
        );
        match proof {
            PaneProof::Died { tail } => {
                assert!(tail.contains("model provider not found"), "{tail}")
            }
            other => panic!("expected Died, got {other:?}"),
        }
    }

    #[test]
    fn prove_pane_worker_absent_from_nonempty_listing_on_first_pass_is_died() {
        // AC3-ERR: one full look happens even at a zero window, and absence
        // from a NON-EMPTY listing is death, never Unproven.
        let probes = staged_probes(
            vec![Some(String::new())],
            vec![Some(1)],
            vec![
                Some(vec![sighting(7, Some(9))]),
                Some(vec![sighting(7, Some(9))]),
            ],
            true,
        );
        let proof = prove_pane_worker(4242, Duration::ZERO, Duration::ZERO, &probes, &no_sleep);
        assert!(matches!(proof, PaneProof::Died { .. }), "{proof:?}");
    }

    #[test]
    fn prove_pane_worker_unreadable_everything_is_unproven_never_died() {
        // AC4-EDGE: an empty/unreadable listing and an unrunnable wait prove
        // nothing. Ambiguity never promotes to Died.
        let probes = staged_probes(
            vec![Some(String::new())],
            vec![None],
            vec![Some(Vec::new()), None],
            true,
        );
        let proof = prove_pane_worker(4242, Duration::ZERO, Duration::ZERO, &probes, &no_sleep);
        match proof {
            PaneProof::Unproven { why, .. } => assert_eq!(why, "the mux did not answer"),
            other => panic!("expected Unproven, got {other:?}"),
        }
    }

    #[test]
    fn prove_pane_worker_listed_but_dead_child_is_unproven() {
        // The final listing finds the pane but its child pid is gone: the
        // honest answer is Unproven (a relaunch may be mid-handoff), never
        // Live and never Died.
        let probes = staged_probes(
            vec![Some(String::new())],
            vec![Some(11)],
            vec![Some(vec![sighting(4242, Some(999_999))])],
            false,
        );
        let proof = prove_pane_worker(4242, Duration::ZERO, Duration::ZERO, &probes, &no_sleep);
        match proof {
            PaneProof::Unproven { why, .. } => {
                assert!(why.contains("child pid is missing or dead"), "{why}")
            }
            other => panic!("expected Unproven, got {other:?}"),
        }
    }

    #[test]
    fn last_lines_keeps_only_the_last_twenty_lines() {
        let text = (0..25)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let kept = last_lines(&text, 20);
        assert!(kept.starts_with("line5"));
        assert!(kept.ends_with("line24"));
        assert_eq!(kept.lines().count(), 20);
    }

    #[test]
    fn pane_death_receipt_shows_the_tail_and_the_attended_escape() {
        let with_tail = pane_death_receipt("resume", "w", 4242, 8, "boom: route missing");
        assert!(with_tail.contains("boom: route missing"), "{with_tail}");
        assert!(with_tail.contains("--print-command"), "{with_tail}");
        let silent = pane_death_receipt("resume", "w", 4242, 8, "");
        assert!(
            silent.contains("exited before any output was captured"),
            "{silent}"
        );
    }

    // ---- relaunch_on_pane (AC5-AC8) ------------------------------------

    use super::{mux_pane_run_failure_message, relaunch_on_pane_with};
    use crate::state::{self, MuxRef};
    use crate::{paths::AgentsHome, AgentStatus};
    use std::fs;

    const SESSION: &str = "01a09bcd-8b5f-7391-83f8-d9ed91b00ac5";
    const VERB: &str = "resume";

    /// A codex pane row for `session` on pane `pane_id`, minted through the
    /// typed struct so the registry write carries a valid row.
    fn pane_row(name: &str, session: &str, pane_id: u64) -> state::RegistryEntry {
        let mut row = state::RegistryEntry {
            name: name.to_string(),
            harness: Some("codex".into()),
            harness_session_id: Some(SESSION.into()),
            codex_session_id: Some(SESSION.into()),
            cwd: "/tmp".into(),
            origin: Some("spawn".into()),
            substrate: Some("pane".into()),
            status: AgentStatus::Live,
            mux: Some(MuxRef {
                session: session.to_string(),
                pane_id,
            }),
            ..Default::default()
        };
        row.created_at = "2026-09-13T00:00:00Z".into();
        row
    }

    /// A fake `fno` whose pane verbs answer from scripted files, and whose
    /// `pane run` prints `pane_out`. Returns the PATH value to restore.
    /// Callers hold PATH_TEST_MUTEX + test_env_lock.
    fn stub_fno(
        dir: &std::path::Path,
        pane_out: &str,
        listing: &str,
        read_text: &str,
        wait_exit: &str,
    ) {
        let script = format!(
            "#!/bin/sh\ncase \"$3\" in\nrun) printf '%s\\n' '{pane_out}' ;;\nwait) exit {wait_exit} ;;\nls) printf '%s\\n' '{listing}' ;;\nread) printf '%s\\n' '{read_text}' ;;\nesac\n"
        );
        let stub = dir.join("fno");
        fs::write(&stub, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = fs::metadata(&stub).unwrap().permissions();
            p.set_mode(0o755);
            fs::set_permissions(&stub, p).unwrap();
        }
    }

    /// Run `relaunch_on_pane` with a zero window against a temp home, the
    /// stub on PATH, and the row already in the registry. `expected` is the
    /// mux ref the caller claims the row still carries.
    fn run_relaunch(home: &AgentsHome, expected: Option<&MuxRef>) -> i32 {
        run_relaunch_with_admit(home, expected, || {
            Ok(crate::spawn_gate::GateGuard::default())
        })
    }

    fn run_relaunch_with_admit<A>(home: &AgentsHome, expected: Option<&MuxRef>, admit: A) -> i32
    where
        A: FnOnce() -> Result<crate::spawn_gate::GateGuard, i32>,
    {
        relaunch_on_pane_with(
            Duration::ZERO,
            Duration::ZERO,
            VERB,
            "repro",
            "codex",
            SESSION,
            "/tmp",
            "main",
            &["mux".to_string(), "pane".into(), "run".into()],
            &[],
            expected,
            ("agent_resumed", "agent_resume_failed"),
            home,
            admit,
        )
    }

    fn events_of(home: &AgentsHome) -> String {
        let p = crate::client_verbs::trace_events_path(home);
        fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn relaunch_on_pane_gate_refusal_skips_the_pane_launch() {
        let _path_guard = crate::PATH_TEST_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env_guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let home = AgentsHome::at(dir.path().join("agents"));
        home.ensure_root().unwrap();
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(pane_row("repro", "main", 2179))
        })
        .unwrap();
        let claims_root = dir.path().join("claims-root");
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        for (key, holder) in [
            (
                format!("session:{SESSION}"),
                format!("resume:{}", std::process::id()),
            ),
            (
                "node:x-repro".to_string(),
                format!("target-session:{SESSION}"),
            ),
        ] {
            assert!(matches!(
                crate::claims::acquire(
                    &key,
                    &holder,
                    crate::claims::AcquireOpts {
                        root: Some(claims_root.clone()),
                        ..Default::default()
                    }
                ),
                crate::claims::AcquireOutcome::Acquired(_)
            ));
        }
        stub_fno(
            dir.path(),
            "4242",
            &format!(
                "[{{\"pane_id\":4242,\"child_pid\":{}}}]",
                std::process::id()
            ),
            "{\"text\":\"codex 5.0\"}",
            "11",
        );
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", crate::path_with(dir.path()));
        let expected = MuxRef {
            session: "main".into(),
            pane_id: 2179,
        };
        let code = run_relaunch_with_admit(&home, Some(&expected), || Err(83));
        match old_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        let session_state =
            crate::claims::status(&format!("session:{SESSION}"), Some(&claims_root)).0;
        let node_state = crate::claims::status("node:x-repro", Some(&claims_root)).0;
        std::env::remove_var("FNO_CLAIMS_ROOT");
        assert_eq!(code, 83);
        assert!(matches!(session_state, crate::claims::ClaimState::Free));
        assert!(matches!(node_state, crate::claims::ClaimState::Free));
        assert!(events_of(&home).is_empty());
        let row = &state::load_registry(&home.registry_json()).unwrap().entries[0];
        assert_eq!(row.mux.as_ref().unwrap().pane_id, 2179);
    }

    #[test]
    fn relaunch_on_pane_rebinds_the_row_when_the_worker_proves_live() {
        // AC5-HP: pane run prints 4242; the listing shows 4242 with this test
        // process as the child pid; zero window. The verb exits 0, rebinds
        // the row (pane 4242, this pid, a start time, Live), and records ONE
        // agent_resumed whose name is the ROW name, never the caller token.
        let _path_guard = crate::PATH_TEST_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env_guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let home = AgentsHome::at(dir.path().join("agents"));
        home.ensure_root().unwrap();
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(pane_row("repro", "main", 2179))
        })
        .unwrap();
        stub_fno(
            dir.path(),
            "4242",
            &format!(
                "[{{\"pane_id\":4242,\"child_pid\":{}}}]",
                std::process::id()
            ),
            "{\"text\":\"codex 5.0\"}",
            "11",
        );
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", crate::path_with(dir.path()));
        let expected = MuxRef {
            session: "main".into(),
            pane_id: 2179,
        };
        let code = run_relaunch(&home, Some(&expected));
        match old_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }

        assert_eq!(code, 0, "expected exit 0 on a live proof");
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let row = &reg.entries[0];
        assert_eq!(row.mux.as_ref().unwrap().pane_id, 4242);
        assert_eq!(row.mux.as_ref().unwrap().session, "main");
        assert_eq!(row.pid, Some(std::process::id()));
        assert!(row.pid_start_time.is_some(), "start time recorded");
        assert!(matches!(row.status, AgentStatus::Live));
        let events = events_of(&home);
        let resumed: Vec<&str> = events
            .lines()
            .filter(|l| l.contains("\"agent_resumed\""))
            .collect();
        assert_eq!(resumed.len(), 1, "{events}");
        assert!(resumed[0].contains("\"name\":\"repro\""), "{events}");
        assert!(resumed[0].contains("\"pane_id\":4242"), "{events}");
    }

    #[test]
    fn relaunch_on_pane_reports_death_with_tail_and_releases_the_claim() {
        // AC6-ERR: the listing lacks the new pane (only pane 7 exists), the
        // pane read shows the exit reason, wait errors. Exit 16, ONE
        // agent_resume_failed (reason pane-exited), NO agent_resumed, the row
        // untouched, and the session claim released. The tail's presence in
        // the receipt is pinned by the pane_death_receipt test.
        let _path_guard = crate::PATH_TEST_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env_guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let home = AgentsHome::at(dir.path().join("agents"));
        home.ensure_root().unwrap();
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(pane_row("repro", "main", 2179))
        })
        .unwrap();
        // Pre-acquire the claim the verb would hold, so the release is
        // observable. FNO_CLAIMS_ROOT routes the global key into the tempdir.
        let claims_root = dir.path().join("claims-root");
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        let pre = crate::claims::acquire(
            &format!("session:{SESSION}"),
            &format!("resume:{}", std::process::id()),
            crate::claims::AcquireOpts::default(),
        );
        assert!(matches!(pre, crate::claims::AcquireOutcome::Acquired(_)));

        stub_fno(
            dir.path(),
            "4242",
            "[{\"pane_id\":7}]",
            r#"{"text":"boom: route missing"}"#,
            "1",
        );
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", crate::path_with(dir.path()));
        let expected = MuxRef {
            session: "main".into(),
            pane_id: 2179,
        };
        let code = run_relaunch(&home, Some(&expected));
        match old_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        // Read the claim under the SAME root the release wrote to, before
        // the env var is dropped (a global-root read would answer someone
        // else's live claim).
        let (st, rec) = crate::claims::status(&format!("session:{SESSION}"), None);
        std::env::remove_var("FNO_CLAIMS_ROOT");

        assert_eq!(code, 16, "death is exit 16");
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let row = &reg.entries[0];
        assert_eq!(row.mux.as_ref().unwrap().pane_id, 2179, "row untouched");
        let events = events_of(&home);
        let failed: Vec<&str> = events
            .lines()
            .filter(|l| l.contains("\"agent_resume_failed\""))
            .collect();
        assert_eq!(failed.len(), 1, "{events}");
        assert!(failed[0].contains("\"reason\":\"pane-exited\""), "{events}");
        assert!(failed[0].contains("\"pane_id\":4242"), "{events}");
        assert!(!events.contains("\"agent_resumed\""), "{events}");
        assert!(
            matches!(st, crate::claims::ClaimState::Free),
            "claim must be released: {st:?} {rec:?}"
        );
    }

    #[test]
    fn relaunch_on_pane_skips_the_rebind_when_the_row_was_rehomed() {
        // AC7-EDGE: the worker proves live but the row's mux ref no longer
        // matches the ref read before launch (a concurrent re-home). The row
        // is NOT overwritten, agent_resumed is still appended (the worker IS
        // live), and the verb exits 16 naming both pane ids.
        let _path_guard = crate::PATH_TEST_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _env_guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        let home = AgentsHome::at(dir.path().join("agents"));
        home.ensure_root().unwrap();
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(pane_row("repro", "main", 3000))
        })
        .unwrap();
        stub_fno(
            dir.path(),
            "4242",
            &format!(
                "[{{\"pane_id\":4242,\"child_pid\":{}}}]",
                std::process::id()
            ),
            "{\"text\":\"codex 5.0\"}",
            "11",
        );
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", crate::path_with(dir.path()));
        // The pre-launch ref the caller holds names pane 2179; the row now
        // carries 3000, so the compare-and-set must skip.
        let expected = MuxRef {
            session: "main".into(),
            pane_id: 2179,
        };
        let code = run_relaunch(&home, Some(&expected));
        match old_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }

        assert_eq!(code, 16);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let row = &reg.entries[0];
        assert_eq!(row.mux.as_ref().unwrap().pane_id, 3000, "not overwritten");
        let events = events_of(&home);
        assert!(events.contains("\"agent_resumed\""), "{events}");
    }

    #[test]
    fn relaunch_rebind_keeps_every_specimen_registry_key() {
        // AC8-EDGE: RegistryEntry has no serde catch-all, so a rebind write
        // must keep every key a real codex pane row carries. The specimen is
        // deserialized from the raw JSON shape, rebound, and every key is
        // asserted on the written file. (Load-derived aliases like
        // codex_session_id are skip_serializing by design: harness_session_id
        // is the sole persisted id and backfill re-derives them on load.)
        let specimen = serde_json::json!({
            "name": "specimen",
            "aliases": ["old-name"],
            "provider": "zai",
            "model": "glm-5.3-flash",
            "model_basis": "requested",
            "effort": "high",
            "liveness": "dead",
            "liveness_measured_at": "2026-09-13T22:25:14Z",
            "harness": "codex",
            "harness_session_id": SESSION,
            "codex_session_id": SESSION,
            "launch_account": "default",
            "launch_account_source": "caller",
            "related_session_id": "11111111-2222-3333-4444-555566667777",
            "node": "x-aaaa",
            "requested_model": "glm-5.3-flash[1m]",
            "requested_provider": "zai",
            "requested_effort": "high",
            "host_mode": "interactive",
            "cwd": "/tmp",
            "status": "live",
            "created_at": "2026-09-13T00:00:00Z",
            "pid": 78665,
            "mux": {"session": "main", "pane_id": 2179},
            "substrate": "pane",
            "log_path": "/tmp/specimen.log",
            "last_reconciled_at": "2026-09-13T22:20:00Z",
            "crown_level": 1,
            "crown_scope": "footnote",
            "crown_grantor": "operator",
            "fno_id": "905e16b3-96fc-47ee-8158-2aa4b6e99551",
            "origin": "spawn",
            "route_settings_path": "/route/<sha16>.json",
            "inside_leg": {"state": "done", "seq": 3, "received_at": "2026-09-13T22:00:00Z"},
        });
        let row: state::RegistryEntry = serde_json::from_value(specimen).unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let home = AgentsHome::at(dir.path().join("agents"));
        home.ensure_root().unwrap();
        state::update_registry(&home.registry_json(), |r| r.entries.push(row)).unwrap();

        // The same rebind the Live branch performs, verbatim.
        let child_pid = std::process::id();
        let new_mux = MuxRef {
            session: "main".into(),
            pane_id: 4242,
        };
        let expected = MuxRef {
            session: "main".into(),
            pane_id: 2179,
        };
        state::update_registry(&home.registry_json(), |reg| {
            let r = reg
                .entries
                .iter_mut()
                .find(|r| r.name == "specimen")
                .unwrap();
            if r.mux.as_ref() != Some(&expected) {
                panic!("CAS miss in test setup");
            }
            r.mux = Some(new_mux.clone());
            r.pid = Some(child_pid);
            r.pid_start_time = crate::daemon::process_start_time(child_pid);
            r.status = AgentStatus::Live;
        })
        .unwrap();

        let written = fs::read_to_string(home.registry_json()).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&written).unwrap();
        let back = &raw["agents"][0];
        for key in [
            "aliases",
            "provider",
            "model",
            "model_basis",
            "effort",
            "liveness",
            "liveness_measured_at",
            "harness",
            "harness_session_id",
            "launch_account",
            "launch_account_source",
            "related_session_id",
            "node",
            "requested_model",
            "requested_provider",
            "requested_effort",
            "host_mode",
            "cwd",
            "pid",
            "mux",
            "substrate",
            "log_path",
            "last_reconciled_at",
            "crown_level",
            "crown_scope",
            "crown_grantor",
            "fno_id",
            "origin",
            "route_settings_path",
            "inside_leg",
        ] {
            assert!(
                back.get(key).map(|v| !v.is_null()).unwrap_or(false),
                "key {key} was dropped by the rebind write: {written}"
            );
        }
        assert_eq!(back["mux"]["pane_id"], 4242);
        // Rebind-updated fields carry the new facts.
        assert_eq!(back["pid"], serde_json::json!(child_pid));
        assert!(back["pid_start_time"].is_u64(), "start time written");
    }

    #[test]
    fn mux_pane_run_argv_fences_the_resumed_command() {
        // D3 + W1: the one-verb form of the manual recovery now
        // carries the row's identity past the fence, in the same `env(1)`
        // assignment-run shape `_mesh_env_wrapper` writes at spawn and
        // `agent_self_from_argv` reads for the pane title. The `--` fence
        // keeps the inner `--resume <uuid>` (and any flag-shaped arg) out of
        // the mux parser, so the resumed command is transported verbatim.
        // AC5: only a path appears, never a value from inside the file.
        let claude = vec![
            "claude".to_string(),
            "--settings".into(),
            "/route/path.json".into(),
            "--resume".into(),
            "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9".into(),
        ];
        let identity =
            mesh_identity_assignments("x-bbbb-mux-chrome", "claude", Some("x-bbbb")).unwrap();
        assert!(identity.iter().all(|t| t.starts_with("FNO_")));
        let pane = mux_pane_run_argv("main", "/wt", &claude, &identity, Some("x-bbbb-mux-chrome"));
        assert_eq!(
            pane,
            vec![
                "mux".to_string(),
                "pane".into(),
                "run".into(),
                "--server".into(),
                "main".into(),
                "--cwd".into(),
                "/wt".into(),
                "--worker".into(),
                "x-bbbb-mux-chrome".into(),
                "--".into(),
                "env".into(),
                "FNO_AGENT_SELF=x-bbbb-mux-chrome".into(),
                "FNO_AGENT_HARNESS=claude".into(),
                "FNO_NODE=x-bbbb".into(),
                "claude".into(),
                "--settings".into(),
                "/route/path.json".into(),
                "--resume".into(),
                "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9".into(),
            ]
        );
        // The fence sits exactly between the mux transport and the command,
        // and the wrapper follows it: `agent_self_from_argv` answers only an
        // argv that STARTS with `env` (env_assignments_start), so assert the
        // token itself, never the absence of a basename.
        assert_eq!(pane.iter().position(|a| a == "--"), Some(9));
        assert_eq!(pane[10], "env");
        assert_eq!(pane[11], "FNO_AGENT_SELF=x-bbbb-mux-chrome");
        assert_eq!(pane[12], "FNO_AGENT_HARNESS=claude");
        // Route values never enter the wrapper; the path rides `--settings`
        // (re-pins #830 AC5 against the identity wrap).
        let joined = pane.join(" ");
        assert!(!joined.contains("FNO_ROUTE=") && !joined.contains("token"));
    }

    #[test]
    fn mux_pane_run_argv_omits_an_unrecordable_worker_name() {
        // AC2-EDGE: a name the mux server's `valid_worker_name` rejects would
        // refuse the whole `pane run`, so the builder omits `--worker` and the
        // rest of the argv is unchanged - the relaunch proceeds unjoined
        // rather than failing on a token the store would drop anyway.
        let argv = vec!["codex".to_string(), "resume".into(), "s-1".into()];
        let pane = mux_pane_run_argv("main", "/wt", &argv, &[], Some("bad name"));
        assert!(!pane.contains(&"--worker".to_string()));
        assert_eq!(pane.iter().position(|a| a == "--"), Some(7));
        // The happy-path token sits before the fence, where spawn puts it.
        let joined = mux_pane_run_argv("main", "/wt", &argv, &[], Some("t-ok_1.2")).join(" ");
        assert!(joined.contains("--worker t-ok_1.2 --"));
        assert_eq!(worker_token(""), None);
        assert_eq!(worker_token(&"x".repeat(65)), None);
        assert_eq!(worker_token("x"), Some("x"));
    }

    #[test]
    fn mesh_identity_assignments_refuse_tokens_that_cannot_ride_env() {
        // A token carrying '=' or a newline cannot ride an env(1) assignment;
        // `agent_self_from_argv` would mis-parse or miss it. Registry names
        // are validated at mint; resume re-validates at the wrap and refuses.
        assert!(mesh_identity_assignments("bad=name", "claude", None).is_err());
        assert!(mesh_identity_assignments("bad\nname", "claude", None).is_err());
        assert!(mesh_identity_assignments("", "claude", None).is_err());
        assert!(mesh_identity_assignments("ok", "c=l", None).is_err());
        // An empty harness or fno_id is optional provenance, not an error:
        // it is omitted (a degenerate row must still print/launch carrying
        // its name), never written as an empty assignment.
        let a = mesh_identity_assignments("ok", "claude", Some("")).unwrap();
        assert_eq!(a, vec!["FNO_AGENT_SELF=ok", "FNO_AGENT_HARNESS=claude"]);
        let b = mesh_identity_assignments("ok", "", None).unwrap();
        assert_eq!(b, vec!["FNO_AGENT_SELF=ok"]);
    }

    #[test]
    fn mux_pane_run_failure_message_names_unanswered_not_absent() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::ExitStatus;
        // exit 20 (EXIT_CONTROL_UNANSWERED): the verb reached the server, so
        // the message must say a pane MAY have started - never the blanket
        // "(no pane started)" the other codes get. Parameterized by verb: the
        // recover arm's twin no longer prints the resume prefix.
        let msg = mux_pane_run_failure_message(
            "resume",
            "worker-A",
            "main",
            ExitStatus::from_raw(20 << 8),
        );
        assert!(msg.contains("MAY have started"), "{msg}");
        assert!(msg.contains("pane ls --session main"), "{msg}");
        assert!(!msg.contains("no pane started"), "{msg}");
        assert!(msg.starts_with("fno agents resume:"), "{msg}");
        let msg = mux_pane_run_failure_message(
            "recover",
            "worker-A",
            "main",
            ExitStatus::from_raw(20 << 8),
        );
        assert!(msg.starts_with("fno agents recover:"), "{msg}");

        // Every other non-zero code keeps the original, stronger claim.
        let msg = mux_pane_run_failure_message(
            "resume",
            "worker-A",
            "main",
            ExitStatus::from_raw(1 << 8),
        );
        assert!(msg.contains("no pane started"), "{msg}");
    }
}

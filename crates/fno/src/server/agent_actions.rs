//! Off-loop shell-outs for sideline row gestures: the core loop hands each
//! gesture to `fno-agents` as a bounded, fail-open subprocess and reports the
//! outcome as a notice.

use std::time::Duration;

use super::{first_line_or, fno_bin};
use crate::spawn_journal::ReentryVerdict;

/// The reentry refused exit, mirrored (crates/fno never links fno-agents);
/// twin of `reentry::REENTRY_REFUSED_EXIT`.
const REENTRY_REFUSED_EXIT: i32 = 3;

/// Stamp the child command only; the server's environment also reaches pane
/// shells, so it must not inherit the mux caller marker.
fn mux_command(bin: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut command = crate::process_admission::tokio_command(bin);
    command.env("FNO_CALLER_KIND", "mux");
    command
}

/// Why `run_resume_argv` failed. The split is load-bearing: the mux
/// gesture fail-opens to the declared-form render ONLY on `Unavailable`; a
/// `Refused` line names the door that restores the route and spawns nothing.
pub(super) enum ResumeArgvError {
    /// The door refuses by name - a routed codex row whose route cannot ride
    /// an argv-only pane spawn. Never fail-open.
    Refused(String),
    /// The verb is missing, timed out, or answered malformed. Fail-open to
    /// the declared form, flagged degraded, as before.
    Unavailable(String),
}

/// Shell `fno-agents resume-argv <harness> <sid> --cwd <dir> [--cd]
/// --json` OFF the core loop, bounded like `run_reentry_plan`. The one
/// implementation of the codex resume argv the gestures consume - this
/// server never rebuilds it. Every failure shape (timeout, missing binary,
/// unparseable JSON) is a typed `Unavailable` the caller fail-opens to the
/// declared-form render; exit 3 from the verb is a `Refused`: the
/// reason names the restoring door, and nothing spawns.
pub(super) async fn run_resume_argv(
    harness: &str,
    session_id: &str,
    grant_cwd: &str,
    pin_cd: bool,
) -> Result<Vec<String>, ResumeArgvError> {
    const ARGV_TIMEOUT: Duration = Duration::from_secs(20);
    let refused = |o: &std::process::Output| {
        ResumeArgvError::Refused(first_line_or(
            &String::from_utf8_lossy(&o.stderr),
            "resume argv: the codex route cannot ride this door; \
             resume it with `fno agents resume <row>`",
        ))
    };
    let mut command = mux_command(crate::digest_overlay::fno_agents_bin());
    command.args([
        "resume-argv",
        harness,
        session_id,
        "--cwd",
        grant_cwd,
        "--json",
    ]);
    if pin_cd {
        command.arg("--cd");
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    match tokio::time::timeout(ARGV_TIMEOUT, fut).await {
        Err(_) => Err(ResumeArgvError::Unavailable(format!(
            "resume argv for {harness}: timed out"
        ))),
        Ok(Err(_)) => Err(ResumeArgvError::Unavailable(format!(
            "resume argv for {harness}: fno-agents unavailable"
        ))),
        Ok(Ok(o)) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let v: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| {
                ResumeArgvError::Unavailable(format!("resume argv for {harness}: {e}"))
            })?;
            let argv: Vec<String> = v
                .get("argv")
                .and_then(|a| a.as_array())
                .ok_or_else(|| {
                    ResumeArgvError::Unavailable(format!(
                        "resume argv for {harness}: no argv in reply"
                    ))
                })?
                .iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect();
            if argv.is_empty() {
                return Err(ResumeArgvError::Unavailable(format!(
                    "resume argv for {harness}: empty argv"
                )));
            }
            Ok(argv)
        }
        Ok(Ok(o)) if o.status.code() == Some(REENTRY_REFUSED_EXIT) => Err(refused(&o)),
        Ok(Ok(o)) => Err(ResumeArgvError::Unavailable(first_line_or(
            &String::from_utf8_lossy(&o.stderr),
            &format!("resume argv for {harness}: refused"),
        ))),
    }
}

/// Shell `fno-agents <verb> <name>` for a sideline lifecycle gesture,
/// bounded + fail-open (the `run_dispatch_one` idiom): a short outcome notice,
/// never a wedge. The registry poll owns the row's truth, so a lost/failed
/// notice degrades to "the row updates a beat later or stays put", not a silent
/// mutation. `verb` is always a fixed literal; the argv is never a shell string.
/// The raw outcome of one `fno-agents <verb> <name>` lifecycle shell. The
/// captured output rides along so a caller composing several verbs into one
/// notice can quote what the daemon actually said.
struct AgentVerbResult {
    ok: bool,
    stdout: String,
    stderr: String,
    timed_out: bool,
    /// The spawn itself failed (binary missing); the stderr field holds the
    /// fixed word so the renderer can say "unavailable", not "failed".
    unavailable: bool,
}

async fn run_agent_verb(verb: &str, name: &str, timeout: Duration) -> AgentVerbResult {
    let mut command = mux_command(crate::digest_overlay::fno_agents_bin());
    command
        .args([verb, name])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    match tokio::time::timeout(timeout, fut).await {
        Err(_) => AgentVerbResult {
            ok: false,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
            unavailable: false,
        },
        Ok(Err(_)) => AgentVerbResult {
            ok: false,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            unavailable: true,
        },
        Ok(Ok(out)) => AgentVerbResult {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            timed_out: false,
            unavailable: false,
        },
    }
}

/// The one-verb notice text. Failure now quotes the daemon's reason instead
/// of a bare "failed": a refusal that names itself is the difference between
/// a row the operator can act on and one they press `x` at again blindly.
fn render_agent_verb(verb: &str, name: &str, r: &AgentVerbResult) -> String {
    let past = if verb == "stop" { "stopped" } else { "removed" };
    if r.timed_out {
        return format!("{verb} {name}: timed out");
    }
    if r.unavailable {
        return format!("{verb} {name}: unavailable");
    }
    if r.ok {
        return format!("{past} {name}");
    }
    match r.stderr.trim() {
        "" => format!("{verb} {name}: failed"),
        reason => format!("{verb} {name}: failed: {reason}"),
    }
}

pub(super) async fn run_agent_action(verb: &str, name: &str) -> String {
    render_agent_verb(
        verb,
        name,
        &run_agent_verb(verb, name, Duration::from_secs(20)).await,
    )
}

/// Shell the per-harness resume door, which owns the route and any race-time
/// refusal. Its receipt is the useful notice; keep the longer bound for the
/// claude background-resume confirmation poll.
pub(super) async fn run_resume(name: &str) -> String {
    let result = run_agent_verb("resume", name, Duration::from_secs(60)).await;
    render_resume_notice(name, &result)
}

fn resume_output_line(output: &str, last: bool) -> Option<String> {
    let mut lines = output.lines().filter_map(|line| {
        let clean: String = line.chars().filter(|c| !c.is_control()).collect();
        (!clean.trim().is_empty()).then_some(clean)
    });
    if last {
        lines.last()
    } else {
        lines.next()
    }
}

fn render_resume_notice(name: &str, result: &AgentVerbResult) -> String {
    if result.timed_out {
        return format!("resume {name}: timed out");
    }
    if result.unavailable {
        return format!("resume {name}: unavailable");
    }
    if result.ok {
        return resume_output_line(&result.stdout, true)
            .or_else(|| resume_output_line(&result.stderr, true))
            .unwrap_or_else(|| format!("resumed {name}"));
    }
    resume_output_line(&result.stderr, false)
        .or_else(|| resume_output_line(&result.stdout, false))
        .unwrap_or_else(|| format!("resume {name}: failed"))
}

/// The daemon's own last non-empty stdout line, for notices that quote the
/// verdict verbatim - "claude row already absent" is the fact that unstuck
/// the operator when the CLI did this by hand. One extraction shared by
/// every rm-quoting notice builder, so they cannot drift.
fn daemon_verdict(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// The remove leg: rm alone. Since law d-81c6da7e the daemon's rm
/// ends a live row's process itself, so the gesture never composes a stop.
pub(super) async fn run_remove(name: &str) -> String {
    let rm = run_agent_verb("rm", name, Duration::from_secs(20)).await;
    measure_remove_notice(name, &rm)
}

/// Pure; the testable half of [`run_remove`]. Both notice shapes
/// name the row, so the client's row stamp resolves against the notice.
fn measure_remove_notice(name: &str, rm: &AgentVerbResult) -> String {
    if !rm.ok {
        return render_agent_verb("rm", name, rm);
    }
    match daemon_verdict(&rm.stdout) {
        Some(v) => format!("removed {name} ({v})"),
        None => render_agent_verb("rm", name, rm),
    }
}

/// Map `fno-agents reap --json` stdout to the `reaped N` notice. The verb
/// exited zero, so the reap ran; unparseable output still reports a success
/// with an unknown count rather than a false failure (the row-vanish is the
/// authoritative truth, this notice is advisory).
fn reap_notice(stdout: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        Ok(v) => match v.get("reaped").and_then(|r| r.as_array()) {
            Some(arr) => format!("reaped {}", arr.len()),
            None => "reaped 0".to_string(),
        },
        Err(_) => "reap: done".to_string(),
    }
}

/// Shell `fno-agents reap --json` once for the bulk-reap gesture,
/// bounded + fail-open like [`run_agent_action`]: on success parse the `reaped`
/// array length into a visible `reaped N` count (zero is a successful `reaped
/// 0`), else a bounded failure notice. The argv is a fixed literal.
pub(super) async fn run_reap() -> String {
    const REAP_TIMEOUT: Duration = Duration::from_secs(20);
    let mut command = mux_command(crate::digest_overlay::fno_agents_bin());
    command
        // --no-mux keeps this gesture on its registry-row contract:
        // the 20s bound kills only the direct child, so a mux tab sweep that
        // outlives it would keep closing visible tabs detached. That half
        // needs the operator verb, which has no fixed bound.
        .args(["reap", "--json", "--no-mux"])
        // The bounded caller reads partial stderr on a timeout; the
        // env asks the sweep for its per-row progress lines. A sweep run
        // without a reader (the daemon's idle tick) stays silent.
        .env("FNO_REAP_PROGRESS", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = match crate::process_admission::tokio_spawn(&mut command) {
        Ok(c) => c,
        Err(_) => return "reap: unavailable".to_string(),
    };
    // Drain both pipes concurrently so a timeout can still read how
    // far the sweep got: the child's partial stderr progress names rows
    // scanned / removed / the row in flight (gc_sweep's stderr lines).
    let stdout_task = child.stdout.take().map(|mut pipe| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut buf).await;
            buf
        })
    });
    let stderr_task = child.stderr.take().map(|mut pipe| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut pipe, &mut buf).await;
            buf
        })
    });
    let stdout_str = |t: Option<tokio::task::JoinHandle<Vec<u8>>>| async move {
        match t {
            Some(task) => String::from_utf8_lossy(&task.await.unwrap_or_default()).to_string(),
            None => String::new(),
        }
    };
    match tokio::time::timeout(REAP_TIMEOUT, child.wait()).await {
        Err(_) => {
            let _ = child.kill().await;
            let stderr = stdout_str(stderr_task).await;
            format!(
                "reap: timed out at 20s; {}. Retrying is safe: the sweep is per-row and a removed row stays gone.",
                reap_progress_note(&stderr)
            )
        }
        Ok(Err(_)) => "reap: unavailable".to_string(),
        Ok(Ok(status)) if status.success() => {
            let out = stdout_str(stdout_task).await;
            reap_notice(&out)
        }
        Ok(Ok(_)) => "reap: failed".to_string(),
    }
}

/// Compress the sweep's partial stderr progress into one clause:
/// rows scanned, rows removed before the deadline, and the row in flight.
/// One line prefix (`reap: `) with a word per phase; anything unparseable
/// degrades to "no rows scanned" in the notice rather than a guessed count.
#[test]
fn reap_notice_maps_reaped_count() {
    // AC1-HP: the reaped array length is the visible count.
    assert_eq!(
        reap_notice(r#"{"reaped":["a","b","c"],"kept_dirty":[]}"#),
        "reaped 3"
    );
    // AC1-EDGE: zero candidates is a successful visible `reaped 0`, not an
    // error and not silence.
    assert_eq!(reap_notice(r#"{"reaped":[],"kept_dirty":[]}"#), "reaped 0");
    // A missing `reaped` key (schema drift) reads as zero reaped.
    assert_eq!(reap_notice(r#"{"kept_dirty":[]}"#), "reaped 0");
    // The verb exited zero, so unparseable stdout still reports success (the
    // row-vanish is authoritative), never a false failure.
    assert_eq!(reap_notice("not json"), "reap: done");
}

/// Compress the sweep's partial stderr progress into one clause:
/// rows scanned, rows removed before the deadline, and the row in flight.
/// One line prefix (`reap: `) with a word per phase; anything unparseable
/// degrades to "no rows scanned" in the notice rather than a guessed count.
fn reap_progress_note(stderr: &str) -> String {
    let mut scanned = 0usize;
    let mut removed = 0usize;
    let mut removed_seen = false;
    let mut in_flight: Option<&str> = None;
    for line in stderr.lines() {
        if let Some(name) = line.strip_prefix("reap: scan ") {
            scanned += 1;
            in_flight = Some(name);
        } else if let Some(rest) = line.strip_prefix("reap: removed ") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                removed = n;
                removed_seen = true;
            }
        } else if let Some(name) = line.strip_prefix("reap: cascade ") {
            in_flight = Some(name);
        }
    }
    if scanned == 0 && !removed_seen {
        return "no rows scanned before the deadline".to_string();
    }
    format!(
        "scanned {scanned} row(s), removed {removed} before the deadline, in flight: {}",
        in_flight.unwrap_or("none")
    )
}

/// Shell `fno-agents rename <token> --name <new>` off-loop with the same 20s
/// bound as [`run_agent_action`]. The SUCCESS notice is the verb's OWN printed
/// line ("renamed <old> -> <new>", resolved under the registry lock), never a
/// caller-side reconstruction: a rename racing between resolve and shell must
/// not be reported with a label the row no longer carried. A refusal surfaces
/// stderr's first line.
pub(super) async fn run_agent_rename(token: &str, new_name: &str) -> Result<String, String> {
    const RENAME_TIMEOUT: Duration = Duration::from_secs(20);
    let mut command = mux_command(crate::digest_overlay::fno_agents_bin());
    command
        .args(["rename", token, "--name", new_name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    match tokio::time::timeout(RENAME_TIMEOUT, fut).await {
        Err(_) => Err(format!("rename {token}: timed out")),
        Ok(Err(_)) => Err(format!("rename {token}: fno-agents unavailable")),
        Ok(Ok(out)) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line = first_line_or(&stdout.trim(), &format!("renamed {token} -> {new_name}"));
            Ok(line.to_string())
        }
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            Err(first_line_or(&stderr, &format!("rename {token}: refused")).to_string())
        }
    }
}

/// Resolve one row's re-entry plan through the canonical resolver
/// (`fno-agents reentry-plan <name> --transition <t>`), OFF the core loop and
/// bounded. The account/route verdict is the one implementation every gesture
/// consumes - this server never rebuilds it. Every failure shape (timeout,
/// missing binary, non-zero refusal, malformed JSON, `resolved != true`) is a
/// typed `Err` the caller surfaces as a notice; no pane starts on it.
pub(super) async fn run_reentry_plan(
    name: &str,
    transition: &str,
) -> Result<ReentryVerdict, String> {
    const PLAN_TIMEOUT: Duration = Duration::from_secs(20);
    let mut command = mux_command(crate::digest_overlay::fno_agents_bin());
    command
        .args(["reentry-plan", name, "--transition", transition])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    match tokio::time::timeout(PLAN_TIMEOUT, fut).await {
        Err(_) => Err(format!("re-entry plan for {name}: timed out")),
        Ok(Err(_)) => Err(format!("re-entry plan for {name}: fno-agents unavailable")),
        Ok(Ok(o)) if o.status.success() => {
            let verdict =
                ReentryVerdict::from_plan_json(&o.stdout).map_err(|e| format!("{name}: {e}"))?;
            // A pane cannot host a bg launcher: the launcher backgrounds the
            // session and exits at once, so the pane would hold a dead shell
            // while the row's real transport is a thread. Refuse and name the
            // door that runs the same plan off-pane.
            if verdict.mechanism.as_deref() == Some("bg-resume") {
                return Err(format!(
                    "{name}: re-enters as a background thread (bg-resume), not a pane; \
                     run `fno agents resume {name}` for it"
                ));
            }
            Ok(verdict)
        }
        Ok(Ok(o)) => Err(first_line_or(
            &String::from_utf8_lossy(&o.stderr),
            &format!("re-entry plan for {name}: refused"),
        )),
    }
}

/// Shell `fno agents mail send <name> <text>` off-loop, bounded + capturing:
/// the CLI's one-line stdout verdict (`msg-<id> delivered|queued`) becomes the
/// notice verbatim; a nonzero exit surfaces the first stderr line. Never silent
/// (Locked Decision 6). Uses the `fno` porcelain; argv array only.
pub(super) async fn run_mail_send(name: &str, text: &str) -> String {
    const MAIL_TIMEOUT: Duration = Duration::from_secs(20);
    // `--` ends option parsing so operator text starting with `-` (e.g. a reply
    // of `--help`) is delivered as the message, not consumed as a CLI flag.
    let mut command = mux_command(fno_bin());
    command
        .args([
            "agents",
            "mail",
            "send",
            "--from-name",
            "mux-peek",
            "--",
            name,
            text,
        ])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    match tokio::time::timeout(MAIL_TIMEOUT, fut).await {
        Err(_) => format!("mail {name}: timed out"),
        Ok(Err(_)) => format!("mail {name}: unavailable"),
        Ok(Ok(o)) if o.status.success() => first_line_or(
            &String::from_utf8_lossy(&o.stdout),
            &format!("mailed {name}"),
        ),
        Ok(Ok(o)) => first_line_or(
            &String::from_utf8_lossy(&o.stderr),
            &format!("mail {name}: failed"),
        ),
    }
}

impl super::Core {
    /// Shell `fno-agents <verb> <name>` OFF the core loop, mirroring
    /// `dispatch_next`: the one-line outcome routes back as a `DispatchResult`
    /// notice, but the AUTHORITATIVE row change is the registry poll's exited
    /// flip / row vanish, not this notice. `verb` is a fixed literal
    /// (`"stop"`/`"rm"`), never operator text; `name` was catalog-validated by
    /// the caller.
    pub(super) fn agent_action(&self, id: u64, verb: &'static str, name: String) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_agent_action(verb, &name).await;
            let _ = core_tx
                .send(super::CoreMsg::DispatchResult { id, notice })
                .await;
        });
    }

    /// Rename a row's label off-loop: the `agent_action` mirror with the new
    /// label as a second argv token. The notice is the verb's own report.
    pub(super) fn agent_rename_action(&self, id: u64, token: String, new_name: String) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_agent_rename(&token, &new_name)
                .await
                .unwrap_or_else(|e| e);
            let _ = core_tx
                .send(super::CoreMsg::DispatchResult { id, notice })
                .await;
        });
    }

    /// The remove gesture: rm alone, off-loop like the sibling
    /// actions. The daemon's rm ends a live row's process itself.
    pub(super) fn remove_agent_action(&self, id: u64, name: String) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_remove(&name).await;
            let _ = core_tx
                .send(super::CoreMsg::DispatchResult { id, notice })
                .await;
        });
    }
}

/// Resolve the restore verb's claude re-entry plans OFF the core
/// loop: squad members resolve their `resume` transition, held portals their
/// `attach` transition (keyed `portal:<name>` so one row that is both a
/// member and a held portal resolves each transition it actually needs).
/// Every failure shape is that row's visible refusal, the same typed `Err`
/// a single plan carries.
pub(super) async fn resolve_restore_plans(
    claude_names: Vec<String>,
    portal_names: Vec<String>,
) -> std::collections::HashMap<String, Result<ReentryVerdict, String>> {
    let mut set = tokio::task::JoinSet::new();
    for name in claude_names {
        set.spawn(async move {
            let verdict = run_reentry_plan(&name, "resume").await;
            (name, verdict)
        });
    }
    for name in portal_names {
        set.spawn(async move {
            let verdict = run_reentry_plan(&name, "attach").await;
            (format!("portal:{name}"), verdict)
        });
    }
    let mut plans = std::collections::HashMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((name, verdict)) = joined {
            plans.insert(name, verdict);
        }
    }
    plans
}

#[cfg(test)]
mod tests {
    use super::*;

    // The old `remove_agent_live_row_refused_stop_first` and the

    // stop-then-rm composition tests are gone with the contracts they
    // pinned: RemoveAgent shells rm alone, covered end to end by the
    // fake-binary test below.

    // -- corpse-safe stop + honest reap timeout -----------------------

    fn verb_result(ok: bool, stdout: &str, stderr: &str) -> AgentVerbResult {
        AgentVerbResult {
            ok,

            stdout: stdout.into(),

            stderr: stderr.into(),

            timed_out: false,

            unavailable: false,
        }
    }

    #[test]

    fn render_agent_verb_quotes_daemon_reason_on_failure() {
        // a bare "stop X: failed" is the same silence the reap timeout

        // used to end with; the daemon's reason is the operator's next action.

        let r = verb_result(false, "", "agent corpse is a pane worker; kill the pane");

        assert_eq!(
            render_agent_verb("stop", "corpse", &r),
            "stop corpse: failed: agent corpse is a pane worker; kill the pane"
        );

        let ok = verb_result(true, "", "");

        assert_eq!(render_agent_verb("stop", "corpse", &ok), "stopped corpse");
    }

    #[test]
    fn measure_remove_notice_quotes_the_daemon_verdict() {
        // rm ok with a daemon verdict: the notice quotes it
        // verbatim and names the row, so the client's row stamp resolves.
        let rm = verb_result(
            true,
            "removed: corpse (fno; claude row already absent)\n",
            "",
        );
        let notice = measure_remove_notice("corpse", &rm);
        assert!(notice.starts_with("removed corpse"), "{notice}");
        assert!(
            notice.contains("claude row already absent"),
            "quotes the verdict: {notice}"
        );
    }

    #[test]
    fn measure_remove_notice_names_the_refusal() {
        // rm refused (the roster still lists the session): the
        // notice names the row and the reason, failure-marked, so the row
        // stays stamped with why. The quoted reason is rm's own claude
        // refusal shape: rm ran its stop, the roster still lists.
        let rm = verb_result(
            false,
            "",
            "agent corpse is still live. rm ran `claude stop`, and its harness row \
             cccccccc is still present in `claude agents --json --all`",
        );
        let notice = measure_remove_notice("corpse", &rm);
        assert!(notice.starts_with("rm corpse: failed: "), "{notice}");
        assert!(notice.contains("rm ran `claude stop`"), "{notice}");
    }

    #[test]

    fn reap_progress_note_names_scanned_removed_and_in_flight() {
        // the timeout notice must say how far the sweep got.

        let stderr = "reap: scan alpha\nreap: scan beta\nreap: removed 1\nreap: cascade beta\n";

        assert_eq!(
            reap_progress_note(stderr),
            "scanned 2 row(s), removed 1 before the deadline, in flight: beta"
        );

        // A deadline hit before the first row classifies says so, not "0".

        assert_eq!(
            reap_progress_note(""),
            "no rows scanned before the deadline"
        );
    }

    /// Serialized process env for the tests that pin FNO_AGENTS_BIN /

    /// FNO_AGENTS_HOME; every other test in this binary reads the same env.

    fn fno_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pins the env pair for one test and restores whatever was there on

    /// drop, assert failures included.

    struct PinnedAgentEnv {
        prev_bin: Option<std::ffi::OsString>,

        prev_home: Option<std::ffi::OsString>,
    }

    impl PinnedAgentEnv {
        fn set(bin: &std::path::Path, home: &std::path::Path) -> Self {
            let prev_bin = std::env::var_os("FNO_AGENTS_BIN");

            let prev_home = std::env::var_os("FNO_AGENTS_HOME");

            std::env::set_var("FNO_AGENTS_BIN", bin);

            std::env::set_var("FNO_AGENTS_HOME", home);

            Self {
                prev_bin,

                prev_home,
            }
        }
    }

    impl Drop for PinnedAgentEnv {
        fn drop(&mut self) {
            match self.prev_bin.take() {
                Some(v) => std::env::set_var("FNO_AGENTS_BIN", v),

                None => std::env::remove_var("FNO_AGENTS_BIN"),
            }

            match self.prev_home.take() {
                Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),

                None => std::env::remove_var("FNO_AGENTS_HOME"),
            }
        }
    }

    fn corpse_fixture(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();

        std::fs::write(
            dir.join("registry.json"),
            r#"{"entries":[{"name":"corpse","cwd":"/w","status":"live","harness":"claude"}]}"#,
        )
        .unwrap();
    }

    fn write_fake_bin(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn resume_fixture(label: &str, body: &str) -> (std::path::PathBuf, PinnedAgentEnv) {
        let tmp =
            std::env::temp_dir().join(format!("fno-resume-probe-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let bin = tmp.join("fake-agents.sh");
        write_fake_bin(&bin, body);
        let env = PinnedAgentEnv::set(&bin, &tmp);
        (tmp, env)
    }

    struct FnoBinGuard(Option<std::ffi::OsString>);

    impl FnoBinGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("FNO_BIN");
            std::env::set_var("FNO_BIN", path);
            Self(previous)
        }
    }

    impl Drop for FnoBinGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("FNO_BIN", value),
                None => std::env::remove_var("FNO_BIN"),
            }
        }
    }

    #[tokio::test]
    async fn mux_mail_send_names_its_arm_without_operator_origin() {
        let _serial = fno_env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let argv_log = tmp.path().join("argv.log");
        let fake_bin = tmp.path().join("fake-fno.sh");
        write_fake_bin(
            &fake_bin,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nprintf 'msg-1 delivered (hosted)\\n'\n",
                argv_log.display()
            ),
        );
        let _fno_bin = FnoBinGuard::set(&fake_bin);

        let receipt = run_mail_send("worker", "--reply-looking-body").await;

        assert_eq!(receipt, "msg-1 delivered (hosted)");
        let argv = std::fs::read_to_string(argv_log).unwrap();
        assert_eq!(
            argv.lines().collect::<Vec<_>>(),
            [
                "agents",
                "mail",
                "send",
                "--from-name",
                "mux-peek",
                "--",
                "worker",
                "--reply-looking-body",
            ]
        );
        assert!(!argv.lines().any(|arg| arg == "--origin"));
    }

    #[tokio::test]

    async fn remove_press_shells_rm_alone() {
        // AC5-HP: one remove press shells `fno-agents rm` exactly
        // once and never `fno-agents stop` - the daemon's rm ends a live
        // row's process itself, so no caller composes a stop leg.

        let _serial = fno_env_lock();

        let tmp = std::env::temp_dir().join(format!("fno-x-aaaa-rm-{}", std::process::id()));

        corpse_fixture(&tmp);

        write_fake_bin(
            &tmp.join("fake-agents.sh"),
            "#!/bin/bash\n\
         printf '%s\\n' \"$*\" >> \"$FNO_AGENTS_HOME/argv.log\"\n\
         echo \"removed: corpse\"\n\
         exit 0\n",
        );

        let _env = PinnedAgentEnv::set(&tmp.join("fake-agents.sh"), &tmp);

        let notice = run_remove("corpse").await;

        assert_eq!(notice, "removed corpse (removed: corpse)");

        let log = std::fs::read_to_string(tmp.join("argv.log")).unwrap_or_default();

        let rm_calls = log.lines().filter(|l| l.trim() == "rm corpse").count();

        let stop_calls = log.lines().filter(|l| l.trim() == "stop corpse").count();

        assert_eq!(rm_calls, 1, "exactly one rm call: {log}");

        assert_eq!(stop_calls, 0, "no stop call: {log}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn resume_success_shells_resume_and_shows_the_cli_receipt() {
        // AC1-HP and AC2-HP: the mux resume gesture delegates once to the
        // resume door and surfaces its codex receipt from stderr.
        let _serial = fno_env_lock();
        let (tmp, _env) = resume_fixture(
            "receipt",
            "#!/bin/bash\n\
             printf '%s\\n' \"$*\" >> \"$FNO_AGENTS_HOME/argv.log\"\n\
             echo 'delivered to resume-probe-codex over the codex daemon' >&2\n\
             exit 0\n",
        );

        let notice = run_resume("resume-probe-codex").await;

        assert_eq!(
            notice,
            "delivered to resume-probe-codex over the codex daemon"
        );
        let log = std::fs::read_to_string(tmp.join("argv.log")).unwrap();
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            ["resume resume-probe-codex"]
        );
        assert!(
            !log.contains("spawn"),
            "resume never forks a new session: {log}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn resume_claude_uses_resume_without_spawning() {
        // AC2-HP: claude uses the same-id resume door, never spawn --resume.
        let _serial = fno_env_lock();
        let (tmp, _env) = resume_fixture(
            "claude",
            "#!/bin/bash\n\
             printf '%s\\n' \"$*\" >> \"$FNO_AGENTS_HOME/argv.log\"\n\
             echo 'claude session resumed'\n\
             exit 0\n",
        );

        let notice = run_resume("claude-worker").await;

        assert_eq!(notice, "claude session resumed");
        let log = std::fs::read_to_string(tmp.join("argv.log")).unwrap();
        assert_eq!(log.lines().collect::<Vec<_>>(), ["resume claude-worker"]);
        assert!(
            !log.contains("spawn"),
            "resume never forks a new session: {log}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn resume_failure_shows_the_refusal_line_without_wrapping_it() {
        // AC3-ERR: the door's refusal is the complete operator notice.
        let _serial = fno_env_lock();
        let (tmp, _env) = resume_fixture(
            "refusal",
            "#!/bin/bash\n\
             printf '%s\\n' \"$*\" >> \"$FNO_AGENTS_HOME/argv.log\"\n\
             printf ' worker became busy before resume \\n' >&2\n\
             exit 13\n",
        );

        let notice = run_resume("raced-worker").await;

        assert_eq!(notice, " worker became busy before resume ");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn resume_timeout_uses_the_sixty_second_bound() {
        // AC4-EDGE: resume waits longer than the ordinary lifecycle verbs,
        // then reports its fixed timeout notice.
        let _serial = fno_env_lock();
        let (tmp, _env) = resume_fixture(
            "timeout",
            "#!/bin/bash\n\
             sleep 80\n",
        );
        let started = std::time::Instant::now();

        let notice = run_resume("slow-worker").await;
        let elapsed = started.elapsed();

        assert_eq!(notice, "resume slow-worker: timed out");
        assert!(
            elapsed >= Duration::from_secs(55),
            "timed out after {elapsed:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    struct PinnedFnoBin {
        previous: Option<std::ffi::OsString>,
    }

    impl PinnedFnoBin {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("FNO_BIN");
            std::env::set_var("FNO_BIN", path);
            Self { previous }
        }
    }

    impl Drop for PinnedFnoBin {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("FNO_BIN", value),
                None => std::env::remove_var("FNO_BIN"),
            }
        }
    }

    struct PinnedCallerKind {
        previous: Option<std::ffi::OsString>,
    }

    impl PinnedCallerKind {
        fn unset() -> Self {
            let previous = std::env::var_os("FNO_CALLER_KIND");
            std::env::remove_var("FNO_CALLER_KIND");
            Self { previous }
        }
    }

    impl Drop for PinnedCallerKind {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("FNO_CALLER_KIND", value),
                None => std::env::remove_var("FNO_CALLER_KIND"),
            }
        }
    }

    #[derive(Clone, Copy)]
    enum RowGesture {
        Resume,
        Stop,
        Remove,
        Mail,
        Reap,
    }

    async fn run_row_gesture(gesture: RowGesture) {
        match gesture {
            RowGesture::Resume => {
                run_resume("agent").await;
            }
            RowGesture::Stop => {
                run_agent_action("stop", "agent").await;
            }
            RowGesture::Remove => {
                run_remove("agent").await;
            }
            RowGesture::Mail => {
                run_mail_send("agent", "hello").await;
            }
            RowGesture::Reap => {
                run_reap().await;
            }
        }
    }

    #[tokio::test]
    async fn mux_row_gestures_stamp_child_and_map_to_one_cli_verb() {
        let _serial = fno_env_lock();
        let _home_guard = crate::pane_send_audit::FNO_AGENTS_HOME_GUARD
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _bin_guard = crate::pane_send_audit::FNO_BIN_GUARD
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _caller_kind = PinnedCallerKind::unset();
        let (tmp, _agents_env) = resume_fixture(
            "gesture-table",
            "#!/bin/bash\n\
             printf 'agents|%s|%s\\n' \"${FNO_CALLER_KIND-unset}\" \"$*\" >> \"$FNO_AGENTS_HOME/argv.log\"\n\
             case \"$1\" in\n\
               resume) echo 'resumed agent' ;;\n\
               stop) echo 'stopped agent' ;;\n\
               rm) echo 'removed: agent' ;;\n\
               reap) printf '{\\\"reaped\\\":[]}\\n' ;;\n\
             esac\n\
             exit 0\n",
        );
        let fno_bin = tmp.join("fake-fno.sh");
        write_fake_bin(
            &fno_bin,
            "#!/bin/bash\n\
             printf 'fno|%s|%s\\n' \"${FNO_CALLER_KIND-unset}\" \"$*\" >> \"$FNO_AGENTS_HOME/argv.log\"\n\
             echo 'msg-1 queued'\n\
             exit 0\n",
        );
        let _fno_bin = PinnedFnoBin::set(&fno_bin);

        let gesture_table = [
            (RowGesture::Resume, "agents|mux|resume agent"),
            (RowGesture::Stop, "agents|mux|stop agent"),
            (RowGesture::Remove, "agents|mux|rm agent"),
            (RowGesture::Mail, "fno|mux|agents mail send -- agent hello"),
            (RowGesture::Reap, "agents|mux|reap --json --no-mux"),
        ];
        for (gesture, _) in gesture_table {
            run_row_gesture(gesture).await;
        }

        let log = std::fs::read_to_string(tmp.join("argv.log")).unwrap();
        let actual: Vec<_> = log.lines().collect();
        let expected: Vec<_> = gesture_table.iter().map(|(_, argv)| *argv).collect();
        assert_eq!(actual, expected);
        assert!(
            std::env::var_os("FNO_CALLER_KIND").is_none(),
            "caller kind belongs to each child command, not the server"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

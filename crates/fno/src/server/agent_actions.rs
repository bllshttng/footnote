//! Off-loop shell-outs for sideline row gestures: the core loop hands each
//! gesture to `fno-agents` as a bounded, fail-open subprocess and reports the
//! outcome as a notice.

use std::time::Duration;

use super::{first_line_or, fno_bin};
use crate::spawn_journal::ReentryVerdict;

#[cfg(test)]
thread_local! {
    /// Hermetic seam over the off-loop re-entry plan: a staged verdict or
    /// refusal comes back without shelling out. Safe across the tokio::spawn
    /// boundary because `#[tokio::test]` defaults to the current-thread
    /// runtime, where the spawned task runs on the test's own thread.
    static REENTRY_PLAN_STUB: std::cell::RefCell<Option<Result<ReentryVerdict, String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Stage (or clear, with `None`) the test verdict `run_reentry_plan` returns.
#[cfg(test)]
pub(crate) fn stage_reentry_plan(plan: Option<Result<ReentryVerdict, String>>) {
    REENTRY_PLAN_STUB.with(|stub| *stub.borrow_mut() = plan);
}

/// Shell `fno-agents <verb> <name>` for a sideline lifecycle gesture (x-76ea),
/// bounded + fail-open (the `run_dispatch_one` idiom): a short outcome notice,
/// never a wedge. The registry poll owns the row's truth, so a lost/failed
/// notice degrades to "the row updates a beat later or stays put", not a silent
/// mutation. `verb` is always a fixed literal; the argv is never a shell string.
/// The raw outcome of one `fno-agents <verb> <name>` lifecycle shell. The
/// captured output rides along so a caller composing several verbs into one
/// notice can quote what the daemon actually said (x-f191).
struct AgentVerbResult {
    ok: bool,
    stdout: String,
    stderr: String,
    timed_out: bool,
    /// The spawn itself failed (binary missing); the stderr field holds the
    /// fixed word so the renderer can say "unavailable", not "failed".
    unavailable: bool,
}

async fn run_agent_verb(verb: &str, name: &str) -> AgentVerbResult {
    const AGENT_ACTION_TIMEOUT: Duration = Duration::from_secs(20);
    let mut command =
        crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin());
    command
        .args([verb, name])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    match tokio::time::timeout(AGENT_ACTION_TIMEOUT, fut).await {
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
    render_agent_verb(verb, name, &run_agent_verb(verb, name).await)
}

/// (x-f191) The sideline's REMOVE gesture, made one-press (scope b): stop
/// then rm, one confirmed intent. A live row is stopped first - its registry
/// flip is exactly what the rm leg reads - and a corpse whose stop no-ops
/// still clears: rm's daemon-side gate probes the live harness roster and
/// removes a provably-absent row (the CLI's "already absent" branch). The
/// stored field decides nothing; the live probe behind rm's gate does.
pub(super) async fn run_stop_or_remove(name: &str) -> String {
    let stop = run_agent_verb("stop", name).await;
    let stop_note = render_agent_verb("stop", name, &stop);
    if stop.unavailable {
        // No binary: the rm leg would fail the same way. Say so once.
        return stop_note;
    }
    let rm = run_agent_verb("rm", name).await;
    compose_stop_remove(&stop_note, name, &rm)
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

/// The combined stop+rm notice: the stop outcome first, then what the rm leg
/// decided. Pure; the testable half of [`run_stop_or_remove`].
fn compose_stop_remove(stop_note: &str, name: &str, rm: &AgentVerbResult) -> String {
    if rm.ok {
        match daemon_verdict(&rm.stdout) {
            Some(v) => format!("{stop_note}; removed it ({v})"),
            None => format!("{stop_note}; removed it"),
        }
    } else {
        format!("{stop_note}; {}", render_agent_verb("rm", name, rm))
    }
}

/// (x-b5d1) The measure-and-remove leg: rm only, no stop leg. An
/// Unmeasured row's stop leg is the leg that times out - nothing answers
/// it - and rm's daemon-side live gate IS the measurement: it probes the
/// live roster, removes a provably-absent row, and refuses a live one
/// with its reason. The verdict is quoted like the composed path does.
pub(super) async fn run_measure_remove(name: &str) -> String {
    let rm = run_agent_verb("rm", name).await;
    measure_remove_notice(name, &rm)
}

/// Pure; the testable half of [`run_measure_remove`]. Both notice shapes
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

/// Shell `fno-agents reap --json` once for the bulk-reap gesture (x-7561),
/// bounded + fail-open like [`run_agent_action`]: on success parse the `reaped`
/// array length into a visible `reaped N` count (zero is a successful `reaped
/// 0`), else a bounded failure notice. The argv is a fixed literal.
pub(super) async fn run_reap() -> String {
    const REAP_TIMEOUT: Duration = Duration::from_secs(20);
    let mut command =
        crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin());
    command
        .args(["reap", "--json"])
        // (x-f191) The bounded caller reads partial stderr on a timeout; the
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
    // (x-f191) Drain both pipes concurrently so a timeout can still read how
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

/// (x-f191) Compress the sweep's partial stderr progress into one clause:
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

/// (x-f191) Compress the sweep's partial stderr progress into one clause:
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
    let mut command =
        crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin());
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

/// (x-d285) Resolve one row's re-entry plan through the canonical resolver
/// (`fno-agents reentry-plan <name> --transition <t>`), OFF the core loop and
/// bounded. The account/route verdict is the one implementation every gesture
/// consumes - this server never rebuilds it. Every failure shape (timeout,
/// missing binary, non-zero refusal, malformed JSON, `resolved != true`) is a
/// typed `Err` the caller surfaces as a notice; no pane starts on it.
pub(super) async fn run_reentry_plan(
    name: &str,
    transition: &str,
) -> Result<ReentryVerdict, String> {
    #[cfg(test)]
    if let Some(staged) = REENTRY_PLAN_STUB.with(|stub| stub.borrow().clone()) {
        return staged;
    }
    const PLAN_TIMEOUT: Duration = Duration::from_secs(20);
    let mut command =
        crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin());
    command
        .args(["reentry-plan", name, "--transition", transition])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    match tokio::time::timeout(PLAN_TIMEOUT, fut).await {
        Err(_) => Err(format!("re-entry plan for {name}: timed out")),
        Ok(Err(_)) => Err(format!("re-entry plan for {name}: fno-agents unavailable")),
        Ok(Ok(o)) if o.status.success() => {
            ReentryVerdict::from_plan_json(&o.stdout).map_err(|e| format!("{name}: {e}"))
        }
        Ok(Ok(o)) => Err(first_line_or(
            &String::from_utf8_lossy(&o.stderr),
            &format!("re-entry plan for {name}: refused"),
        )),
    }
}

/// (x-9c5f) Shell `fno agents mail send <name> <text>` off-loop, bounded + capturing:
/// the CLI's one-line stdout verdict (`msg-<id> delivered|queued`) becomes the
/// notice verbatim; a nonzero exit surfaces the first stderr line. Never silent
/// (Locked Decision 6). Uses the `fno` porcelain; argv array only.
pub(super) async fn run_mail_send(name: &str, text: &str) -> String {
    const MAIL_TIMEOUT: Duration = Duration::from_secs(20);
    // `--` ends option parsing so operator text starting with `-` (e.g. a reply
    // of `--help`) is delivered as the message, not consumed as a CLI flag.
    let mut command = crate::process_admission::tokio_command(fno_bin());
    command
        .args(["agents", "mail", "send", "--", name, text])
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
    /// Shell `fno-agents <verb> <name>` OFF the core loop (x-76ea), mirroring
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

    /// (x-b5d1) The measuring remove: rm only (the gate in the RemoveAgent
    /// arm already refused a live row), off-loop like the sibling actions.
    pub(super) fn remove_agent_action(&self, id: u64, name: String, measure: bool) {
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = if measure {
                run_measure_remove(&name).await
            } else {
                run_stop_or_remove(&name).await
            };
            let _ = core_tx
                .send(super::CoreMsg::DispatchResult { id, notice })
                .await;
        });
    }

    /// (x-b5d1) The registry row's liveness, for the measuring-remove gate.
    /// Resolution is by the resolved label; the resolver refused ambiguity,
    /// so at most one non-external row answers.
    pub(super) fn registry_liveness(&self, label: &str) -> Option<crate::agents_view::Liveness> {
        self.agents
            .iter()
            .filter(|a| !a.external && a.name == label)
            .map(|a| a.liveness)
            .next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // (x-f191 scope b) The old `remove_agent_live_row_refused_stop_first` is

    // gone with the contract it pinned: RemoveAgent no longer refuses a

    // stored-live row - it orchestrates stop-then-rm in one confirmed

    // gesture, covered end to end by the fake-binary tests below.

    // -- x-f191 corpse-safe stop + honest reap timeout -----------------------

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
        // x-f191: a bare "stop X: failed" is the same silence the reap timeout

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

    fn compose_stop_remove_reaches_the_already_absent_branch() {
        // The operator's measured case: stop no-ops on the corpse, the rm leg

        // removes it, and the notice quotes the daemon's own verdict.

        let rm = verb_result(
            true,
            "removed: corpse (fno; claude row already absent)\n",
            "",
        );

        let notice = compose_stop_remove("stop corpse: failed: no such session", "corpse", &rm);

        assert!(
            notice.contains("already absent"),
            "quotes the verdict: {notice}"
        );

        assert!(
            notice.starts_with("stop corpse: failed"),
            "stop outcome leads: {notice}"
        );

        // A refusal is named, not swallowed: the row stays and says why.

        let refused = verb_result(false, "", "agent corpse is still live - stop it first");

        let notice = compose_stop_remove("stopped corpse", "corpse", &refused);

        assert!(notice.contains("; rm corpse: failed: agent corpse is still live"));

        // The ordinary one-gesture case: stopped, then removed.

        let clean = verb_result(true, "", "");

        let notice = compose_stop_remove("stopped corpse", "corpse", &clean);

        assert_eq!(notice, "stopped corpse; removed it");
    }

    #[test]
    fn measure_remove_notice_quotes_the_daemon_verdict() {
        // (x-b5d1) rm ok with a daemon verdict: the notice quotes it
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
        // (x-b5d1) rm refused (the roster still lists the session): the
        // notice names the row and the reason, failure-marked, so the row
        // stays stamped with why.
        let rm = verb_result(false, "", "agent corpse is still live - stop it first");
        let notice = measure_remove_notice("corpse", &rm);
        assert_eq!(
            notice,
            "rm corpse: failed: agent corpse is still live - stop it first"
        );
    }

    #[test]

    fn reap_progress_note_names_scanned_removed_and_in_flight() {
        // x-f191: the timeout notice must say how far the sweep got.

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

    #[tokio::test]

    async fn stop_falls_through_to_rm_on_a_stored_live_corpse() {
        // The operator's measured case, end to end: the registry says live,

        // the harness says gone. Stop fails honestly; the rm leg reaches the

        // already-absent branch and the notice quotes its verdict.

        let _serial = fno_env_lock();

        let tmp = std::env::temp_dir().join(format!("fno-x-f191-corpse-{}", std::process::id()));

        corpse_fixture(&tmp);

        write_fake_bin(
            &tmp.join("fake-agents.sh"),
            "#!/bin/bash\n\

         if [ \"$1\" = \"stop\" ]; then\n\

         echo \"claude stop corpse failed: agent not found\" >&2\n\

         exit 1\n\

         fi\n\

         echo \"removed: corpse (fno; claude row already absent)\"\n\

         exit 0\n",
        );

        let _env = PinnedAgentEnv::set(&tmp.join("fake-agents.sh"), &tmp);

        let notice = run_stop_or_remove("corpse").await;

        assert!(
            notice.starts_with("stop corpse: failed: claude stop corpse failed"),
            "stop outcome leads: {notice}"
        );

        assert!(
            notice.contains("removed it (removed: corpse (fno; claude row already absent))"),
            "rm verdict quoted: {notice}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]

    async fn remove_orchestrates_stop_then_rm_on_a_live_row() {
        // (x-f191 scope b) One confirmed gesture: the stop flips the row

        // exited and the rm leg removes it in the same press.

        let _serial = fno_env_lock();

        let tmp = std::env::temp_dir().join(format!("fno-x-f191-live-{}", std::process::id()));

        corpse_fixture(&tmp);

        write_fake_bin(

        &tmp.join("fake-agents.sh"),

        "#!/bin/bash\n\

         if [ \"$1\" = \"stop\" ]; then\n\

         printf '{\"entries\":[{\"name\":\"corpse\",\"cwd\":\"/w\",\"status\":\"exited\"}]}' > \"$FNO_AGENTS_HOME/registry.json\"\n\

         exit 0\n\

         fi\n\

         echo \"removed: corpse\"\n\

         exit 0\n",

    );

        let _env = PinnedAgentEnv::set(&tmp.join("fake-agents.sh"), &tmp);

        let notice = run_stop_or_remove("corpse").await;

        assert_eq!(notice, "stopped corpse; removed it (removed: corpse)");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

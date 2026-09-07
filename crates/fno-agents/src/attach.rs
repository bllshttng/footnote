//! `fno-agents attach <name>`: may fno reach this row's live session, and
//! how. The daemon-kept lane (the mux thread portal) is tried first for
//! every harness; then a harness's own declared attach form, then pi's
//! join; claude attaches inline under its re-entry plan; every other row is
//! asked of the capability table's `features.attach` claim and refused by
//! name. Split out of `client_verbs.rs`, which is over the file budget.

use std::path::Path;

use serde_json::Value;

use crate::claude_ask::ClaudeHome;
use crate::client_verbs::{
    append_agents_event, claude_attach_pointer, echo_extra, is_codex_thread_row, py_repr_str,
    read_registry_entries, resolve_entry_with_heal, trace_events_path, validate_lifecycle_name,
    which_on_path,
};
use crate::paths::AgentsHome;

/// The mux thread portal, `fno mux thread <name>`, reached through the mux
/// front door like the pane relaunch above: the fno crate is a dev-only
/// dependency here, so the reach is a process, not a call. `None` falls
/// through to the inline path: no live server, usage (a deployed mux older
/// than the verb), an unanswered verb, or no `fno` binary at all. Any
/// other non-zero exit is the server refusing; surface it verbatim and
/// never double-attach.
fn attach_via_mux_thread(name: &str, harness: &str, events_path: &Path) -> Option<i32> {
    let output = std::process::Command::new("fno")
        .args(["mux", "thread", name])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let code = output.status.code().unwrap_or(1);
    match code {
        0 => {
            print!("{}", String::from_utf8_lossy(&output.stdout));
            append_agents_event(
                events_path,
                "agent_attached",
                &[
                    ("name", Value::String(name.to_string())),
                    (
                        "provider",
                        Value::String(
                            if harness.is_empty() {
                                "claude"
                            } else {
                                harness
                            }
                            .to_string(),
                        ),
                    ),
                    ("route", Value::String("mux-thread-pane".to_string())),
                ],
            );
            Some(0)
        }
        MUX_THREAD_NO_SERVER | MUX_THREAD_USAGE | MUX_THREAD_UNANSWERED => None,
        other => {
            let text = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            eprint!("{}", String::from_utf8_lossy(text));
            Some(other)
        }
    }
}

/// The `fno mux thread` exits that mean "no server reached", mirrored from
/// `fno::mux_cli` (a dev-only dependency) and pinned against it by
/// `mux_thread_fallthrough_exits_match_the_mux`.
const MUX_THREAD_NO_SERVER: i32 = 24;
const MUX_THREAD_USAGE: i32 = 2;
const MUX_THREAD_UNANSWERED: i32 = 20;

/// The declared state of `features.attach` for `harness`. A row that
/// declares no attach claim, an unknown harness and an unreadable contract
/// all read `unmeasured`: the honest answer to a row nobody measured.
fn attach_feature_state(harness: &str) -> String {
    crate::harness_capabilities::HarnessContract::packaged()
        .ok()
        .and_then(|contract| {
            contract
                .capabilities(harness)
                .ok()
                .and_then(|caps| caps.features.get("attach").map(|claim| claim.state.clone()))
        })
        .unwrap_or_else(|| "unmeasured".to_string())
}

/// The inline refusal for a non-claude row that neither the daemon-kept
/// lane nor a declared form took: `(event reason, exit code, message)`.
/// `native` means the harness can be attached to and the wired arm is the
/// mux thread portal the caller already tried, so the lane is down: exit 24,
/// the same code the portal verb uses for "no live mux server". Any other
/// state refuses by name with the key, the state and the probe that settles
/// the row, exit 13.
fn attach_table_refusal(harness: &str, name: &str) -> (String, i32, String) {
    attach_refusal_for_state(&attach_feature_state(harness), harness, name)
}

fn attach_refusal_for_state(state: &str, harness: &str, name: &str) -> (String, i32, String) {
    let label = if harness.is_empty() {
        "an undeclared harness"
    } else {
        harness
    };
    if state == "native" {
        return (
            "attach-lane-no-server".to_string(),
            MUX_THREAD_NO_SERVER,
            format!(
                "attach to {} ({label}): the capability row records features.attach = \
                 \"native\", and the wired arm is the daemon-kept lane (the mux thread \
                 portal), but this row declares no attachable thread of its own and no \
                 live mux server answered. Start one with 'fno mux serve' and retry; \
                 'fno agents logs {name} --follow' shows live output meanwhile.",
                py_repr_str(name)
            ),
        );
    }
    let capability = match state {
        "absent" => "has no attachable session",
        "capable" => "can hold a live session, but fno has wired no attach arm for it",
        _ => "has not had attach reachability measured",
    };
    // A row with no harness recorded has no capability row to probe: the
    // remedy is the registration, not a probe aimed at nothing.
    let remedy = if harness.is_empty() {
        "Re-register the row with its harness ('fno agents register')".to_string()
    } else {
        format!("Settle the row with 'fno agents harness probe {harness}'")
    };
    (
        format!("features-attach-{state}"),
        13,
        format!(
            "attach to {} ({label}) is refused: its capability row records \
             features.attach = {state:?} - it {capability}. {remedy}. \
             'fno agents logs {name} --follow' still shows live output.",
            py_repr_str(name)
        ),
    )
}

/// (x-296f) Attach through the harness's OWN declared `interactive_attach`
/// form, or `None` when it declares none (the caller then keeps its refusal).
///
/// EXEC, never proxy: this replaces the process, so the terminal's child is
/// the harness's own TUI and fno renders nothing. The argv is the same one the
/// mux viewport renders from the declaration - one declaration, two doors,
/// pinned byte-identical by `attach_argv_matches_the_mux_renderer`.
///
/// The row-shape predicate (`is_codex_thread_row`) is load-bearing: a declared
/// form widens WHICH harnesses can attach, never which row shapes. Cursor
/// Agent is not exempted - its row declares interactive_attach unsupported
/// (a second --resume is a rival TUI, not a join), so the probe below reads
/// "declares none" and the row keeps the generic arms.
fn attach_via_declared_form(
    harness: &str,
    entry: &Value,
    name: &str,
    events_path: &Path,
) -> Option<i32> {
    if !is_codex_thread_row(entry) {
        return None;
    }
    let render = |session: Option<&str>, short: Option<&str>| {
        crate::harness_capabilities::render_session_argv_with_ids(
            harness,
            "interactive_attach",
            session,
            short,
        )
    };
    // A form takes EXACTLY ONE id, and the renderer refuses the other spelling
    // rather than ignoring it - so probe with a placeholder to learn whether
    // this harness declares a form at all, before the row's real (possibly
    // empty) ids can turn "declares none" and "declares one I cannot fill"
    // into the same answer.
    let declares =
        render(Some("probe-session"), None).is_ok() || render(None, Some("probeid")).is_ok();
    if !declares {
        return None;
    }
    let session_id = entry
        .get("harness_session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let short_id = entry
        .get("short_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let argv = match render(Some(session_id), None).or_else(|_| render(None, Some(short_id))) {
        Ok(argv) => argv,
        Err(_) => {
            // A thread with no turns yet has no session id recorded, and the
            // harness resolves a session BY the rollout its first turn writes.
            // Name that rather than handing the operator the vendor's "no
            // rollout found for thread id" (measured 2026-08-28,
            // codex-cli 0.149.1).
            eprintln!(
                "{harness} worker {} has no session id on file yet; nothing to attach to. \
                 Follow it instead: fno agents peek {} --follow",
                py_repr_str(name),
                name
            );
            append_agents_event(
                events_path,
                "agent_attach_refused",
                &[
                    ("name", Value::String(name.to_string())),
                    ("provider", Value::String(harness.to_string())),
                    ("reason", Value::String("no-session-id-yet".to_string())),
                ],
            );
            return Some(13);
        }
    };
    if !which_on_path(&argv[0]) {
        eprintln!("{} not on PATH", argv[0]);
        return Some(14);
    }
    // A TUI needs a terminal. Without this the exec still happens and the
    // operator gets the vendor's bare "stdin is not a terminal" with no clue
    // which command produced it or what to do instead.
    // SAFETY: isatty performs no I/O and only reads the descriptor's mode.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        eprintln!(
            "attach needs a terminal ({harness} draws its own interface, and fno never renders \
             one). From a script, read instead: fno agents peek {name} --follow"
        );
        append_agents_event(
            events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.to_string())),
                ("provider", Value::String(harness.to_string())),
                ("reason", Value::String("no-tty".to_string())),
            ],
        );
        return Some(13);
    }
    append_agents_event(
        events_path,
        "agent_attach_exec",
        &[
            ("name", Value::String(name.to_string())),
            ("provider", Value::String(harness.to_string())),
            ("session_id", Value::String(session_id.to_string())),
        ],
    );
    // Replace this process: the terminal's child is the harness itself. A
    // failed pre-exec inside the composed script still runs the attach, which
    // produces the more specific of the two errors in the terminal the
    // operator is already looking at.
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some(cwd) = entry
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        command.current_dir(cwd);
    }
    let err = command.exec();
    eprintln!("fno agents attach: failed to exec {}: {err}", argv[0]);
    Some(1)
}

/// (x-c198) Attach to a pi session by EXEC'ing pi's own TUI on the same
/// session id, in the row's own cwd.
///
/// `None` means this is not a pi thread row and the caller should fall through
/// to its refusal. `Some(code)` means this function owned the outcome.
///
/// One argv builder, two doors: the mux viewport's `Reach::Drive` arm runs the
/// same command. Neither renders anything.
///
/// **This is a JOIN, and the cwd is what makes it one.** pi's session store is
/// cwd-scoped, so the TUI finds the rpc lane's live session only when it runs
/// in the same directory. Run elsewhere, the same argv CREATES a second
/// session under one id and says nothing, which is the silent half of this
/// harness. So the cwd is read off the row and the child is placed in it; a
/// row with no cwd recorded is refused rather than defaulting to this
/// process's own directory.
fn attach_pi_session(entry: &Value, name: &str, events_path: &Path) -> Option<i32> {
    // NOT `is_codex_thread_row`, and the difference is load-bearing. That
    // predicate excludes a pane-hosted row, which is right for codex (its
    // thread lives in a daemon, and a pane row's process already has a place)
    // and wrong for every pi row fno can produce today: the pi spawn lane IS
    // the pane lane, so gating on it refused every real row with "pi agents
    // are one-shot", contradicting the docs shipped in the same change.
    //
    // pi needs neither exclusion, because a second pi on one session id is a
    // measured-safe JOIN rather than a rival launch. What it does need is the
    // PAIR: a session id, and the cwd that scopes it.
    let session_id = entry
        .get("harness_session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let cwd = entry
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty());
    let Some(cwd) = cwd else {
        eprintln!(
            "fno agents attach: registry row {name:?} records no cwd, and a pi session is \
the pair (cwd, session id). Attaching from the wrong directory would CREATE a second session \
under this id rather than joining the live one, so this is refused."
        );
        append_agents_event(
            events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.to_string())),
                ("provider", Value::String("pi".to_string())),
                ("reason", Value::String("pi-row-has-no-cwd".to_string())),
            ],
        );
        return Some(13);
    };
    if !which_on_path("pi") {
        eprintln!("pi CLI not on PATH");
        return Some(14);
    }
    // The cwd has to EXIST, and checking it here is what lets the NotFound arm
    // below mean the binary and only the binary. A pruned worktree is this
    // fleet's normal lifecycle, and `Command::current_dir` on a missing
    // directory fails with ErrorKind::NotFound, which that arm would report as
    // "pi CLI not on PATH" - sending an operator to reinstall pi over a stale
    // registry row.
    let cwd_path = Path::new(cwd);
    if !cwd_path.is_dir() {
        eprintln!(
            "fno agents attach: the cwd recorded for {name:?} is gone: {cwd}. A pi session is \
the pair (cwd, session id), so there is nothing here to attach to."
        );
        append_agents_event(
            events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.to_string())),
                ("provider", Value::String("pi".to_string())),
                ("reason", Value::String("pi-row-cwd-missing".to_string())),
                ("detail", Value::String(cwd.to_string())),
            ],
        );
        return Some(13);
    }

    // The duplicate refusal, fired at the door where a human reads it. pi's own
    // behaviour on an ambiguous id is to pick the OLDEST file and print
    // nothing, which is how three sessions of real work became unreachable.
    // An `Unknown` reading is NOT a duplicate and never blocks an attach: it
    // means the store could not be read, and an unreadable store is evidence
    // of nothing.
    let lookup = crate::pi::lookup_sessions(cwd_path, session_id);
    if let Some(refusal) = crate::pi::duplicate_resume_refusal(cwd_path, session_id, &lookup) {
        eprintln!("fno agents attach: {refusal}");
        append_agents_event(
            events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.to_string())),
                ("provider", Value::String("pi".to_string())),
                (
                    "reason",
                    Value::String("pi-session-id-ambiguous".to_string()),
                ),
                ("session_id", Value::String(session_id.to_string())),
            ],
        );
        return Some(13);
    }

    // An attach during a live CREATE is a second create, not a join, and the
    // store cannot say so: a session's file appears at the first turn ATTEMPT,
    // so for the first seconds of a spawn the lookup above reads `None` for a
    // session that is being made right now. Joining is safe; creating twice is
    // the silent race this whole lane exists to close, and the only instrument
    // that sees the window is the claim the spawn holds across it.
    //
    // So this reads that claim and refuses while it is held. `Live` and
    // `Suspect` both mean held: `Suspect` is an unexpired TTL whose holder is
    // not provably alive, and the acquire path already declines to steal it.
    // `Free`, `Stale` and `Corrupted` are not evidence of a create in flight
    // and never block an attach - a refusal on an unreadable claim would fail
    // closed against the operator over a file that proves nothing.
    let create_key = crate::pi::create_claim_key(cwd_path, session_id);
    let (claim_state, claim_record) = crate::claims::status(&create_key, None);
    if crate::pi::attach_blocked_by_create(claim_state) {
        let holder = claim_record
            .as_ref()
            .map(|r| r.holder.clone())
            .unwrap_or_else(|| "an unnamed holder".to_string());
        eprintln!(
            "fno agents attach: a pi session CREATE is in flight for {session_id:?} in {cwd}, \
held by {holder}. pi writes its session file at the first turn ATTEMPT, so the store cannot \
tell a session being made right now from one that is absent, and attaching into that window \
CREATES a second session under this id rather than joining. Wait for the holder to finish, \
then attach again."
        );
        append_agents_event(
            events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.to_string())),
                ("provider", Value::String("pi".to_string())),
                ("reason", Value::String("pi-create-in-flight".to_string())),
                ("session_id", Value::String(session_id.to_string())),
                ("detail", Value::String(holder)),
            ],
        );
        return Some(13);
    }

    let argv = crate::pi::pi_attach_argv(session_id);
    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]);
    command.current_dir(cwd);
    // Inherit stdio so pi's TUI takes over this terminal; mirror its exit code.
    match command.status() {
        Ok(status) => {
            let exit_code = status.code().unwrap_or(1);
            append_agents_event(
                events_path,
                "agent_attached",
                &[
                    ("name", Value::String(name.to_string())),
                    ("provider", Value::String("pi".to_string())),
                    ("session_id", Value::String(session_id.to_string())),
                    ("pi_exit", Value::from(exit_code)),
                ],
            );
            Some(exit_code)
        }
        Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("pi CLI not on PATH");
            Some(14)
        }
        Err(exc) => {
            eprintln!("fno agents attach: pi session attach failed: {exc}");
            Some(1)
        }
    }
}

/// `fno-agents attach <name>` -- interactive attach to a running claude agent,
/// a codex thread, or a pi session (every other harness is refused). Mirrors
/// Python `dispatch.attach_agent` + the `cmd_attach` Typer wrapper.
pub fn run_attach(rest: &[String], home: &AgentsHome) -> i32 {
    let mut name: Option<String> = None;
    for a in rest {
        match a.as_str() {
            other if other.starts_with("--") => {
                eprintln!("fno-agents: unknown attach flag: {other}");
                return 2;
            }
            other => {
                if name.is_some() {
                    eprintln!(
                        "fno-agents: attach takes one NAME (got extra: {}).",
                        echo_extra(other)
                    );
                    return 2;
                }
                name = Some(other.to_string());
            }
        }
    }
    let name = match name {
        Some(n) => n,
        None => {
            eprintln!("fno-agents: attach needs a <name>");
            return 2;
        }
    };

    if let Err((code, msg)) = validate_lifecycle_name(&name) {
        eprintln!("{msg}");
        return code;
    }

    let entries = match read_registry_entries(&home.registry_json()) {
        Ok(e) => e,
        Err(exc) => {
            eprintln!("registry read failed: {exc}");
            return 12;
        }
    };
    let entry = match resolve_entry_with_heal(&entries, &name, &home.registry_json()) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("{}", err.message());
            return 2;
        }
    };
    let entry = &entry;

    let harness = entry
        .get("harness")
        .and_then(Value::as_str)
        .or_else(|| entry.get("provider").and_then(Value::as_str))
        .unwrap_or("");
    let events_path = trace_events_path(home);

    // The daemon-kept lane first, for every harness: a live mux server
    // drives the one dedicated thread pane and picks attach, follow or
    // locate per row from capability. A working lane outranks the table
    // read below, which governs only the inline refusal.
    if let Some(code) = attach_via_mux_thread(&name, harness, &events_path) {
        return code;
    }

    // (x-296f) A harness whose contract row DECLARES an interactive_attach
    // form execs it - codex today, whatever a harness declares tomorrow -
    // still gated on the thread-row shape. One mechanism replaces the old
    // two: the declared `pre_exec` starts the harness's own service (codex's
    // `app-server daemon start`) where a separate `ensure_codex_daemon`
    // pre-flight used to refuse, and its `codex-daemon-unavailable` refusal
    // event is gone with it. A failed daemon start still runs the attach and
    // surfaces codex's own more specific error.
    if harness != "claude" {
        if let Some(code) = attach_via_declared_form(harness, entry, &name, &events_path) {
            return code;
        }
    }

    // (x-c198) A pi row execs pi's own TUI on the same session id, which JOINS
    // the session its rpc lane is driving. pi declares no form (its argv
    // carries env-dependent provider/model), so it keeps its own builder.
    if harness == "pi" {
        if let Some(code) = attach_pi_session(entry, &name, &events_path) {
            return code;
        }
    }

    // Every other non-claude harness is asked of the table: features.attach
    // answers reachability, never the verb inventory and never a gap in this
    // file. `!= "claude"` instead of an allowlist so a provider added to the
    // roster inherits the read rather than falling through to a
    // claude-shaped attach (x-51f6 US1).
    if harness != "claude" {
        let (reason, code, message) = attach_table_refusal(harness, &name);
        eprintln!("{message}");
        append_agents_event(
            &events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.clone())),
                ("provider", Value::String(harness.to_string())),
                ("reason", Value::String(reason)),
            ],
        );
        return code;
    }

    let short_id = entry.get("short_id").and_then(Value::as_str).unwrap_or("");
    if short_id.is_empty() {
        eprintln!(
            "registry entry {} has no short id on file; cannot attach.",
            py_repr_str(&name)
        );
        return 12;
    }

    // Attach stays live-only, but a dead claude row (supervisor gone) with a
    // recorded session uuid refuses with the exact revival commands instead of
    // dead-ending in claude's own "session not found" (US3). The decision is a
    // pure helper so it is testable without the exec path.
    if let Some(msg) = claude_attach_pointer(&ClaudeHome::from_env(), entry, &name) {
        eprintln!("{msg}");
        append_agents_event(
            &events_path,
            "agent_attach_refused",
            &[
                ("name", Value::String(name.clone())),
                ("provider", Value::String("claude".to_string())),
                (
                    "reason",
                    Value::String("exited-revivable-pointer".to_string()),
                ),
            ],
        );
        return 13;
    }

    if !which_on_path("claude") {
        eprintln!("claude CLI not on PATH");
        return 14;
    }

    // x-d285: the inline attach consumes the canonical re-entry plan. A fresh
    // claude process re-resolves its account namespace from ambient env, so a
    // bare `claude attach` from the wrong shell lands in the wrong config
    // namespace (the falsified "attach has nothing to do" premise). The plan
    // restores the recorded account namespace and route settings together, or
    // refuses before anything launches. A proven default row keeps the
    // historical bare invocation: the plan carries no env and no --settings.
    let plan = match crate::reentry::resolve_reentry(
        &home.registry_json(),
        &name,
        crate::reentry::ReentryTransition::Attach,
        None,
        None,
    ) {
        Ok(p) => p,
        Err(reason) => {
            eprintln!("fno agents attach: refused: {reason}");
            append_agents_event(
                &events_path,
                "agent_attach_refused",
                &[
                    ("name", Value::String(name.clone())),
                    ("provider", Value::String("claude".to_string())),
                    ("reason", Value::String("reentry-plan-refused".to_string())),
                    ("detail", Value::String(reason)),
                ],
            );
            return crate::reentry::REENTRY_REFUSED_EXIT;
        }
    };
    let mut command = std::process::Command::new(&plan.argv[0]);
    command.args(&plan.argv[1..]);
    for (key, value) in &plan.env {
        command.env(key, value);
    }

    // Inherit stdio so the claude TUI takes over; mirror its exit code.
    match command.status() {
        Ok(status) => {
            let exit_code = status.code().unwrap_or(1);
            append_agents_event(
                &events_path,
                "agent_attached",
                &[
                    ("name", Value::String(name.clone())),
                    ("provider", Value::String("claude".to_string())),
                    ("short_id", Value::String(short_id.to_string())),
                    ("claude_exit", Value::from(exit_code)),
                ],
            );
            exit_code
        }
        Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("claude CLI not on PATH");
            14
        }
        Err(exc) => {
            append_agents_event(
                &events_path,
                "agent_attached",
                &[
                    ("name", Value::String(name.clone())),
                    ("provider", Value::String("claude".to_string())),
                    ("short_id", Value::String(short_id.to_string())),
                    ("claude_exit", Value::Null),
                    ("error", Value::String(exc.to_string())),
                    ("error_type", Value::String("OSError".to_string())),
                ],
            );
            eprintln!("claude attach failed: {exc}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The attach-refusal family (x-296f), a file of its own beside this verb.
    #[path = "x296f_attach_refusals.rs"]
    mod x296f_attach_refusals;

    #[test]
    fn mux_thread_fallthrough_exits_match_the_mux() {
        assert_eq!(MUX_THREAD_NO_SERVER, fno::mux_cli::EXIT_NO_SERVER);
        assert_eq!(MUX_THREAD_USAGE, fno::mux_cli::EXIT_USAGE);
        assert_eq!(MUX_THREAD_UNANSWERED, fno::mux_cli::EXIT_CONTROL_UNANSWERED);
    }

    #[test]
    fn attach_refusal_reads_the_features_row_by_name() {
        // native: the daemon-kept lane was tried and found no server, so the
        // refusal names the portal and takes the portal's own exit code.
        let (reason, code, msg) = attach_refusal_for_state("native", "opencode", "oc");
        assert_eq!(reason, "attach-lane-no-server");
        assert_eq!(code, fno::mux_cli::EXIT_NO_SERVER);
        assert!(msg.contains("features.attach = \"native\""), "{msg}");
        assert!(msg.contains("fno mux serve"), "{msg}");
        // Every other state refuses by name: the key, the state, the probe.
        for (state, capability) in [
            ("absent", "has no attachable session"),
            ("capable", "wired no attach arm"),
            ("unmeasured", "not had attach reachability measured"),
        ] {
            let (reason, code, msg) = attach_refusal_for_state(state, "gemini", "gm");
            assert_eq!(reason, format!("features-attach-{state}"));
            assert_eq!(code, 13);
            assert!(
                msg.contains(&format!("features.attach = {state:?}")),
                "{msg}"
            );
            assert!(msg.contains(capability), "{msg}");
            assert!(msg.contains("fno agents harness probe gemini"), "{msg}");
            assert!(msg.contains("fno agents logs gm --follow"), "{msg}");
        }
        // The packaged table: opencode measured native on the daemon-kept
        // lane; gemini declares no attach claim and reads unmeasured, as
        // does a harness with no row at all.
        assert_eq!(attach_feature_state("opencode"), "native");
        assert_eq!(attach_feature_state("gemini"), "unmeasured");
        assert_eq!(attach_feature_state("no-such-harness"), "unmeasured");
        assert_eq!(
            attach_table_refusal("gemini", "gm").0,
            "features-attach-unmeasured"
        );
        // A row with no harness recorded is sent to register, never to a
        // probe aimed at no harness.
        let (_, code, msg) = attach_refusal_for_state("unmeasured", "", "x");
        assert_eq!(code, 13);
        assert!(msg.contains("fno agents register"), "{msg}");
        assert!(!msg.contains("harness probe"), "{msg}");
    }
}

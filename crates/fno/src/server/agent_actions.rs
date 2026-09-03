//! Off-loop shell-outs for sideline row gestures: the core loop hands each
//! gesture to `fno-agents` as a bounded, fail-open subprocess and reports the
//! outcome as a notice.

use std::time::Duration;

use super::{first_line_or, fno_bin, ReentryVerdict};

/// Shell `fno-agents <verb> <name>` for a sideline lifecycle gesture (x-76ea),
/// bounded + fail-open (the `run_dispatch_one` idiom): a short outcome notice,
/// never a wedge. The registry poll owns the row's truth, so a lost/failed
/// notice degrades to "the row updates a beat later or stays put", not a silent
/// mutation. `verb` is always a fixed literal; the argv is never a shell string.
pub(super) async fn run_agent_action(verb: &str, name: &str) -> String {
    const AGENT_ACTION_TIMEOUT: Duration = Duration::from_secs(20);
    let mut command =
        crate::process_admission::tokio_command(crate::digest_overlay::fno_agents_bin());
    command
        .args([verb, name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_status(&mut command);
    let past = if verb == "stop" { "stopped" } else { "removed" };
    match tokio::time::timeout(AGENT_ACTION_TIMEOUT, fut).await {
        Err(_) => format!("{verb} {name}: timed out"),
        Ok(Err(_)) => format!("{verb} {name}: unavailable"),
        Ok(Ok(status)) if status.success() => format!("{past} {name}"),
        Ok(Ok(_)) => format!("{verb} {name}: failed"),
    }
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

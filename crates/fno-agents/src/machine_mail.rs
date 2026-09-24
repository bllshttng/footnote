//! Binary-direct mail transport for the events and backlog-note adapters.

use crate::paths::AgentsHome;
use std::io::Write;
use std::time::Duration;
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MailArm {
    EventsPush,
    NotePointer,
}

impl MailArm {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "events-push" => Ok(Self::EventsPush),
            "note-pointer" => Ok(Self::NotePointer),
            other => Err(format!(
                "unknown arm {other:?}; expected events-push or note-pointer"
            )),
        }
    }

    fn origin(self) -> Option<&'static str> {
        match self {
            Self::EventsPush => Some("scheduler"),
            Self::NotePointer => None,
        }
    }

    fn fallback_from(self) -> &'static str {
        match self {
            Self::EventsPush => "events-push",
            Self::NotePointer => "note-pointer",
        }
    }
}

struct Request {
    arm: MailArm,
    recipient: String,
    body: String,
    timeout_secs: u64,
}

fn parse_args(args: &[String]) -> Result<Request, String> {
    let mut arm = None;
    let mut recipient = None;
    let mut body = None;
    let mut timeout_secs = 30;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--arm" => {
                let value = it.next().ok_or("--arm needs events-push or note-pointer")?;
                arm = Some(MailArm::parse(value)?);
            }
            "--to" => {
                recipient = Some(it.next().ok_or("--to needs a recipient")?.clone());
            }
            "--body" => {
                body = Some(it.next().ok_or("--body needs text")?.clone());
            }
            "--timeout-secs" => {
                let value = it.next().ok_or("--timeout-secs needs a number")?;
                timeout_secs = value
                    .parse::<u64>()
                    .map_err(|_| format!("bad timeout: {value}"))?;
                if !(1..=120).contains(&timeout_secs) {
                    return Err("--timeout-secs must be between 1 and 120".into());
                }
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Request {
        arm: arm.ok_or("--arm is required")?,
        recipient: recipient.ok_or("--to is required")?,
        body: body.ok_or("--body is required")?,
        timeout_secs,
    })
}

fn sender_name(arm: MailArm, session_id: Option<&str>) -> Option<String> {
    match (arm, session_id) {
        (MailArm::EventsPush, Some(_)) => None,
        (MailArm::EventsPush, None) => Some(arm.fallback_from().into()),
        (MailArm::NotePointer, Some(session_id)) => {
            Some(crate::identity::canonical_handle(session_id))
        }
        (MailArm::NotePointer, None) => Some(arm.fallback_from().into()),
    }
}

fn mail_argv(arm: MailArm, session_id: Option<&str>, recipient: &str, body: &str) -> Vec<String> {
    let mut argv = vec!["agents".into(), "mail".into(), "send".into()];
    if let Some(from_name) = sender_name(arm, session_id) {
        argv.extend(["--from-name".into(), from_name]);
    }
    if arm == MailArm::NotePointer {
        argv.extend(["--lock-timeout".into(), "5".into()]);
    }
    if let Some(origin) = arm.origin() {
        argv.extend(["--origin".into(), origin.into()]);
    }
    argv.push("--".into());
    argv.extend([recipient.into(), body.into()]);
    argv
}

fn current_session_id() -> Option<String> {
    let get = |name: &str| std::env::var(name).ok();
    crate::spawn_context::resolve_self_identity(&get, None, None, &AgentsHome::from_env())
        .session_id
}

fn note_pointer_receipt(stdout: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let msg_id = line.split_whitespace().next().unwrap_or("");
    if line.contains("delivered (hosted)") {
        format!("hosted {msg_id}")
    } else if line.contains("queued (durable)") || line.contains("appended (durable)") {
        format!("durable {msg_id}")
    } else {
        line.to_string()
    }
}

/// Run one fixed machine-mail arm. Callers pass the resolved recipient and
/// body; this layer owns sender identity, origin, and the existing mail CLI.
pub async fn run(args: &[String]) -> i32 {
    let request = match parse_args(args) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("machine-mail-send: {error}");
            return 2;
        }
    };
    let argv = mail_argv(
        request.arm,
        current_session_id().as_deref(),
        &request.recipient,
        &request.body,
    );
    let mut command = Command::new(crate::scrape::fno_bin());
    command
        .args(argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(request.timeout_secs), command.output()).await {
        Err(_) => {
            eprintln!(
                "machine-mail-send: timed out after {}s",
                request.timeout_secs
            );
            124
        }
        Ok(Err(error)) => {
            eprintln!("machine-mail-send: fno unavailable: {error}");
            127
        }
        Ok(Ok(output)) => {
            let stdout = if request.arm == MailArm::NotePointer && output.status.success() {
                format!("{}\n", note_pointer_receipt(&output.stdout)).into_bytes()
            } else {
                output.stdout
            };
            let _ = std::io::stdout().write_all(&stdout);
            let _ = std::io::stderr().write_all(&output.stderr);
            output.status.code().unwrap_or(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{mail_argv, note_pointer_receipt, MailArm};

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn events_push_uses_named_fallback_and_scheduler_origin_without_a_session() {
        assert_eq!(
            mail_argv(MailArm::EventsPush, None, "parent", "body"),
            strings(&[
                "agents",
                "mail",
                "send",
                "--from-name",
                "events-push",
                "--origin",
                "scheduler",
                "--",
                "parent",
                "body",
            ])
        );
    }

    #[test]
    fn events_push_leaves_a_resolved_session_to_the_mail_cli() {
        assert_eq!(
            mail_argv(
                MailArm::EventsPush,
                Some("a1535d0b88424e4dbcafd733b8defc9c"),
                "parent",
                "body",
            ),
            strings(&[
                "agents",
                "mail",
                "send",
                "--origin",
                "scheduler",
                "--",
                "parent",
                "body",
            ])
        );
    }

    #[test]
    fn note_pointer_uses_canonical_session_or_named_fallback_without_origin() {
        assert_eq!(
            mail_argv(
                MailArm::NotePointer,
                Some("a1535d0b88424e4dbcafd733b8defc9c"),
                "reader",
                "body",
            ),
            strings(&[
                "agents",
                "mail",
                "send",
                "--from-name",
                "a1535d0b",
                "--lock-timeout",
                "5",
                "--",
                "reader",
                "body",
            ])
        );
        assert_eq!(
            mail_argv(MailArm::NotePointer, None, "reader", "body"),
            strings(&[
                "agents",
                "mail",
                "send",
                "--from-name",
                "note-pointer",
                "--lock-timeout",
                "5",
                "--",
                "reader",
                "body",
            ])
        );
    }

    #[test]
    fn note_pointer_receipt_keeps_the_existing_delivery_summary() {
        assert_eq!(
            note_pointer_receipt(b"msg-abc12345 delivered (hosted)\n"),
            "hosted msg-abc12345"
        );
        assert_eq!(
            note_pointer_receipt(b"msg-abc12345 queued (durable) for reader\n"),
            "durable msg-abc12345"
        );
    }
}

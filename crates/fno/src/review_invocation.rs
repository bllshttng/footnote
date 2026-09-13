//! The mux client's `review_invocation` journal row: how a `/review`
//! invocation was fired over the pane transport, and whether its transport
//! confirmed delivery. Pure move out of `mux_cli.rs` (file-budget shrink),
//! no edits.

use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn review_invocation_command(bytes: &[u8]) -> Option<(String, String)> {
    let raw = std::str::from_utf8(bytes).ok()?.trim();
    let mut parts = raw.splitn(2, char::is_whitespace);
    let token = parts.next()?.strip_prefix('/')?;
    let name = token.rsplit(':').next()?;
    if !matches!(
        name,
        "code-review" | "review" | "review-changes" | "sigma-review"
    ) {
        return None;
    }
    Some((
        format!("/{name}"),
        parts.next().unwrap_or("").trim_start().to_string(),
    ))
}

fn review_invocation_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("ri-{nanos:x}-{}", std::process::id())
}

fn review_events_path() -> PathBuf {
    if let Ok(path) = std::env::var("FNO_EVENTS_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut command = crate::process_admission::std_command("git");
    command.args(["-C", &cwd.to_string_lossy(), "rev-parse", "--show-toplevel"]);
    let probe = crate::process_admission::std_output(&mut command);
    if let Err(error) = &probe {
        // The spawn itself failed (admission refusal, missing git). Say so, or
        // the fallback silently lands the journal where no reader looks.
        eprintln!("fno mux: review-events git probe failed ({error}); events fall back to the current directory");
    }
    let root = probe
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|root| PathBuf::from(root.trim()))
        .filter(|root| !root.as_os_str().is_empty())
        .unwrap_or(cwd);
    root.join(".fno/events.jsonl")
}

fn review_invocation_branch_and_head() -> (Option<String>, Option<String>) {
    let git = |args: &[&str]| {
        let mut command = crate::process_admission::std_command("git");
        command.args(["rev-parse", "--verify"]).args(args);
        let probe = crate::process_admission::std_output(&mut command);
        if let Err(error) = &probe {
            eprintln!(
                "fno mux: review-evidence git probe failed ({error}); evidence fields will be null"
            );
        }
        probe
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let head = git(&["HEAD"]);
    let mut command = crate::process_admission::std_command("git");
    command.args(["rev-parse", "--abbrev-ref", "HEAD"]);
    let branch = crate::process_admission::std_output(&mut command)
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != "HEAD");
    (branch, head)
}

pub(crate) fn append_review_invocation(
    session: &str,
    command: Option<(String, String)>,
    submitted: bool,
    submit_confirmed: bool,
    receipt: &str,
) {
    if std::env::var_os("FNO_REVIEW_INVOCATION_ID").is_some() {
        return;
    }
    append_review_invocation_at(
        &review_events_path(),
        session,
        command,
        submitted,
        submit_confirmed,
        receipt,
    );
}

fn append_review_invocation_at(
    path: &Path,
    session: &str,
    command: Option<(String, String)>,
    submitted: bool,
    submit_confirmed: bool,
    receipt: &str,
) {
    let Some((verb, args_raw)) = command else {
        return;
    };
    let (branch, head_sha) = review_invocation_branch_and_head();
    let invocation_id = review_invocation_id();
    // `submitted` records whether this send carried a submit key at all. A
    // plain text write is not a submit, so it must not claim one: the row is
    // the transport fact an operator debugs a wedged review against.
    let (submit_required, submit_key, submit_confirmed) = if submitted {
        (true, "\\r", submit_confirmed)
    } else {
        (false, "none", false)
    };
    let data = serde_json::json!({
        "invocation_id": invocation_id,
        "stage": "sent",
        "verb": verb,
        "args_raw": args_raw,
        "transport": "mux_pane_send_raw",
        "initiator": "unknown",
        "target_session_id": session,
        "submit_required": submit_required,
        "submit_key": submit_key,
        "submit_confirmed": submit_confirmed,
        "receipt": receipt,
        "branch": branch,
        "head_sha": head_sha,
    });
    let event = serde_json::json!({
        "ts": review_invocation_timestamp(),
        "type": "review_invocation",
        "source": "daemon",
        "data": data,
    });
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{event}")
    })();
    let _ = result;
}

pub(crate) fn review_invocation_timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_part = (5 * doy + 2) / 153;
    let day = doy - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 {
        month_part + 3
    } else {
        month_part - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3_600,
        (remainder % 3_600) / 60,
        remainder % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_invocation_records_a_canonical_positive_receipt() {
        let path = std::env::temp_dir().join(format!(
            "fno-review-invocation-{}-{}.jsonl",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);

        let command = review_invocation_command(b"/review medium --comment");
        let command_plain = command.clone();
        assert_eq!(
            command,
            Some(("/review".to_string(), "medium --comment".to_string()))
        );
        append_review_invocation_at(
            &path,
            "mux-session",
            command,
            true,
            false,
            "text delivered, submission unconfirmed",
        );

        let line = std::fs::read_to_string(&path).unwrap();
        let event: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(event["type"], "review_invocation");
        assert_eq!(event["source"], "daemon");
        assert_eq!(event["data"]["stage"], "sent");
        assert_eq!(event["data"]["transport"], "mux_pane_send_raw");
        assert_eq!(
            event["data"]["receipt"],
            "text delivered, submission unconfirmed"
        );
        assert_eq!(event["data"]["submit_required"], true);
        assert_eq!(event["data"]["submit_key"], "\\r");
        assert_eq!(event["data"]["submit_confirmed"], false);
        assert!(event["data"]["invocation_id"]
            .as_str()
            .unwrap()
            .starts_with("ri-"));
        let _ = std::fs::remove_file(&path);

        // A plain text write with no submit key must not claim one.
        append_review_invocation_at(
            &path,
            "mux-session",
            command_plain,
            false,
            false,
            "text delivered, no submit requested",
        );
        let line = std::fs::read_to_string(&path).unwrap();
        let event: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(event["data"]["submit_required"], false);
        assert_eq!(event["data"]["submit_key"], "none");
        assert_eq!(event["data"]["submit_confirmed"], false);
        let _ = std::fs::remove_file(path);
    }
}

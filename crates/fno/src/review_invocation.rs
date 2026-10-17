//! The mux transport half of the review-invocation owner: the command
// projection and the journal writers. The pure core (parse, flags, mint,
// sidecar) is spliced from review_invocation_core.rs, the file vendored
// byte-identical into fno-agents.

include!("review_invocation_core.rs");

/// The mux transport's projection: verb and args only, the signature
/// `mux_cli.rs` has always called. The full parse is the one owner above.
pub(crate) fn review_invocation_command(bytes: &[u8]) -> Option<(String, String)> {
    let raw = std::str::from_utf8(bytes).ok()?;
    let parsed = parse_review_invocation(raw)?;
    Some((parsed.verb, parsed.args_raw))
}

/// One journal resolver for the crate: setup's per-run events ride the
/// same path review invocations write, so no second implementation drifts.
pub(crate) fn review_events_path() -> PathBuf {
    if let Ok(path) = std::env::var("FNO_EVENTS_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // The resolver names the journal the store reads (the project space). A
    // rev-parse guess is how these rows landed in the checkout journal, a
    // file the store and every store reader ignore.
    let mut command = crate::process_admission::std_command("fno-agents");
    command.args(["state", "path", "events"]);
    command.current_dir(&cwd);
    if let Ok(output) = crate::process_admission::std_output(&mut command) {
        if output.status.success() {
            if let Ok(text) = String::from_utf8(output.stdout) {
                let path = text.trim();
                if !path.is_empty() {
                    return PathBuf::from(path);
                }
            }
        }
    }
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
    let invocation_id = mint_invocation_id();
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
    let _ = crate::pane_send_audit::append_agents_event(path, &event);
}

/// Wall-clock RFC3339 UTC timestamp, seconds precision, no chrono in the
/// dependency surface. Shared with the invocations door's appends.

// Distinct from the spliced core's `tests`: the include! pastes that module
// into this one, and two `tests` in one namespace is E0428 under cfg(test).
#[cfg(test)]
mod review_transport_tests {
    use super::*;

    fn stored_event(path: &std::path::Path) -> serde_json::Value {
        let rows = crate::event_store::query_events(
            path,
            &crate::event_store::EventQuery::of_types(&["review_invocation"]),
        )
        .expect("the store beside the journal opened");
        let row = rows.last().expect("the row committed");
        serde_json::from_str(&row.line).unwrap()
    }

    #[test]
    fn review_invocation_records_a_canonical_positive_receipt() {
        let path = std::env::temp_dir().join(format!(
            "fno-review-invocation-{}-{}.jsonl",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db"));

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

        let event = stored_event(&path);
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

        // A plain text write with no submit key must not claim one. The
        // store keeps the first row, so the read takes the last.
        append_review_invocation_at(
            &path,
            "mux-session",
            command_plain,
            false,
            false,
            "text delivered, no submit requested",
        );
        let event = stored_event(&path);
        assert_eq!(event["data"]["submit_required"], false);
        assert_eq!(event["data"]["submit_key"], "none");
        assert_eq!(event["data"]["submit_confirmed"], false);
        let _ = std::fs::remove_file(path.with_extension("db"));
        let _ = std::fs::remove_file(path);
    }
}

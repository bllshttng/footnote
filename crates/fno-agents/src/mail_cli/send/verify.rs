// Ported from the single-file draft of send.rs; split by question.
use super::*;

// ---------------------------------------------------------------------------
// Liveness, attendance, escalation (receipts.py / deferred_liveness.py twins)
// ---------------------------------------------------------------------------

/// Python `session_truth.fmt_age`: compact floored age, 45s / 12m / 3h.
pub(super) fn fmt_age(seconds: f64) -> String {
    let s = seconds.max(0.0) as i64;
    if s < 60 {
        return format!("{s}s");
    }
    if s < 3600 {
        return format!("{}m", s / 60);
    }
    format!("{}h", s / 3600)
}

/// Python `session_truth._humanize_age`: fixed-width-4 so a column never
/// reflows; None renders `   ?`, never 0s.
pub(super) fn humanize_age_fixed(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds else {
        return "   ?".to_string();
    };
    let total = seconds.max(0.0) as i64;
    if total < 60 {
        return format!("{:>4}", format!("{total}s"));
    }
    if total < 3600 {
        return format!("{:>4}", format!("{}m", total / 60));
    }
    if total < 86400 {
        return format!("{:>4}", format!("{}h", total / 3600));
    }
    format!("{:>4}", format!("{}d", (total / 86400).min(999)))
}

/// Python `receipts._live_miss_age_suffix`: the transcript-age discriminator
/// on a live-miss receipt. An unreadable transcript prints unknown, never 0s.
pub(super) fn live_miss_age_suffix(recipient: &str) -> String {
    let Some(probe) = crate::truth_probe::family1_truth_probe(recipient) else {
        return ", transcript age unknown".to_string();
    };
    match probe.last_activity_age_s {
        Some(age) => format!(", transcript quiet {}", fmt_age(age)),
        None => ", transcript age unknown".to_string(),
    }
}

/// Python `deferred_liveness.deferred_liveness_head`: the receipt preamble
/// repeats the one liveness oracle's verdict; only a positively stalled
/// transcript may warn the recipient may never drain.
pub(super) fn deferred_liveness_head(target: &str) -> Option<String> {
    const TAIL: &str = " queued durably as recovery only - the recipient must drain its inbox to read this, and may never do so\n";
    let probe = crate::truth_probe::family1_truth_probe(target)?;
    let age_text = humanize_age_fixed(probe.last_activity_age_s)
        .trim()
        .to_string();
    if probe.state == "unknown" {
        return Some(format!(
            "mail: liveness of {target} could not be established;{TAIL}"
        ));
    }
    if probe.state == "stalled" {
        return Some(format!(
            "mail: {target} reads stalled on its transcript (silent {age_text});{TAIL}"
        ));
    }
    Some(format!(
        "mail: {target} reads {} on its transcript (activity {age_text} ago);{TAIL}",
        probe.state
    ))
}

/// Python `_recipient_is_attended`: the registry row's origin=operator stamp,
/// read through the mail-receipt door. A door failure escalates nothing.
pub(super) fn recipient_is_attended(recipient: &str) -> bool {
    matches!(
        render_receipt(&["attended", "--recipient", recipient]),
        Ok(text) if text == "true"
    )
}

/// Python `_codex_daemon_socket_absent`: the connect probe, never the file's
/// existence; the socket file outlives the daemon that made it.
pub(super) fn codex_daemon_socket_absent() -> bool {
    use std::os::unix::net::UnixStream;
    let path = crate::codex_inject::codex_app_server_socket_path();
    UnixStream::connect(&path).is_err()
}

/// Python `_escalate_to_human`: the debounced send-time human nudge. The
/// overlay event emits BEFORE the notifier verdict so a headless host still
/// renders it. Best-effort throughout; never affects the send's exit.
pub(super) fn escalate_to_human(
    sender: &str,
    recipient: &str,
    summary: &str,
    reason: &str,
    msg_id: Option<&str>,
) -> &'static str {
    let pair = escalation_pair(sender, recipient);
    let marker_dir = state_dir().join("mail-escalations");
    let verdict = render_receipt(&[
        "debounce",
        "--marker-dir",
        &marker_dir.to_string_lossy(),
        "--pair",
        &pair,
        "--window-secs",
        &ESCALATION_DEBOUNCE_S.to_string(),
    ])
    .unwrap_or_else(|_| "claim".to_string());
    if verdict == "debounced" {
        return "debounced";
    }
    let one_line: String = summary
        .split('\n')
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect();
    emit_event(
        "mail_escalation",
        json!({
            "reason": reason,
            "sender": sender,
            "recipient": recipient,
            "summary": one_line,
            "msg_id": msg_id,
        }),
    );
    let label = if reason == "attended-miss" || reason == "reachable-miss" {
        "missed you"
    } else {
        "question"
    };
    let displayed = crate::operator_notice::notify_operator(
        &format!("fno agents mail: {label} from {sender}"),
        &format!("{one_line} - run `fno agents mail drain-self`"),
        None,
    );
    if displayed {
        "escalated"
    } else {
        "notifier-unavailable"
    }
}

// ---------------------------------------------------------------------------
// Post-send landing verify (landed.py twins)
// ---------------------------------------------------------------------------

/// Python `post_send_settle_seconds`: the beat a just-sent row gets before
/// the sender-side verify reads; the env override exists so tests read now.
pub(super) fn post_send_settle_seconds() -> f64 {
    std::env::var("FNO_MAIL_LANDED_SETTLE_S")
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .map(|v| v.max(0.0))
        .unwrap_or(2.0)
}

/// One pass over the bus log, newest last, best-effort.
pub(super) fn bus_rows() -> Vec<Value> {
    let mut rows = Vec::new();
    let Ok(text) = std::fs::read_to_string(bus_log_path()) else {
        return rows;
    };
    for line in text.lines() {
        if let Ok(row) = serde_json::from_str::<Value>(line) {
            rows.push(row);
        }
    }
    rows
}

/// Python `bus.log.landed_ids`: ids proven landed by a same-sender/recipient
/// control row. The acknowledging row's own id never joins.
pub(super) fn landed_ids_from(rows: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for m in rows {
        if str_field(m, "kind") != "landed" {
            continue;
        }
        let Some(target_id) = m.pointer("/meta/landed").and_then(Value::as_str) else {
            continue;
        };
        let Some(target) = rows.iter().find(|r| str_field(r, "id") == target_id) else {
            continue;
        };
        if str_field(target, "from") == str_field(m, "from")
            && str_field(target, "to") == str_field(m, "to")
        {
            out.push(target_id.to_string());
        }
    }
    out
}

/// Python `bus.cursor.read_cursor`: the consumer's last-seen id, or None.
pub(super) fn read_cursor(name: &str) -> Option<String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    let path = bus_log_path()
        .parent()?
        .join("cursors")
        .join(format!("{name}.json"));
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    doc.get("last_seen_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Python `reply_resolve._transcript_path`: the recipient transcript for the
/// landed tail-grep, resolved over the ambient claude projects dir.
pub(super) fn transcript_path(to_harness: &str, to_session: &str) -> Option<PathBuf> {
    if to_harness != "claude" {
        return None;
    }
    let projects = crate::claude_drive::claude_projects_dir();
    for entry in std::fs::read_dir(projects).ok()? {
        let Ok(entry) = entry else { continue };
        let candidate = entry.path().join(format!("{to_session}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Python `landed.post_send_landed`: three proofs, not only the transcript -
/// the bus landed record, the recipient's read cursor past this row, and the
/// raw id in the recipient transcript's 4MiB tail.
pub(super) fn post_send_landed(
    msg_id: &str,
    to: Option<&str>,
    to_harness: Option<&str>,
    to_session: Option<&str>,
) -> (bool, &'static str) {
    let rows = bus_rows();
    if landed_ids_from(&rows).iter().any(|id| id == msg_id) {
        return (true, "bus landed record");
    }
    if let Some(to) = to {
        if let Some(cursor) = read_cursor(to) {
            let ids: Vec<&str> = rows.iter().map(|r| str_field(r, "id")).collect();
            if let (Some(ci), Some(mi)) = (
                ids.iter().position(|id| *id == cursor),
                ids.iter().position(|id| *id == msg_id),
            ) {
                if ci >= mi {
                    return (true, "bus claim (recipient read past it)");
                }
            }
        }
    }
    if let (Some(harness), Some(session)) = (to_harness, to_session) {
        if let Some(path) = transcript_path(harness, session) {
            if let Ok(handle) = std::fs::File::open(&path) {
                use std::io::{Read as _, Seek as _, SeekFrom};
                let mut fh = handle;
                let size = fh.seek(SeekFrom::End(0)).unwrap_or(0);
                let start = size.saturating_sub(4 << 20);
                if fh.seek(SeekFrom::Start(start)).is_ok() {
                    let mut buf = Vec::new();
                    if fh.read_to_end(&mut buf).is_ok() {
                        let text = String::from_utf8_lossy(&buf);
                        if text.contains(msg_id) {
                            return (true, "recipient transcript");
                        }
                    }
                }
            }
        }
    }
    (false, "")
}

/// Python `receipts._live_pane_for`: the LIVE pane list join, never the
/// registry row. Any mux failure answers None: a guessed pane number is
/// worse than a generic verify line.
pub(super) fn live_pane_for(target: &str, session_id: Option<&str>) -> Option<i64> {
    let front = crate::mail_remote::fno_binary();
    let argv = vec![
        front.to_string_lossy().into_owned(),
        "mux".to_string(),
        "pane".to_string(),
        "ls".to_string(),
        "--json".to_string(),
    ];
    let captured = run_captured(&argv, "", false, 10.0);
    if captured.status != Some(0) {
        return None;
    }
    let rows: Value = serde_json::from_str(captured.stdout.trim()).ok()?;
    let rows = rows.as_array()?;
    for row in rows {
        let hit = if let Some(sid) = session_id {
            row.get("fno_id").and_then(Value::as_str) == Some(sid)
                || row.get("harness_session_id").and_then(Value::as_str) == Some(sid)
        } else {
            !target.is_empty() && row.get("name").and_then(Value::as_str) == Some(target)
        };
        if hit {
            if let Some(pane) = row.get("pane_id").and_then(Value::as_i64) {
                return Some(pane);
            }
        }
    }
    None
}

/// Python `receipts.report_landing`: after the settle window, re-read the
/// recipient and print `landed (<how>)` or the NOT LANDED block. True when
/// landed; the caller owns the non-zero exit.
pub(super) fn report_landing(
    msg_id: &str,
    target: &str,
    to: Option<&str>,
    to_harness: Option<&str>,
    to_session: Option<&str>,
) -> bool {
    let settle = post_send_settle_seconds();
    if settle > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(settle));
    }
    let (landed, how) = post_send_landed(msg_id, to, to_harness, to_session);
    if landed {
        println!("{msg_id} landed ({how})");
        return true;
    }
    let pane = live_pane_for(target, to_session);
    let mut door: Vec<String> = vec![
        "not-landed".to_string(),
        "--msg-id".to_string(),
        msg_id.to_string(),
        "--target".to_string(),
        target.to_string(),
    ];
    if let Some(p) = pane {
        door.push("--pane".to_string());
        door.push(p.to_string());
    }
    if let Some(h) = to_harness {
        door.push("--harness".to_string());
        door.push(h.to_string());
    }
    if let Some(s) = to_session {
        door.push("--session-id".to_string());
        door.push(s.to_string());
    }
    let refs: Vec<&str> = door.iter().map(String::as_str).collect();
    match render_receipt(&refs) {
        Ok(text) => println!("{text}"),
        Err(exc) => println!("{exc}"),
    }
    false
}

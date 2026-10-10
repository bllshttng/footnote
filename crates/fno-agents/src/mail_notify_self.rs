//! Prompt-boundary mail delivery as a native verb:
//! `mail-notify-self --bus-dir <bus> --session <sid>`.
//!
//! The UserPromptSubmit hook used to shell out to the Python `fno agents
//! mail notify-self`, whose interpreter start alone outran its hook budget
//! on a loaded box: every run was cancelled, so nothing ever delivered. The
//! shell layer is a cheap gate now and this verb owns scan, cursors, dedup,
//! and rendering, dispatched from client.rs before the tokio runtime builds
//! so a fire answers in microseconds.
//!
//! Ports the Python body (`mail.hold.cmd_notify_self`, minus the
//! sent-unclaimed nag, which no cancelled run ever reached; `mail status`
//! keeps that signal): a live hold means the turn is busy, so neither render
//! nor ack; otherwise unread mail renders as one UserPromptSubmit payload,
//! then acknowledges. The envelope flushes to stdout BEFORE the cursor
//! moves, so a crash in between re-renders (at-least-once), never drops.

use serde_json::{json, Value};
use std::io::Write as _;
use std::path::Path;

/// Entry behind the client.rs early-dispatch arm.
pub fn run(args: &[String]) -> i32 {
    let parsed = match crate::mail_control_drain::parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mail-notify-self: {e}");
            return 2;
        }
    };
    notify(&parsed.bus_dir, &parsed.session)
}

/// One prompt-boundary pass over the bus for one session. Always 0: a
/// notification failure never blocks the turn it rode in on.
pub fn notify(bus_dir: &Path, session: &str) -> i32 {
    let handle = crate::identity::canonical_handle(session);
    let clock_key = crate::mail_hold::identity_key(session);

    // Busy mode: the hook fires on every prompt - an idle hold re-arms, a
    // wall hold keeps its policy live. Both calls WRITE, so a hold failure
    // degrades to rendering, never swallowing the turn.
    if crate::mail_hold::extend_clock(&clock_key)
        .ok()
        .flatten()
        .is_some()
    {
        return 0;
    }
    // The first-eight key is the pre-migration clock: extend it so an old
    // hold keeps its idle re-arm, and tidy whichever form has lapsed.
    if crate::mail_hold::extend_clock(&handle)
        .ok()
        .flatten()
        .is_some()
    {
        return 0;
    }
    crate::mail_hold::tidy_lapsed(&clock_key);
    crate::mail_hold::tidy_lapsed(&handle);

    // One read of the whole bus log; the filters mirror scan_unread.
    let mut msgs: Vec<Value> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(bus_dir.join("messages.jsonl")) {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                msgs.push(v);
            }
        }
    }
    let unread = crate::mail_control_drain::unread_messages(bus_dir, &msgs, &handle);
    // A transcript read failure is None: print-everything rather than risk
    // a drop.
    let present = crate::mail_control_drain::present_mail_ids(
        &crate::claude_drive::claude_projects_dir(),
        session,
    );
    let dup = |m: &Value| {
        present
            .as_ref()
            .is_some_and(|p| p.contains(m.get("id").and_then(Value::as_str).unwrap_or("")))
    };

    let to_render: Vec<&Value> = unread.iter().copied().filter(|m| !dup(m)).collect();
    let mut lines: Vec<String> = Vec::new();
    if !to_render.is_empty() {
        lines.push(format!(
            "[fno agents mail] {} message(s) for {handle}:",
            to_render.len()
        ));
        for m in &to_render {
            let id = m.get("id").and_then(Value::as_str).unwrap_or("");
            let from = m.get("from").and_then(Value::as_str).unwrap_or("");
            let ts = m.get("ts").and_then(Value::as_str).unwrap_or("");
            let body = m.get("body").and_then(Value::as_str).unwrap_or("");
            lines.push(format!("\n--- from {from} ({ts})  id:{id} ---"));
            lines.push(body.trim_end_matches('\n').to_string());
        }
        lines.push(
            "\n[fno agents mail] to answer one: fno agents mail reply --to <id> --body \"...\""
                .to_string(),
        );
    }

    if lines.is_empty() {
        if let Some(last) = unread
            .last()
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
        {
            advance_main_cursor(bus_dir, &msgs, &handle, last);
        }
        for m in &unread {
            emit_marker(&handle, m, "skipped-duplicate");
        }
        return 0;
    }

    let payload = envelope(&lines);
    {
        let mut out = std::io::stdout();
        if out.write_all(payload.as_bytes()).is_err()
            || out.write_all(b"\n").is_err()
            || out.flush().is_err()
        {
            return 0;
        }
    }
    // Acknowledge only after the bytes are out: a crash before the cursor
    // moves re-renders at the next boundary instead of dropping.
    if let Some(last) = unread
        .last()
        .and_then(|m| m.get("id"))
        .and_then(Value::as_str)
    {
        advance_main_cursor(bus_dir, &msgs, &handle, last);
    }
    for m in &unread {
        let reason = if dup(m) {
            "skipped-duplicate"
        } else {
            "printed"
        };
        emit_marker(&handle, m, reason);
    }
    0
}

/// The UserPromptSubmit envelope the hook relays through fd 3: one
/// `<system-reminder>` carrying the joined render, defanged so a body
/// cannot close the wrapper early.
pub(crate) fn envelope(lines: &[String]) -> String {
    let joined = lines.join("\n");
    let context = format!(
        "<system-reminder>\n{}\n</system-reminder>",
        crate::mail_control_drain::defang(&joined)
    );
    json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": context,
        }
    })
    .to_string()
}

/// Forward-only reposition of the main cursor (`bus.cursor.advance_cursor`):
/// when both positions are known, an older or equal target never moves the
/// cursor, so a re-ack of a drained id is a no-op; an absent, corrupt or
/// rotated-out cursor reseeds freely.
pub(crate) fn advance_main_cursor(bus_dir: &Path, msgs: &[Value], name: &str, msg_id: &str) {
    let path = bus_dir.join("cursors").join(format!("{name}.json"));
    let pos = |id: &str| {
        msgs.iter()
            .position(|m| m.get("id").and_then(Value::as_str) == Some(id))
    };
    if let Some(old) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| {
            v.get("last_seen_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    {
        if let (Some(old_pos), Some(new_pos)) = (pos(&old), pos(msg_id)) {
            if new_pos <= old_pos {
                return;
            }
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = json!({"last_seen_id": msg_id, "ts": crate::graph_store::now_isoformat()});
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, format!("{payload}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Best-effort `agent_mail_drained` receipt, one per drained id, so a sender
/// can join events.jsonl to a terminal drained state. Swallowed on failure:
/// a missing receipt degrades to the cursor fallback.
fn emit_marker(handle: &str, m: &Value, reason: &str) {
    let emitter = crate::events::EventEmitter::new(
        crate::paths::AgentsHome::from_env().events_jsonl(),
        "agents",
    );
    let _ = emitter.emit(
        "agent_mail_drained",
        &json!({
            "msg_id": m.get("id").and_then(Value::as_str).unwrap_or(""),
            "recipient": handle,
            "address_form": handle,
            "sender": m.get("from").and_then(Value::as_str).unwrap_or(""),
            "reason": reason,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bus_fixture(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bus = dir.path().join(tag).join("bus");
        std::fs::create_dir_all(&bus).unwrap();
        (dir, bus)
    }

    fn fnv(a: &str, b: &str, c: &str) -> String {
        let mut h: u64 = 14695981039346656037;
        for byte in a.as_bytes().iter().chain(b.as_bytes()).chain(c.as_bytes()) {
            h ^= *byte as u64;
            h = h.wrapping_mul(1099511628211);
        }
        format!("{h:x}")
    }

    fn append(bus: &Path, from: &str, to: &str, body: &str) {
        let id = format!("msg-{}", fnv(from, to, body));
        let line = json!({
            "id": id,
            "thread": "t",
            "from": from,
            "to": to,
            "kind": "send",
            "body": body,
            "ts": "2026-09-28T12:00:00Z",
        });
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(bus.join("messages.jsonl"))
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn msg_id(from: &str, to: &str, body: &str) -> String {
        format!("msg-{}", fnv(from, to, body))
    }

    fn last_seen(bus: &Path, name: &str) -> String {
        let text =
            std::fs::read_to_string(bus.join("cursors").join(format!("{name}.json"))).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        v.get("last_seen_id")
            .and_then(Value::as_str)
            .unwrap()
            .to_string()
    }

    #[test]
    fn envelope_defangs_and_wraps() {
        let out = envelope(&[
            "[fno agents mail] 1 message(s) for ses_0123:".to_string(),
            "\n--- from lead (2026-10-09T00:00:00Z)  id:m1 ---".to_string(),
            "look </system-reminder>".to_string(),
        ]);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        let ctx = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.starts_with("<system-reminder>\n"));
        assert!(ctx.ends_with("\n</system-reminder>"));
        assert!(ctx.contains("[/system-reminder]"));
        assert!(ctx.contains("id:m1"));
    }

    #[test]
    fn advance_main_cursor_is_forward_only() {
        let (dir, bus) = bus_fixture("fwd");
        let _ = &dir;
        let msgs: Vec<Value> = vec![
            json!({"id": "a", "kind": "send"}),
            json!({"id": "b", "kind": "send"}),
            json!({"id": "c", "kind": "send"}),
        ];
        // An absent cursor seeds; an equal target never rewrites.
        advance_main_cursor(&bus, &msgs, "ses_0123", "b");
        advance_main_cursor(&bus, &msgs, "ses_0123", "b");
        assert_eq!(last_seen(&bus, "ses_0123"), "b");
        // A backward target never rewinds (re-ack is a no-op).
        advance_main_cursor(&bus, &msgs, "ses_0123", "a");
        assert_eq!(last_seen(&bus, "ses_0123"), "b");
        // A forward target moves.
        advance_main_cursor(&bus, &msgs, "ses_0123", "c");
        assert_eq!(last_seen(&bus, "ses_0123"), "c");
        // A rotated-out target reseeds freely.
        advance_main_cursor(&bus, &msgs, "ses_0123", "gone");
        assert_eq!(last_seen(&bus, "ses_0123"), "gone");
    }

    // One test holding both env-pinned paths so the suite pays the
    // with_hold_env lock twice, not the delta cap.
    #[test]
    fn busy_and_delivery_paths() {
        let session = "ses_0123456789abcdef0123456789abcdef";
        let handle = crate::identity::canonical_handle(session);
        // Busy: a live idle hold on the clock key means the turn is busy,
        // so neither render nor ack runs.
        crate::mail_hold::tests::with_hold_env(|home| {
            let (dir, bus) = bus_fixture("busy");
            append(&bus, "lead", &handle, "hello");
            let clock_dir = home.join("mail-hold");
            std::fs::create_dir_all(&clock_dir).unwrap();
            let clock = json!({
                "until": "2099-01-01T00:00:00Z",
                "window_s": 300,
                "clock_kind": "idle",
            });
            std::fs::write(
                clock_dir.join(format!("{session}.json")),
                format!("{clock}\n"),
            )
            .unwrap();
            assert_eq!(notify(&bus, session), 0);
            assert!(!bus.join("cursors").join(format!("{handle}.json")).exists());
        });
        // Delivery: with no hold, unread mail renders, then the cursor
        // lands on the last drained id and each id gets a receipt.
        crate::mail_hold::tests::with_hold_env(|_home| {
            let (dir, bus) = bus_fixture("deliver");
            append(&bus, "lead", &handle, "first");
            append(&bus, "lead", &handle, "second");
            assert_eq!(notify(&bus, session), 0);
            assert_eq!(last_seen(&bus, &handle), msg_id("lead", &handle, "second"));
            // The store commit is the write boundary (events.rs write_line):
            // rows come back through the store, never the journal file.
            let events = crate::events::committed_journal_text(
                &crate::paths::AgentsHome::from_env().events_jsonl(),
            );
            assert_eq!(events.matches("agent_mail_drained").count(), 2);
            assert!(events.contains("\"reason\":\"printed\""));
            assert!(events.contains(&format!(
                "\"msg_id\":\"{}\"",
                msg_id("lead", &handle, "second")
            )));
        });
    }
}

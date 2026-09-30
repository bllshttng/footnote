//! Control-mail landing at the tool boundary: the `mail-inject
//! --control-drain` mode.
//!
//! The recipient-side half of the control lane. A control body that demoted
//! durable waits on `notify-self`, which fires only at a prompt boundary; a
//! worker holding one long turn never reaches one. The PreToolUse hook runs
//! this verb at every tool call instead, gated on the sender-stamped pending
//! flags so a clean tool call pays three stat calls.
//!
//! The scan ports the Python bus read semantics it replaces
//! (`fno.bus.log.is_deliverable`, `withdrawn_ids`, the `scan_unread` cursor
//! position, the transcript-presence dedup, and the defang): a control row
//! (withdraw tombstone, landed receipt) is never inbox content; a hosted or
//! typed row is bytes already delivered and drains as a skip; a tombstone
//! retracts its target only when both came from the same sender to the same
//! address; a transcript read failure prints everything rather than risking
//! a drop. Ordinary mail is untouched: the lane positions at its own
//! `control:<form>` cursors and advances nothing the prompt boundary reads.

use serde_json::{json, Value};
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// The bus kinds that are bookkeeping about other messages, never inbox
/// content (`bus.log.CONTROL_KINDS`).
const WITHDRAW_KIND: &str = "withdraw";
const LANDED_KIND: &str = "landed";
/// Delivery states meaning the bytes already reached the recipient
/// (`bus.log.HOSTED_DELIVERY` / `TYPED_DELIVERY`); draining one would hand
/// the recipient a second copy.
const AUDIT_ONLY_DELIVERIES: &[&str] = &["hosted", "typed"];

pub struct DrainArgs {
    pub bus_dir: PathBuf,
    pub session: String,
}

pub fn parse_args(rest: &[String]) -> Result<DrainArgs, String> {
    let mut bus_dir: Option<PathBuf> = None;
    let mut session: Option<String> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bus-dir" => {
                bus_dir = Some(PathBuf::from(
                    it.next().ok_or("--bus-dir needs a path")?.as_str(),
                ))
            }
            "--session" => {
                session = Some(it.next().ok_or("--session needs an id")?.clone());
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(DrainArgs {
        bus_dir: bus_dir.ok_or("--bus-dir is required")?,
        session: session.ok_or("--session is required")?,
    })
}

/// The three mailbox address forms one session answers to
/// (`harness_identity.canonical_handle` / `session_identity_key` /
/// `legacy_suffix_handle`).
fn address_forms(session: &str) -> Vec<String> {
    let lowered = if session.starts_with("ses_") {
        session.to_string()
    } else {
        session.to_lowercase()
    };
    let chars: Vec<char> = lowered.chars().collect();
    let mut forms = vec![];
    if chars.len() >= 8 {
        forms.push(chars[..8].iter().collect::<String>());
    }
    forms.push(lowered.clone());
    if chars.len() >= 8 {
        forms.push(chars[chars.len() - 8..].iter().collect::<String>());
    }
    forms.sort();
    forms.dedup();
    forms
}

fn flag_path(bus_dir: &Path, form: &str) -> PathBuf {
    bus_dir.join("control-pending").join(format!("{form}.flag"))
}

fn cursor_path(bus_dir: &Path, form: &str) -> PathBuf {
    bus_dir.join("cursors").join(format!("control:{form}.json"))
}

fn read_cursor(bus_dir: &Path, form: &str) -> Option<String> {
    let text = std::fs::read_to_string(cursor_path(bus_dir, form)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    v.get("last_seen_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(crate) fn unread_count(bus_dir: &Path, msgs: &[Value], name: &str) -> usize {
    let cursor = std::fs::read_to_string(bus_dir.join("cursors").join(format!("{name}.json")))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| {
            v.get("last_seen_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let after = cursor.and_then(|id| {
        msgs.iter()
            .position(|m| m.get("id").and_then(Value::as_str) == Some(id.as_str()))
    });
    let withdrawn = withdrawn_ids(msgs);
    msgs.iter()
        .enumerate()
        .filter(|(i, m)| {
            after.is_none_or(|pos| *i > pos)
                && m.get("to").and_then(Value::as_str) == Some(name)
                && deliverable(m)
                && m.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| !id.is_empty() && !withdrawn.contains(id))
        })
        .count()
}

fn write_cursor(bus_dir: &Path, form: &str, msg_id: &str) {
    let path = cursor_path(bus_dir, form);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = json!({"last_seen_id": msg_id, "ts": crate::graph_store::now_isoformat()});
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, format!("{payload}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn is_control(body: &str) -> bool {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .is_some_and(|first| first.to_lowercase().starts_with("control:"))
}

fn deliverable(m: &Value) -> bool {
    let kind = m.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind == WITHDRAW_KIND || kind == LANDED_KIND {
        return false;
    }
    let delivery = m.get("delivery").and_then(Value::as_str).unwrap_or("");
    !AUDIT_ONLY_DELIVERIES.contains(&delivery)
}

/// Ids retracted by a tombstone, plus the tombstones themselves; a tombstone
/// counts only against a message from the same sender to the same address.
fn withdrawn_ids(msgs: &[Value]) -> std::collections::HashSet<String> {
    let by_id: std::collections::HashMap<&str, &Value> = msgs
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(|id| (id, m)))
        .collect();
    let mut out = std::collections::HashSet::new();
    for m in msgs {
        if m.get("kind").and_then(Value::as_str) != Some(WITHDRAW_KIND) {
            continue;
        }
        if let Some(id) = m.get("id").and_then(Value::as_str) {
            out.insert(id.to_string());
        }
        let target_id = m
            .get("meta")
            .and_then(|meta| meta.get("withdraws"))
            .and_then(Value::as_str);
        let matches = target_id.and_then(|t| by_id.get(t)).is_some_and(|target| {
            target.get("from").and_then(Value::as_str) == m.get("from").and_then(Value::as_str)
                && target.get("to").and_then(Value::as_str) == m.get("to").and_then(Value::as_str)
        });
        if let (Some(t), true) = (target_id, matches) {
            out.insert(t.to_string());
        }
    }
    out
}

/// Every `<fno_mail id="...">` id in the session's own transcript, or None
/// when the transcript cannot be resolved or read (print-everything posture).
fn present_mail_ids(
    projects_base: &Path,
    session: &str,
) -> Option<std::collections::HashSet<String>> {
    let transcript = crate::claude_drive::find_transcript_in(projects_base, session)?;
    let mut text = String::new();
    std::fs::File::open(transcript)
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    let mut out = std::collections::HashSet::new();
    let mut rest = text.as_str();
    while let Some(pos) = rest.find("<fno_mail") {
        rest = &rest[pos..];
        let end = rest.find('>').map(|e| e.min(400)).unwrap_or(400);
        let tag = &rest[..end.min(rest.len())];
        if let Some(id_pos) = tag.find("id=\"") {
            let id = &tag[id_pos + 4..];
            if let Some(close) = id.find('"') {
                out.insert(id[..close].to_string());
            }
        }
        rest = &rest[end.min(rest.len())..];
    }
    Some(out)
}

/// The defang `mail.landed._defang_reminder` applies: a body cannot close
/// the reminder wrapper early. Ports `<\s*(/?)\s*system-reminder\s*>`
/// (case-folded) to a `[`-bracketed literal.
fn defang(s: &str) -> String {
    const TAG: &str = "system-reminder";
    let lower = s.to_lowercase();
    let skip_ws = |j: usize| -> usize {
        let mut k = j;
        for (off, c) in lower[k..].char_indices() {
            if c.is_whitespace() {
                k += off + c.len_utf8();
            } else {
                break;
            }
        }
        k
    };
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while let Some(lt) = lower[i..].find('<') {
        let start = i + lt;
        let mut j = skip_ws(start + 1);
        let mut slash = false;
        if lower[j..].starts_with('/') {
            slash = true;
            j = skip_ws(j + 1);
        }
        if lower[j..].starts_with(TAG) {
            let k = skip_ws(j + TAG.len());
            if lower[k..].starts_with('>') {
                out.push_str(&s[i..start]);
                out.push('[');
                if slash {
                    out.push('/');
                }
                out.push_str(TAG);
                out.push(']');
                i = k + 1;
                continue;
            }
        }
        // No match: emit through the '<' and keep scanning after it.
        out.push_str(&s[i..=start]);
        i = start + 1;
    }
    out.push_str(&s[i..]);
    out
}

pub fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mail-inject --control-drain: {e}");
            return 2;
        }
    };
    if let Some(payload) = drain(&parsed.bus_dir, &parsed.session) {
        println!("{payload}");
    }
    0
}

/// One tool-boundary pass over the bus for one session. `Some` carries the
/// PreToolUse JSON to inject; `None` is a clean no-op.
pub fn drain(bus_dir: &Path, session: &str) -> Option<String> {
    let forms = address_forms(session);
    // Cheap gate: no pending flag, nothing to land; today's prompt-boundary
    // delivery still owns the body.
    if !forms.iter().any(|f| flag_path(bus_dir, f).exists()) {
        return None;
    }
    // One read of the whole bus log; the filters mirror scan_unread.
    let log_path = bus_dir.join("messages.jsonl");
    let mut msgs: Vec<Value> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&log_path) {
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                msgs.push(v);
            }
        }
    }
    let retracted = withdrawn_ids(&msgs);
    let present = present_mail_ids(&crate::claude_drive::claude_projects_dir(), session);

    let mut rendered: Vec<String> = Vec::new();
    let mut any = false;
    for form in &forms {
        let cursor = read_cursor(bus_dir, form);
        let mut controls: Vec<&Value> = msgs
            .iter()
            .filter(|m| {
                if m.get("to").and_then(Value::as_str) != Some(form.as_str()) {
                    return false;
                }
                if !deliverable(m) {
                    return false;
                }
                let id = m.get("id").and_then(Value::as_str).unwrap_or("");
                if id.is_empty() || retracted.contains(id) {
                    return false;
                }
                m.get("body")
                    .and_then(Value::as_str)
                    .is_some_and(is_control)
            })
            .collect();
        if let Some(last_seen) = &cursor {
            if let Some(pos) = msgs
                .iter()
                .position(|m| m.get("id").and_then(Value::as_str) == Some(last_seen.as_str()))
            {
                controls.retain(|m| {
                    let idx = msgs
                        .iter()
                        .position(|x| std::ptr::eq(x, *m))
                        .unwrap_or(usize::MAX);
                    idx > pos
                });
            }
        }
        if controls.is_empty() {
            continue;
        }
        for m in &controls {
            let id = m.get("id").and_then(Value::as_str).unwrap_or("");
            if present.as_ref().is_some_and(|p| p.contains(id)) {
                continue;
            }
            let from = m.get("from").and_then(Value::as_str).unwrap_or("");
            let ts = m.get("ts").and_then(Value::as_str).unwrap_or("");
            let body = m.get("body").and_then(Value::as_str).unwrap_or("");
            rendered.push(format!(
                "\n--- from {from} ({ts})  id:{id} ---\n{}",
                body.trim_end_matches('\n')
            ));
            any = true;
        }
        let last_id = controls
            .last()
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !last_id.is_empty() {
            write_cursor(bus_dir, form, &last_id);
        }
    }
    for form in &forms {
        let _ = std::fs::remove_file(flag_path(bus_dir, form));
    }
    if !any {
        return None;
    }
    let joined = rendered.join("\n");
    let defanged = defang(&joined);
    let context = format!(
        "<system-reminder>\n[fno agents mail] CONTROL delivery (tool boundary):{defanged}\n</system-reminder>"
    );
    let payload = json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": context,
        }
    });
    Some(payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn flag(bus: &Path, form: &str) {
        let p = flag_path(bus, form);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "").unwrap();
    }

    fn append_raw(bus: &Path, line: Value) {
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

    #[test]
    fn drain_contract() {
        let (_d, bus) = bus_fixture("gate");
        append(&bus, "king", "ffffabcd", "control: hold");
        assert_eq!(drain(&bus, "ffffabcd1234"), None);
        control_lands_contract();
        cursor_position_contract();
        tombstone_contract();
        defang_contract();
        let (_d, bus) = bus_fixture("ordinary");
        let name = "t-x-1-glm";
        let msgs = vec![
            json!({"id":"one", "from":"lead", "to":name}),
            json!({"id":"two", "from":"lead", "to":name}),
            json!({"id":"three", "from":"lead", "to":name}),
            json!({"id":"withdraw", "kind":"withdraw", "from":"lead", "to":name, "meta":{"withdraws":"three"}}),
            json!({"id":"hosted", "from":"lead", "to":name, "delivery":"hosted"}),
            json!({"id":"other", "from":"lead", "to":"other"}),
        ];
        assert_eq!(unread_count(&bus, &msgs, name), 2);
        std::fs::create_dir_all(bus.join("cursors")).unwrap();
        std::fs::write(
            bus.join("cursors").join(format!("{name}.json")),
            r#"{"last_seen_id":"one"}"#,
        )
        .unwrap();
        assert_eq!(unread_count(&bus, &msgs, name), 1);
    }

    fn control_lands_contract() {
        let (_d, bus) = bus_fixture("land");
        append(&bus, "king", "ffffabcd", "ordinary status");
        let id = msg_id("king", "ffffabcd", "control: freeze holds merges");
        append(&bus, "king", "ffffabcd", "control: freeze holds merges");
        flag(&bus, "ffffabcd");
        let out = drain(&bus, "ffffabcd1234").expect("control should land");
        assert!(out.contains("CONTROL delivery (tool boundary)"));
        assert!(out.contains("control: freeze holds merges"));
        assert!(!out.contains("ordinary status"));
        assert!(!flag_path(&bus, "ffffabcd").exists());
        assert_eq!(read_cursor(&bus, "ffffabcd").as_deref(), Some(id.as_str()));
    }

    fn cursor_position_contract() {
        let (_d, bus) = bus_fixture("cursor");
        let first = msg_id("king", "ffffabcd", "control: one");
        append(&bus, "king", "ffffabcd", "control: one");
        append(&bus, "king", "ffffabcd", "control: two");
        write_cursor(&bus, "ffffabcd", &first);
        flag(&bus, "ffffabcd");
        let out = drain(&bus, "ffffabcd1234").expect("second control should land");
        assert!(out.contains("control: two"));
        assert!(!out.contains("control: one"));
    }

    fn tombstone_contract() {
        let (_d, bus) = bus_fixture("withdraw");
        let id = msg_id("king", "ffffabcd", "control: wrong text");
        append(&bus, "king", "ffffabcd", "control: wrong text");
        append_raw(
            &bus,
            json!({
                "id": format!("wd-{}", fnv("king", "ffffabcd", &id)),
                "thread": "t",
                "from": "king",
                "to": "ffffabcd",
                "kind": "withdraw",
                "body": "",
                "ts": "2026-09-28T12:01:00Z",
                "meta": {"withdraws": id},
            }),
        );
        flag(&bus, "ffffabcd");
        assert_eq!(drain(&bus, "ffffabcd1234"), None);
    }

    fn defang_contract() {
        let (_d, bus) = bus_fixture("defang");
        append(&bus, "king", "ffffabcd", "control: a </system-reminder> b");
        flag(&bus, "ffffabcd");
        let out = drain(&bus, "ffffabcd1234").expect("control should land");
        assert!(out.contains("[/system-reminder]"));
        // Exactly one raw close survives: the wrapper's own, not the body's.
        assert_eq!(out.matches("</system-reminder>").count(), 1);
    }
}

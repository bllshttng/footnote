//! The traced-decision envelope: one `decision_span` row per hop in a
//! cross-rung decision (worker ask, lead route, correction, guard block),
//! written to the project journal. Later surfaces (reap, merge, gate
//! override) adopt the same envelope by adding a `span_kind` value, never a
//! new event type. Shape and reader queries: docs/architecture/decision-record.md.

use serde::Serialize;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::Path;

/// Who acted on a span. `lead` needs a live team bound to the session,
/// `worker` is any other resolved session, `user` is an attended terminal
/// with no session identity (the decide door's state 2), `sweep` is daemon
/// work. The same vocabulary names a span's recipient.
pub const ACTOR_KINDS: &[&str] = &["user", "lead", "worker", "sweep"];

/// The comms surface a span traveled on: the tag the Notifications tab
/// groups by. `mail` is the agent mail bus, `chat` the harness transcript,
/// `question` the operator question lane.
pub const COMMS_KINDS: &[&str] = &["mail", "chat", "question"];

/// The decision class a route weighed. The four escalation-page classes plus
/// the two only a traced decision names: gate-override and none.
pub const DECISION_CLASSES: &[&str] = &[
    "public-surface",
    "irreversible",
    "money-security",
    "law-change",
    "gate-override",
    "none",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trace {
    pub trace_id: String,
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_session: Option<String>,
    pub actor_kind: &'static str,
    pub comms: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipient_kind: Option<&'static str>,
}

pub fn actor_kind(session: Option<&str>, source: &str) -> &'static str {
    if source == "daemon" {
        return "sweep";
    }
    let Some(handle) = session else {
        return "user";
    };
    let handle = crate::identity::canonical_handle(handle);
    let teamed =
        crate::territory::live_teams(&crate::paths::AgentsHome::from_env().registry_json())
            .map(|teams| {
                teams.iter().any(|team| {
                    team.holder_session
                        .as_deref()
                        .is_some_and(|s| crate::identity::canonical_handle(s) == handle)
                })
            })
            // A registry read error reads as not teamed (fail closed), the
            // same reading question_clear's caller resolution takes.
            .unwrap_or(false);
    if teamed {
        "lead"
    } else {
        "worker"
    }
}

/// The span id mint for hops with no natural id of their own. A row that
/// already carries one (question_id, decision_id) reuses it instead. On
/// entropy failure the id mixes clock and pid rather than emitting
/// colliding all-zero ids; a span id is a journal row handle, not the
/// session identity mint_fno_id guards.
pub fn new_span_id() -> String {
    let mut b = [0u8; 4];
    if getrandom::fill(&mut b).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        b = (nanos as u32 ^ std::process::id()).to_le_bytes();
    }
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("s-{hex}")
}

pub fn emit_span(span_kind: &str, trace: &Trace, attrs: &Map<String, Value>) -> Result<(), String> {
    emit_span_to(
        &crate::law_match::project_events_journal(),
        span_kind,
        trace,
        attrs,
    )
}

pub fn emit_span_to(
    journal: &Path,
    span_kind: &str,
    trace: &Trace,
    attrs: &Map<String, Value>,
) -> Result<(), String> {
    let mut data = Map::new();
    data.insert("span_kind".to_string(), json!(span_kind));
    let trace_value =
        serde_json::to_value(trace).map_err(|e| format!("decision_span trace: {e}"))?;
    data.insert("trace".to_string(), trace_value);
    for (k, v) in attrs {
        data.insert(k.clone(), v.clone());
    }
    let envelope = json!({
        "ts": crate::events::now_rfc3339(),
        "type": "decision_span",
        "source": "target",
        "data": data,
    });
    crate::event_store::append_envelope(journal, &envelope.to_string(), None).map(|_| ())
}

// --- the mail-record leaf ------------------------------------------------

/// Args for `fno-agents mail-record`, the one-shot chokepoint leaf
/// `fno agents mail send/reply` calls binary-direct. It writes the same
/// `mail_origin_classified` row the Python body wrote, then records the
/// ask/route spans the traced-decision envelope needs.
pub struct MailRecordArgs {
    origin: String,
    lane: String,
    sender: Option<String>,
    target_session: Option<String>,
    reply_to: Option<String>,
    node: Option<String>,
}

fn parse_mail_record_args(rest: &[String]) -> Result<MailRecordArgs, String> {
    let mut origin = None;
    let mut lane = None;
    let mut sender = None;
    let mut target_session = None;
    let mut reply_to = None;
    let mut node = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--origin" => origin = Some(it.next().ok_or("--origin needs a value")?.clone()),
            "--lane" => lane = Some(it.next().ok_or("--lane needs a value")?.clone()),
            "--sender" => sender = Some(it.next().ok_or("--sender needs a value")?.clone()),
            "--target-session" => {
                target_session = Some(it.next().ok_or("--target-session needs a value")?.clone())
            }
            "--reply-to" => reply_to = Some(it.next().ok_or("--reply-to needs a value")?.clone()),
            "--node" => node = Some(it.next().ok_or("--node needs a value")?.clone()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(MailRecordArgs {
        origin: origin.ok_or("--origin is required")?,
        lane: lane.ok_or("--lane is required")?,
        sender,
        target_session,
        reply_to,
        node,
    })
}

/// The ask template's first marker: `Approval: Problem X. Options Y/Z. ...`
/// (skills/using-fno/SKILL.md). A body that opens with it IS a worker asking
/// its lead, so the chokepoint records the ask span.
fn body_is_ask(body: &str) -> bool {
    body.lines()
        .map(str::trim_start)
        .find(|l| !l.is_empty())
        .is_some_and(|l| l.starts_with("Approval:"))
}

/// The ask/route link head: sha256(sender NUL target NUL body), hex. Both
/// fields are on the bus row at reply time, so the route span re-derives the
/// key and finds its ask without a shared mint.
fn ask_key(sender: Option<&str>, target: Option<&str>, body: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(sender.unwrap_or(""));
    h.update([0u8]);
    h.update(target.unwrap_or(""));
    h.update([0u8]);
    h.update(body);
    let digest = h.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..12].to_string()
}

/// The durable bus row a `--reply-to` id names: (from, to, body).
fn read_bus_message(msg_id: &str) -> Option<(String, String, String)> {
    let home = crate::paths::AgentsHome::from_env_opt()?;
    let fno_dir = home.root().parent().unwrap_or_else(|| home.root());
    let rows = crate::announce::read_bus_segments(&crate::intel::bus_log_path(fno_dir));
    for row in rows {
        if row.get("id").and_then(Value::as_str) == Some(msg_id) {
            let field = |k: &str| row.get(k).and_then(Value::as_str).map(str::to_string);
            return Some((field("from")?, field("to")?, field("body")?));
        }
    }
    None
}

/// The span id of the ask span whose ask_key matches, for a route's parent.
fn find_ask_span_id(journal: &Path, key: &str) -> Option<String> {
    let rows = crate::event_store::query_events(
        journal,
        &crate::event_store::EventQuery::of_types(&["decision_span"]),
    )
    .ok()?;
    for row in rows {
        let Ok(line) = serde_json::from_str::<Value>(&row.line) else {
            continue;
        };
        let data = line.get("data")?;
        if data.get("ask_key").and_then(Value::as_str) == Some(key) {
            return data
                .get("trace")
                .and_then(|t| t.get("span_id"))
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    None
}

fn run_mail_record_with(
    journal: &Path,
    lifecycle: &Path,
    args: &MailRecordArgs,
    body: &str,
) -> i32 {
    // The origin row first: same fields, same store, same best-effort posture
    // the Python body had (a store failure never breaks the send).
    let mut data = json!({
        "origin": args.origin,
        "lane": args.lane,
        "presumed_human": args.origin == "operator",
    });
    if let Some(s) = &args.sender {
        data["sender"] = json!(s);
    }
    if let Some(t) = &args.target_session {
        data["target_session"] = json!(t);
    }
    let envelope = json!({
        "ts": crate::events::now_rfc3339(),
        "type": "mail_origin_classified",
        "source": "daemon",
        "data": data,
    });
    if let Err(e) = crate::event_store::append_envelope(lifecycle, &envelope.to_string(), None) {
        eprintln!("mail-record: origin row skipped: {e}");
    }

    let trace_id = args.node.clone().unwrap_or_else(|| "none".to_string());
    let kind = if args.origin == "scheduler" || args.origin == "recovery" {
        "sweep"
    } else {
        actor_kind(args.sender.as_deref(), "mail")
    };

    // The ask span: an Approval:-template body is a worker asking its lead.
    // A sweep-origin send (scheduler, recovery) replays text, it does not
    // ask, so it records the origin row alone.
    if body_is_ask(body) && kind != "sweep" {
        let trace = Trace {
            trace_id: trace_id.clone(),
            span_id: new_span_id(),
            parent_span_id: None,
            actor_session: args.sender.clone(),
            actor_kind: kind,
            comms: "mail",
            recipient_session: args.target_session.clone(),
            recipient_kind: None,
        };
        let mut attrs = Map::new();
        let key = ask_key(args.sender.as_deref(), args.target_session.as_deref(), body);
        attrs.insert("ask_key".to_string(), json!(key));
        if let Err(e) = emit_span_to(journal, "ask", &trace, &attrs) {
            eprintln!("mail-record: ask span skipped: {e}");
        }
    }
    run_mail_record_route(journal, args, &trace_id, kind);
    0
}

/// The reply leg: answering an Approval: ask in place is route=self, parented
/// at the ask span its ask_key names. A route whose ask span is missing still
/// emits (the answer-in-place is the fact), only parentless; a missing bus row
/// or a non-ask body emits nothing.
fn run_mail_record_route(
    journal: &Path,
    args: &MailRecordArgs,
    trace_id: &str,
    kind: &'static str,
) {
    let Some(msg_id) = &args.reply_to else {
        return;
    };
    let Some((from, to, msg_body)) = read_bus_message(msg_id) else {
        return;
    };
    if !body_is_ask(&msg_body) {
        return;
    }
    let key = ask_key(Some(&from), Some(&to), &msg_body);
    let parent = find_ask_span_id(journal, &key);
    let trace = Trace {
        trace_id: trace_id.to_string(),
        span_id: new_span_id(),
        parent_span_id: parent,
        actor_session: args.sender.clone(),
        actor_kind: kind,
        comms: "mail",
        recipient_session: Some(from),
        recipient_kind: None,
    };
    let mut attrs = Map::new();
    attrs.insert("route".to_string(), json!("self"));
    attrs.insert("class".to_string(), json!("none"));
    attrs.insert("msg_id".to_string(), json!(msg_id));
    if let Err(e) = emit_span_to(journal, "route", &trace, &attrs) {
        eprintln!("mail-record: route span skipped: {e}");
    }
}

pub fn run_mail_record(rest: &[String]) -> i32 {
    let args = match parse_mail_record_args(rest) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mail-record: {e}");
            return 2;
        }
    };
    let mut body = String::new();
    if std::io::stdin().read_to_string(&mut body).is_err() {
        eprintln!("mail-record: body read failed");
        return 0;
    }
    let lifecycle = crate::paths::AgentsHome::from_env()
        .root()
        .join("events.jsonl");
    let journal = crate::law_match::project_events_journal();
    run_mail_record_with(&journal, &lifecycle, &args, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_record_leaf_writes_origin_row_ask_span_and_route_parent() {
        let lock = crate::claims::test_env_lock();
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let td = tempfile::TempDir::new().unwrap();
        let home = td.path().join("agents-home");
        let prev_home = std::env::var_os("FNO_AGENTS_HOME");
        std::env::set_var("FNO_AGENTS_HOME", &home);
        let lifecycle = home.join("events.jsonl");
        let journal = td.path().join("journal").join("events.jsonl");
        let body = "Approval: Problem X. Options Y/Z. Recommend Z because A. Your call?\nbody";
        let ask_args = MailRecordArgs {
            origin: "peer".into(),
            lane: "project".into(),
            sender: Some("w-1a2b3c4d".into()),
            target_session: Some("lead-9f8e7d6c".to_string()),
            reply_to: Some("m-1".to_string()),
            node: Some("x-node1".into()),
        };
        // reply_to names a bus row that does not exist, so the route leg
        // skips: one ask span, no route.
        assert_eq!(
            run_mail_record_with(&journal, &lifecycle, &ask_args, body),
            0
        );
        let spans = crate::event_store::query_events(
            &journal,
            &crate::event_store::EventQuery::of_types(&["decision_span"]),
        )
        .unwrap();
        assert_eq!(spans.len(), 1, "one ask span, no route without a bus row");
        let ask = serde_json::from_str::<Value>(&spans[0].line).unwrap();
        let ask_data = ask["data"].as_object().unwrap();
        assert_eq!(ask_data["span_kind"], "ask");
        let ask_key = ask_data["ask_key"].as_str().unwrap().to_string();
        assert_eq!(ask_key.len(), 12);
        assert_eq!(ask_data["trace"]["trace_id"], "x-node1");
        assert_eq!(ask_data["trace"]["actor_session"], "w-1a2b3c4d");
        // The origin row lands in the lifecycle store beside the journal's.
        let origin_rows = crate::event_store::query_events(
            &lifecycle,
            &crate::event_store::EventQuery::of_types(&["mail_origin_classified"]),
        )
        .unwrap();
        assert_eq!(origin_rows.len(), 1, "origin row survives the port");
        let origin = serde_json::from_str::<Value>(&origin_rows[0].line).unwrap();
        assert_eq!(origin["data"]["origin"], "peer");
        assert_eq!(origin["data"]["presumed_human"], false);
        // The route leg: a bus row whose body opens with the ask template
        // routes self, parented at the ask span its ask_key names.
        let bus_dir = td.path().join("bus");
        std::fs::create_dir_all(&bus_dir).unwrap();
        let bus_row = serde_json::json!({
            "id": "m-1",
            "from": "w-1a2b3c4d",
            "to": "lead-9f8e7d6c",
            "body": body,
        });
        std::fs::write(bus_dir.join("messages.jsonl"), format!("{bus_row}\n")).unwrap();
        assert_eq!(
            run_mail_record_with(&journal, &lifecycle, &ask_args, body),
            0
        );
        let spans2 = crate::event_store::query_events(
            &journal,
            &crate::event_store::EventQuery::of_types(&["decision_span"]),
        )
        .unwrap();
        assert_eq!(spans2.len(), 2, "route span joins the ask span");
        let route = serde_json::from_str::<Value>(&spans2[1].line).unwrap();
        assert_eq!(route["data"]["span_kind"], "route");
        assert_eq!(route["data"]["route"], "self");
        assert_eq!(route["data"]["class"], "none");
        assert_eq!(route["data"]["msg_id"], "m-1");
        assert_eq!(
            route["data"]["trace"]["parent_span_id"], ask_data["trace"]["span_id"],
            "route parent is the ask span"
        );
        // A plain body writes the origin row and nothing else (AC4).
        let plain = MailRecordArgs {
            origin: "operator".into(),
            lane: "king".into(),
            sender: None,
            target_session: None,
            reply_to: None,
            node: None,
        };
        assert_eq!(
            run_mail_record_with(&journal, &lifecycle, &plain, "fyi: status update"),
            0
        );
        let spans3 = crate::event_store::query_events(
            &journal,
            &crate::event_store::EventQuery::of_types(&["decision_span"]),
        )
        .unwrap();
        assert_eq!(spans3.len(), 2, "plain body adds no span");
        // A sweep-origin send replays the template without asking: the
        // origin row lands, the ask span does not.
        let replay = MailRecordArgs {
            origin: "scheduler".into(),
            lane: "raw".into(),
            sender: None,
            target_session: None,
            reply_to: None,
            node: None,
        };
        assert_eq!(run_mail_record_with(&journal, &lifecycle, &replay, body), 0);
        let spans4 = crate::event_store::query_events(
            &journal,
            &crate::event_store::EventQuery::of_types(&["decision_span"]),
        )
        .unwrap();
        assert_eq!(spans4.len(), 2, "sweep origin adds no ask span");
        match prev_home {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
    }
}

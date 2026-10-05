//! Event rules: a declared table of event, condition and action rows run at
//! the Stop boundary (`docs/architecture/event-rules.md`). A row names the
//! event it watches (`stop`, a journal type, or `decision_span:<kind>`), the
//! named predicates that must all hold, and one action (`block`, `nudge`,
//! `notify`, `emit`). Every fire writes a `decision_span` span whose `rule`
//! and `matched_event` attrs are the dedupe ledger, so a nudge fires once
//! per matched event and a block caps at [`BLOCK_CAP`] per Stop turn. Rows
//! enable through `event_rules.<id>` config overrides read with
//! `config_value_deep`; the shipped table lives beside this file and is
//! `include_str!`ed like `merge_posture.toml`.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::decision_trace::Trace;

const TABLE: &str = include_str!("event_rules.toml");

/// Block fires one rule may spend on one Stop turn before the engine allows
/// and the fire's span carries `cap_reached`.
pub(crate) const BLOCK_CAP: usize = 3;

const ACTIONS: &[&str] = &["block", "nudge", "notify", "emit"];

/// The four classes only the user decides; `class_user_only` reads them off
/// a question's escalate route span.
const USER_CLASSES: &[&str] = &[
    "public-surface",
    "irreversible",
    "money-security",
    "law-change",
];

/// Every predicate name a row may name, `not:`-prefixable. An unknown name
/// fails the table load test, never the runtime match below.
const PREDICATES: &[&str] = &[
    "crowned",
    "last_message_decision_ask",
    "turn_filed_question",
    "route_self",
    "actor_is_session",
    "reversible",
    "has_recommendation",
    "why_user_set",
    "class_user_only",
];

struct Row {
    id: String,
    event: String,
    when: Vec<String>,
    action: String,
    what: String,
    why: String,
    instead: String,
    default: bool,
}

/// One fired action. `reason` is the Stop block text for `block`/`nudge`;
/// `notify` carries the (title, body, pointer) the caller sends through
/// `notify_operator` - the engine decides, the caller executes, so tests
/// never spawn a process.
pub struct Fire {
    pub rule: String,
    pub action: &'static str,
    pub reason: String,
    pub notify: Option<(String, String, Option<String>)>,
}

fn parse_table() -> Result<Vec<Row>, String> {
    let raw: toml::Value = toml::from_str(TABLE).map_err(|e| format!("event_rules.toml: {e}"))?;
    let items = raw
        .get("rule")
        .and_then(toml::Value::as_array)
        .ok_or("event_rules.toml: the [[rule]] array is missing")?;
    let mut rows = Vec::new();
    for item in items {
        let get = |k: &str| {
            item.get(k)
                .and_then(toml::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let row = Row {
            id: get("id"),
            event: get("event"),
            when: item
                .get("when")
                .and_then(toml::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            action: get("action"),
            what: get("what"),
            why: get("why"),
            instead: get("instead"),
            default: item
                .get("default")
                .and_then(toml::Value::as_bool)
                .unwrap_or(true),
        };
        if row.id.is_empty() {
            return Err("event_rules.toml: a row has no id".into());
        }
        if !ACTIONS.contains(&row.action.as_str()) {
            return Err(format!(
                "event_rules.toml: {} names unknown action {:?}",
                row.id, row.action
            ));
        }
        if row.event.is_empty() {
            return Err(format!("event_rules.toml: {} names no event", row.id));
        }
        if row.what.is_empty() || row.why.is_empty() || row.instead.is_empty() {
            return Err(format!(
                "event_rules.toml: {} must carry non-empty what, why and instead",
                row.id
            ));
        }
        for p in &row.when {
            let name = p.strip_prefix("not:").unwrap_or(p);
            if !PREDICATES.contains(&name) {
                return Err(format!(
                    "event_rules.toml: {} names unknown predicate {p:?}",
                    row.id
                ));
            }
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Err("event_rules.toml: no rows".into());
    }
    Ok(rows)
}

fn table() -> &'static Vec<Row> {
    static CELL: OnceLock<Vec<Row>> = OnceLock::new();
    CELL.get_or_init(|| parse_table().expect("event_rules.toml must load"))
}

fn enabled(cwd: &Path, row: &Row) -> bool {
    match crate::agents_config::config_value_deep(cwd, &["event_rules", &row.id]) {
        Some(v) => v.as_bool().unwrap_or(row.default),
        None => row.default,
    }
}

/// What one engine run reads and answers against.
struct Ctx<'a> {
    journal: &'a Path,
    home: &'a crate::paths::AgentsHome,
    /// The firing session's canonical (head-8) handle.
    session: String,
    last_message: String,
    /// The matched_event key Stop rows dedupe and cap on.
    matched_event: String,
    /// The turn window `turn_filed_question` scans: since the transcript's
    /// last user turn, or the last 30 minutes when it cannot be read.
    window_ms: i64,
}

/// Evaluate the table for one Stop fire. Every harness's Stop rides through
/// here, owned session or not: the rules a session owes do not depend on a
/// target or king manifest. Best-effort throughout - a journal read failure
/// answers no fires rather than blocking an unreadable turn.
pub fn eval_stop(cwd: &Path, payload: &str) -> Vec<Fire> {
    let journal = crate::law_match::project_events_journal();
    let home = crate::paths::AgentsHome::from_env();
    eval_stop_in(cwd, &journal, &home, payload)
}

pub fn eval_stop_in(
    cwd: &Path,
    journal: &Path,
    home: &crate::paths::AgentsHome,
    payload: &str,
) -> Vec<Fire> {
    let Ok(parsed) = serde_json::from_str::<Value>(payload.trim()) else {
        return Vec::new();
    };
    let session_id = parsed
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if session_id.is_empty() {
        return Vec::new();
    }
    let session = crate::identity::canonical_handle(session_id);
    let last_message = parsed
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let turn_id = parsed
        .get("turn_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let transcript_path = parsed
        .get("transcript_path")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let now_ms = now_ms();
    let turn_ts = transcript_turn_window(transcript_path);
    let window_ms = turn_ts.unwrap_or(now_ms - 30 * 60 * 1000);
    let turn_key = match (!turn_id.is_empty(), turn_ts) {
        (true, _) => turn_id,
        (false, Some(ts)) => format!("t{ts}"),
        (false, None) => format!("b{}", now_ms / 1_800_000),
    };
    let matched_event = format!("stop:{session}:{turn_key}");
    let ctx = Ctx {
        journal,
        home,
        session,
        last_message,
        matched_event,
        window_ms,
    };

    let (cursor_seq, cursor_ts) = read_cursor(journal, &ctx.session);
    let since_ms = cursor_ts.unwrap_or(now_ms - 30 * 60 * 1000);
    let query = crate::event_store::EventQuery {
        since_ms: Some(since_ms),
        ..crate::event_store::EventQuery::of_types(&["decision_span", "operator_question"])
    };
    let rows = crate::event_store::query_events(journal, &query).unwrap_or_default();
    let fresh: Vec<&crate::event_store::EventRow> =
        rows.iter().filter(|r| r.seq > cursor_seq).collect();
    let max_seq = fresh.iter().map(|r| r.seq).max().unwrap_or(cursor_seq);
    let max_ts = fresh
        .iter()
        .max_by_key(|r| r.seq)
        .map(|r| r.ts_ms)
        .or(cursor_ts)
        .unwrap_or(now_ms);

    // The dedupe ledger loads on the first candidate fire: a Stop with no
    // ask and no journal event never scans the span table.
    let mut ledger: Option<Vec<(String, String, String)>> = None;
    let mut fires = Vec::new();
    for row in table() {
        if !enabled(cwd, row) {
            continue;
        }
        match row.event.as_str() {
            "stop" => {
                if row.when.iter().all(|p| predicate(&ctx, p, None)) {
                    let ledger = match &mut ledger {
                        Some(l) => l,
                        None => ledger.insert(ledger_rows(journal)),
                    };
                    if let Some(fire) = stop_block(&ctx, row, ledger, journal) {
                        fires.push(fire);
                    }
                }
            }
            "decision_span:route" | "operator_question" => {
                let wanted_kind = row
                    .event
                    .strip_prefix("decision_span:")
                    .map(|k| k.to_string());
                for r in &fresh {
                    let Ok(v) = serde_json::from_str::<Value>(&r.line) else {
                        continue;
                    };
                    if row.event == "decision_span:route" && span_kind(&v) != wanted_kind.as_deref()
                    {
                        continue;
                    }
                    if row.event == "operator_question" && r.r#type != "operator_question" {
                        continue;
                    }
                    if row.when.iter().all(|p| predicate(&ctx, p, Some(&v))) {
                        let ledger = match &mut ledger {
                            Some(l) => l,
                            None => ledger.insert(ledger_rows(journal)),
                        };
                        if let Some(fire) = event_action(&ctx, row, &v, ledger, journal) {
                            fires.push(fire);
                        }
                    }
                }
            }
            // Unknown events fail the table load test; this arm is unreachable.
            _ => {}
        }
    }
    if max_seq > cursor_seq || cursor_ts.is_none() {
        write_cursor(journal, &ctx.session, max_seq, max_ts);
    }
    fires
}

/// The block action on a Stop row: repeat while the condition holds, capped
/// at [`BLOCK_CAP`] fires per turn key, the last one carrying `cap_reached`.
fn stop_block(
    ctx: &Ctx,
    row: &Row,
    ledger: &mut Vec<(String, String, String)>,
    journal: &Path,
) -> Option<Fire> {
    let prior = ledger
        .iter()
        .filter(|(r, m, k)| r == &row.id && m == &ctx.matched_event && k == "block")
        .count();
    if prior >= BLOCK_CAP {
        return None;
    }
    let mut attrs = Map::new();
    if prior + 1 >= BLOCK_CAP {
        attrs.insert("cap_reached".to_string(), json!(true));
    }
    write_fire_span(
        journal,
        ctx,
        "block",
        &row.id,
        &ctx.matched_event,
        attrs,
        "none",
        None,
        "chat",
    );
    Some(Fire {
        rule: row.id.clone(),
        action: "block",
        reason: block_reason(row),
        notify: None,
    })
}

/// The notify/nudge actions on a journal-event row: once per matched event,
/// deduped on the span ledger (`rule` + `matched_event`).
fn event_action(
    ctx: &Ctx,
    row: &Row,
    event_row: &Value,
    ledger: &mut Vec<(String, String, String)>,
    journal: &Path,
) -> Option<Fire> {
    let data = event_row.get("data")?;
    let trace = data.get("trace")?;
    let event_id = trace.get("span_id").and_then(Value::as_str)?;
    if ledger.iter().any(|(r, m, _)| r == &row.id && m == event_id) {
        return None;
    }
    let trace_id = trace
        .get("trace_id")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let comms = comms_of(trace.get("comms").and_then(Value::as_str).unwrap_or("chat"));
    let reason = block_reason(row);
    match row.action.as_str() {
        "notify" => {
            let actor = trace
                .get("actor_session")
                .and_then(Value::as_str)
                .unwrap_or(&ctx.session);
            let rec = data
                .get("recommendation")
                .and_then(Value::as_str)
                .unwrap_or("none");
            let body = format!(
                "{actor} decided {trace_id} itself: {rec}. Reverse with fno inbox decide --overturns {event_id} --authority operator."
            );
            write_fire_span(
                journal,
                ctx,
                "notify",
                &row.id,
                event_id,
                Map::new(),
                trace_id,
                Some(event_id.to_string()),
                comms,
            );
            Some(Fire {
                rule: row.id.clone(),
                action: "notify",
                reason,
                notify: Some((
                    format!("event rule {}", row.id),
                    body,
                    Some("fno inbox outstanding".to_string()),
                )),
            })
        }
        "nudge" => {
            write_fire_span(
                journal,
                ctx,
                "nudge",
                &row.id,
                event_id,
                Map::new(),
                trace_id,
                Some(event_id.to_string()),
                comms,
            );
            Some(Fire {
                rule: row.id.clone(),
                action: "nudge",
                reason,
                notify: None,
            })
        }
        // `emit` rows only mark their event seen (the cursor is the record);
        // `block` is a Stop-event action. Neither reaches here today.
        _ => None,
    }
}

/// The fixed predicate match. Unknown names never reach the wildcard: the
/// table load refuses them, so the wildcard is a safe default, not a seam.
fn predicate(ctx: &Ctx, name: &str, ev: Option<&Value>) -> bool {
    let (negated, name) = match name.strip_prefix("not:") {
        Some(rest) => (true, rest),
        None => (false, name),
    };
    let value = match name {
        "crowned" => {
            crate::decision_trace::actor_kind_in(ctx.home, Some(&ctx.session), "hook") == "lead"
        }
        "last_message_decision_ask" => decision_ask(&ctx.last_message),
        "turn_filed_question" => turn_filed_question(ctx),
        "route_self" => {
            ev.and_then(|e| e.get("data"))
                .and_then(|d| d.get("route"))
                .and_then(Value::as_str)
                == Some("self")
        }
        "actor_is_session" => ev.is_some_and(|e| actor_is_session(ctx, e)),
        "reversible" => ev
            .and_then(|e| e.get("data"))
            .and_then(|d| d.get("context"))
            .and_then(|c| c.get("reversible"))
            .and_then(Value::as_str)
            .is_some_and(|v| v.eq_ignore_ascii_case("yes")),
        "has_recommendation" => ev
            .and_then(|e| e.get("data"))
            .and_then(|d| d.get("context"))
            .and_then(|c| c.get("recommendation"))
            .is_some_and(|r| r.as_object().is_some_and(|m| !m.is_empty())),
        "why_user_set" => ev
            .and_then(|e| e.get("data"))
            .and_then(|d| d.get("why_user"))
            .and_then(Value::as_str)
            .is_some_and(|v| !v.trim().is_empty()),
        "class_user_only" => ev.is_some_and(|e| class_user_only(ctx, e)),
        _ => false,
    };
    negated != value
}

/// A worker ask addressed to its lead (the `Approval:` template), or any ask
/// sentence plus a numbered list of two or more options - the chat shape the
/// R1 rows police.
fn decision_ask(text: &str) -> bool {
    if text
        .lines()
        .any(|l| l.trim_start().starts_with("Approval:"))
    {
        return true;
    }
    if crate::repeated_asks::asks_in(text).is_empty() {
        return false;
    }
    numbered_options(text) >= 2
}

fn numbered_options(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        let at_start = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if at_start && bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len()
                && (bytes[j] == b'.' || bytes[j] == b')')
                && bytes.get(j + 1).is_some_and(u8::is_ascii_whitespace)
            {
                count += 1;
                i = j;
            }
        }
        i += 1;
    }
    count
}

/// The journal already holds something this session filed or routed since
/// the turn began: an `operator_question` it asked, or a `decision_span`
/// route span it acted on. That is the ask the chat message points at.
fn turn_filed_question(ctx: &Ctx) -> bool {
    let query = crate::event_store::EventQuery {
        since_ms: Some(ctx.window_ms),
        ..crate::event_store::EventQuery::of_types(&["operator_question", "decision_span"])
    };
    let Ok(rows) = crate::event_store::query_events(ctx.journal, &query) else {
        return false;
    };
    rows.iter().any(|row| {
        let Ok(v) = serde_json::from_str::<Value>(&row.line) else {
            return false;
        };
        match row.r#type.as_str() {
            "operator_question" => actor_is_session(ctx, &v),
            _ => span_kind(&v) == Some("route") && actor_is_session(ctx, &v),
        }
    })
}

/// The row is this session's doing: its `asker` or its trace actor resolves
/// to the same canonical handle.
fn actor_is_session(ctx: &Ctx, row: &Value) -> bool {
    let data = row.get("data");
    let asker = data.and_then(|d| d.get("asker")).and_then(Value::as_str);
    let actor = data
        .and_then(|d| d.get("trace"))
        .and_then(|t| t.get("actor_session"))
        .and_then(Value::as_str);
    [asker, actor]
        .into_iter()
        .flatten()
        .any(|h| crate::identity::canonical_handle(h) == ctx.session)
}

/// The question's escalate route span declared one of the four user classes.
/// No parent span, or a parent whose class is absent, reads as not
/// user-only: the question never claimed a class, so the escape check fires.
fn class_user_only(ctx: &Ctx, row: &Value) -> bool {
    let parent = row
        .get("data")
        .and_then(|d| d.get("trace"))
        .and_then(|t| t.get("parent_span_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(parent) = parent else {
        return false;
    };
    let Ok(spans) = crate::event_store::query_events(
        ctx.journal,
        &crate::event_store::EventQuery::of_types(&["decision_span"]),
    ) else {
        return false;
    };
    for s in spans {
        let Ok(v) = serde_json::from_str::<Value>(&s.line) else {
            continue;
        };
        let span_id = v
            .get("data")
            .and_then(|d| d.get("trace"))
            .and_then(|t| t.get("span_id"))
            .and_then(Value::as_str);
        if span_id != Some(parent.as_str()) {
            continue;
        }
        let class = v
            .get("data")
            .and_then(|d| d.get("class"))
            .and_then(Value::as_str)
            .unwrap_or("none");
        return USER_CLASSES.contains(&class);
    }
    false
}

fn span_kind(row: &Value) -> Option<&str> {
    row.get("data")
        .and_then(|d| d.get("span_kind"))
        .and_then(Value::as_str)
}

fn comms_of(comms: &str) -> &'static str {
    match comms {
        "mail" => "mail",
        "question" => "question",
        _ => "chat",
    }
}

fn block_reason(row: &Row) -> String {
    format!(
        "rule {}: {}. Why: {}. Do instead: {}.",
        row.id, row.what, row.why, row.instead
    )
}

/// Every span already attributed to a rule: (rule, matched_event, span_kind).
/// The ledger the actions dedupe and cap on - a journal read, not a second
/// store.
fn ledger_rows(journal: &Path) -> Vec<(String, String, String)> {
    let Ok(spans) = crate::event_store::query_events(
        journal,
        &crate::event_store::EventQuery::of_types(&["decision_span"]),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for s in spans {
        let Ok(v) = serde_json::from_str::<Value>(&s.line) else {
            continue;
        };
        let data = v.get("data");
        let rule = data.and_then(|d| d.get("rule")).and_then(Value::as_str);
        let matched = data
            .and_then(|d| d.get("matched_event"))
            .and_then(Value::as_str);
        let kind = span_kind(&v).unwrap_or_default();
        if let (Some(rule), Some(matched)) = (rule, matched) {
            out.push((rule.to_string(), matched.to_string(), kind.to_string()));
        }
    }
    out
}

/// One fire's journal row. The span IS the ledger entry: the next run reads
/// it back instead of keeping state anywhere else.
fn write_fire_span(
    journal: &Path,
    ctx: &Ctx,
    kind: &str,
    rule: &str,
    matched_event: &str,
    mut attrs: Map<String, Value>,
    trace_id: &str,
    parent_span_id: Option<String>,
    comms: &'static str,
) {
    attrs.insert("rule".to_string(), json!(rule));
    attrs.insert("matched_event".to_string(), json!(matched_event));
    let trace = Trace {
        trace_id: trace_id.to_string(),
        span_id: crate::decision_trace::new_span_id(),
        parent_span_id,
        actor_session: Some(ctx.session.clone()),
        actor_kind: crate::decision_trace::actor_kind_in(ctx.home, Some(&ctx.session), "hook"),
        comms,
        recipient_session: None,
        recipient_kind: None,
    };
    if let Err(e) = crate::decision_trace::emit_span_to(journal, kind, &trace, &attrs) {
        eprintln!("event_rules: {rule} span skipped: {e}");
    }
}

// ── the per-session cursor ──────────────────────────────────────────────────

/// Where this session's engine left off: the seq and ts of the newest
/// journal row it processed. Session-keyed files in a subfolder, per the
/// state-root inventory; a missing or malformed cursor reads as a first
/// fire and the engine starts 30 minutes back.
fn cursor_path(journal: &Path, session: &str) -> PathBuf {
    let dir = journal
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let key = if session.is_empty() {
        "anon".to_string()
    } else {
        session.to_string()
    };
    dir.join("event-rules").join(format!("{key}.cursor"))
}

fn read_cursor(journal: &Path, session: &str) -> (i64, Option<i64>) {
    let Ok(text) = std::fs::read_to_string(cursor_path(journal, session)) else {
        return (0, None);
    };
    let mut parts = text.split_whitespace();
    let seq = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (seq, parts.next().and_then(|s| s.parse().ok()))
}

fn write_cursor(journal: &Path, session: &str, seq: i64, ts_ms: i64) {
    let path = cursor_path(journal, session);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(path, format!("{seq} {ts_ms}\n")).ok();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The last user turn's timestamp in a harness transcript, when the file
/// carries one. ponytail: reads the whole transcript each fire, as
/// repeated_asks does; seek to a tail window if the read ever shows.
fn transcript_turn_window(path: &str) -> Option<i64> {
    if path.is_empty() {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines().rev() {
        if !line.contains("user") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let is_user = row.get("type").and_then(Value::as_str) == Some("user")
            || crate::provenance::is_user_turn(&row);
        if !is_user {
            continue;
        }
        if let Some(ts) =
            find_timestamp(&row).and_then(|s| crate::event_store::parse_rfc3339_ms(&s))
        {
            return Some(ts);
        }
    }
    None
}

fn find_timestamp(v: &Value) -> Option<String> {
    match v {
        Value::Object(m) => {
            if let Some(s) = m.get("timestamp").and_then(Value::as_str) {
                return Some(s.to_string());
            }
            m.values().find_map(find_timestamp)
        }
        _ => None,
    }
}

/// `fno-agents hook rules --event stop` - the transport entry adapters call
/// beside their own loop-check when their Stop cannot carry the rules
/// (opencode, pi, agy). The Stop payload rides stdin; a block or nudge
/// prints the same JSON the stop gate prints and the adapter relays the
/// reason to its session.
pub fn run_hook(args: &[String]) -> i32 {
    let mut event = String::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--event" => event = it.next().cloned().unwrap_or_default(),
            other => {
                eprintln!("fno-agents hook rules: unknown argument {other}");
                return 2;
            }
        }
    }
    if event != "stop" {
        eprintln!("fno-agents hook rules: only --event stop is wired");
        return 2;
    }
    let payload = crate::hook::read_stdin();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let fires = eval_stop(&cwd, &payload);
    for fire in &fires {
        if let Some((title, body, pointer)) = &fire.notify {
            crate::operator_notice::notify_operator(title, body, pointer.as_deref());
        }
    }
    if let Some(fire) = fires
        .iter()
        .find(|f| f.action == "block" || f.action == "nudge")
    {
        println!(
            "{}",
            serde_json::json!({"decision": "block", "reason": fire.reason})
        );
        return 0;
    }
    println!("{{}}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SESSION: &str = "ses_kd63be01";

    /// One engine test's world: a temp agents home (its registry crowns the
    /// test session when asked), a temp journal, and the canonical handle.
    struct Rig {
        dir: tempfile::TempDir,
        home: crate::paths::AgentsHome,
        journal: PathBuf,
        cwd: PathBuf,
        session: String,
    }

    impl Rig {
        fn new(crowned: bool) -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let home_dir = dir.path().join("agents-home");
            let journal_dir = dir.path().join("journal");
            let cwd = dir.path().join("project");
            std::fs::create_dir_all(&home_dir).unwrap();
            std::fs::create_dir_all(&journal_dir).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            std::env::set_var("FNO_AGENTS_HOME", &home_dir);
            let session = crate::identity::canonical_handle(SESSION);
            if crowned {
                let registry = json!({
                    "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
                    "agents": [{
                        "name": "lead-rig",
                        "status": "live",
                        "crown_scope": "e-63be",
                        "crown_level": 2,
                        "created_at": "2026-10-05T00:00:00Z",
                        "cwd": home_dir.display().to_string(),
                        "harness_session_id": session,
                    }],
                });
                std::fs::write(
                    home_dir.join("registry.json"),
                    serde_json::to_string(&registry).unwrap(),
                )
                .unwrap();
            }
            Rig {
                dir,
                home: crate::paths::AgentsHome::from_env(),
                journal: journal_dir.join("events.jsonl"),
                cwd,
                session,
            }
        }

        fn payload(&self, last: &str) -> String {
            json!({
                "session_id": SESSION,
                "last_assistant_message": last,
                "transcript_path": "",
                "cwd": self.cwd.display().to_string(),
            })
            .to_string()
        }

        fn seed(&self, envelope: Value) {
            crate::event_store::append_envelope(&self.journal, &envelope.to_string(), None)
                .unwrap();
        }

        /// A route span this session wrote, as mail-record and the intake
        /// write them.
        fn route_span(&self, span_id: &str, route: &str, class: Option<&str>) {
            let mut data = json!({
                "span_kind": "route",
                "route": route,
                "trace": {
                    "trace_id": "x-node1",
                    "span_id": span_id,
                    "actor_session": self.session,
                    "actor_kind": "worker",
                    "comms": "mail",
                },
            });
            if let Some(cls) = class {
                data["class"] = json!(cls);
            }
            self.seed(json!({
                "ts": crate::events::now_rfc3339(),
                "type": "decision_span",
                "source": "test",
                "data": data,
            }));
        }

        /// An operator_question this session asked, optional escalate route
        /// parent carrying `class`.
        fn question(&self, span_id: &str, parent: Option<(&str, &str)>) {
            let mut data = json!({
                "question_id": span_id,
                "question": "which lane?",
                "asker": self.session,
                "why_user": "the operator asked for the pick",
                "context": {
                    "reversible": "yes",
                    "recommendation": {"option": 1, "why": "it reverses cleanly"},
                },
                "trace": {
                    "trace_id": "x-node1",
                    "span_id": span_id,
                    "actor_session": self.session,
                    "actor_kind": "worker",
                    "comms": "question",
                },
            });
            if let Some((route_id, class)) = parent {
                self.route_span(route_id, "escalate", Some(class));
                data["trace"]["parent_span_id"] = json!(route_id);
            }
            self.seed(json!({
                "ts": crate::events::now_rfc3339(),
                "type": "operator_question",
                "source": "test",
                "data": data,
            }));
        }

        fn spans(&self) -> Vec<Value> {
            crate::event_store::query_events(
                &self.journal,
                &crate::event_store::EventQuery::of_types(&["decision_span"]),
            )
            .unwrap()
            .iter()
            .map(|r| serde_json::from_str::<Value>(&r.line).unwrap())
            .collect()
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            std::env::remove_var("FNO_AGENTS_HOME");
        }
    }

    #[test]
    fn table_loads_and_overrides_disable() {
        let rows = parse_table().expect("the shipped table loads");
        for id in [
            "chat_ask_unfiled",
            "chat_ask_unfiled_worker",
            "decided_ask_fyi",
            "why_user_escape",
        ] {
            assert!(rows.iter().any(|r| r.id == id), "row {id} ships");
        }
        let worker = rows
            .iter()
            .find(|r| r.id == "chat_ask_unfiled_worker")
            .unwrap();
        assert!(!worker.default, "the worker row ships disabled");
        let ask_row = rows.iter().find(|r| r.id == "chat_ask_unfiled").unwrap();
        assert!(ask_row.instead.contains("fno inbox outstanding ask"));
        config_override_leg();
    }

    fn config_override_leg() {
        let rig = Rig::new(true);
        let ask = "1. merge 2. hold. Your call?";
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload(ask));
        assert!(
            fires.iter().any(|f| f.rule == "chat_ask_unfiled"),
            "the crowned row fires under the default config"
        );
        let config = rig.cwd.join(".fno-config.toml");
        std::fs::write(
            &config,
            "[event_rules]\nchat_ask_unfiled = false\nchat_ask_unfiled_worker = true\n",
        )
        .unwrap();
        let saved = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", &config);
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload(ask));
        assert!(
            !fires.iter().any(|f| f.rule == "chat_ask_unfiled"),
            "the override disables the row"
        );
        match saved {
            Some(v) => std::env::set_var("FNO_CONFIG", v),
            None => std::env::remove_var("FNO_CONFIG"),
        }
    }

    #[test]
    fn r1_blocks_unfiled_chat_asks_allows_filed_and_caps() {
        let lock = crate::claims::test_env_lock();
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let rig = Rig::new(true);
        let ask = "1. merge 2. hold. Your call?";

        // AC3: the block names the rule and the ask command, and the journal
        // holds one block span.
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload(ask));
        let block = fires
            .iter()
            .find(|f| f.rule == "chat_ask_unfiled")
            .expect("the chat ask blocks Stop");
        assert!(block.reason.contains("rule chat_ask_unfiled"));
        assert!(block.reason.contains("fno inbox outstanding ask"));
        let spans = rig.spans();
        assert_eq!(spans.len(), 1, "one block span for one fire");
        assert_eq!(spans[0]["data"]["span_kind"], "block");
        assert_eq!(spans[0]["data"]["rule"], "chat_ask_unfiled");

        // AC4: the same ask with a question filed this turn allows.
        rig.question("q-rig01", None);
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload(ask));
        assert!(
            !fires.iter().any(|f| f.rule == "chat_ask_unfiled"),
            "a filed question clears the rule"
        );

        // A plain status message never fires the row.
        let fires = eval_stop_in(
            &rig.cwd,
            &rig.journal,
            &rig.home,
            &rig.payload("wave 1 merged clean"),
        );
        assert!(fires.is_empty(), "no ask, no fire");
        r1_uncrowned_and_cap_legs();
        r2_and_r3_legs();
    }

    fn r1_uncrowned_and_cap_legs() {
        // AC5: uncrowned, no manifest, default config - the same ask allows.
        let rig = Rig::new(false);
        let ask = "1. merge 2. hold. Your call?";
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload(ask));
        assert!(
            fires.is_empty(),
            "an uncrowned session owes nothing by default"
        );

        // The cap: three blocks per turn key, then allow.
        let rig = Rig::new(true);
        for i in 0..BLOCK_CAP + 2 {
            let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload(ask));
            let blocked = fires.iter().any(|f| f.rule == "chat_ask_unfiled");
            if i < BLOCK_CAP {
                assert!(blocked, "fire {i} blocks");
            } else {
                assert!(!blocked, "fire {i} past the cap allows");
            }
        }
        let spans = rig.spans();
        assert_eq!(
            spans.len(),
            BLOCK_CAP,
            "one block span per fire, none past the cap"
        );
        assert_eq!(
            spans[BLOCK_CAP - 1]["data"]["cap_reached"],
            true,
            "the last fire's span carries cap_reached"
        );
    }

    fn r2_and_r3_legs() {
        let rig = Rig::new(false);
        rig.seed(json!({
            "ts": crate::events::now_rfc3339(),
            "type": "decision_span",
            "source": "test",
            "data": {
                "span_kind": "route",
                "route": "self",
                "recommendation": "merge now",
                "trace": {
                    "trace_id": "x-node1",
                    "span_id": "s-rig0099",
                    "actor_session": rig.session,
                    "actor_kind": "worker",
                    "comms": "mail",
                },
            },
        }));

        // AC6: the first Stop notifies once, the second does not repeat it.
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload("done"));
        let notes: Vec<&Fire> = fires.iter().filter(|f| f.action == "notify").collect();
        assert_eq!(notes.len(), 1, "one notify for one route span");
        let (title, body, pointer) = notes[0].notify.as_ref().unwrap();
        assert!(title.contains("decided_ask_fyi"));
        assert!(body.contains("decided x-node1 itself: merge now"));
        assert!(body.contains("--overturns s-rig0099"));
        assert_eq!(pointer.as_deref(), Some("fno inbox outstanding"));
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload("done"));
        assert!(
            !fires.iter().any(|f| f.action == "notify"),
            "the second Stop does not repeat the notify"
        );
        let all_spans = rig.spans();
        let notifies: Vec<&Value> = all_spans
            .iter()
            .filter(|s| s["data"]["span_kind"] == "notify")
            .collect();
        assert_eq!(notifies.len(), 1, "one notify span is the ledger");
        assert_eq!(notifies[0]["data"]["matched_event"], "s-rig0099");
        r3_legs();
    }

    fn r3_legs() {
        // AC7: reversible, recommended, why_user set, class none - one block.
        let rig = Rig::new(false);
        rig.question("q-rig02", None);
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload("done"));
        let nudges: Vec<&Fire> = fires
            .iter()
            .filter(|f| f.rule == "why_user_escape")
            .collect();
        assert_eq!(nudges.len(), 1, "the escape blocks once");
        assert!(nudges[0].reason.contains("why_user_escape"));
        assert!(nudges[0].reason.contains("fno backlog decide"));
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload("done"));
        assert!(
            !fires.iter().any(|f| f.rule == "why_user_escape"),
            "the next Stop allows"
        );

        // AC8: the same question whose escalate route declared a user-only
        // class allows.
        let rig = Rig::new(false);
        rig.question("q-rig03", Some(("s-rig-route8", "irreversible")));
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload("done"));
        assert!(
            !fires.iter().any(|f| f.rule == "why_user_escape"),
            "a user-only class escapes the escape check"
        );
    }
}

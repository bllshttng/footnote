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
    "promoted",
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

/// The table parses through the shared rule schema (`crate::rules`); this
/// maps each rule onto the Stop runtime's action names. The runtime's
/// `nudge` is a `warn`, and `notify` and `emit` are `inform` rows with a
/// delivery channel.
fn parse_table() -> Result<Vec<Row>, String> {
    let rules = crate::rules::parse_table(TABLE).map_err(|e| format!("event_rules.toml: {e}"))?;
    let mut rows = Vec::new();
    for rule in rules {
        let action = match (rule.action.as_str(), rule.deliver()) {
            ("block", _) => "block",
            ("warn", _) => "nudge",
            ("inform", "notify") => "notify",
            ("inform", "emit") => "emit",
            (action, deliver) => {
                return Err(format!(
                "event_rules.toml: {}: action {action} with deliver {deliver} has no Stop runtime",
                rule.id
            ))
            }
        };
        let [event] = rule.when.events.as_slice() else {
            return Err(format!(
                "event_rules.toml: {} must name exactly one event",
                rule.id
            ));
        };
        let row = Row {
            event: event.strip_prefix("journal:").unwrap_or(event).to_string(),
            when: rule.when.predicates.clone(),
            action: action.to_string(),
            default: rule.default,
            id: rule.id,
            what: rule.what,
            why: rule.why,
            instead: rule.instead,
        };
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
    Ok(rows)
}

fn table() -> &'static Vec<Row> {
    static CELL: OnceLock<Vec<Row>> = OnceLock::new();
    CELL.get_or_init(|| parse_table().expect("event_rules.toml must load"))
}

/// One config read per fire: the whole `event_rules` table comes back
/// once and each row's override is looked up in it.
fn overrides(cwd: &Path) -> std::collections::HashMap<String, bool> {
    let mut out = std::collections::HashMap::new();
    if let Some(table) = crate::agents_config::config_value_deep(cwd, &["event_rules"]) {
        if let Some(map) = table.as_table() {
            for (key, value) in map {
                if let Some(flag) = value.as_bool() {
                    out.insert(key.clone(), flag);
                }
            }
        }
    }
    out
}

fn enabled(overrides: &std::collections::HashMap<String, bool>, row: &Row) -> bool {
    match overrides.get(&row.id) {
        Some(v) => *v,
        None => row.default,
    }
}

/// What one engine run reads and answers against.
struct Ctx<'a> {
    cwd: &'a Path,
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
/// target or lead manifest. Best-effort throughout - a journal read failure
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
        // No turn id and no readable transcript: a day bucket. The cap
        // is the anti-loop bound, so the coarsest sticky key is the safe
        // reading - a half-hour bucket re-armed the cap mid-conversation.
        (false, None) => format!("b{}", now_ms / 86_400_000),
    };
    let matched_event = format!("stop:{session}:{turn_key}");
    let ctx = Ctx {
        cwd,
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
    let overrides = overrides(cwd);
    for row in table() {
        if !enabled(&overrides, row) {
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
        "promoted" => {
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

/// A worker ask addressed to its lead (the `Approval:` template), or an ask
/// sentence with its options: the numbered list right under the ask line, or
/// the options inline on the ask's own line. An ask word in one paragraph and
/// a status list in another is a report, not an ask.
fn decision_ask(text: &str) -> bool {
    if text
        .lines()
        .any(|l| l.trim_start().starts_with("Approval:"))
    {
        return true;
    }
    let lines: Vec<&str> = text.lines().collect();
    lines.iter().enumerate().any(|(i, line)| {
        if crate::repeated_asks::asks_in(line).is_empty() {
            return false;
        }
        if numbered_options(line) >= 2 {
            return true;
        }
        let blank = |l: &&&str| l.trim().is_empty();
        let item = |l: &&&str| is_numbered_item(l);
        !is_numbered_item(line)
            && (lines[i + 1..]
                .iter()
                .skip_while(blank)
                .take_while(item)
                .count()
                >= 2
                // "Options: 1. x 2. y" then "Which do you want?" is an ask too.
                || (line.trim_end().ends_with('?')
                    && lines[..i]
                        .iter()
                        .rev()
                        .skip_while(blank)
                        .take_while(item)
                        .count()
                        >= 2))
    })
}

fn is_numbered_item(line: &str) -> bool {
    let line = line.trim_start();
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0
        && line[digits..]
            .strip_prefix(['.', ')'])
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
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
/// route span it acted on. That is the ask the chat message points at. A
/// message that names a question still open on the board points at a filed
/// ask too, whoever filed it: a successor lead relays its predecessor's.
fn turn_filed_question(ctx: &Ctx) -> bool {
    names_open_question(ctx) || session_filed_this_turn(ctx)
}

/// The `q-xxxxxxxx` ids the text names.
fn named_qids(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    for (i, _) in text.match_indices("q-") {
        let boundary = |b: Option<&u8>| !b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'-');
        let id = text.get(i..i + 10).unwrap_or_default();
        if id.len() == 10
            && id[2..].bytes().all(|b| b.is_ascii_hexdigit())
            && boundary(i.checked_sub(1).and_then(|j| bytes.get(j)))
            && boundary(bytes.get(i + 10))
        {
            out.push(id);
        }
    }
    out
}

fn names_open_question(ctx: &Ctx) -> bool {
    let qids = named_qids(&ctx.last_message);
    if qids.is_empty() {
        return false;
    }
    let fno_dir = ctx
        .home
        .root()
        .parent()
        .map_or_else(|| std::path::PathBuf::from(".fno"), Path::to_path_buf);
    let mut journals = vec![ctx.journal.to_path_buf()];
    // Resolving the space dir can panic with no hermetic state root; the
    // Stop path fails open to the journals it can read.
    if let Ok(family) =
        std::panic::catch_unwind(|| crate::needs::question_journals(&fno_dir, ctx.cwd))
    {
        journals.extend(family);
    }
    let mut asked = std::collections::HashSet::new();
    let mut closed = std::collections::HashSet::new();
    for journal in &journals {
        let (a, c) = crate::question_intake::question_rows(journal);
        asked.extend(a.keys().cloned());
        closed.extend(c);
    }
    qids.iter()
        .any(|q| asked.contains(*q) && !closed.contains(*q))
}

fn session_filed_this_turn(ctx: &Ctx) -> bool {
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

    /// One engine test's world: a temp agents home (its registry roles the
    /// test session when asked), a temp journal, and the canonical handle.
    struct Rig {
        _dir: tempfile::TempDir,
        home: crate::paths::AgentsHome,
        journal: PathBuf,
        cwd: PathBuf,
        session: String,
    }

    impl Rig {
        fn new(promoted: bool) -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let home_dir = dir.path().join("agents-home");
            let journal_dir = dir.path().join("journal");
            let cwd = dir.path().join("project");
            std::fs::create_dir_all(&home_dir).unwrap();
            std::fs::create_dir_all(&journal_dir).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            std::env::set_var("FNO_AGENTS_HOME", &home_dir);
            let session = crate::identity::canonical_handle(SESSION);
            if promoted {
                let registry = json!({
                    "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
                    "agents": [{
                        "name": "lead-rig",
                        "status": "live",
                        "role_scope": "e-63be",
                        "role_level": 2,
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
                _dir: dir,
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
        r1_unpromoted_and_cap_legs();
        r2_and_r3_legs();
    }

    #[test]
    fn relayed_open_question_counts_as_filed() {
        let lock = crate::claims::test_env_lock();
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let rig = Rig::new(true);
        // A predecessor lead filed q-1f456994; this session only relays it.
        rig.seed(json!({
            "ts": crate::events::now_rfc3339(),
            "type": "operator_question",
            "source": "test",
            "data": {"question_id": "q-1f456994", "question": "which lane?", "asker": "b46c5135"},
        }));
        let relay = "Waiting on you in q-1f456994: 1. merge 2. hold. Your call?";
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload(relay));
        assert!(
            !fires.iter().any(|f| f.rule == "chat_ask_unfiled"),
            "an inherited open qid counts as filed"
        );
        rig.seed(json!({
            "ts": crate::events::now_rfc3339(),
            "type": crate::needs::QUESTION_TYPES[1],
            "source": "test",
            "data": {"question_id": "q-1f456994"},
        }));
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload(relay));
        assert!(
            fires.iter().any(|f| f.rule == "chat_ask_unfiled"),
            "a closed qid files nothing"
        );
        assert_eq!(
            named_qids("see q-1f456994, not q-1f4569941 or xq-1f456994"),
            ["q-1f456994"]
        );
    }

    #[test]
    fn status_reports_are_not_decision_asks() {
        // 2026-10-12: the ASK marker "confirm" substring-matched "confirmed",
        // so a numbered status report blocked Stop as an unfiled ask. The
        // confirm asks that remain are the pointed ones.
        let status =
            "Checks are green and the merge is confirmed safe.\n1. PR 3376 open\n2. CI green";
        assert!(!decision_ask(status));
        let ask = "Please confirm the pick.\n1. merge now\n2. hold";
        assert!(decision_ask(ask));
        // A bare "reply " cue matched every sentence that mentioned a reply.
        let reply = "The reply from codex landed.\n1. PR 3376 open\n2. CI green";
        assert!(
            !decision_ask(reply),
            "mentioning a reply is not a reply ask"
        );
        assert!(decision_ask("Reply 1 or 2.\n1. merge\n2. hold"));
        let table = "| node | state |\n|---|---|\n| x-1 | ? |\n1. PR open\n2. CI pending";
        assert!(!decision_ask(table));
        let next = "1. merged\n2. reconciled\nNext: nothing from you.";
        assert!(!decision_ask(next));
        let receipt = "You ruled hold, which was your call.\n1. held\n2. noted";
        assert!(!decision_ask(receipt));
        // A numbered status list plus an unrelated "want" sentence in another
        // paragraph is a report: the options must sit under the ask line.
        let split =
            "Prep list:\n1. read the node\n2. cut the worktree\n\nLet me know if the fix grows.";
        assert!(!decision_ask(split));
        let spaced = "Should I merge?\n\n1. merge now\n2. hold";
        assert!(decision_ask(spaced));
        assert!(decision_ask(
            "Options:\n1. merge now\n2. hold\nWhich do you want?"
        ));
        assert!(!decision_ask(
            "Done:\n1. merged\n2. reconciled\nLet me know if it grows."
        ));
    }

    fn r1_unpromoted_and_cap_legs() {
        // AC5: unpromoted, no manifest, default config - the same ask allows.
        let rig = Rig::new(false);
        let ask = "1. merge 2. hold. Your call?";
        let fires = eval_stop_in(&rig.cwd, &rig.journal, &rig.home, &rig.payload(ask));
        assert!(
            fires.is_empty(),
            "an unpromoted session owes nothing by default"
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

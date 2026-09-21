//! Native morning and end-of-day readback over the existing project records.

use chrono::{DateTime, FixedOffset, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Default)]
pub struct DayInputs {
    pub kind: String,
    pub now: String,
    pub questions_raw: String,
    pub decisions_raw: String,
    pub decisions_path: String,
    pub graph_entries: Vec<Value>,
    pub event_journals: Vec<(String, String)>,
    /// (path, state) pairs parallel to `event_journals`: `read`, `missing` or
    /// `unreadable`. Empty means every provided journal was read.
    pub journal_states: Vec<(String, String)>,
    /// Precomputed `king_history::scan_scopes(paths, None)` payload, read by
    /// the IO layer. `fold_day` stays pure: it filters this Value to the
    /// window and never opens a store itself.
    pub checkin_scan: Option<Value>,
    /// `read`, `missing` or `unreadable` for the check-in scan itself.
    pub checkin_state: String,
    pub questions_state: String,
    pub questions_path: String,
}

fn parse_ts(raw: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(raw).ok()
}

fn format_ts(ts: DateTime<FixedOffset>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn stamp(v: &Value) -> Option<&str> {
    v.get("ts").and_then(Value::as_str)
}

fn data(v: &Value) -> Option<&serde_json::Map<String, Value>> {
    v.get("data").and_then(Value::as_object)
}

fn string(data: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    data.get(key).and_then(Value::as_str).map(str::to_string)
}

fn local_midnight(now: DateTime<FixedOffset>) -> DateTime<FixedOffset> {
    now.offset()
        .from_local_datetime(&now.date_naive().and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap_or(now)
}

fn in_window(raw: &str, from: DateTime<FixedOffset>, to: DateTime<FixedOffset>) -> bool {
    parse_ts(raw).is_some_and(|ts| ts >= from && ts < to)
}

fn boundary_id(kind: &str, now: DateTime<FixedOffset>) -> String {
    let checksum = now
        .to_rfc3339_opts(SecondsFormat::AutoSi, true)
        .bytes()
        .fold(0u16, |sum, byte| sum.wrapping_add(byte as u16));
    format!(
        "day-{kind}-{}-{:04x}",
        now.date_naive().format("%Y%m%d"),
        checksum
    )
}

fn prior_boundaries(raw: &str) -> Vec<Value> {
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(|row| row.get("type").and_then(Value::as_str) == Some("day_boundary"))
        .collect()
}

fn latest_boundary(rows: &[Value]) -> Option<&Value> {
    rows.iter().max_by_key(|row| {
        stamp(row)
            .and_then(parse_ts)
            .map(|ts| ts.timestamp_millis())
            .unwrap_or(i64::MIN)
    })
}

fn question_items(raw: &str) -> Vec<Value> {
    crate::needs::fold(raw, "", 0, crate::needs::DEFAULT_FIRES_FLOOR)
        .into_iter()
        .filter(|item| item.kind == "operator_question")
        .map(|item| {
            json!({
                "id": item.session_id,
                "question": item.evidence,
                "ts": item.ts,
                "node": item.node,
            })
        })
        .collect()
}

fn attention(open: &[Value], history: &[Value]) -> Vec<String> {
    // Presentation history from prior boundaries: which ids were featured,
    // and when they were featured last (the boundary row's ts).
    let mut last_featured: HashMap<&str, &str> = HashMap::new();
    for row in history {
        if let Some(ids) = row
            .get("data")
            .and_then(|d| d.get("featured"))
            .and_then(Value::as_array)
        {
            let row_ts = stamp(row).unwrap_or("");
            for id in ids.iter().filter_map(Value::as_str) {
                let newer = last_featured.get(id).is_none_or(|prior| row_ts >= **prior);
                if newer {
                    last_featured.insert(id, row_ts);
                }
            }
        }
    }
    let id_of = |item: &Value| item.get("id").and_then(Value::as_str);
    // Slots 1-3: the shared rank's own order (needs order, newest first).
    let mut selected: Vec<String> = open
        .iter()
        .take(3)
        .filter_map(id_of)
        .map(str::to_string)
        .collect();
    // Reserved places: never-featured first, oldest ts leading, so old asks
    // cannot sit unseen forever; then the least recently featured. Ties on id.
    let mut never: Vec<&Value> = open
        .iter()
        .filter(|item| id_of(item).is_some_and(|id| !last_featured.contains_key(id)))
        .collect();
    never.sort_by(|a, b| {
        let key = |item: &Value| (stamp(item).unwrap_or(""), id_of(item).unwrap_or(""));
        key(a).cmp(&key(b))
    });
    let mut carried: Vec<&Value> = open
        .iter()
        .filter(|item| id_of(item).is_some_and(|id| last_featured.contains_key(id)))
        .collect();
    carried.sort_by(|a, b| {
        let key = |item: &Value| {
            (
                last_featured
                    .get(id_of(item).unwrap_or(""))
                    .copied()
                    .unwrap_or(""),
                id_of(item).unwrap_or(""),
            )
        };
        key(a).cmp(&key(b))
    });
    for item in never.into_iter().chain(carried) {
        if selected.len() >= 5 {
            break;
        }
        let Some(id) = id_of(item) else {
            continue;
        };
        if !selected.iter().any(|selected_id| selected_id == id) {
            selected.push(id.to_string());
        }
    }
    // Any leftover place comes from the shared rank order.
    for item in open {
        if selected.len() >= 5 {
            break;
        }
        if let Some(id) = id_of(item) {
            if !selected.iter().any(|selected_id| selected_id == id) {
                selected.push(id.to_string());
            }
        }
    }
    selected
}

fn collect_retractions(raw: &str, kind: &str, out: &mut Vec<Value>) {
    for line in raw.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(row_kind) = row.get("type").and_then(Value::as_str) else {
            continue;
        };
        let is_retraction = row_kind == "decision_retracted"
            || (row_kind == "review_attestation"
                && data(&row).is_some_and(|d| d.contains_key("retracts_attester")));
        if !is_retraction {
            continue;
        }
        let Some(ts) = stamp(&row) else { continue };
        let Some(fields) = data(&row) else { continue };
        out.push(json!({
            "kind": kind,
            "target": string(fields, "target").or_else(|| string(fields, "decision_id")).unwrap_or_default(),
            "reason": string(fields, "reason").unwrap_or_default(),
            "ts": ts,
        }));
    }
}

/// Filter the precomputed `king_history::scan_scopes(paths, None)` payload to
/// the window and group its canonical rows by scope. The canonical/legacy
/// classification stays in king_history: this reads its verdict, never a
/// second classifier over raw journal text.
fn checkin_summary(
    scan: Option<&Value>,
    state: &str,
    from: DateTime<FixedOffset>,
    to: DateTime<FixedOffset>,
) -> Value {
    let mut scopes: HashMap<String, (u64, String)> = HashMap::new();
    let events = scan
        .and_then(|scan| scan.get("events"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for event in events {
        let Some(ts) = stamp(&event).or_else(|| event.get("ts").and_then(Value::as_str)) else {
            continue;
        };
        if !in_window(ts, from, to) {
            continue;
        }
        let scope = event
            .get("data")
            .and_then(|d| d.get("scope"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let change = event
            .get("data")
            .and_then(|d| d.get("change"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let entry = scopes.entry(scope).or_insert((0, String::new()));
        entry.0 += 1;
        if parse_ts(ts)
            .is_some_and(|current| parse_ts(&entry.1).is_none_or(|previous| current >= previous))
        {
            entry.1 = change;
        }
    }
    let rejected = scan
        .and_then(|scan| scan.get("rejected"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "state": state,
        "rejected": rejected,
        "scopes": scopes.into_iter().map(|(scope, (count, latest_change))| {
            json!({"scope": scope, "count": count, "latest_change": latest_change})
        }).collect::<Vec<_>>(),
    })
}

pub fn fold_day(inputs: &DayInputs) -> Result<Value, String> {
    let now =
        parse_ts(&inputs.now).ok_or_else(|| format!("invalid --now RFC3339: {}", inputs.now))?;
    if inputs.kind != "start" && inputs.kind != "end" {
        return Err("--kind must be start or end".to_string());
    }
    let history = prior_boundaries(&inputs.questions_raw);
    let newest = latest_boundary(&history);
    if let Some(saved) = newest {
        let saved_kind = data(saved)
            .and_then(|d| d.get("kind"))
            .and_then(Value::as_str);
        let saved_date = stamp(saved)
            .and_then(parse_ts)
            .map(|ts| ts.with_timezone(now.offset()).date_naive());
        if saved_kind == Some(inputs.kind.as_str()) && saved_date == Some(now.date_naive()) {
            let mut reused = data(saved).cloned().unwrap_or_default();
            reused.insert("reused".into(), json!(true));
            return Ok(Value::Object(reused));
        }
    }
    let from = newest
        .and_then(|row| {
            data(row)
                .and_then(|d| d.get("cutoff"))
                .and_then(Value::as_str)
        })
        .and_then(parse_ts)
        .unwrap_or_else(|| local_midnight(now));
    let label = if newest.is_some() {
        "since previous boundary"
    } else {
        "first boundary"
    };
    let projection = crate::feed::project(&inputs.questions_raw, &inputs.graph_entries, &[]);
    // node_ended rows carry no PR reference; the graph entry does. Join the
    // completion card to its PR through the entry the feed already walked.
    let mut pr_by_node: HashMap<&str, Option<i64>> = HashMap::new();
    for entry in &inputs.graph_entries {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        pr_by_node.insert(id, entry.get("pr_number").and_then(Value::as_i64));
    }
    let mut completed_items = Vec::new();
    let mut opened = 0u64;
    let mut closed = 0u64;
    for row in projection
        .rows
        .iter()
        .filter(|row| in_window(&row.ts, from, now))
    {
        match row.kind.as_str() {
            "node_ended" => completed_items.push(
                json!({
                    "node": row.node,
                    "title": row.title,
                    "ref": row.r#ref,
                    "pr_number": row.node.as_deref().and_then(|node| pr_by_node.get(node)).cloned().flatten(),
                    "ts": row.ts
                }),
            ),
            "question_asked" => opened += 1,
            "question_closed" => closed += 1,
            _ => {}
        }
    }
    let open_items = question_items(&inputs.questions_raw);
    let open_count = if inputs.questions_state == "read" {
        json!(open_items.len())
    } else {
        json!("unknown")
    };
    let featured = attention(&open_items, &history);
    let first_ask = featured
        .first()
        .and_then(|id| {
            open_items
                .iter()
                .find(|item| item.get("id").and_then(Value::as_str) == Some(id.as_str()))
        })
        .and_then(|item| item.get("question").and_then(Value::as_str))
        .map(|ask| {
            if ask.chars().count() > 80 {
                ask.chars().take(80).collect::<String>()
            } else {
                ask.to_string()
            }
        })
        .unwrap_or_default();
    let mut retractions = Vec::new();
    collect_retractions(&inputs.decisions_raw, "decision", &mut retractions);
    for (_path, raw) in &inputs.event_journals {
        collect_retractions(raw, "review", &mut retractions);
    }
    retractions.retain(|row| {
        row.get("ts")
            .and_then(Value::as_str)
            .is_some_and(|ts| in_window(ts, from, now))
    });
    let mut receipts = vec![
        json!({"source":"questions", "path": inputs.questions_path, "state": inputs.questions_state, "scanned": inputs.questions_raw.lines().count(), "matched": open_items.len()}),
        json!({"source":"decisions", "path": inputs.decisions_path, "state":"read", "scanned": inputs.decisions_raw.lines().count(), "matched": retractions.iter().filter(|r| r["kind"] == "decision").count()}),
        json!({"source":"graph", "path":"graph.json", "state":"read", "scanned": inputs.graph_entries.len(), "matched": completed_items.len()}),
    ];
    for (path, raw) in &inputs.event_journals {
        let state = inputs
            .journal_states
            .iter()
            .find(|(state_path, _)| state_path == path)
            .map(|(_, state)| state.as_str())
            .unwrap_or("read");
        receipts.push(json!({"source":"journal", "path":path, "state":state, "scanned":raw.lines().count(), "matched":raw.lines().filter(|line| line.contains("reign_checkin")).count()}));
    }
    let id = boundary_id(&inputs.kind, now);
    Ok(json!({
        "boundary_id": id,
        "kind": inputs.kind,
        "reused": false,
        "cutoff": format_ts(now),
        "window": {"from": format_ts(from), "to": format_ts(now), "label": label},
        "prior_boundary_id": newest.and_then(|row| data(row).and_then(|d| d.get("boundary_id")).and_then(Value::as_str)),
        "completed": {"count": completed_items.len(), "items": completed_items.into_iter().take(10).collect::<Vec<_>>()},
        "questions": {"open": open_count, "opened": opened, "closed": closed, "featured": featured, "first_ask": first_ask},
        "questions_state": inputs.questions_state,
        "questions_path": inputs.questions_path,
        "questions_rows": inputs.questions_raw.lines().count() as u64,
        "retractions": retractions,
        "checkins": checkin_summary(inputs.checkin_scan.as_ref(), &inputs.checkin_state, from, now),
        "receipts": receipts,
    }))
}

fn render(payload: &Value) -> String {
    let questions = &payload["questions"];
    let questions_state = payload["questions_state"].as_str().unwrap_or("read");
    let questions_path = payload["questions_path"]
        .as_str()
        .unwrap_or("questions.jsonl");
    let action = questions["featured"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_str);
    let first = if questions_state == "missing" {
        format!("open questions: unknown (questions store missing: {questions_path})")
    } else {
        match (payload["kind"].as_str(), action) {
            (Some("start"), Some(id)) => {
                let ask = questions["first_ask"].as_str().unwrap_or("");
                let ask = if ask.is_empty() {
                    String::new()
                } else {
                    format!(": {ask}")
                };
                format!("Start: answer {id}{ask}")
            }
            (Some("end"), Some(id)) => {
                let ask = questions["first_ask"].as_str().unwrap_or("");
                let ask = if ask.is_empty() {
                    String::new()
                } else {
                    format!(": {ask}")
                };
                format!("Tomorrow: {id}{ask}")
            }
            _ => format!(
                "Nothing waits on you: {} open questions ({} {}, {} rows)",
                questions["open"],
                questions_path,
                questions_state,
                payload["questions_rows"].as_u64().unwrap_or(0)
            ),
        }
    };
    let mut lines = vec![first];
    lines.push(format!("Completed: {}", payload["completed"]["count"]));
    lines.push(format!(
        "Questions: +{} / -{}; showing {} of {} open",
        questions["opened"],
        questions["closed"],
        questions["featured"].as_array().map_or(0, Vec::len),
        questions["open"]
    ));
    lines.push(format!(
        "Retractions: {}",
        payload["retractions"].as_array().map_or(0, Vec::len)
    ));
    let checkins = &payload["checkins"];
    let scope_parts: Vec<String> = checkins["scopes"]
        .as_array()
        .map(|scopes| {
            scopes
                .iter()
                .map(|scope| {
                    format!(
                        "{} {} (latest: {})",
                        scope["scope"], scope["count"], scope["latest_change"]
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    lines.push(format!(
        "Check-ins: {} (rejected {})",
        if scope_parts.is_empty() {
            "none in window".to_string()
        } else {
            scope_parts.join(", ")
        },
        checkins["rejected"]
    ));
    lines.push(format!(
        "Window: {} to {} ({})",
        payload["window"]["from"], payload["window"]["to"], payload["window"]["label"]
    ));
    lines.push(format!(
        "Sources: {}",
        payload["receipts"].as_array().map_or(0, Vec::len)
    ));
    lines.join("\n")
}

fn graph_path(home: &crate::paths::AgentsHome) -> PathBuf {
    home.root()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".fno"))
        .join("graph.json")
}

pub fn run_day(rest: &[String], home: &crate::paths::AgentsHome) -> i32 {
    let mut kind = None;
    let mut now = Utc::now().to_rfc3339();
    let mut json_output = false;
    let mut event_paths = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--kind" if i + 1 < rest.len() => {
                kind = Some(rest[i + 1].clone());
                i += 2;
            }
            "--now" if i + 1 < rest.len() => {
                now = rest[i + 1].clone();
                i += 2;
            }
            "--events-path" if i + 1 < rest.len() => {
                event_paths.push(PathBuf::from(&rest[i + 1]));
                i += 2;
            }
            "--json" | "-J" => {
                json_output = true;
                i += 1;
            }
            other => {
                eprintln!("fno-agents day: unknown flag {other}");
                return 2;
            }
        }
    }
    let Some(kind) = kind else {
        eprintln!("fno-agents day: --kind start|end is required");
        return 2;
    };
    let state_dir = home
        .root()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let questions_path = state_dir.join("questions.jsonl");
    let (questions_raw, questions_state) = match std::fs::read_to_string(&questions_path) {
        Ok(raw) => (raw, "read".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            (String::new(), "missing".to_string())
        }
        Err(error) => {
            eprintln!(
                "fno-agents day: questions store unreadable: {}: {error}",
                questions_path.display()
            );
            return 1;
        }
    };
    let decisions_path = state_dir.join("decisions.jsonl");
    let decisions_raw = std::fs::read_to_string(&decisions_path).unwrap_or_default();
    let graph_entries = match crate::graph_store::read_raw(&graph_path(home)) {
        Ok(crate::graph_store::RawRead::Entries(entries)) => entries,
        _ => Vec::new(),
    };
    let mut journals: Vec<(String, String)> = Vec::new();
    let mut journal_states: Vec<(String, String)> = Vec::new();
    for path in event_paths {
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                journal_states.push((path.display().to_string(), "missing".to_string()));
                String::new()
            }
            Err(error) => {
                // A journal that cannot be read is an incomplete source, not
                // a reason to lose the whole read; the receipt names it.
                journal_states.push((path.display().to_string(), "unreadable".to_string()));
                String::new()
            }
        };
        journals.push((path.display().to_string(), raw));
    }
    let (checkin_scan, checkin_state) = if event_paths.is_empty() {
        (None, "missing".to_string())
    } else {
        match crate::king_history::scan_scopes(&event_paths, None) {
            Ok(scan) => (Some(scan), "read".to_string()),
            Err(_) => (None, "unreadable".to_string()),
        }
    };
    let input = DayInputs {
        kind,
        now,
        questions_raw,
        decisions_raw,
        decisions_path: decisions_path.display().to_string(),
        graph_entries,
        event_journals: journals,
        journal_states,
        checkin_scan,
        checkin_state,
        questions_state,
        questions_path: questions_path.display().to_string(),
    };
    match fold_day(&input) {
        Ok(payload) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&payload).unwrap_or_default()
                );
            } else {
                println!("{}", render(&payload));
            }
            0
        }
        Err(error) => {
            eprintln!("fno-agents day: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inputs(kind: &str, now: &str, questions: &str) -> DayInputs {
        DayInputs {
            kind: kind.to_string(),
            now: now.to_string(),
            questions_raw: questions.to_string(),
            questions_state: "read".to_string(),
            questions_path: "questions.jsonl".to_string(),
            ..DayInputs::default()
        }
    }

    #[test]
    fn prior_boundary_limits_completed_work_to_the_interval() {
        let mut input = inputs(
            "start",
            "2026-09-10T18:00:00Z",
            r#"{"ts":"2026-09-10T08:00:00Z","type":"day_boundary","source":"operator","data":{"kind":"end","boundary_id":"day-end-old","cutoff":"2026-09-10T08:00:00Z"}}"#,
        );
        input.graph_entries = vec![
            json!({"id":"x-in","title":"inside","completed_at":"2026-09-10T09:00:00Z"}),
            json!({"id":"x-out","title":"outside","completed_at":"2026-09-10T07:00:00Z"}),
        ];
        let payload = fold_day(&input).unwrap();
        assert_eq!(payload["completed"]["count"], json!(1));
        assert_eq!(payload["window"]["from"], json!("2026-09-10T08:00:00Z"));
        assert_eq!(payload["prior_boundary_id"], json!("day-end-old"));
    }

    #[test]
    fn retractions_include_decisions_and_review_attestations_in_window() {
        let mut input = inputs("end", "2026-09-10T12:00:00Z", "");
        input.decisions_raw = r#"{"ts":"2026-09-10T10:00:00Z","type":"decision_retracted","data":{"target":"d-1","reason":"new evidence"}}"#.to_string();
        input.event_journals = vec![(
            "events.jsonl".to_string(),
            r#"{"ts":"2026-09-10T11:00:00Z","type":"review_attestation","data":{"retracts_attester":"bot-1","target":"pr-1","reason":"head changed"}}"#.to_string(),
        )];
        let payload = fold_day(&input).unwrap();
        assert_eq!(payload["retractions"].as_array().unwrap().len(), 2);
        assert_eq!(payload["retractions"][0]["reason"], json!("new evidence"));
        assert_eq!(payload["retractions"][1]["target"], json!("pr-1"));
    }

    #[test]
    fn old_open_question_counts_but_is_not_a_new_arrival() {
        let input = inputs(
            "start",
            "2026-09-10T12:00:00Z",
            r#"{"ts":"2026-09-09T12:00:00Z","type":"operator_question","source":"target","data":{"question_id":"q-old","question":"still open"}}"#,
        );
        let payload = fold_day(&input).unwrap();
        assert_eq!(payload["questions"]["open"], json!(1));
        assert_eq!(payload["questions"]["opened"], json!(0));
    }

    #[test]
    fn attention_features_are_bounded_and_carry_forward_history() {
        let questions: Vec<String> = (0..12)
            .map(|i| format!(r#"{{"ts":"2026-09-09T{:02}:00:00Z","type":"operator_question","source":"target","data":{{"question_id":"q-{i:02}","question":"question {i}"}}}}"#, i))
            .collect();
        let raw = questions.join("\n");
        let first = fold_day(&inputs("start", "2026-09-10T12:00:00Z", &raw)).unwrap();
        let first_featured = first["questions"]["featured"].as_array().unwrap().clone();
        assert!(first_featured.len() <= 5);
        let prior = json!({"ts":"2026-09-10T12:00:00Z","type":"day_boundary","source":"operator","data":{"kind":"start","boundary_id":"day-start-old","cutoff":"2026-09-10T12:00:00Z","featured":first_featured}});
        let second_input = inputs("end", "2026-09-10T18:00:00Z", &format!("{prior}\n{raw}"));
        let second = fold_day(&second_input).unwrap();
        let second_featured = second["questions"]["featured"].as_array().unwrap();
        assert!(second_featured.len() <= 5);
        assert!(second_featured
            .iter()
            .any(|id| !first["questions"]["featured"]
                .as_array()
                .unwrap()
                .contains(id)));
    }

    #[test]
    fn same_kind_on_same_local_day_reuses_the_saved_boundary() {
        let input = inputs(
            "start",
            "2026-09-10T12:00:00Z",
            r#"{"ts":"2026-09-10T08:00:00Z","type":"day_boundary","source":"operator","data":{"kind":"start","boundary_id":"day-start-saved","cutoff":"2026-09-10T08:00:00Z","featured":["q-1"]}}"#,
        );
        let payload = fold_day(&input).unwrap();
        assert_eq!(payload["reused"], json!(true));
        assert_eq!(payload["boundary_id"], json!("day-start-saved"));
    }

    #[test]
    fn first_boundary_starts_at_local_midnight() {
        let input = inputs("start", "2026-09-10T12:34:56-07:00", "");
        let payload = fold_day(&input).unwrap();
        assert_eq!(payload["window"]["label"], json!("first boundary"));
        assert_eq!(
            payload["window"]["from"],
            json!("2026-09-10T00:00:00-07:00")
        );
    }

    #[test]
    fn missing_questions_store_keeps_open_count_unknown() {
        let mut input = inputs("start", "2026-09-10T12:34:56-07:00", "");
        input.questions_state = "missing".to_string();
        input.questions_path = "/tmp/questions.jsonl".to_string();
        let payload = fold_day(&input).unwrap();
        assert_eq!(payload["questions"]["open"], json!("unknown"));
        assert_eq!(payload["questions_state"], json!("missing"));
    }

    #[test]
    fn checkins_group_by_scope_from_the_scan_payload() {
        let mut input = inputs("start", "2026-09-10T12:00:00Z", "");
        input.checkin_state = "read".to_string();
        input.checkin_scan = Some(json!({
            "matched": 3,
            "rejected": 1,
            "events": [
                {"ts":"2026-09-10T08:10:00Z","type":"reign_checkin","data":{"scope":"fno","change":"green"}},
                {"ts":"2026-09-10T09:10:00Z","type":"reign_checkin","data":{"scope":"x-a792","change":"stalled"}},
                {"ts":"2026-09-09T23:00:00Z","type":"reign_checkin","data":{"scope":"fno","change":"early"}}
            ]
        }));
        let payload = fold_day(&input).unwrap();
        let scopes = payload["checkins"]["scopes"].as_array().unwrap();
        assert_eq!(scopes.len(), 2);
        assert_eq!(payload["checkins"]["rejected"], json!(1));
        let fno_scope = scopes
            .iter()
            .find(|scope| scope["scope"] == json!("fno"))
            .unwrap();
        assert_eq!(
            fno_scope["count"],
            json!(1),
            "pre-window rows are not counted"
        );
        assert_eq!(fno_scope["latest_change"], json!("green"));
    }

    #[test]
    fn render_names_the_first_action_within_twelve_lines() {
        let input = inputs(
            "start",
            "2026-09-10T12:00:00Z",
            r#"{"ts":"2026-09-10T08:00:00Z","type":"operator_question","source":"target","data":{"question_id":"q-1","question":"pick the runner"}}"#,
        );
        let payload = fold_day(&input).unwrap();
        let text = render(&payload);
        let first = text.lines().next().unwrap();
        assert!(first.starts_with("Start: answer q-1"), "{first}");
        assert!(first.contains("pick the runner"), "{first}");
        assert!(text.lines().count() <= 12, "{text}");
    }
}

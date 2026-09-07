//! `fno-agents feed` - one projection joining questions, decisions and node
//! lifecycle into an ordered feed (x-4433).
//!
//! Three stores hold one timeline and nothing joined them:
//!   - `~/.fno/questions.jsonl`: `operator_question` / `operator_question_closed`
//!     / `operator_decision` rows, the operator-facing half.
//!   - `~/.fno/graph.json`: node lifecycle as FIELDS, not events -
//!     `sessions[].started_at`, a ship-phase row beside `pr_number`,
//!     `completed_at`. Derived at read time, never copied: the graph stays the
//!     one truth (AGENTS.md principle 9).
//!   - `~/.fno/events.jsonl` is deliberately NOT read here: 72% ticks, and the
//!     lifecycle kinds it does carry restate what the graph already stamps.
//!
//! Every row carries the identity the mux deep link needs (node id and
//! session id), so `Command::AttachAgent` resolves it with no new server path.
//! Read-only; exits 0 on missing or unreadable stores (one stderr line each).

use crate::graph_store;
use crate::paths::AgentsHome;
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;

/// One ordered feed row. `ref` carries the question id, decision id or PR
/// number as a string; `session_id` is what the mux `AttachAgent` path
/// resolves.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FeedRow {
    pub ts: String,
    /// `question_asked` | `question_closed` | `decision_recorded` |
    /// `node_started` | `pr_created` | `node_ended`
    pub kind: String,
    pub node: Option<String>,
    pub session_id: Option<String>,
    pub harness: Option<String>,
    pub title: String,
    #[serde(rename = "ref")]
    pub r#ref: Option<String>,
}

/// Rows plus what the projection had to skip. Malformed question lines and
/// non-object graph entries are counted, never fatal.
#[derive(Debug, PartialEq)]
pub struct Projection {
    pub rows: Vec<FeedRow>,
    pub skipped_lines: usize,
    pub skipped_entries: usize,
}

/// One line, for titles and the plain output: cut at the first newline.
fn one_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

fn s_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Sort key: epoch millis when the ts parses as RFC3339; unparseable stamps
/// sort first and keep their raw string (the row is still shown).
fn ts_key(ts: &str) -> (u8, i64) {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => (1, t.timestamp_millis()),
        Err(_) => (0, 0),
    }
}

/// The pure projection: questions.jsonl text + graph entries -> ordered rows.
/// Ascending by ts, so a consumer reads history forward and `--limit` trims
/// from the newest end.
pub fn project(questions_raw: &str, graph_entries: &[Value]) -> Projection {
    let mut rows = Vec::new();
    let mut skipped_lines = 0usize;
    let mut skipped_entries = 0usize;

    for line in questions_raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            skipped_lines += 1;
            continue;
        };
        let Some(data) = v.get("data") else {
            skipped_lines += 1;
            continue;
        };
        let ts = match s_field(&v, "ts") {
            Some(t) => t,
            None => {
                skipped_lines += 1;
                continue;
            }
        };
        let kind = match v.get("type").and_then(Value::as_str) {
            Some("operator_question") => {
                let title = data
                    .get("question")
                    .and_then(Value::as_str)
                    .map(one_line)
                    .unwrap_or_default();
                FeedRow {
                    ts,
                    kind: "question_asked".into(),
                    node: s_field(data, "node"),
                    session_id: s_field(data, "session_id"),
                    harness: None,
                    title,
                    r#ref: s_field(data, "question_id"),
                }
            }
            Some("operator_question_closed") => {
                let title = data
                    .get("answer")
                    .and_then(Value::as_str)
                    .map(one_line)
                    .unwrap_or_default();
                FeedRow {
                    ts,
                    kind: "question_closed".into(),
                    node: None,
                    session_id: s_field(data, "closed_by"),
                    harness: None,
                    title,
                    r#ref: s_field(data, "question_id"),
                }
            }
            Some("operator_decision") => {
                let subject = s_field(data, "subject").unwrap_or_default();
                let decision = s_field(data, "decision").unwrap_or_default();
                let title = if subject.is_empty() {
                    decision
                } else {
                    format!("{subject}: {decision}")
                };
                FeedRow {
                    ts,
                    kind: "decision_recorded".into(),
                    node: None,
                    session_id: s_field(data, "decided_by"),
                    harness: None,
                    title: one_line(&title),
                    r#ref: s_field(data, "decision_id"),
                }
            }
            _ => {
                skipped_lines += 1;
                continue;
            }
        };
        rows.push(kind);
    }

    for entry in graph_entries {
        let Some(node_id) = graph_store::entry_id(entry) else {
            skipped_entries += 1;
            continue;
        };
        let node_title = graph_store::s_str(entry, "title")
            .unwrap_or(node_id)
            .to_string();
        let sessions = entry
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // node_started: every do row that started. node_pr: a ship row that
        // started on a node carrying pr_number.
        let mut latest_session: Option<(String, Option<String>)> = None;
        for row in &sessions {
            let phase = graph_store::s_str(row, "phase").unwrap_or_default();
            let sid = s_field(row, "session_id");
            let harness = s_field(row, "harness");
            let Some(started) = s_field(row, "started_at") else {
                continue;
            };
            if phase == "do" {
                rows.push(FeedRow {
                    ts: started.to_string(),
                    kind: "node_started".into(),
                    node: Some(node_id.to_string()),
                    session_id: sid.clone(),
                    harness: harness.clone(),
                    title: node_title.clone(),
                    r#ref: None,
                });
            }
            if phase == "ship" {
                if let Some(pr) = entry.get("pr_number") {
                    let url = graph_store::s_str(entry, "pr_url").unwrap_or(node_id);
                    rows.push(FeedRow {
                        ts: started.to_string(),
                        kind: "pr_created".into(),
                        node: Some(node_id.to_string()),
                        session_id: sid.clone(),
                        harness: harness.clone(),
                        title: format!("PR {} - {url}", pr),
                        r#ref: Some(pr.to_string()),
                    });
                }
            }
            if phase == "do" || phase == "ship" {
                let stamp = started.to_string();
                let newer = latest_session
                    .as_ref()
                    .is_none_or(|(cur, _)| ts_key(&stamp) >= ts_key(cur));
                if newer {
                    latest_session = Some((stamp, sid));
                }
            }
        }

        // node_ended: completed_at is the statement; the newest do/ship row
        // supplies the session the deep link lands in.
        if let Some(completed) = s_field(entry, "completed_at") {
            let status = graph_store::s_str(entry, "status")
                .unwrap_or("done")
                .to_string();
            rows.push(FeedRow {
                ts: completed,
                kind: "node_ended".into(),
                node: Some(node_id.to_string()),
                session_id: latest_session.map(|(_, sid)| sid).flatten(),
                harness: None,
                title: status,
                r#ref: None,
            });
        }
    }

    rows.sort_by(|a, b| ts_key(&a.ts).cmp(&ts_key(&b.ts)));
    Projection {
        rows,
        skipped_lines,
        skipped_entries,
    }
}

/// The filters the CLI flags express, applied after ordering: `--node`,
/// `--session`, `--since-epoch` (unparseable ts rows survive a since filter),
/// then `--limit` from the newest end, output kept ascending. Pure so the
/// flags are testable without files.
pub fn filter_rows(
    rows: Vec<FeedRow>,
    node: Option<&str>,
    session: Option<&str>,
    since_epoch: Option<u64>,
    limit: Option<usize>,
) -> Vec<FeedRow> {
    let mut rows: Vec<FeedRow> = rows
        .into_iter()
        .filter(|r| node.is_none_or(|n| r.node.as_deref() == Some(n)))
        .filter(|r| session.is_none_or(|s| r.session_id.as_deref() == Some(s)))
        .filter(|r| match since_epoch {
            Some(since) => match chrono::DateTime::parse_from_rfc3339(&r.ts) {
                Ok(t) => t.timestamp() >= since as i64,
                Err(_) => true,
            },
            None => true,
        })
        .collect();
    if let Some(limit) = limit {
        let keep = limit.min(rows.len());
        let start = rows.len() - keep;
        rows.drain(0..start);
    }
    rows
}

struct FeedArgs {
    json: bool,
    since_epoch: Option<u64>,
    limit: Option<usize>,
    node: Option<String>,
    session: Option<String>,
}

fn parse_args(rest: &[String]) -> Result<FeedArgs, String> {
    let mut args = FeedArgs {
        json: false,
        since_epoch: None,
        limit: None,
        node: None,
        session: None,
    };
    let mut it = crate::client_verbs::expand_eq(rest).into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" | "-J" => args.json = true,
            "--since-epoch" => {
                args.since_epoch = Some(
                    it.next()
                        .and_then(|v| v.parse::<u64>().ok())
                        .ok_or("--since-epoch needs a non-negative integer")?,
                )
            }
            "--limit" => {
                args.limit = Some(
                    it.next()
                        .and_then(|v| v.parse::<usize>().ok())
                        .ok_or("--limit needs a positive integer")?,
                )
            }
            "--node" => args.node = Some(it.next().ok_or("--node needs an id")?),
            "--session" => args.session = Some(it.next().ok_or("--session needs an id")?),
            other => return Err(format!("unknown feed flag: {other}")),
        }
    }
    Ok(args)
}

/// The graph path, resolved as the fno crate's `backlog_view::graph_path`
/// does: `FNO_GRAPH_JSON` > `$HOME/.fno/graph.json` (the agents home's parent,
/// so a test home redirects it too).
fn graph_path(home: &AgentsHome) -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_GRAPH_JSON") {
        return PathBuf::from(v);
    }
    home.root()
        .parent()
        .map(|d| d.join("graph.json"))
        .unwrap_or_else(|| PathBuf::from("graph.json"))
}

/// The `fno-agents feed` verb. A missing or unreadable store is not fatal: the
/// rows the other store yielded still emit, with one stderr line naming the
/// store skipped. Non-JSON output is one line per row, `ts kind node session title`.
pub async fn run_feed(rest: &[String], home: &AgentsHome) -> i32 {
    let args = match parse_args(rest) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("fno-agents: {msg}");
            return 2;
        }
    };

    let fno_dir = home
        .root()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".fno"));
    let questions_raw =
        std::fs::read_to_string(fno_dir.join("questions.jsonl")).unwrap_or_else(|_| {
            eprintln!(
                "fno-agents feed: questions store unreadable, skipped: {}",
                fno_dir.join("questions.jsonl").display()
            );
            String::new()
        });

    let (graph_entries, graph_note): (Vec<Value>, Option<String>) =
        match graph_store::read_raw(&graph_path(home)) {
            Ok(graph_store::RawRead::Entries(list)) => (list, None),
            // An absent graph is still a skipped store for the feed's
            // purposes: the operator should know the lifecycle leg is absent,
            // even though read_raw treats absent as empty (AC3).
            Ok(graph_store::RawRead::Empty) => {
                (Vec::new(), Some("graph store skipped (absent)".to_string()))
            }
            Ok(graph_store::RawRead::MalformedRoot) => (
                Vec::new(),
                Some("graph store skipped (root carries no entries key)".to_string()),
            ),
            Ok(graph_store::RawRead::Corrupt(why)) => {
                (Vec::new(), Some(format!("graph store skipped ({why})")))
            }
            Err(e) => (Vec::new(), Some(format!("graph store skipped ({e})"))),
        };
    if let Some(note) = graph_note {
        eprintln!("fno-agents feed: {note}");
    }

    let Projection {
        rows,
        skipped_lines,
        skipped_entries,
    } = project(&questions_raw, &graph_entries);
    if skipped_lines > 0 {
        eprintln!("fno-agents feed: skipped {skipped_lines} malformed question line(s)");
    }
    if skipped_entries > 0 {
        eprintln!("fno-agents feed: skipped {skipped_entries} non-object graph entr(ies)");
    }

    let rows = filter_rows(
        rows,
        args.node.as_deref(),
        args.session.as_deref(),
        args.since_epoch,
        Some(args.limit.unwrap_or(200)),
    );

    if args.json {
        println!(
            "{}",
            serde_json::to_string(&rows).expect("serializing an owned value never fails")
        );
    } else {
        for r in &rows {
            println!(
                "{} {} {} {} {}",
                r.ts,
                r.kind,
                r.node.as_deref().unwrap_or("-"),
                r.session_id.as_deref().unwrap_or("-"),
                r.title
            );
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    // The x-9223 shape: a blueprint row with only ended_at, a do row and a
    // ship row each with started_at, on a node carrying pr_number and
    // completed_at.
    fn graph_fixture() -> Vec<Value> {
        vec![serde_json::json!({
            "id": "x-9223",
            "status": "done",
            "title": "feed marker node",
            "pr_number": 1395,
            "pr_url": "https://github.com/bllshttng/footnote/pull/1395",
            "completed_at": "2026-09-05T16:41:25Z",
            "sessions": [
                {"phase": "blueprint", "harness": "claude", "session_id": "s-blue",
                 "ended_at": "2026-09-02T16:00:00Z"},
                {"phase": "do", "harness": "claude", "session_id": "s-do",
                 "started_at": "2026-09-02T17:12:52Z"},
                {"phase": "ship", "harness": "claude", "session_id": "s-ship",
                 "started_at": "2026-09-02T18:27:06Z"}
            ]
        })]
    }

    fn questions_fixture() -> String {
        [
            r#"{"ts":"2026-09-02T17:00:00Z","type":"operator_question","source":"target","data":{"question_id":"q-1","question":"line one\nline two","session_id":"s-ask","node":"x-9223"}}"#,
            r#"{"ts":"2026-09-02T19:00:00Z","type":"operator_question_closed","source":"operator","data":{"question_id":"q-1","answer":"ruling: yes\ndo it","closed_by":"s-op"}}"#,
            r#"{"ts":"2026-09-03T09:00:00Z","type":"operator_decision","source":"operator","data":{"decision_id":"d-1","decision":"strict equality stands","subject":"revert-dispute","decided_by":"s-op"}}"#,
        ]
        .join("\n")
    }

    fn kinds(rows: &[FeedRow]) -> Vec<&str> {
        rows.iter().map(|r| r.kind.as_str()).collect()
    }

    #[test]
    fn lifecycle_rows_come_from_the_graph() {
        let p = project("", &graph_fixture());
        assert_eq!(kinds(&p.rows), ["node_started", "pr_created", "node_ended"]);
        let started = &p.rows[0];
        assert_eq!(started.node.as_deref(), Some("x-9223"));
        assert_eq!(started.session_id.as_deref(), Some("s-do"));
        let pr = &p.rows[1];
        assert_eq!(pr.session_id.as_deref(), Some("s-ship"));
        assert_eq!(pr.r#ref.as_deref(), Some("1395"));
        let ended = &p.rows[2];
        assert_eq!(ended.session_id.as_deref(), Some("s-ship"));
        assert_eq!(ended.title, "done");
    }

    #[test]
    fn empty_graph_slice_yields_zero_lifecycle_rows() {
        // The marker: a projection fed only an events-style stream yields none
        // of the three lifecycle rows - they derive from the graph and nowhere
        // else.
        let p = project(&questions_fixture(), &[]);
        assert_eq!(
            kinds(&p.rows),
            ["question_asked", "question_closed", "decision_recorded"]
        );
    }

    #[test]
    fn question_rows_carry_ids_and_asker_session() {
        let p = project(&questions_fixture(), &graph_fixture());
        let asked = p.rows.iter().find(|r| r.kind == "question_asked").unwrap();
        assert_eq!(asked.r#ref.as_deref(), Some("q-1"));
        assert_eq!(asked.session_id.as_deref(), Some("s-ask"));
        assert_eq!(asked.node.as_deref(), Some("x-9223"));
        assert_eq!(asked.title, "line one");
        let closed = p.rows.iter().find(|r| r.kind == "question_closed").unwrap();
        assert_eq!(closed.r#ref.as_deref(), Some("q-1"));
        assert_eq!(closed.session_id.as_deref(), Some("s-op"));
        assert_eq!(closed.title, "ruling: yes");
        let decision = p
            .rows
            .iter()
            .find(|r| r.kind == "decision_recorded")
            .unwrap();
        assert_eq!(decision.r#ref.as_deref(), Some("d-1"));
        assert_eq!(decision.title, "revert-dispute: strict equality stands");
    }

    #[test]
    fn rows_interleave_by_ts_ascending() {
        let p = project(&questions_fixture(), &graph_fixture());
        assert_eq!(
            kinds(&p.rows),
            [
                "question_asked",    // 09-02 17:00
                "node_started",      // 09-02 17:12
                "pr_created",        // 09-02 18:27
                "question_closed",   // 09-02 19:00
                "decision_recorded", // 09-03 09:00
                "node_ended",        // 09-05 16:41
            ]
        );
    }

    #[test]
    fn malformed_lines_and_non_object_entries_are_counted() {
        let questions =
            "not json\n{\"ts\":\"2026-09-02T17:00:00Z\",\"type\":\"other\",\"data\":{}}\n"
                .to_string()
                + &questions_fixture();
        let mut entries = vec![serde_json::json!("a bare string")];
        entries.extend(graph_fixture());
        let p = project(&questions, &entries);
        assert_eq!(p.skipped_lines, 2);
        assert_eq!(p.skipped_entries, 1);
        assert!(p.rows.iter().all(|r| matches!(
            r.kind.as_str(),
            "question_asked"
                | "question_closed"
                | "decision_recorded"
                | "node_started"
                | "pr_created"
                | "node_ended"
        )));
    }

    #[test]
    fn unparseable_ts_sorts_first_and_survives_since() {
        let questions = r#"{"ts":"yesterday-ish","type":"operator_question","source":"t","data":{"question_id":"q-0","question":"odd stamp","session_id":"s-x"}}"#.to_string();
        let p = project(&questions, &[]);
        assert_eq!(kinds(&p.rows)[0], "question_asked");
        assert_eq!(p.rows[0].ts, "yesterday-ish");
        let kept = filter_rows(p.rows, None, None, Some(1_700_000_000), None);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn filter_node_session_and_limit_from_newest_end() {
        let p = project(&questions_fixture(), &graph_fixture());
        let node_rows = filter_rows(p.rows.clone(), Some("x-9223"), None, None, None);
        // The fixture question carries node x-9223, so a node filter keeps it
        // alongside the lifecycle rows.
        assert_eq!(
            kinds(&node_rows),
            ["question_asked", "node_started", "pr_created", "node_ended"]
        );
        let ship_rows = filter_rows(p.rows.clone(), None, Some("s-ship"), None, None);
        assert_eq!(kinds(&ship_rows), ["pr_created", "node_ended"]);
        let newest_two = filter_rows(p.rows, None, None, None, Some(2));
        assert_eq!(kinds(&newest_two), ["decision_recorded", "node_ended"]);
    }

    #[test]
    fn node_without_pr_number_gets_no_pr_row() {
        let mut entry = graph_fixture().remove(0);
        entry
            .as_object_mut()
            .unwrap()
            .remove("pr_number")
            .expect("fixture carries pr_number");
        let p = project("", &[entry]);
        assert_eq!(kinds(&p.rows), ["node_started", "node_ended"]);
    }
}

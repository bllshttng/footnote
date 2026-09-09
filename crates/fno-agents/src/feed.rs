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
/// resolves, and it is set ONLY when the source value is a session handle -
/// see [`is_session_handle`]. Every field added after `ref` is skipped when
/// absent, so a row that carries none of them serializes as it did before.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct FeedRow {
    pub ts: String,
    /// `question_asked` | `question_closed` | `decision_recorded` |
    /// `node_created` | `node_started` | `pr_created` | `node_ended` |
    /// `session_reaped`
    pub kind: String,
    pub node: Option<String>,
    pub session_id: Option<String>,
    pub harness: Option<String>,
    pub title: String,
    #[serde(rename = "ref")]
    pub r#ref: Option<String>,
    /// Who took the action, when that is a mechanism rather than a session:
    /// `stale-escalate`, `fno agents stale-escalate`. It is provenance, never
    /// an attach target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The model observed for the session that produced this row, at event
    /// time. Never a current lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The effort recorded for that session, at event time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// The pipeline phase the session was in (`do`, `ship`, `blueprint`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// A rendered recovery line the panel hands over verbatim. Set on a
    /// `session_reaped` row; the resume string is copied, never re-derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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

/// True when `s` has the shape of a session handle a server can resolve:
/// an fno session id (`20260904T151442Z-cl54345-58af0c`) or a harness uuid
/// (`00847995-e0db-47c2-ab5b-24468ba1a4f5`).
///
/// The questions store puts a MECHANISM in the same field a session goes in:
/// `decided_by` is the literal `fno agents stale-escalate` on every decision
/// row, and `closed_by` is `stale-escalate` on most closure rows. Projecting
/// those into `session_id` makes the panel offer an attach the server answers
/// with `no such agent`. So the shape decides, and everything else becomes
/// `actor`.
fn is_session_handle(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    let all_hex = |p: &str, n: usize| p.len() == n && p.bytes().all(|c| c.is_ascii_hexdigit());
    // fno: <8 digits>T<6 digits>Z-<alnum tag>-<6 hex>
    if parts.len() == 3 {
        let stamp = parts[0].as_bytes();
        let stamp_ok = stamp.len() == 16
            && stamp[8] == b'T'
            && stamp[15] == b'Z'
            && stamp[..8].iter().all(u8::is_ascii_digit)
            && stamp[9..15].iter().all(u8::is_ascii_digit);
        if stamp_ok
            && !parts[1].is_empty()
            && parts[1].bytes().all(|c| c.is_ascii_alphanumeric())
            && all_hex(parts[2], 6)
        {
            return true;
        }
    }
    // uuid: 8-4-4-4-12 hex
    parts.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(n, p)| all_hex(p, *n))
}

/// Split a `closed_by` / `decided_by` value into the session handle it may be
/// and the actor label it always is: `(session_id, actor)`.
fn split_actor(raw: Option<String>) -> (Option<String>, Option<String>) {
    match raw {
        Some(v) if is_session_handle(&v) => (Some(v), None),
        Some(v) => (None, Some(v)),
        None => (None, None),
    }
}

/// Sort key: epoch millis when the ts parses as RFC3339; unparseable stamps
/// sort first and keep their raw string (the row is still shown).
fn ts_key(ts: &str) -> (u8, i64) {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => (1, t.timestamp_millis()),
        Err(_) => (0, 0),
    }
}

/// The pure projection: questions.jsonl text + graph entries + reap receipts
/// -> ordered rows. Ascending by ts, so a consumer reads history forward and
/// `--limit` trims from the newest end.
pub fn project(
    questions_raw: &str,
    graph_entries: &[Value],
    receipts: &[crate::receipt::ReapReceipt],
) -> Projection {
    let mut rows = Vec::new();
    let mut skipped_lines = 0usize;
    let mut skipped_entries = 0usize;

    // A closure and a decision carry only their `question_id`; the ASKING row
    // is the one place the node and session association exists. Index it in
    // the same pass the rows are read, then fill the association below.
    let mut asked: std::collections::HashMap<String, (Option<String>, Option<String>)> =
        std::collections::HashMap::new();

    for line in questions_raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("operator_question") {
            continue;
        }
        let Some(data) = v.get("data") else { continue };
        let Some(qid) = s_field(data, "question_id") else {
            continue;
        };
        asked.insert(qid, (s_field(data, "node"), s_field(data, "session_id")));
    }

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
                    title,
                    r#ref: s_field(data, "question_id"),
                    actor: s_field(data, "asker"),
                    ..FeedRow::default()
                }
            }
            Some("operator_question_closed") => {
                let title = data
                    .get("answer")
                    .and_then(Value::as_str)
                    .map(one_line)
                    .unwrap_or_default();
                let qid = s_field(data, "question_id");
                // The asking row supplies the ASSOCIATION only. Its session
                // asked the question; it did not close it, and borrowing it
                // here would put a guess where the provenance goes.
                let node = qid
                    .as_deref()
                    .and_then(|q| asked.get(q))
                    .and_then(|(n, _)| n.clone());
                let (closer_session, actor) = split_actor(s_field(data, "closed_by"));
                FeedRow {
                    ts,
                    kind: "question_closed".into(),
                    node,
                    session_id: closer_session,
                    title,
                    r#ref: qid,
                    actor,
                    ..FeedRow::default()
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
                let qid = s_field(data, "question_id");
                let node = qid
                    .as_deref()
                    .and_then(|q| asked.get(q))
                    .and_then(|(n, _)| n.clone());
                let (decider_session, actor) = split_actor(s_field(data, "decided_by"));
                FeedRow {
                    ts,
                    kind: "decision_recorded".into(),
                    node,
                    session_id: decider_session,
                    title: one_line(&title),
                    r#ref: s_field(data, "decision_id"),
                    actor,
                    ..FeedRow::default()
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

        // node_created needs no emitter and no new store: `created_at` parses
        // on every entry the graph holds.
        if let Some(created) = s_field(entry, "created_at") {
            rows.push(FeedRow {
                ts: created,
                kind: "node_created".into(),
                node: Some(node_id.to_string()),
                title: node_title.clone(),
                ..FeedRow::default()
            });
        }

        // node_started: every do row that started. node_pr: a ship row that
        // started on a node carrying pr_number.
        let mut latest_session: Option<(String, Option<String>, Option<String>)> = None;
        for row in &sessions {
            let phase = graph_store::s_str(row, "phase").unwrap_or_default();
            let sid = s_field(row, "session_id");
            let harness = s_field(row, "harness");
            // The lane this session ran, as OBSERVED at event time. A current
            // registry lookup would answer a different question. The graph
            // stores this as a MEASUREMENT, not a string: `{kind, model,
            // samples}`, where kind is `observed`, `observed-multiple`,
            // `not-file-backed`, `no-transcript` or `unreadable`. Only the
            // kinds that actually name a model yield one; the rest carry no
            // model key at all and read as not recorded rather than blank.
            let model = row
                .get("observed_model")
                .and_then(|v| v.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let effort = s_field(row, "effort");
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
                    model: model.clone(),
                    effort: effort.clone(),
                    phase: Some(phase.to_string()),
                    ..FeedRow::default()
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
                        model: model.clone(),
                        effort: effort.clone(),
                        phase: Some(phase.to_string()),
                        ..FeedRow::default()
                    });
                }
            }
            if phase == "do" || phase == "ship" {
                let stamp = started.to_string();
                let newer = latest_session
                    .as_ref()
                    .is_none_or(|(cur, _, _)| ts_key(&stamp) >= ts_key(cur));
                if newer {
                    latest_session = Some((stamp, sid, harness));
                }
            }
        }

        // node_ended: completed_at is the statement; the newest do/ship row
        // supplies the session the deep link lands in.
        if let Some(completed) = s_field(entry, "completed_at") {
            let status = graph_store::s_str(entry, "status")
                .unwrap_or("done")
                .to_string();
            let (sid, harness) = latest_session
                .map(|(_, sid, harness)| (sid, harness))
                .unwrap_or((None, None));
            rows.push(FeedRow {
                ts: completed,
                kind: "node_ended".into(),
                node: Some(node_id.to_string()),
                session_id: sid,
                harness,
                title: status,
                ..FeedRow::default()
            });
        }
    }

    // The removal leg. The receipts store qualifies where `events.jsonl` does
    // not: it never rotates, it holds exactly one durable row per removal, and
    // it IS the record rather than a restatement of one.
    for r in receipts {
        let node = r
            .ledger
            .as_ref()
            .and_then(|l| l.get("graph_node_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let removed_by = r.removed_by.as_deref().unwrap_or("reap");
        rows.push(FeedRow {
            ts: r.reaped_at.clone(),
            kind: "session_reaped".into(),
            node,
            session_id: Some(r.harness_session_id.clone()),
            harness: Some(r.harness.clone()),
            title: format!("{} removed by {removed_by}", r.row_name),
            actor: r.removed_by.clone(),
            // Copied verbatim. It was rendered from the capability table at
            // reap time; re-deriving it would answer a different question if
            // that table has moved since.
            detail: Some(format!("resume: {} - cwd {}", r.resume, r.cwd)),
            ..FeedRow::default()
        });
    }

    rows.sort_by(|a, b| ts_key(&a.ts).cmp(&ts_key(&b.ts)));
    Projection {
        rows,
        skipped_lines,
        skipped_entries,
    }
}

/// The filters the CLI flags express, applied after ordering: `--node`,
/// `--session`, `--kind`, `--since-epoch` (unparseable ts rows survive a since
/// filter), then `--limit` from the newest end, output kept ascending. Pure so
/// the flags are testable without files.
pub fn filter_rows(
    rows: Vec<FeedRow>,
    node: Option<&str>,
    session: Option<&str>,
    kind: Option<&str>,
    since_epoch: Option<u64>,
    limit: Option<usize>,
) -> Vec<FeedRow> {
    let mut rows: Vec<FeedRow> = rows
        .into_iter()
        .filter(|r| node.is_none_or(|n| r.node.as_deref() == Some(n)))
        .filter(|r| session.is_none_or(|s| r.session_id.as_deref() == Some(s)))
        .filter(|r| kind.is_none_or(|k| r.kind == k))
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
    kind: Option<String>,
}

fn parse_args(rest: &[String]) -> Result<FeedArgs, String> {
    let mut args = FeedArgs {
        json: false,
        since_epoch: None,
        limit: None,
        node: None,
        session: None,
        kind: None,
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
            "--kind" => args.kind = Some(it.next().ok_or("--kind needs a kind name")?),
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

/// Read every reap receipt under `<agents home>/reap-receipts/`. An absent or
/// unreadable directory is not fatal and yields one note naming the store
/// skipped, matching the questions and graph legs; a receipt that will not
/// parse is counted, not fatal.
fn read_receipts(home: &AgentsHome) -> (Vec<crate::receipt::ReapReceipt>, Option<String>, usize) {
    let dir = home.root().join("reap-receipts");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            return (
                Vec::new(),
                Some(format!(
                    "reap-receipts store skipped ({e}): {}",
                    dir.display()
                )),
                0,
            )
        }
    };
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match crate::receipt::read_reap_receipt(&path) {
            Ok(r) => out.push(r),
            Err(_) => skipped += 1,
        }
    }
    (out, None, skipped)
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

    let (receipts, receipts_note, skipped_receipts) = read_receipts(home);
    if let Some(note) = receipts_note {
        eprintln!("fno-agents feed: {note}");
    }
    if skipped_receipts > 0 {
        eprintln!("fno-agents feed: skipped {skipped_receipts} unreadable reap receipt(s)");
    }

    let Projection {
        rows,
        skipped_lines,
        skipped_entries,
    } = project(&questions_raw, &graph_entries, &receipts);
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
        args.kind.as_deref(),
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
            // The who column falls back to the ACTOR, so a plain reader still
            // sees who acted on a row whose actor is a mechanism. Printing a
            // bare dash there dropped the only name those rows carry.
            let who = r
                .session_id
                .as_deref()
                .or(r.actor.as_deref())
                .unwrap_or("-");
            println!(
                "{} {} {} {} {}",
                r.ts,
                r.kind,
                r.node.as_deref().unwrap_or("-"),
                who,
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
            "created_at": "2026-09-01T08:00:00Z",
            "completed_at": "2026-09-05T16:41:25Z",
            "sessions": [
                {"phase": "blueprint", "harness": "claude", "session_id": "s-blue",
                 "ended_at": "2026-09-02T16:00:00Z"},
                {"phase": "do", "harness": "claude", "session_id": "s-do",
                 "observed_model": {"kind": "observed", "model": "claude-opus-5", "samples": 12},
                 "effort": "high",
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
        let p = project("", &graph_fixture(), &[]);
        assert_eq!(
            kinds(&p.rows),
            ["node_created", "node_started", "pr_created", "node_ended"]
        );
        let started = &p.rows[1];
        assert_eq!(started.node.as_deref(), Some("x-9223"));
        assert_eq!(started.session_id.as_deref(), Some("s-do"));
        let pr = &p.rows[2];
        assert_eq!(pr.session_id.as_deref(), Some("s-ship"));
        assert_eq!(pr.r#ref.as_deref(), Some("1395"));
        let ended = &p.rows[3];
        assert_eq!(ended.session_id.as_deref(), Some("s-ship"));
        assert_eq!(ended.title, "done");
    }

    #[test]
    fn empty_graph_slice_yields_zero_lifecycle_rows() {
        // The marker: a projection fed only an events-style stream yields none
        // of the three lifecycle rows - they derive from the graph and nowhere
        // else.
        let p = project(&questions_fixture(), &[], &[]);
        assert_eq!(
            kinds(&p.rows),
            ["question_asked", "question_closed", "decision_recorded"]
        );
    }

    #[test]
    fn question_rows_carry_ids_and_asker_session() {
        let p = project(&questions_fixture(), &graph_fixture(), &[]);
        let asked = p.rows.iter().find(|r| r.kind == "question_asked").unwrap();
        assert_eq!(asked.r#ref.as_deref(), Some("q-1"));
        assert_eq!(asked.session_id.as_deref(), Some("s-ask"));
        assert_eq!(asked.node.as_deref(), Some("x-9223"));
        assert_eq!(asked.title, "line one");
        let closed = p.rows.iter().find(|r| r.kind == "question_closed").unwrap();
        assert_eq!(closed.r#ref.as_deref(), Some("q-1"));
        // `s-op` is not a session handle, so it is the ACTOR and the closure
        // offers no attach target. The node comes from the asking row.
        assert_eq!(closed.session_id, None);
        assert_eq!(closed.actor.as_deref(), Some("s-op"));
        assert_eq!(closed.node.as_deref(), Some("x-9223"));
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
        let p = project(&questions_fixture(), &graph_fixture(), &[]);
        assert_eq!(
            kinds(&p.rows),
            [
                "node_created",      // 09-01 08:00
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
        let p = project(&questions, &entries, &[]);
        assert_eq!(p.skipped_lines, 2);
        assert_eq!(p.skipped_entries, 1);
        assert!(p.rows.iter().all(|r| matches!(
            r.kind.as_str(),
            "question_asked"
                | "question_closed"
                | "decision_recorded"
                | "node_created"
                | "node_started"
                | "pr_created"
                | "node_ended"
                | "session_reaped"
        )));
    }

    #[test]
    fn unparseable_ts_sorts_first_and_survives_since() {
        let questions = r#"{"ts":"yesterday-ish","type":"operator_question","source":"t","data":{"question_id":"q-0","question":"odd stamp","session_id":"s-x"}}"#.to_string();
        let p = project(&questions, &[], &[]);
        assert_eq!(kinds(&p.rows)[0], "question_asked");
        assert_eq!(p.rows[0].ts, "yesterday-ish");
        let kept = filter_rows(p.rows, None, None, None, Some(1_700_000_000), None);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn filter_node_session_and_limit_from_newest_end() {
        let p = project(&questions_fixture(), &graph_fixture(), &[]);
        let node_rows = filter_rows(p.rows.clone(), Some("x-9223"), None, None, None, None);
        // The fixture question carries node x-9223, so a node filter keeps it
        // alongside the lifecycle rows - and its CLOSURE now too, because the
        // closure inherits the association from the row that asked.
        assert_eq!(
            kinds(&node_rows),
            [
                "node_created",
                "question_asked",
                "node_started",
                "pr_created",
                "question_closed",
                "node_ended"
            ]
        );
        let ship_rows = filter_rows(p.rows.clone(), None, Some("s-ship"), None, None, None);
        assert_eq!(kinds(&ship_rows), ["pr_created", "node_ended"]);
        let newest_two = filter_rows(p.rows, None, None, None, None, Some(2));
        assert_eq!(kinds(&newest_two), ["decision_recorded", "node_ended"]);
    }

    fn receipt_fixture() -> crate::receipt::ReapReceipt {
        crate::receipt::ReapReceipt {
            row_name: "t-d145".into(),
            short_id: "d145".into(),
            harness: "claude".into(),
            harness_session_id: "00847995-e0db-47c2-ab5b-24468ba1a4f5".into(),
            cwd: "/tmp/wt".into(),
            log_path: None,
            created_at: "2026-09-04T10:00:00Z".into(),
            reaped_at: "2026-09-06T10:00:00Z".into(),
            resume: "claude --resume 00847995".into(),
            ledger: Some(serde_json::json!({"graph_node_id": "x-9223"})),
            removed_by: None,
            schema_version: Some(2),
            identity: None,
            native_locator: None,
            model_provenance: None,
            resume_argv: Vec::new(),
            effects: Vec::new(),
            assignment: None,
            details_expired_at: None,
            writer_build: None,
        }
    }

    #[test]
    fn an_actor_that_is_not_a_session_never_becomes_an_attach_target() {
        // The live shape: `decided_by` is the literal verb on every decision
        // row, and `closed_by` is a mechanism name on most closure rows.
        let questions = [
            r#"{"ts":"2026-09-02T17:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-1","question":"ask","node":"x-1111"}}"#,
            r#"{"ts":"2026-09-02T19:00:00Z","type":"operator_question_closed","source":"d","data":{"question_id":"q-1","answer":"superseded","closed_by":"stale-escalate"}}"#,
            r#"{"ts":"2026-09-03T09:00:00Z","type":"operator_decision","source":"d","data":{"decision_id":"d-1","decision":"stands","subject":"s","question_id":"q-1","decided_by":"fno agents stale-escalate"}}"#,
        ]
        .join("\n");
        let p = project(&questions, &[], &[]);
        let closed = p.rows.iter().find(|r| r.kind == "question_closed").unwrap();
        assert_eq!(closed.session_id, None);
        assert_eq!(closed.actor.as_deref(), Some("stale-escalate"));
        assert_eq!(closed.node.as_deref(), Some("x-1111"));
        let decided = p
            .rows
            .iter()
            .find(|r| r.kind == "decision_recorded")
            .unwrap();
        assert_eq!(decided.session_id, None);
        assert_eq!(decided.actor.as_deref(), Some("fno agents stale-escalate"));
        assert_eq!(decided.node.as_deref(), Some("x-1111"));
    }

    #[test]
    fn a_closer_that_is_a_session_handle_stays_a_session() {
        let questions = [
            r#"{"ts":"2026-09-02T17:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-1","question":"ask"}}"#,
            r#"{"ts":"2026-09-02T19:00:00Z","type":"operator_question_closed","source":"d","data":{"question_id":"q-1","answer":"yes","closed_by":"20260904T151442Z-cl54345-58af0c"}}"#,
        ]
        .join("\n");
        let p = project(&questions, &[], &[]);
        let closed = p.rows.iter().find(|r| r.kind == "question_closed").unwrap();
        assert_eq!(
            closed.session_id.as_deref(),
            Some("20260904T151442Z-cl54345-58af0c")
        );
        assert_eq!(closed.actor, None);
    }

    #[test]
    fn a_receipt_becomes_one_reaped_row_carrying_its_resume_line() {
        let r = receipt_fixture();
        let p = project("", &[], std::slice::from_ref(&r));
        let row = p
            .rows
            .iter()
            .find(|row| row.kind == "session_reaped")
            .expect("one reaped row");
        assert!(row.title.contains("t-d145"), "title was {}", row.title);
        assert_eq!(
            row.detail.as_deref(),
            Some("resume: claude --resume 00847995 - cwd /tmp/wt")
        );
        assert_eq!(row.node.as_deref(), Some("x-9223"));
        assert_eq!(
            row.session_id.as_deref(),
            Some("00847995-e0db-47c2-ab5b-24468ba1a4f5")
        );
        let only_reaped = filter_rows(p.rows, None, None, Some("session_reaped"), None, None);
        assert_eq!(only_reaped.len(), 1);
    }

    #[test]
    fn an_absent_receipts_directory_yields_no_rows_and_no_panic() {
        let home = AgentsHome::at(std::path::PathBuf::from(
            "/nonexistent/fno-feed-test/agents",
        ));
        let (receipts, note, skipped) = read_receipts(&home);
        assert!(receipts.is_empty());
        assert_eq!(skipped, 0);
        let note = note.expect("an absent store is reported, never silent");
        assert!(
            note.contains("reap-receipts store skipped"),
            "note was {note}"
        );
    }

    #[test]
    fn every_graph_entry_yields_a_node_created_row_and_the_lane_it_ran() {
        let p = project("", &graph_fixture(), &[]);
        let created = p
            .rows
            .iter()
            .find(|r| r.kind == "node_created")
            .expect("created_at projects with no emitter");
        assert_eq!(created.node.as_deref(), Some("x-9223"));
        let started = p.rows.iter().find(|r| r.kind == "node_started").unwrap();
        assert_eq!(started.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(started.session_id.as_deref(), Some("s-do"));
        assert_eq!(started.effort.as_deref(), Some("high"));
        assert_eq!(started.phase.as_deref(), Some("do"));
        let ended = p.rows.iter().find(|r| r.kind == "node_ended").unwrap();
        assert_eq!(ended.harness.as_deref(), Some("claude"));
    }

    #[test]
    fn node_without_pr_number_gets_no_pr_row() {
        let mut entry = graph_fixture().remove(0);
        entry
            .as_object_mut()
            .unwrap()
            .remove("pr_number")
            .expect("fixture carries pr_number");
        let p = project("", &[entry], &[]);
        assert_eq!(
            kinds(&p.rows),
            ["node_created", "node_started", "node_ended"]
        );
    }
}

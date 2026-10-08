//! `fno-agents feed` - one projection joining questions, decisions and node
//! lifecycle into an ordered feed.
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
    /// `node_created` | `node_started` | `node_shipped` | `node_ended` |
    /// `session_spawned` | `session_reaped` | `team_granted` |
    /// `team_vacated` | `day_boundary`
    pub kind: String,
    pub node: Option<String>,
    /// The node's project directory. Present on `node_created` rows so a
    /// launch opened from the fleet feed starts in the node's repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub harness: Option<String>,
    pub title: String,
    #[serde(rename = "ref")]
    pub r#ref: Option<String>,
    /// The registry row's worker name, set on a removal: the handle the
    /// resume gesture and the copied `fno agents resume` command address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// receipt-backed `session_reaped` row; the resume string is copied,
    /// never re-derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Why the row happened, when the source records one: a removal's
    /// recorded cause, verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `L{level} {scope}` for the team kinds and a teamed removal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// The teamed worker's name on `team_granted` and `team_vacated`
    /// rows; the feed search answers `l:` through it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    /// The lead or epic the row rolls up to, set on non-team rows only:
    /// `lead {holder} L{level}` or `epic {parent} {title}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The session that spawned this row's session, from the birth event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The PR URL, on a ship row. The mux's provenance action opens it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Which slice of the fleet the row belongs to: mail, backlog, ship,
    /// agents, mux, fleet or ci. A pure function of the kind, so no migration
    /// ever backfills it.
    pub area: String,
    /// The role holder the row rolls up to, when the row's scope is held.
    /// The search answers `l:` through it, else through `owner`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<String>,
    /// The row's position in the total order, as a six-string JSON array
    /// (`[ts, kind, node, session_id, ref, title]`, absent fields as "").
    /// The `--before` / `--after` flags take one back. Always serialized:
    /// the client pages by it without parsing anything else.
    pub cursor: String,
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
pub(crate) fn ts_key(ts: &str) -> (u8, i64) {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => (1, t.timestamp_millis()),
        Err(_) => (0, 0),
    }
}

/// The total row order: parsed time first, then kind, node, session, ref and
/// title as strings. Hundreds of rows share a ts, so the ts alone cannot
/// bound a page; this key can, and the cursor is exactly its string half.
pub(crate) fn order_key(r: &FeedRow) -> ((u8, i64), &str, &str, &str, &str, &str) {
    (
        ts_key(&r.ts),
        &r.kind,
        r.node.as_deref().unwrap_or(""),
        r.session_id.as_deref().unwrap_or(""),
        r.r#ref.as_deref().unwrap_or(""),
        &r.title,
    )
}

/// The cursor string for a row: the six order strings as one JSON array.
fn cursor_of(r: &FeedRow) -> String {
    serde_json::to_string(&[
        r.ts.clone(),
        r.kind.clone(),
        r.node.clone().unwrap_or_default(),
        r.session_id.clone().unwrap_or_default(),
        r.r#ref.clone().unwrap_or_default(),
        r.title.clone(),
    ])
    .expect("serializing six strings never fails")
}

/// Decode a cursor into the six strings it must carry. The ts element is
/// re-parsed through `ts_key` when the key is compared, so a decoded cursor
/// orders exactly as the rows around it do.
fn parse_cursor(raw: &str) -> Result<[String; 6], String> {
    let v: Value = serde_json::from_str(raw).map_err(|_| "not a JSON array".to_string())?;
    let arr = v.as_array().ok_or("not a JSON array")?;
    if arr.len() != 6 {
        return Err(format!("expected six elements, got {}", arr.len()));
    }
    let mut out = [const { String::new() }; 6];
    for (slot, v) in out.iter_mut().zip(arr) {
        *slot = v.as_str().ok_or("element is not a string")?.to_string();
    }
    Ok(out)
}

/// The pure projection: questions text + graph entries + removals + the
/// agents journal's spawn events + the team events (both journals) ->
/// ordered rows. Ascending by ts, so a consumer reads history forward and
/// `--limit` trims from the newest end.
pub fn project(
    questions_raw: &str,
    graph_entries: &[Value],
    removals: &[crate::removals::Removal],
    spawns_raw: &str,
    team_raw: &str,
    closes_raw: &str,
    main_raw: &str,
    node_set: Option<&[String]>,
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
        let kind = match v
            .get("type")
            .and_then(Value::as_str)
            .map(crate::event_store::event_type_alias)
        {
            Some("day_boundary") => {
                let boundary_kind = s_field(data, "kind").unwrap_or_default();
                FeedRow {
                    ts,
                    kind: "day_boundary".into(),
                    title: format!("day {boundary_kind}"),
                    r#ref: s_field(data, "boundary_id"),
                    ..FeedRow::default()
                }
            }
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
        // A node filter cuts the graph leg before any row derives: the entry
        // set is the operator's whole backlog, and only the asked-for nodes
        // pay the derivation.
        if node_set.is_some_and(|set| !set.iter().any(|n| n == node_id)) {
            continue;
        }
        let node_title = graph_store::s_str(entry, "title")
            .unwrap_or(node_id)
            .to_string();
        let sessions = entry
            .get("sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // node_created needs no emitter and no new store: `created_at` parses
        // on every entry the graph holds. The creating session rides the
        // entry's source fields when the graph recorded them.
        if let Some(created) = s_field(entry, "created_at") {
            // Model, effort, parent session and team ride the entry's
            // write-time source stamps (the creating session's registry row
            // at birth). An entry from before the stamps reads them as
            // absent, never as a current lookup.
            rows.push(FeedRow {
                ts: created,
                kind: "node_created".into(),
                node: Some(node_id.to_string()),
                cwd: s_field(entry, "cwd"),
                session_id: s_field(entry, "source_session_id").filter(|s| is_session_handle(s)),
                harness: s_field(entry, "source_harness"),
                model: s_field(entry, "source_model"),
                effort: s_field(entry, "source_effort"),
                parent: s_field(entry, "source_parent_session"),
                team: s_field(entry, "source_team"),
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
            if phase == "execute" {
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
                        kind: "node_shipped".into(),
                        node: Some(node_id.to_string()),
                        session_id: sid.clone(),
                        harness: harness.clone(),
                        title: format!("PR {} - {url}", pr),
                        r#ref: Some(pr.to_string()),
                        model: model.clone(),
                        effort: effort.clone(),
                        phase: Some(phase.to_string()),
                        url: Some(url.to_string()),
                        ..FeedRow::default()
                    });
                }
            }
            if phase == "execute" || phase == "ship" {
                let stamp = started.to_string();
                let newer = latest_session
                    .as_ref()
                    .is_none_or(|(cur, _, _)| ts_key(&stamp) >= ts_key(cur));
                if newer {
                    latest_session = Some((stamp, sid, harness));
                }
            }
        }

        // Completion: a merged node reads pr_merged (the merge IS the end);
        // everything else reads node_ended. completed_at is the statement;
        // the newest do/ship row supplies the session the deep link lands in.
        if let Some(completed) = s_field(entry, "completed_at") {
            let status = graph_store::s_str(entry, "status")
                .unwrap_or("done")
                .to_string();
            let (sid, harness) = latest_session
                .map(|(_, sid, harness)| (sid, harness))
                .unwrap_or((None, None));
            let merged = graph_store::s_str(entry, "merge_status").as_deref() == Some("merged");
            let pr = entry.get("pr_number").and_then(Value::as_i64);
            if let (true, Some(pr)) = (merged, pr) {
                let url = graph_store::s_str(entry, "pr_url").unwrap_or(node_id);
                rows.push(FeedRow {
                    ts: completed,
                    kind: "pr_merged".into(),
                    node: Some(node_id.to_string()),
                    session_id: sid,
                    harness,
                    title: format!("PR {pr} merged - {url}"),
                    r#ref: Some(pr.to_string()),
                    url: Some(url.to_string()),
                    ..FeedRow::default()
                });
            } else {
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
    }

    // The removal leg. The receipts store qualifies where `events.jsonl` does
    // not: it never rotates, it holds exactly one durable row per removal, and
    // it IS the record rather than a restatement of one. The fold also
    // recovers the receipt-less removals (a never-bound row the receipt
    // builder refused) from their `registry_row_removed` events.
    for r in removals {
        let removed_by = if r.removed_by.is_empty() {
            None
        } else {
            Some(r.removed_by.clone())
        };
        rows.push(FeedRow {
            ts: r.ts.clone(),
            kind: "session_reaped".into(),
            node: r.node.clone(),
            session_id: r.session_id.clone(),
            harness: r.harness.clone(),
            // The receipt is the only surviving record of the lane this row
            // ran. Dropping it here would report a model the store holds as
            // NOT RECORDED.
            model: r.model.clone(),
            title: format!("{} removed", r.name),
            name: Some(r.name.clone()),
            actor: removed_by,
            reason: r.reason.clone(),
            team: r.team.clone(),
            // Copied verbatim from the receipt, and set only when a receipt
            // exists: the provenance view treats any `detail` as a recovery
            // line, so a recovered receipt-less removal must not carry one.
            detail: r.receipt.as_ref().map(|_| {
                format!(
                    "resume: {} - cwd {} - trigger {}",
                    r.resume.as_deref().unwrap_or(""),
                    r.cwd.as_deref().unwrap_or(""),
                    r.trigger
                        .as_deref()
                        .filter(|t| !t.is_empty())
                        .unwrap_or("unknown")
                )
            }),
            ..FeedRow::default()
        });
    }

    // Close rows: every pane close and server stop the mux
    // recorded, plus the composer's bang-mode shell rows (one run or
    // refusal each). The reason rides verbatim; cause is the enum's word,
    // so the row tells the operator WHO closed it (operator vs the death
    // path) and WHY in one line.
    for line in closes_raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = match v
            .get("type")
            .and_then(Value::as_str)
            .map(crate::event_store::event_type_alias)
        {
            Some(
                k @ ("pane_closed"
                | "server_stopped"
                | "composer_shell_ran"
                | "composer_shell_refused"
                | "daemon_started"),
            ) => k,
            _ => continue,
        };
        let Some(data) = v.get("data") else { continue };
        let Some(ts) = s_field(&v, "ts") else {
            continue;
        };
        if kind == "daemon_started" {
            let version = s_field(data, "version").unwrap_or_default();
            let pid = data.get("pid").and_then(Value::as_u64).unwrap_or(0);
            rows.push(FeedRow {
                ts: ts.to_string(),
                kind: "daemon_restarted".into(),
                title: format!("daemon {version} pid {pid}"),
                ..FeedRow::default()
            });
            continue;
        }
        if kind == "server_stopped" {
            let cause = s_field(data, "cause").unwrap_or_else(|| "unknown".into());
            rows.push(FeedRow {
                ts: ts.to_string(),
                kind: "server_stopped".into(),
                title: format!("mux server stopped: {cause}"),
                ..FeedRow::default()
            });
            continue;
        }
        if kind == "composer_shell_ran" {
            let cwd = s_field(data, "cwd").unwrap_or_default();
            let line = s_field(data, "line").unwrap_or_default();
            let pane = data.get("pane").and_then(Value::as_u64).unwrap_or(0);
            rows.push(FeedRow {
                ts: ts.to_string(),
                kind: "composer_shell_ran".into(),
                title: format!("shell in {cwd}: {line} (pane {pane})"),
                ..FeedRow::default()
            });
            continue;
        }
        if kind == "composer_shell_refused" {
            let reason = s_field(data, "reason").unwrap_or_else(|| "no reason recorded".into());
            rows.push(FeedRow {
                ts: ts.to_string(),
                kind: "composer_shell_refused".into(),
                title: format!("shell refused: {reason}"),
                reason: Some(reason),
                ..FeedRow::default()
            });
            continue;
        }

        let name = s_field(data, "name").unwrap_or_default();
        let pane = data
            .get("pane")
            .and_then(Value::as_u64)
            .map(|p| p.to_string())
            .unwrap_or_default();
        let reason = s_field(data, "reason").unwrap_or_else(|| "no reason recorded".into());
        let title = if name.is_empty() {
            format!("pane {pane} closed: {reason}")
        } else {
            format!("pane {name} ({pane}) closed: {reason}")
        };
        rows.push(FeedRow {
            ts: ts.to_string(),
            kind: "pane_closed".into(),
            session_id: s_field(data, "harness_session"),
            harness: s_field(data, "harness"),
            title,
            reason: Some(reason),
            ..FeedRow::default()
        });
    }

    // Spawn rows: `agent_spawned` (agents journal, already window-bounded by
    // the caller). A birth carries the substrate it landed on, the model it
    // was asked for, and the session that spawned it.
    for line in spawns_raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("agent_spawn_refused") {
            // A pre-birth refusal: the feed shows the launch the operator
            // watched refuse, carrying the door's own fatal line.
            let Some(data) = v.get("data") else { continue };
            let Some(ts) = s_field(&v, "ts") else {
                continue;
            };
            rows.push(FeedRow {
                ts,
                kind: "session_spawn_refused".into(),
                harness: s_field(data, "harness"),
                title: format!(
                    "spawn refused: {}",
                    s_field(data, "reason").unwrap_or_else(|| "unknown reason".into())
                ),
                ..FeedRow::default()
            });
            continue;
        }
        if v.get("type").and_then(Value::as_str) != Some("agent_spawned") {
            continue;
        }
        let Some(data) = v.get("data") else { continue };
        let (Some(name), Some(ts)) = (s_field(data, "name"), s_field(&v, "ts")) else {
            continue;
        };
        rows.push(FeedRow {
            ts,
            kind: "session_spawned".into(),
            node: s_field(data, "node"),
            session_id: s_field(data, "harness_session_id").filter(|s| is_session_handle(s)),
            harness: s_field(data, "harness").or_else(|| s_field(data, "provider")),
            model: s_field(data, "model"),
            parent: s_field(data, "spawned_by_session"),
            title: format!(
                "{} spawned ({})",
                name,
                s_field(data, "substrate").unwrap_or_else(|| "unknown".into())
            ),
            ..FeedRow::default()
        });
    }

    // Coverage kinds from the durable main store. spawn_gate_refused keeps
    // the session_spawn_refused kind and carries the gate axis as its reason;
    // the update trio folds into update_started / update_finished.
    for line in main_raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = match v
            .get("type")
            .and_then(Value::as_str)
            .map(crate::event_store::event_type_alias)
        {
            Some(
                k @ ("spawn_gate_refused"
                | "worker_silent"
                | "blocked"
                | "fno_update_started"
                | "fno_update_installed"
                | "fno_update_failed"),
            ) => k,
            _ => continue,
        };
        let Some(data) = v.get("data") else { continue };
        let Some(ts) = s_field(&v, "ts") else {
            continue;
        };
        match kind {
            "spawn_gate_refused" => {
                let reason = s_field(data, "reason").unwrap_or_else(|| "unknown".into());
                let name = s_field(data, "name").unwrap_or_default();
                rows.push(FeedRow {
                    ts,
                    kind: "session_spawn_refused".into(),
                    title: format!("spawn refused ({reason}): {name}"),
                    reason: Some(reason),
                    ..FeedRow::default()
                });
            }
            "worker_silent" => {
                let handle = s_field(data, "handle").unwrap_or_default();
                let age = data.get("age_s").map(|v| v.to_string()).unwrap_or_default();
                rows.push(FeedRow {
                    ts,
                    kind: "worker_stalled".into(),
                    node: s_field(data, "node"),
                    harness: s_field(data, "harness"),
                    title: format!("{handle} silent {age}s"),
                    ..FeedRow::default()
                });
            }
            "blocked" => {
                let bkind = s_field(data, "kind").unwrap_or_default();
                let reason = s_field(data, "reason").unwrap_or_default();
                let title = if reason.is_empty() {
                    bkind
                } else {
                    format!("{bkind}: {}", one_line(&reason))
                };
                rows.push(FeedRow {
                    ts,
                    kind: "help_emitted".into(),
                    node: s_field(&v, "node"),
                    harness: s_field(&v, "harness"),
                    reason: if reason.is_empty() {
                        None
                    } else {
                        Some(one_line(&reason))
                    },
                    title,
                    ..FeedRow::default()
                });
            }
            "fno_update_started" | "fno_update_installed" => {
                let rev = s_field(data, "new_rev").unwrap_or_default();
                let short: String = rev.chars().take(8).collect();
                let started = kind == "fno_update_started";
                rows.push(FeedRow {
                    ts,
                    kind: if started {
                        "update_started".into()
                    } else {
                        "update_finished".into()
                    },
                    title: format!(
                        "fno update {} ({short})",
                        if started { "started" } else { "installed" }
                    ),
                    ..FeedRow::default()
                });
            }
            _ => {
                let reason = s_field(data, "reason").unwrap_or_else(|| "failed".into());
                rows.push(FeedRow {
                    ts,
                    kind: "update_finished".into(),
                    title: format!("fno update failed ({})", one_line(&reason)),
                    reason: Some(one_line(&reason)),
                    ..FeedRow::default()
                });
            }
        }
    }

    // Team rows from the team events, which land in BOTH journals, so rows
    // dedupe on (ts, scope, holder, cause). The same parse feeds the owner
    // assignment below.
    let mut team_events = parse_team_events(team_raw);
    team_events.sort_by_key(|c| ts_key(&c.ts));
    // The themes render once per fold, not once per row: one tolerant store
    // read feeds every team row's title below.
    let themes = crate::paths::AgentsHome::from_env_opt()
        .map(|home| crate::team_names::theme_map(&home.team_names_json()))
        .unwrap_or_default();
    // Scope -> CURRENT team name, read once per fold beside the themes: the
    // lead column resolves at render time, never from the event-time holder.
    let lead_names = crate::paths::AgentsHome::from_env_opt()
        .map(|home| {
            crate::team_names::live_names(&home.team_names_json(), &home.registry_json())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let rank = |level: i64, scope: &str| -> String {
        let theme = themes
            .get(crate::territory::canonical_scope(scope).as_str())
            .cloned();
        crate::team_names::title(level as u32, scope, theme.as_deref())
    };
    let mut seen_teams: std::collections::HashSet<(String, String, String, String)> =
        std::collections::HashSet::new();
    for c in &team_events {
        let key = (
            c.ts.clone(),
            c.scope.clone(),
            c.holder.clone(),
            c.cause.clone().unwrap_or_default(),
        );
        if !seen_teams.insert(key) {
            continue;
        }
        match c.action {
            TeamAction::Granted => {
                let mut title = format!("{} teamed L{} {}", c.holder, c.level, c.scope);
                if let Some(from) = &c.vacated_scope {
                    title.push_str(&format!(" (moved from {from})"));
                }
                rows.push(FeedRow {
                    ts: c.ts.clone(),
                    kind: "team_granted".into(),
                    team: Some(rank(c.level, &c.scope)),
                    holder: Some(c.holder.clone()),
                    actor: c.actor.clone(),
                    title,
                    ..FeedRow::default()
                });
            }
            TeamAction::Vacated => {
                let mut title = format!(
                    "{} left L{} {}: {}",
                    c.holder,
                    c.level,
                    c.scope,
                    c.cause.clone().unwrap_or_else(|| "unknown".into())
                );
                if let Some(succ) = &c.successor {
                    title.push_str(&format!(" -> {succ}"));
                }
                rows.push(FeedRow {
                    ts: c.ts.clone(),
                    kind: "team_vacated".into(),
                    team: Some(rank(c.level, &c.scope)),
                    holder: Some(c.holder.clone()),
                    actor: c.actor.clone(),
                    title,
                    ..FeedRow::default()
                });
            }
        }
    }

    rows.sort_by(|a, b| order_key(a).cmp(&order_key(b)));

    // session -> harness, from the graph's session rows and the spawn events.
    // A row whose source names no harness reads the lane its session ran, at
    // event time; a session in neither source stays absent (no guess).
    let mut harness_by_session: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for entry in graph_entries {
        for row in entry
            .get("sessions")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            if let (Some(sid), Some(h)) = (s_field(row, "session_id"), s_field(row, "harness")) {
                harness_by_session.insert(sid, h);
            }
        }
    }
    for line in spawns_raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("agent_spawned") {
            continue;
        }
        let Some(data) = v.get("data") else { continue };
        if let (Some(sid), Some(h)) = (
            s_field(data, "harness_session_id").filter(|s| is_session_handle(s)),
            s_field(data, "harness").or_else(|| s_field(data, "provider")),
        ) {
            harness_by_session.entry(sid).or_insert(h);
        }
    }
    for r in &mut rows {
        if r.harness.is_none() {
            if let Some(sid) = r.session_id.as_deref() {
                r.harness = harness_by_session.get(sid).cloned();
            }
        }
    }

    // spawn session -> worker name, for the lead rollup of node-less rows.
    let mut spawn_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for line in spawns_raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("agent_spawned") {
            continue;
        }
        let Some(data) = v.get("data") else { continue };
        if let (Some(sid), Some(name)) = (
            s_field(data, "harness_session_id").filter(|s| is_session_handle(s)),
            s_field(data, "name"),
        ) {
            spawn_names.insert(sid, name);
        }
    }
    assign_owners(
        &mut rows,
        &team_events,
        graph_entries,
        &themes,
        &lead_names,
        &spawn_names,
    );
    for r in &mut rows {
        r.area = area_of(&r.kind).to_string();
        r.cursor = cursor_of(r);
    }
    Projection {
        rows,
        skipped_lines,
        skipped_entries,
    }
}

/// One parsed team event, used twice: for the team rows and for the owner
/// assignment's scope-to-holder timeline.
#[derive(Debug, Clone)]
struct TeamEvent {
    ts: String,
    action: TeamAction,
    holder: String,
    scope: String,
    level: i64,
    cause: Option<String>,
    successor: Option<String>,
    actor: Option<String>,
    vacated_scope: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TeamAction {
    Granted,
    Vacated,
}

fn parse_team_events(team_raw: &str) -> Vec<TeamEvent> {
    let mut out = Vec::new();
    for line in team_raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let Some(ts) = s_field(&v, "ts") else {
            continue;
        };
        match v
            .get("type")
            .and_then(Value::as_str)
            .map(crate::event_store::event_type_alias)
        {
            Some("agent_teamed") => {
                let Some(name) = s_field(&data, "name") else {
                    continue;
                };
                let Some(level) = data.get("level").and_then(Value::as_i64) else {
                    continue;
                };
                let Some(scope) = s_field(&data, "scope") else {
                    continue;
                };
                out.push(TeamEvent {
                    ts,
                    action: TeamAction::Granted,
                    holder: name.to_string(),
                    scope: scope.to_string(),
                    level,
                    cause: None,
                    successor: None,
                    actor: s_field(&data, "grantor"),
                    vacated_scope: s_field(&data, "vacated_scope"),
                });
            }
            Some("agent_team_vacated") => {
                let Some(holder) = s_field(&data, "holder") else {
                    continue;
                };
                let Some(level) = data.get("level").and_then(Value::as_i64) else {
                    continue;
                };
                let Some(scope) = s_field(&data, "scope") else {
                    continue;
                };
                out.push(TeamEvent {
                    ts,
                    action: TeamAction::Vacated,
                    holder: holder.to_string(),
                    scope: scope.to_string(),
                    level,
                    cause: s_field(&data, "cause"),
                    successor: s_field(&data, "successor"),
                    actor: None,
                    vacated_scope: None,
                });
            }
            _ => {}
        }
    }
    out
}

/// Assign `owner` to every non-team row. Team events folded in time order
/// hold a map from scope to its holder; a removal row whose `team` is set
/// clears its scope at its own ts. A row whose node, or that node's graph
/// parent, sits in a held scope gets `lead {holder} L{level}`; otherwise a
/// row whose node has a graph parent gets `epic {parent} {parent title}`.
/// The printed lead name resolves at render time through `lead_names` (the
/// live team store): a renamed or successed team shows its current name,
/// the event-time holder only when no live team covers the node.
fn assign_owners(
    rows: &mut [FeedRow],
    team_events: &[TeamEvent],
    graph_entries: &[Value],
    themes: &std::collections::BTreeMap<String, String>,
    lead_names: &std::collections::BTreeMap<String, String>,
    spawn_names: &std::collections::HashMap<String, String>,
) {
    if rows.is_empty() {
        return;
    }
    // One id -> entry map replaces a linear scan per row; the projection
    // runs on every page read, so the walk is O(entries) once, not O(rows x
    // entries).
    let by_id: std::collections::HashMap<&str, &Value> = graph_entries
        .iter()
        .filter_map(|e| graph_store::entry_id(e).map(|id| (id, e)))
        .collect();
    let parent_of =
        |node: &str| -> Option<String> { by_id.get(node).and_then(|e| s_field(e, "parent")) };
    let title_of = |node: &str| -> Option<String> {
        by_id
            .get(node)
            .and_then(|e| graph_store::s_str(e, "title"))
            .map(str::to_string)
    };
    // The scope timeline in time order: (ts_key, grant?, scope, holder, level).
    // Owned strings, because a clear entry derives from a row's team string
    // while the walk below borrows `rows` mutably.
    let mut timeline: Vec<((u8, i64), bool, String, String, i64)> = Vec::new();
    for c in team_events {
        timeline.push((
            ts_key(&c.ts),
            true,
            c.scope.clone(),
            c.holder.clone(),
            c.level,
        ));
    }
    // A removal whose team is set clears that team's scope from its own ts:
    // after the successor's removal the territory has no lead, so later rows stop
    // rolling up to it.
    for r in rows.iter() {
        if r.kind == "session_reaped" && r.team.is_some() {
            let scope = r
                .team
                .as_deref()
                .and_then(|c| c.strip_prefix("L"))
                .and_then(|rest| rest.split_once(' '))
                .map(|(_, scope)| scope.to_string());
            if let Some(scope) = scope {
                timeline.push((ts_key(&r.ts), false, scope, String::new(), 0));
            }
        }
    }
    timeline.sort_by_key(|(k, _, _, _, _)| *k);
    let mut held: Vec<(String, String, i64)> = Vec::new();
    let mut ti = 0usize;
    for r in rows.iter_mut() {
        let k = ts_key(&r.ts);
        while ti < timeline.len() && timeline[ti].0 <= k {
            let (_, grant, ref scope, ref holder, level) = timeline[ti];
            if grant {
                held.push((scope.clone(), holder.clone(), level));
            } else {
                held.retain(|(s, _, _)| s != scope);
            }
            ti += 1;
        }
        if r.kind == "team_granted"
            || r.kind == "team_vacated"
            || (r.kind == "session_reaped" && r.team.is_some())
        {
            // A team-band row carries no owner: the band IS its group.
            continue;
        }
        let parent = r.node.as_deref().and_then(parent_of);
        let lead = held.iter().find(|(scope, _, _)| {
            r.node.as_deref().is_some_and(|n| scope_holds(scope, n))
                || parent.as_deref().is_some_and(|p| scope_holds(scope, p))
        });
        if let Some((_, holder, level)) = lead {
            let scope = held
                .iter()
                .find(|(s, _, _)| {
                    r.node.as_deref().is_some_and(|n| scope_holds(s, n))
                        || parent.as_deref().is_some_and(|p| scope_holds(s, p))
                })
                .map(|(s, _, _)| s.clone())
                .unwrap_or_default();
            let theme = themes
                .get(crate::territory::canonical_scope(&scope).as_str())
                .cloned();
            let rank = crate::team_names::title(*level as u32, &scope, theme.as_deref());
            let canon = crate::territory::canonical_scope(&scope);
            // Both names: the owner string keeps the lead AT EVENT TIME, the
            // lead column resolves to the team's CURRENT name.
            r.owner = match lead_display(None, holder) {
                Some(name) => Some(format!("{rank} ({name})")),
                None => Some(rank),
            };
            r.lead = lead_display(lead_names.get(canon.as_str()), holder);
        } else if r.node.is_none() {
            // A node-less row whose parent session IS a held role holder's
            // session rolls up to that holder: the question a lead's own
            // session asked belongs to the lead it holds.
            let named = r.parent.as_deref().and_then(|p| spawn_names.get(p));
            if let Some(name) = named {
                if let Some((scope, holder, level)) = held.iter().find(|(_, h, _)| h == name) {
                    let theme = themes
                        .get(crate::territory::canonical_scope(scope).as_str())
                        .cloned();
                    let rank = crate::team_names::title(*level as u32, scope, theme.as_deref());
                    let canon = crate::territory::canonical_scope(scope);
                    r.owner = match lead_display(None, holder) {
                        Some(n) => Some(format!("{rank} ({n})")),
                        None => Some(rank),
                    };
                    r.lead = lead_display(lead_names.get(canon.as_str()), holder);
                }
            }
        } else if let Some(p) = parent {
            let title = title_of(&p).unwrap_or_default();
            r.owner = Some(if title.is_empty() {
                format!("epic {p}")
            } else {
                format!("epic {p} {title}")
            });
        }
    }
}

/// True when a team scope (comma-separated node ids) holds `node`.
fn scope_holds(scope: &str, node: &str) -> bool {
    scope.split(',').any(|seg| seg.trim() == node)
}

/// The lead name a feed row prints: `current`, the covering scope's live
/// store name (always person-shaped; `name_team` validates it), when given,
/// else the event-time holder when it is one. A dispatch slug carries
/// digits (`king-4d9b-op`) and never prints - the row keeps its rank rollup
/// without a placeholder name. `None` for `current` reads the event-time
/// spelling alone (the owner string's contract).
fn lead_display(current: Option<&String>, stored: &str) -> Option<String> {
    if let Some(name) = current {
        return Some(name.clone());
    }
    (!stored.bytes().any(|b| b.is_ascii_digit())).then(|| stored.to_string())
}

/// The filters the CLI flags express, applied after ordering: `--node`,
/// `--session`, `--kind`, `--since-epoch` (unparseable ts rows survive a since
/// filter), then the page bounds. `--before` keeps the rows strictly below
/// the cursor key and trims to the newest `limit`; `--after` keeps the rows
/// strictly above and trims to the OLDEST `limit`, so the page stays
/// contiguous with its cursor. Without a cursor `--limit` keeps the newest,
/// as it always has. Output stays ascending. Pure so the flags are testable
/// without files.
pub(crate) fn filter_rows(
    rows: Vec<FeedRow>,
    page: &Page,
    pre: &Prefilter,
    since_epoch: Option<u64>,
    until_epoch: Option<u64>,
) -> Vec<FeedRow> {
    // The parsed cursors stay in locals; the comparison keys borrow from
    // them, so a page bound costs no per-row allocation.
    let before_cur = page
        .before
        .as_deref()
        .and_then(|raw| parse_cursor(raw).ok());
    let after_cur = page.after.as_deref().and_then(|raw| parse_cursor(raw).ok());
    let before = before_cur.as_ref().map(|c| {
        (
            ts_key(&c[0]),
            c[1].as_str(),
            c[2].as_str(),
            c[3].as_str(),
            c[4].as_str(),
            c[5].as_str(),
        )
    });
    let after = after_cur.as_ref().map(|c| {
        (
            ts_key(&c[0]),
            c[1].as_str(),
            c[2].as_str(),
            c[3].as_str(),
            c[4].as_str(),
            c[5].as_str(),
        )
    });
    // One flag, comma-OR over exact segments. An empty segment never
    // matches, so a trailing comma cannot widen the filter to everything.
    let in_set = |v: &str, set: &str| {
        set.split(',')
            .filter(|seg| !seg.is_empty())
            .any(|seg| seg == v)
    };
    // One flag, comma-OR over prefix segments. An exact kind is its own
    // prefix, so today's exact `--kind` keeps matching.
    let prefix_set = |v: &str, set: &str| {
        set.split(',')
            .filter(|seg| !seg.is_empty())
            .any(|seg| v.starts_with(seg))
    };
    let mut rows: Vec<FeedRow> = rows
        .into_iter()
        .filter(|r| {
            pre.node
                .as_deref()
                .filter(|n| !n.is_empty())
                .is_none_or(|n| r.node.as_deref().is_some_and(|v| in_set(v, &n)))
        })
        .filter(|r| {
            pre.kind
                .as_deref()
                .filter(|k| !k.is_empty())
                .is_none_or(|k| prefix_set(&r.kind, &k))
        })
        .filter(|r| {
            pre.area
                .as_deref()
                .filter(|a| !a.is_empty())
                .is_none_or(|a| in_set(&r.area, &a))
        })
        .filter(|r| {
            pre.session
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_none_or(|s| {
                    r.session_id
                        .as_deref()
                        .is_some_and(|v| v.starts_with(&s) || v.ends_with(&s))
                })
        })
        .filter(|r| {
            pre.agent
                .as_deref()
                .filter(|a| !a.is_empty())
                .is_none_or(|a| r.name.as_deref().is_some_and(|v| v.starts_with(&a)))
        })
        .filter(|r| {
            pre.harness
                .as_deref()
                .filter(|h| !h.is_empty())
                .is_none_or(|h| {
                    r.harness
                        .as_deref()
                        .is_some_and(|v| h.split(',').any(|seg| v.eq_ignore_ascii_case(seg)))
                })
        })
        .filter(|r| {
            pre.lead
                .as_deref()
                .filter(|l| !l.is_empty())
                .is_none_or(|l| {
                    r.lead
                        .as_deref()
                        .or(r.owner.as_deref())
                        .is_some_and(|v| v.contains(&l))
                })
        })
        .filter(|r| match since_epoch {
            Some(since) => match chrono::DateTime::parse_from_rfc3339(&r.ts) {
                Ok(t) => t.timestamp() >= since as i64,
                Err(_) => true,
            },
            None => true,
        })
        .filter(|r| match until_epoch {
            Some(until) => match chrono::DateTime::parse_from_rfc3339(&r.ts) {
                Ok(t) => t.timestamp() <= until as i64,
                Err(_) => true,
            },
            None => true,
        })
        .filter(|r| before.is_none_or(|b| order_key(r) < b))
        .filter(|r| after.is_none_or(|a| order_key(r) > a))
        .collect();
    if let Some(limit) = page.limit {
        let keep = limit.min(rows.len());
        if page.after.is_some() {
            rows.truncate(keep);
        } else {
            let start = rows.len() - keep;
            rows.drain(0..start);
        }
    }
    rows
}

/// Which slice of the fleet a kind belongs to. A pure function of the kind,
/// so no migration ever backfills it; an unknown kind maps to `other`, and a
/// unit test fails if any kind the projection emits lands there.
pub(crate) fn area_of(kind: &str) -> &'static str {
    match kind {
        "question_asked" | "question_closed" | "decision_recorded" | "sendmessage_fallback" => {
            "mail"
        }
        "node_created" | "node_started" | "node_ended" => "backlog",
        "node_shipped" | "pr_merged" => "ship",
        "session_spawned"
        | "session_spawn_refused"
        | "session_reaped"
        | "worker_stalled"
        | "help_emitted"
        | "team_granted"
        | "team_vacated" => "agents",
        "pane_closed" | "server_stopped" | "composer_shell_ran" | "composer_shell_refused" => "mux",
        "day_boundary" | "daemon_restarted" | "update_started" | "update_finished" => "fleet",
        "main_ci_changed" => "ci",
        _ => "other",
    }
}

/// The kinds each store leg can emit, so a prefilter that excludes them all
/// skips the read entirely.
pub(crate) const QUESTION_KINDS: &[&str] = &[
    "question_asked",
    "question_closed",
    "decision_recorded",
    "day_boundary",
];
pub(crate) const GRAPH_KINDS: &[&str] = &[
    "node_created",
    "node_started",
    "node_ended",
    "node_shipped",
    "pr_merged",
];
pub(crate) const REMOVAL_KINDS: &[&str] = &["session_reaped"];
/// The feed kinds the main store leg derives (its store kinds are
/// spawn_gate_refused, worker_silent, blocked and the fno_update_* trio).
pub(crate) const MAIN_KINDS: &[&str] = &[
    "session_spawn_refused",
    "worker_stalled",
    "help_emitted",
    "update_started",
    "update_finished",
];
pub(crate) const SPAWN_KINDS: &[&str] = &["session_spawned", "session_spawn_refused"];
pub(crate) const CLOSE_KINDS: &[&str] = &[
    "pane_closed",
    "server_stopped",
    "composer_shell_ran",
    "composer_shell_refused",
    "daemon_restarted",
];
pub(crate) const TEAM_KINDS: &[&str] = &["team_granted", "team_vacated"];

/// True when the prefilter cannot match any kind this leg emits, so the leg
/// is not read at all. An empty leg is never skipped.
pub(crate) fn leg_skipped(pre: &Prefilter, kinds: &[&str]) -> bool {
    if kinds.is_empty() {
        return false;
    }
    if let Some(k) = pre.kind.as_deref() {
        let wanted: Vec<&str> = k.split(',').filter(|s| !s.is_empty()).collect();
        if !kinds
            .iter()
            .any(|kd| wanted.iter().any(|w| kd.starts_with(w)))
        {
            return true;
        }
    }
    if let Some(a) = pre.area.as_deref() {
        let wanted: Vec<&str> = a.split(',').filter(|s| !s.is_empty()).collect();
        if !kinds
            .iter()
            .any(|kd| wanted.iter().any(|w| area_of(kd) == *w))
        {
            return true;
        }
    }
    false
}

/// One keyset page request: the cursor bounds and the size. `--before` keeps
/// the rows strictly below the cursor key, newest `limit` of them; `--after`
/// keeps the rows strictly above, OLDEST `limit`. Both at once is refused.
#[derive(Debug, Default)]
pub(crate) struct Page {
    before: Option<String>,
    after: Option<String>,
    limit: Option<usize>,
}

/// The flat WHERE clause the client may send: every flag ANDs, and the
/// values inside one flag are comma-OR. No grammar, no negation, no OR
/// across flags: the client sends only what one positive group can say.
#[derive(Debug, Default, Clone)]
pub(crate) struct Prefilter {
    /// Exact node ids, comma-OR.
    node: Option<String>,
    /// Kind prefixes, comma-OR.
    kind: Option<String>,
    /// Area names, exact, comma-OR.
    area: Option<String>,
    /// Session id prefix or tail.
    session: Option<String>,
    /// Worker name prefix.
    agent: Option<String>,
    /// Harness names, case-insensitive, comma-OR.
    harness: Option<String>,
    /// Substring of the lead, else the owner.
    lead: Option<String>,
}

#[derive(Debug)]
struct FeedArgs {
    json: bool,
    since_epoch: Option<u64>,
    until_epoch: Option<u64>,
    page: Page,
    pre: Prefilter,
}

fn parse_args(rest: &[String]) -> Result<FeedArgs, String> {
    let mut args = FeedArgs {
        json: false,
        since_epoch: None,
        until_epoch: None,
        page: Page::default(),
        pre: Prefilter::default(),
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
                args.page.limit = Some(
                    it.next()
                        .and_then(|v| v.parse::<usize>().ok())
                        .ok_or("--limit needs a positive integer")?,
                )
            }
            "--before" => {
                let raw = it.next().ok_or("--before needs a cursor")?;
                parse_cursor(&raw).map_err(|e| format!("--before: {e}"))?;
                args.page.before = Some(raw);
            }
            "--after" => {
                let raw = it.next().ok_or("--after needs a cursor")?;
                parse_cursor(&raw).map_err(|e| format!("--after: {e}"))?;
                args.page.after = Some(raw);
            }
            "--until-epoch" => {
                args.until_epoch = Some(
                    it.next()
                        .and_then(|v| v.parse::<u64>().ok())
                        .ok_or("--until-epoch needs a non-negative integer")?,
                )
            }
            "--node" => args.pre.node = Some(it.next().ok_or("--node needs an id")?),
            "--session" => args.pre.session = Some(it.next().ok_or("--session needs an id")?),
            "--kind" => args.pre.kind = Some(it.next().ok_or("--kind needs a kind name")?),
            "--area" => args.pre.area = Some(it.next().ok_or("--area needs a name")?),
            "--agent" => args.pre.agent = Some(it.next().ok_or("--agent needs a name")?),
            "--harness" => args.pre.harness = Some(it.next().ok_or("--harness needs a name")?),
            "--lead" => args.pre.lead = Some(it.next().ok_or("--lead needs a name")?),
            other => return Err(format!("unknown feed flag: {other}")),
        }
    }
    if args.page.before.is_some() && args.page.after.is_some() {
        return Err("--before and --after cannot combine: page from one edge".into());
    }
    let live = |v: Option<String>| v.filter(|s| !s.is_empty());
    args.pre.node = live(args.pre.node);
    args.pre.kind = live(args.pre.kind);
    args.pre.area = live(args.pre.area);
    args.pre.session = live(args.pre.session);
    args.pre.agent = live(args.pre.agent);
    args.pre.harness = live(args.pre.harness);
    args.pre.lead = live(args.pre.lead);
    Ok(args)
}

/// The store path, resolved as the fno crate's `backlog_view::graph_path`
/// does: `FNO_GRAPH_JSON` > `$HOME/.fno/graph.json` (the agents home's parent,
/// so a test home redirects it too), through the layout table.
pub(crate) fn graph_path(home: &AgentsHome) -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_GRAPH_JSON") {
        return PathBuf::from(v);
    }
    let root = home
        .root()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    crate::state_layout::place(&root, "graph.json")
}

/// The agents journal's text filtered to `types`, bounded by the CLI's
/// `since_epoch` (seconds), or empty when the store is unreadable (the
/// removal leg's notes name the store when THAT store fails; the spawn leg
/// degrades to no rows, in step with the questions leg's posture).
fn agents_journal(
    home: &AgentsHome,
    types: &[&str],
    since_epoch: Option<u64>,
    until_epoch: Option<u64>,
) -> String {
    let query = crate::event_store::EventQuery {
        since_ms: since_epoch.map(|s| s as i64 * 1000),
        until_ms: until_epoch.map(|u| u as i64 * 1000),
        ..crate::event_store::EventQuery::of_types(types)
    };
    crate::event_store::journal_text_checked(&home.events_jsonl(), &query).unwrap_or_default()
}

/// The team events, concatenated from BOTH journals so a team row dedupes
/// on (ts, scope, holder, cause) instead of depending on which store an
/// emitter wrote. Unbounded by the window: a team is granted before the
/// events it owns.
fn team_journals(home: &AgentsHome) -> String {
    let types = ["agent_teamed", "agent_team_vacated"];
    let mut raw = agents_journal(home, &types, None, None);
    if let Some(parent) = home.root().parent() {
        let path = parent.join("events.jsonl");
        if let Ok(text) = crate::event_store::journal_text_checked(
            &path,
            &crate::event_store::EventQuery::of_types(&types),
        ) {
            raw.push('\n');
            raw.push_str(&text);
        }
    }
    raw
}

/// The main store's typed read for the coverage kinds. `events.jsonl` routes
/// to db/events.db, the durable store; a store neither present nor readable
/// notes its skip once and yields no rows (AC7-EDGE).
fn main_store_journal(
    home: &AgentsHome,
    since_epoch: Option<u64>,
    until_epoch: Option<u64>,
) -> (String, Option<String>) {
    let Some(parent) = home.root().parent() else {
        return (String::new(), None);
    };
    let path = parent.join("events.jsonl");
    if !path.is_file() && !crate::event_store::store_path(&path).is_file() {
        return (
            String::new(),
            Some(format!("main store skipped (absent): {}", path.display())),
        );
    }
    match crate::event_store::journal_text_checked(
        &path,
        &crate::event_store::EventQuery {
            since_ms: since_epoch.map(|s| s as i64 * 1000),
            until_ms: until_epoch.map(|u| u as i64 * 1000),
            ..crate::event_store::EventQuery::of_types(&[
                "spawn_gate_refused",
                "worker_silent",
                "blocked",
                "fno_update_started",
                "fno_update_installed",
                "fno_update_failed",
            ])
        },
    ) {
        Ok(text) => (text, None),
        Err(e) => (String::new(), Some(format!("main store skipped ({e})"))),
    }
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
    let questions_path = fno_dir.join("questions.jsonl");
    // Typed read: the four question kinds plus the empty type (corrupt rows
    // still count). The store's other kinds, 119k+ attention_delivery rows
    // alone, never reach the parser, so they stop reading as malformed.
    let questions_query = crate::event_store::EventQuery {
        since_ms: args.since_epoch.map(|s| s as i64 * 1000),
        until_ms: args.until_epoch.map(|u| u as i64 * 1000),
        ..crate::event_store::EventQuery::of_types(&[
            "operator_question",
            "operator_question_closed",
            "operator_decision",
            "day_boundary",
        ])
    };
    let questions_raw = if leg_skipped(&args.pre, QUESTION_KINDS) {
        String::new()
    } else {
        match crate::event_store::journal_text_checked(&questions_path, &questions_query) {
            Ok(raw) => raw,
            Err(e) => {
                eprintln!(
                    "fno-agents feed: questions store unreadable, skipped: {} ({e})",
                    questions_path.display()
                );
                String::new()
            }
        }
    };

    let (graph_entries, graph_note): (Vec<Value>, Option<String>) = {
        // The lifecycle leg reads the store, never the file. An absent store
        // is still a skipped store for the feed's purposes: the operator
        // should know the lifecycle leg is absent, so its absence keeps its
        // own note (AC3) where a present-but-empty store is just empty.
        let path = graph_path(home);
        if leg_skipped(&args.pre, GRAPH_KINDS) {
            (Vec::new(), None)
        } else if !crate::backlog::database_path(&path).exists() {
            (Vec::new(), Some("graph store skipped (absent)".to_string()))
        } else {
            match crate::graph_store::read_rows_where(
                &path,
                &crate::backlog::RowQuery {
                    fields: Some(
                        [
                            "id",
                            "title",
                            "parent",
                            "cwd",
                            "sessions",
                            "created_at",
                            "completed_at",
                            "status",
                            "merge_status",
                            "pr_number",
                            "pr_url",
                            "source_session_id",
                            "source_harness",
                            "source_model",
                            "source_effort",
                            "source_parent_session",
                            "source_team",
                        ]
                        .into_iter()
                        .map(str::to_string)
                        .collect(),
                    ),
                    with_blockers: true,
                    ..Default::default()
                },
            )
            .map_err(|error| crate::backlog::api::ApiError(error.to_string()))
            {
                Ok(list) => (list, None),
                Err(e) => (Vec::new(), Some(format!("graph store skipped ({})", e.0))),
            }
        }
    };
    if let Some(note) = graph_note {
        eprintln!("fno-agents feed: {note}");
    }

    let (removals, removal_notes) = if leg_skipped(&args.pre, REMOVAL_KINDS) {
        (Vec::new(), Vec::new())
    } else {
        crate::removals::read(home, args.since_epoch.map(|s| s as i64 * 1000))
    };
    for note in &removal_notes {
        eprintln!("fno-agents feed: {note}");
    }
    let spawns_raw = if leg_skipped(&args.pre, SPAWN_KINDS) {
        String::new()
    } else {
        agents_journal(
            home,
            &["agent_spawned", "agent_spawn_refused"],
            args.since_epoch,
            args.until_epoch,
        )
    };
    let team_raw = if leg_skipped(&args.pre, TEAM_KINDS) {
        String::new()
    } else {
        team_journals(home)
    };
    let closes_raw = if leg_skipped(&args.pre, CLOSE_KINDS) {
        String::new()
    } else {
        agents_journal(
            home,
            &[
                "pane_closed",
                "server_stopped",
                "composer_shell_ran",
                "composer_shell_refused",
                "daemon_started",
            ],
            args.since_epoch,
            args.until_epoch,
        )
    };
    let (main_raw, main_note) = if leg_skipped(&args.pre, MAIN_KINDS) {
        (String::new(), None)
    } else {
        main_store_journal(home, args.since_epoch, args.until_epoch)
    };
    if let Some(note) = main_note {
        eprintln!("fno-agents feed: {note}");
    }

    // The projection sees the node set so the graph leg can skip whole
    // entries; the other legs filter after projection (node_id is sparse in
    // the stores).
    let node_set: Option<Vec<String>> = args
        .pre
        .node
        .as_deref()
        .map(|s| s.split(',').map(str::to_string).collect());
    let Projection {
        rows,
        skipped_lines,
        skipped_entries,
    } = project(
        &questions_raw,
        &graph_entries,
        &removals,
        &spawns_raw,
        &team_raw,
        &closes_raw,
        &main_raw,
        node_set.as_deref(),
    );
    if skipped_lines > 0 {
        eprintln!("fno-agents feed: skipped {skipped_lines} malformed question line(s)");
    }
    if skipped_entries > 0 {
        eprintln!("fno-agents feed: skipped {skipped_entries} non-object graph entr(ies)");
    }

    let mut page = args.page;
    // A page the caller did not size is the head page: the newest 200.
    page.limit = page.limit.or(Some(200));
    let rows = filter_rows(rows, &page, &args.pre, args.since_epoch, args.until_epoch);

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

    // The nd-aaaa shape: a blueprint row with only ended_at, a do row and a
    // ship row each with started_at, on a node carrying pr_number and
    // completed_at.
    fn graph_fixture() -> Vec<Value> {
        vec![serde_json::json!({
            "id": "nd-aaaa",
            "status": "done",
            "title": "feed marker node",
            "pr_number": 1395,
            "pr_url": "https://github.com/bllshttng/footnote/pull/1395",
            "created_at": "2026-09-01T08:00:00Z",
            "cwd": "/workspace/node-project",
            "source_session_id": "s-do",
            "source_harness": "claude",
            "source_model": "claude-opus-5",
            "source_effort": "high",
            "source_parent_session": "s-parent",
            "source_team": "L2 e-0001",
            "completed_at": "2026-09-05T16:41:25Z",
            "sessions": [
                {"phase": "blueprint", "harness": "claude", "session_id": "s-blue",
                 "ended_at": "2026-09-02T16:00:00Z"},
                {"phase": "execute", "harness": "claude", "session_id": "s-do",
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
            r#"{"ts":"2026-09-02T17:00:00Z","type":"operator_question","source":"target","data":{"question_id":"q-1","question":"line one\nline two","session_id":"s-ask","node":"nd-aaaa"}}"#,
            r#"{"ts":"2026-09-02T19:00:00Z","type":"operator_question_closed","source":"operator","data":{"question_id":"q-1","answer":"ruling: yes\ndo it","closed_by":"s-op"}}"#,
            r#"{"ts":"2026-09-03T09:00:00Z","type":"operator_decision","source":"operator","data":{"decision_id":"d-1","decision":"strict equality stands","subject":"revert-dispute","decided_by":"s-op"}}"#,
        ]
        .join("\n")
    }

    fn kinds(rows: &[FeedRow]) -> Vec<&str> {
        rows.iter().map(|r| r.kind.as_str()).collect()
    }

    #[test]
    fn life_rows() {
        let p = project("", &graph_fixture(), &[], "", "", "", "", None);
        assert_eq!(
            kinds(&p.rows),
            ["node_created", "node_started", "node_shipped", "node_ended"]
        );
        let started = &p.rows[1];
        assert_eq!(started.node.as_deref(), Some("nd-aaaa"));
        assert_eq!(started.session_id.as_deref(), Some("s-do"));
        let pr = &p.rows[2];
        assert_eq!(pr.session_id.as_deref(), Some("s-ship"));
        assert_eq!(pr.r#ref.as_deref(), Some("1395"));
        let ended = &p.rows[3];
        assert_eq!(ended.session_id.as_deref(), Some("s-ship"));
        assert_eq!(ended.title, "done");

        // The marker: a projection fed only an events-style stream yields none
        // of the three lifecycle rows - they derive from the graph and nowhere
        // else.
        let p = project(&questions_fixture(), &[], &[], "", "", "", "", None);
        assert_eq!(
            kinds(&p.rows),
            ["question_asked", "question_closed", "decision_recorded"]
        );

        let questions = r#"{"ts":"2026-09-13T08:00:00Z","type":"day_boundary","source":"operator","data":{"kind":"start","boundary_id":"day-start-20260913-ab12"}}"#;
        let p = project(questions, &[], &[], "", "", "", "", None);
        assert_eq!(p.skipped_lines, 0);
        assert_eq!(kinds(&p.rows), ["day_boundary"]);
        assert_eq!(p.rows[0].title, "day start");
        assert_eq!(p.rows[0].r#ref.as_deref(), Some("day-start-20260913-ab12"));

        let p = project(
            &questions_fixture(),
            &graph_fixture(),
            &[],
            "",
            "",
            "",
            "",
            None,
        );
        let asked = p.rows.iter().find(|r| r.kind == "question_asked").unwrap();
        assert_eq!(asked.r#ref.as_deref(), Some("q-1"));
        assert_eq!(asked.session_id.as_deref(), Some("s-ask"));
        assert_eq!(asked.node.as_deref(), Some("nd-aaaa"));
        assert_eq!(asked.title, "line one");
        let closed = p.rows.iter().find(|r| r.kind == "question_closed").unwrap();
        assert_eq!(closed.r#ref.as_deref(), Some("q-1"));
        // `s-op` is not a session handle, so it is the ACTOR and the closure
        // offers no attach target. The node comes from the asking row.
        assert_eq!(closed.session_id, None);
        assert_eq!(closed.actor.as_deref(), Some("s-op"));
        assert_eq!(closed.node.as_deref(), Some("nd-aaaa"));
        assert_eq!(closed.title, "ruling: yes");
        let decision = p
            .rows
            .iter()
            .find(|r| r.kind == "decision_recorded")
            .unwrap();
        assert_eq!(decision.r#ref.as_deref(), Some("d-1"));
        assert_eq!(decision.title, "revert-dispute: strict equality stands");

        let p = project(
            &questions_fixture(),
            &graph_fixture(),
            &[],
            "",
            "",
            "",
            "",
            None,
        );
        assert_eq!(
            kinds(&p.rows),
            [
                "node_created",      // 09-01 08:00
                "question_asked",    // 09-02 17:00
                "node_started",      // 09-02 17:12
                "node_shipped",      // 09-02 18:27
                "question_closed",   // 09-02 19:00
                "decision_recorded", // 09-03 09:00
                "node_ended",        // 09-05 16:41
            ]
        );

        let questions =
            "not json\n{\"ts\":\"2026-09-02T17:00:00Z\",\"type\":\"other\",\"data\":{}}\n"
                .to_string()
                + &questions_fixture();
        let mut entries = vec![serde_json::json!("a bare string")];
        entries.extend(graph_fixture());
        let p = project(&questions, &entries, &[], "", "", "", "", None);
        assert_eq!(p.skipped_lines, 2);
        assert_eq!(p.skipped_entries, 1);
        assert!(p.rows.iter().all(|r| matches!(
            r.kind.as_str(),
            "question_asked"
                | "question_closed"
                | "decision_recorded"
                | "node_created"
                | "node_started"
                | "node_shipped"
                | "node_ended"
                | "session_reaped"
        )));

        let questions = r#"{"ts":"yesterday-ish","type":"operator_question","source":"t","data":{"question_id":"q-0","question":"odd stamp","session_id":"s-x"}}"#.to_string();
        let p = project(&questions, &[], &[], "", "", "", "", None);
        assert_eq!(kinds(&p.rows)[0], "question_asked");
        assert_eq!(p.rows[0].ts, "yesterday-ish");
        let kept = filter_rows(
            p.rows,
            &Page::default(),
            &Prefilter::default(),
            Some(1_700_000_000),
            None,
        );
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn filter_gate_rows() {
        let p = project(
            &questions_fixture(),
            &graph_fixture(),
            &[],
            "",
            "",
            "",
            "",
            None,
        );
        let node_rows = filter_rows(
            p.rows.clone(),
            &Page::default(),
            &Prefilter {
                node: Some("nd-aaaa".into()),
                ..Default::default()
            },
            None,
            None,
        );
        // The fixture question carries node nd-aaaa, so a node filter keeps it
        // alongside the lifecycle rows - and its CLOSURE now too, because the
        // closure inherits the association from the row that asked.
        assert_eq!(
            kinds(&node_rows),
            [
                "node_created",
                "question_asked",
                "node_started",
                "node_shipped",
                "question_closed",
                "node_ended"
            ]
        );
        let ship_rows = filter_rows(
            p.rows.clone(),
            &Page::default(),
            &Prefilter {
                session: Some("s-ship".into()),
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(kinds(&ship_rows), ["node_shipped", "node_ended"]);
        let shipped = ship_rows.iter().find(|r| r.kind == "node_shipped").unwrap();
        assert_eq!(
            shipped.url.as_deref(),
            Some("https://github.com/bllshttng/footnote/pull/1395"),
            "the ship row carries the PR URL the provenance action opens"
        );
        let newest_two = filter_rows(
            p.rows,
            &Page {
                limit: Some(2),
                ..Page::default()
            },
            &Prefilter::default(),
            None,
            None,
        );
        assert_eq!(kinds(&newest_two), ["decision_recorded", "node_ended"]);

        // The live shape: `decided_by` is the literal verb on every decision
        // row, and `closed_by` is a mechanism name on most closure rows.
        let questions = [
            r#"{"ts":"2026-09-02T17:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-1","question":"ask","node":"x-1111"}}"#,
            r#"{"ts":"2026-09-02T19:00:00Z","type":"operator_question_closed","source":"d","data":{"question_id":"q-1","answer":"superseded","closed_by":"stale-escalate"}}"#,
            r#"{"ts":"2026-09-03T09:00:00Z","type":"operator_decision","source":"d","data":{"decision_id":"d-1","decision":"stands","subject":"s","question_id":"q-1","decided_by":"fno agents stale-escalate"}}"#,
        ]
        .join("\n");
        let p = project(&questions, &[], &[], "", "", "", "", None);
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

        let questions = [
            r#"{"ts":"2026-09-02T17:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-1","question":"ask"}}"#,
            r#"{"ts":"2026-09-02T19:00:00Z","type":"operator_question_closed","source":"d","data":{"question_id":"q-1","answer":"yes","closed_by":"20260904T151442Z-cl54345-58af0c"}}"#,
        ]
        .join("\n");
        let p = project(&questions, &[], &[], "", "", "", "", None);
        let closed = p.rows.iter().find(|r| r.kind == "question_closed").unwrap();
        assert_eq!(
            closed.session_id.as_deref(),
            Some("20260904T151442Z-cl54345-58af0c")
        );
        assert_eq!(closed.actor, None);

        // Keyset pages: 450 rows, 20 sharing each ts stamp, pages of 200
        // chained by --before concatenate to the whole set in order with no
        // row read twice or skipped.
        let mut rows = Vec::new();
        for i in 0..450usize {
            rows.push(FeedRow {
                ts: format!("2026-09-01T00:{:02}:00Z", i / 20),
                kind: format!("k{i:03}"),
                title: format!("row {i}"),
                ..FeedRow::default()
            });
        }
        rows.sort_by(|a, b| order_key(a).cmp(&order_key(b)));
        for r in &mut rows {
            r.cursor = cursor_of(r);
        }
        let back = parse_cursor(&rows[0].cursor).unwrap();
        assert_eq!(back[1], rows[0].kind, "cursor round-trips the key strings");
        let mut before: Option<String> = None;
        let mut chain: Vec<Vec<FeedRow>> = Vec::new();
        loop {
            let got = filter_rows(
                rows.clone(),
                &Page {
                    before: before.clone(),
                    limit: Some(200),
                    ..Page::default()
                },
                &Prefilter::default(),
                None,
                None,
            );
            assert!(!got.is_empty(), "page chain ended empty");
            chain.push(got);
            before = Some(chain.last().unwrap()[0].cursor.clone());
            if chain.last().unwrap().len() < 200 {
                break;
            }
        }
        assert_eq!(chain.len(), 3, "450 rows page into three");
        // The chain runs newest page first and every page is internally
        // ascending; the client prepends each older page, so the window is
        // the chain walked oldest page first.
        let all: Vec<FeedRow> = chain.iter().rev().flatten().cloned().collect();
        assert_eq!(all.len(), 450, "no row read twice or skipped");
        for (i, r) in all.iter().enumerate() {
            assert_eq!(r.kind, rows[i].kind, "row {i} out of order");
        }

        // --after keeps the rows strictly above the cursor, trimmed to the
        // OLDEST limit, so the page stays contiguous with its cursor.
        let got = filter_rows(
            rows.clone(),
            &Page {
                after: Some(rows[10].cursor.clone()),
                limit: Some(200),
                ..Page::default()
            },
            &Prefilter::default(),
            None,
            None,
        );
        assert_eq!(got.len(), 200);
        assert_eq!(
            got[0].kind, rows[11].kind,
            "after starts just past the cursor"
        );

        // Both cursors at once is refused; an unparseable cursor names its flag.
        let err = parse_args(&[
            "--before".into(),
            r#"["a","b","c","d","e","f"]"#.into(),
            "--after".into(),
            r#"["a","b","c","d","e","f"]"#.into(),
        ])
        .unwrap_err();
        assert!(err.contains("--before") && err.contains("--after"), "{err}");
        let err = parse_args(&["--before".into(), "not json".into()]).unwrap_err();
        assert!(err.starts_with("--before"), "{err}");
        let err = parse_args(&["--after".into(), "[1,2]".into()]).unwrap_err();
        assert!(err.starts_with("--after"), "{err}");

        // AC3-HP: the flat flags AND together; kinds match by prefix, and a
        // page bound still applies under a filter.
        let mk = |ts: &str, kind: &str, harness: Option<&str>| FeedRow {
            ts: ts.to_string(),
            kind: kind.to_string(),
            harness: harness.map(str::to_string),
            title: format!("{kind} title"),
            ..FeedRow::default()
        };
        let mut rows = vec![
            mk("2026-09-02T17:00:00Z", "question_asked", Some("claude")),
            mk("2026-09-02T18:00:00Z", "question_closed", Some("codex")),
            mk("2026-09-02T19:00:00Z", "node_started", Some("codex")),
            mk("2026-09-02T20:00:00Z", "session_spawned", None),
        ];
        rows.sort_by(|a, b| order_key(a).cmp(&order_key(b)));
        for r in &mut rows {
            r.cursor = cursor_of(r);
        }
        let pre = Prefilter {
            harness: Some("codex".into()),
            kind: Some("question".into()),
            ..Default::default()
        };
        let got = filter_rows(rows.clone(), &Page::default(), &pre, None, None);
        assert_eq!(kinds(&got), ["question_closed"]);
        let got = filter_rows(
            rows.clone(),
            &Page {
                before: Some(rows[3].cursor.clone()),
                ..Default::default()
            },
            &pre,
            None,
            None,
        );
        assert_eq!(kinds(&got), ["question_closed"], "filter still pages");
        let got = filter_rows(
            rows.clone(),
            &Page {
                before: Some(rows[1].cursor.clone()),
                ..Default::default()
            },
            &pre,
            None,
            None,
        );
        assert!(got.is_empty(), "the bound cuts under the filter too");

        // AC3-EDGE: an empty value is ignored, an unknown area answers empty
        // (an empty answer, not an error), and a harness set is
        // case-insensitive.
        let got = filter_rows(
            rows.clone(),
            &Page::default(),
            &Prefilter {
                kind: Some(String::new()),
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(got.len(), 4, "empty --kind filters nothing");
        let got = filter_rows(
            rows.clone(),
            &Page::default(),
            &Prefilter {
                area: Some("nosuch".into()),
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(got.len(), 0, "unknown area matches no row");
        let got = filter_rows(
            rows.clone(),
            &Page::default(),
            &Prefilter {
                harness: Some("CODEX,pi".into()),
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(kinds(&got), ["question_closed", "node_started"]);
        let until_got = filter_rows(
            rows.clone(),
            &Page::default(),
            &Prefilter::default(),
            None,
            Some(1_788_372_000), /* 2026-09-02T18:00:00Z */
        );
        assert_eq!(
            kinds(&until_got),
            ["question_asked", "question_closed"],
            "--until-epoch bounds the newer end"
        );
        // AC4-HP (projection half): the graph leg skips entries outside the
        // node set before deriving; the other legs filter after projection.
        let entries = vec![
            serde_json::json!({"id": "nd-aaaa", "title": "a", "created_at": "2026-09-02T08:00:00Z"}),
            serde_json::json!({"id": "nd-bbbb", "title": "b", "created_at": "2026-09-02T09:00:00Z"}),
        ];
        let spawns = r#"{"ts":"2026-09-02T10:00:00Z","type":"agent_spawned","source":"python","data":{"node":"nd-bbbb","name":"w","substrate":"pane"}}"#;
        let set = vec!["nd-aaaa".to_string()];
        let p = project(
            &questions_fixture(),
            &entries,
            &[],
            spawns,
            "",
            "",
            "",
            Some(&set),
        );
        assert!(
            !p.rows
                .iter()
                .any(|r| r.kind == "node_created" && r.node.as_deref() == Some("nd-bbbb")),
            "a filtered entry derives no rows"
        );
        let pre = Prefilter {
            node: Some("nd-aaaa".into()),
            ..Default::default()
        };
        let got = filter_rows(p.rows, &Page::default(), &pre, None, None);
        assert!(
            got.iter().all(|r| r.node.as_deref() != Some("nd-bbbb")),
            "legs without a store node filter after projection"
        );
        assert!(got.iter().any(|r| r.node.as_deref() == Some("nd-aaaa")));

        // AC5: area is a pure function of kind, and nothing the projection
        // emits lands in `other`.
        for k in [
            "question_asked",
            "question_closed",
            "decision_recorded",
            "node_created",
            "node_started",
            "node_ended",
            "node_shipped",
            "session_spawned",
            "session_spawn_refused",
            "session_reaped",
            "worker_stalled",
            "help_emitted",
            "team_granted",
            "team_vacated",
            "pane_closed",
            "server_stopped",
            "composer_shell_ran",
            "composer_shell_refused",
            "day_boundary",
            "daemon_restarted",
            "update_started",
            "update_finished",
            "main_ci_changed",
            "pr_merged",
        ] {
            assert_ne!(area_of(k), "other", "{k} has no area");
        }
        assert_eq!(area_of("question_asked"), "mail");
        assert_eq!(area_of("node_shipped"), "ship");
        assert_eq!(area_of("pane_closed"), "mux");
        let projected = project(
            &questions_fixture(),
            &graph_fixture(),
            &[],
            "",
            "",
            "",
            "",
            None,
        );
        assert!(
            projected
                .rows
                .iter()
                .all(|r| area_of(&r.kind) == r.area && r.area != "other"),
            "every projected row carries its area"
        );

        // A prefilter that excludes a leg's kinds skips the read entirely.
        let pre = Prefilter {
            area: Some("mux,backlog".into()),
            ..Default::default()
        };
        assert!(
            leg_skipped(&pre, QUESTION_KINDS),
            "questions leg excluded by --area mux,backlog"
        );
        assert!(
            !leg_skipped(&pre, GRAPH_KINDS),
            "the graph leg emits backlog kinds, so it stays"
        );
        let pre = Prefilter {
            kind: Some("question".into()),
            ..Default::default()
        };
        assert!(leg_skipped(&pre, SPAWN_KINDS));
        assert!(!leg_skipped(&pre, QUESTION_KINDS));
        assert!(!leg_skipped(&Prefilter::default(), QUESTION_KINDS));

        // AC6-HP: a question row whose session spawned through a codex
        // agent_spawned reads harness codex, and --harness codex returns it.
        let spawns = r#"{"ts":"2026-09-02T16:00:00Z","type":"agent_spawned","source":"python","data":{"name":"w","provider":null,"harness":"codex","harness_session_id":"00847995-e0db-47c2-ab5b-24468ba1a4f5","substrate":"pane","spawned_by_session":"s-op"}}"#;
        let questions = r#"{"ts":"2026-09-02T17:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-h","question":"ask","session_id":"00847995-e0db-47c2-ab5b-24468ba1a4f5","node":"nd-aaaa"}}"#;
        let p = project(questions, &[], &[], spawns, "", "", "", None);
        let asked = p.rows.iter().find(|r| r.kind == "question_asked").unwrap();
        assert_eq!(asked.harness.as_deref(), Some("codex"));
        let pre = Prefilter {
            harness: Some("codex".into()),
            ..Default::default()
        };
        let got = filter_rows(p.rows, &Page::default(), &pre, None, None);
        assert!(
            got.iter().any(|r| r.kind == "question_asked"),
            "the question row returns under --harness codex"
        );

        // AC6-EDGE: a session in no spawn or graph row stays absent, and the
        // harness filter does not guess it in.
        let questions = r#"{"ts":"2026-09-02T17:00:00Z","type":"operator_question","source":"t","data":{"question_id":"q-h","question":"ask","session_id":"s-orphan","node":"nd-aaaa"}}"#;
        let p = project(questions, &[], &[], "", "", "", "", None);
        let asked = p.rows.iter().find(|r| r.kind == "question_asked").unwrap();
        assert_eq!(asked.harness, None);
        let got = filter_rows(
            p.rows,
            &Page::default(),
            &Prefilter {
                harness: Some("codex".into()),
                ..Default::default()
            },
            None,
            None,
        );
        assert!(got.is_empty(), "no harness, no return");

        // The lead route: a node-less row whose parent session is a held
        // role holder's spawn rolls up to that holder; the node route keeps
        // setting lead beside owner.
        let team = r#"{"ts":"2026-09-02T15:30:00Z","type":"agent_teamed","source":"python","data":{"grantor":"s-lead","level":2,"name":"successor","scope":"nd-aaaa"}}"#;
        let spawns = concat!(
            r#"{"ts":"2026-09-02T16:00:00Z","type":"agent_spawned","source":"python","data":{"name":"successor","harness":"claude","harness_session_id":"20260904T151442Z-cl54345-58af0c","substrate":"pane","spawned_by_session":"s-op"}}"#,
            "\n",
            r#"{"ts":"2026-09-02T17:30:00Z","type":"agent_spawned","source":"python","data":{"name":"w1","harness":"codex","harness_session_id":"00847995-e0db-47c2-ab5b-24468ba1a4f5","substrate":"pane","spawned_by_session":"20260904T151442Z-cl54345-58af0c"}}"#,
        );
        let p = project("", &[], &[], spawns, team, "", "", None);
        let child = p
            .rows
            .iter()
            .find(|r| r.session_id.as_deref() == Some("00847995-e0db-47c2-ab5b-24468ba1a4f5"))
            .unwrap();
        assert_eq!(child.lead.as_deref(), Some("successor"));
        assert_eq!(child.owner.as_deref(), Some("Lead of nd-aaaa (successor)"));
        let holder_row = p
            .rows
            .iter()
            .find(|r| r.session_id.as_deref() == Some("20260904T151442Z-cl54345-58af0c"))
            .unwrap();
        assert_eq!(
            holder_row.lead, None,
            "the holder's own birth rolls to nobody"
        );

        // AC7-HP: one row of each new main-store type projects its kind and
        // area, and a merged node reads pr_merged instead of node_ended.
        let main = concat!(
            r#"{"ts":"2026-09-30T10:00:00Z","type":"spawn_gate_refused","source":"agents","data":{"reason":"lead_share","name":"t-w-glm","substrate":"bg","gate":"python"}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:01:00Z","type":"worker_silent","source":"daemon","data":{"handle":"t-w","harness":"claude","age_s":802,"deadline_s":600,"node":null}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:02:00Z","type":"blocked","source":"target","data":{"reason":"need a lever","kind":"help"},"node":"nd-aaaa","harness":"claude"}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:03:00Z","type":"fno_update_started","source":"python","data":{"new_rev":"55f04eda70a99c3f4e72a3cfe62e80d219c66d6f"}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:04:00Z","type":"fno_update_installed","source":"python","data":{"new_rev":"55f04eda70a99c3f4e72a3cfe62e80d219c66d6f"}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:05:00Z","type":"fno_update_failed","source":"python","data":{"rc":"1","stage":"install","reason":"install exited 1"}}"#,
        );
        let daemon = r#"{"ts":"2026-09-30T10:06:00Z","type":"daemon_started","source":"daemon","data":{"pid":15714,"version":"0.4.1","recovery_mode":"preserve"}}"#;
        let p = project("", &[], &[], "", "", daemon, main, None);
        let find = |k: &str| {
            p.rows
                .iter()
                .find(|r| r.kind == k)
                .unwrap_or_else(|| panic!("no {k} row"))
        };
        assert_eq!(
            find("session_spawn_refused").title,
            "spawn refused (lead_share): t-w-glm"
        );
        assert_eq!(
            find("session_spawn_refused").reason.as_deref(),
            Some("lead_share")
        );
        assert_eq!(find("worker_stalled").title, "t-w silent 802s");
        assert_eq!(find("worker_stalled").area, "agents");
        assert_eq!(find("help_emitted").node.as_deref(), Some("nd-aaaa"));
        assert_eq!(find("help_emitted").title, "help: need a lever");
        assert_eq!(
            find("update_started").title,
            "fno update started (55f04eda)"
        );
        assert_eq!(find("update_finished").area, "fleet");
        assert_eq!(
            p.rows
                .iter()
                .filter(|r| r.kind == "update_finished")
                .count(),
            2,
            "installed and failed both fold into update_finished"
        );
        assert_eq!(find("daemon_restarted").title, "daemon 0.4.1 pid 15714");

        let mut entry = graph_fixture().remove(0);
        entry
            .as_object_mut()
            .unwrap()
            .insert("merge_status".to_string(), serde_json::json!("merged"));
        let p = project("", &[entry], &[], "", "", "", "", None);
        assert!(
            kinds(&p.rows).contains(&"pr_merged"),
            "{:?}",
            kinds(&p.rows)
        );
        assert!(!kinds(&p.rows).contains(&"node_ended"));
        let merged = p.rows.iter().find(|r| r.kind == "pr_merged").unwrap();
        assert_eq!(merged.r#ref.as_deref(), Some("1395"));
        assert_eq!(merged.area, "ship");

        let a = parse_args(&[
            "--kind".to_string(),
            String::new(),
            "--harness".into(),
            "codex".into(),
            "--until-epoch".into(),
            "123".into(),
        ])
        .unwrap();
        assert_eq!(a.pre.kind, None, "an empty flag value is dropped");
        assert_eq!(a.pre.harness.as_deref(), Some("codex"));
        assert_eq!(a.until_epoch, Some(123));
    }

    fn removal_fixture() -> crate::removals::Removal {
        crate::removals::Removal {
            ts: "2026-09-06T10:00:00Z".into(),
            name: "t-d145".into(),
            short_id: Some("d145".into()),
            harness: Some("claude".into()),
            session_id: Some("00847995-e0db-47c2-ab5b-24468ba1a4f5".into()),
            node: Some("nd-aaaa".into()),
            removed_by: "gc-sweep".into(),
            verb: None,
            reason: Some("every named node done: nd-aaaa".into()),
            cause: Some("agent_row_reaped".into()),
            cause_at: Some("2026-09-06T10:00:00Z".into()),
            team: None,
            resume: Some("claude --resume 00847995".into()),
            cwd: Some("/tmp/wt".into()),
            trigger: Some("unattended".into()),
            model: None,
            receipt: Some(std::path::PathBuf::from("/tmp/reap-t-d145.json")),
        }
    }

    #[test]
    fn reap_rows() {
        let r = removal_fixture();
        let p = project("", &[], std::slice::from_ref(&r), "", "", "", "", None);
        let row = p
            .rows
            .iter()
            .find(|row| row.kind == "session_reaped")
            .expect("one reaped row");
        assert!(row.title.contains("t-d145"), "title was {}", row.title);
        // The remover moved to the actor field; the title says what happened.
        assert_eq!(row.title, "t-d145 removed", "title was {}", row.title);
        assert_eq!(row.name.as_deref(), Some("t-d145"));
        assert_eq!(row.actor.as_deref(), Some("gc-sweep"));
        assert_eq!(
            row.reason.as_deref(),
            Some("every named node done: nd-aaaa")
        );
        assert_eq!(
            row.detail.as_deref(),
            Some("resume: claude --resume 00847995 - cwd /tmp/wt - trigger unattended")
        );
        assert_eq!(row.node.as_deref(), Some("nd-aaaa"));
        assert_eq!(
            row.session_id.as_deref(),
            Some("00847995-e0db-47c2-ab5b-24468ba1a4f5")
        );
        let only_reaped = filter_rows(
            p.rows,
            &Page::default(),
            &Prefilter {
                kind: Some("session_reaped".into()),
                ..Default::default()
            },
            None,
            None,
        );
        assert_eq!(only_reaped.len(), 1);

        let mut r = removal_fixture();
        r.removed_by.clear();
        r.trigger = None;
        let p = project("", &[], std::slice::from_ref(&r), "", "", "", "", None);
        let row = p
            .rows
            .iter()
            .find(|row| row.kind == "session_reaped")
            .expect("one reaped row");
        assert_eq!(row.title, "t-d145 removed", "title was {}", row.title);
        assert_eq!(row.actor, None);
        assert!(
            row.detail
                .as_deref()
                .is_some_and(|d| d.ends_with("trigger unknown")),
            "detail was {:?}",
            row.detail
        );

        let mut r = removal_fixture();
        r.model = Some("glm-5.3-flash[1m]".into());
        let p = project("", &[], std::slice::from_ref(&r), "", "", "", "", None);
        let row = p
            .rows
            .iter()
            .find(|row| row.kind == "session_reaped")
            .expect("one reaped row");
        assert_eq!(row.model.as_deref(), Some("glm-5.3-flash[1m]"));
        // A receipt without the field stays silent rather than inventing one.
        let p = project(
            "",
            &[],
            std::slice::from_ref(&removal_fixture()),
            "",
            "",
            "",
            "",
            None,
        );
        let bare = p
            .rows
            .iter()
            .find(|row| row.kind == "session_reaped")
            .expect("one reaped row");
        assert_eq!(bare.model, None);

        let home = AgentsHome::at(std::path::PathBuf::from(
            "/nonexistent/fno-feed-test/agents",
        ));
        let (removals, notes) = crate::removals::read(&home, None);
        assert!(removals.is_empty());
        assert!(
            notes
                .iter()
                .any(|n| n.contains("reap-receipts store skipped")),
            "notes were {notes:?}"
        );
    }

    // A pre-stamp receipt carries no writer. The feed once invented the
    // word `reap` for it; now the title says what happened and the actor
    // stays empty rather than naming a door nobody named.

    // The receipt outlives the registry row, so it is the only surviving
    // record of the lane. A provenance view that drops it reports NOT
    // RECORDED for a model the store is holding.

    #[test]
    fn proj_rows() {
        let p = project("", &graph_fixture(), &[], "", "", "", "", None);
        let created = p
            .rows
            .iter()
            .find(|r| r.kind == "node_created")
            .expect("created_at projects with no emitter");
        assert_eq!(created.node.as_deref(), Some("nd-aaaa"));
        // The birth stamps: the creating session's registry lane facts,
        // taken at write time.
        assert_eq!(created.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(created.effort.as_deref(), Some("high"));
        assert_eq!(created.parent.as_deref(), Some("s-parent"));
        assert_eq!(created.team.as_deref(), Some("L2 e-0001"));
        let wire = serde_json::to_value(created).unwrap();
        assert_eq!(
            wire.get("cwd").and_then(Value::as_str),
            Some("/workspace/node-project")
        );
        let mut cwdless_entry = graph_fixture().remove(0);
        cwdless_entry.as_object_mut().unwrap().remove("cwd");
        let cwdless = project("", &[cwdless_entry], &[], "", "", "", "", None);
        let cwdless_created = cwdless
            .rows
            .iter()
            .find(|r| r.kind == "node_created")
            .expect("cwd-less graph row still projects");
        let cwdless_wire = serde_json::to_value(cwdless_created).unwrap();
        assert!(cwdless_wire.get("cwd").is_none());
        let started = p.rows.iter().find(|r| r.kind == "node_started").unwrap();
        assert_eq!(started.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(started.session_id.as_deref(), Some("s-do"));
        assert_eq!(started.effort.as_deref(), Some("high"));
        assert_eq!(started.phase.as_deref(), Some("execute"));
        let ended = p.rows.iter().find(|r| r.kind == "node_ended").unwrap();
        assert_eq!(ended.harness.as_deref(), Some("claude"));

        let mut entry = graph_fixture().remove(0);
        entry
            .as_object_mut()
            .unwrap()
            .remove("pr_number")
            .expect("fixture carries pr_number");
        let p = project("", &[entry], &[], "", "", "", "", None);
        assert_eq!(
            kinds(&p.rows),
            ["node_created", "node_started", "node_ended"]
        );

        // AC12-FEED: a store-only operator_question reaches the questions leg.
        let dir = tempfile::tempdir().unwrap();
        let questions = dir.path().join("questions.jsonl");
        let row = serde_json::json!({
            "ts": "2026-09-17T12:00:00Z", "type": "operator_question", "source": "test",
            "data": {"question_id": "q-feed-1", "question": "proceed?", "blocks": []}
        });
        crate::event_store::append_envelope(&questions, &row.to_string(), None).unwrap();
        let raw = crate::event_store::journal_text_checked(
            &questions,
            &crate::event_store::EventQuery::of_types(&[]),
        )
        .unwrap();
        assert!(raw.contains("q-feed-1"), "{raw}");

        // AC2-EDGE: the typed questions read keeps the parser to the four
        // question kinds. Attention rows never reach it, so they stop reading
        // as malformed; an empty-type corrupt line still counts, once.
        let dir = tempfile::tempdir().unwrap();
        let questions = dir.path().join("questions.jsonl");
        for i in 0..3 {
            let row = serde_json::json!({
                "ts": "2026-09-17T11:00:00Z", "type": "attention_delivery", "source": "test",
                "data": {"attention_id": format!("a-{i}"), "item_id": format!("it-{i}"), "rung": "inbox", "outcome": "delivered"}
            });
            crate::event_store::append_envelope(&questions, &row.to_string(), None).unwrap();
        }
        let q = serde_json::json!({
            "ts": "2026-09-17T12:00:00Z", "type": "operator_question", "source": "test",
            "data": {"question_id": "q-typed", "question": "still read?", "blocks": []}
        });
        crate::event_store::append_envelope(&questions, &q.to_string(), None).unwrap();
        let raw = crate::event_store::journal_text_checked(
            &questions,
            &crate::event_store::EventQuery::of_types(&[
                "operator_question",
                "operator_question_closed",
                "operator_decision",
                "day_boundary",
            ]),
        )
        .unwrap();
        assert!(raw.contains("q-typed"));
        assert!(
            !raw.contains("a-1"),
            "attention rows never reach the questions parser"
        );
        let text = format!("{raw}\nnot json\n");
        let p = project(&text, &[], &[], "", "", "", "", None);
        assert_eq!(
            p.skipped_lines, 1,
            "the corrupt line is the only malformed count"
        );

        // AC1: the jolly-finch shape, projected. The removal is recovered from
        // its registry_row_removed event; the feed row carries the deeper
        // uncaptured reason, the team it held, and no detail (a recovered
        // removal has no resume line to hand over).
        let mut r = removal_fixture();
        r.session_id = None;
        r.harness = Some("codex".into());
        r.name = "jolly-finch".into();
        r.removed_by = "fno-py".into();
        r.verb = Some("fno-py agents spawn --substrate pane --team".into());
        r.reason = Some("no unique codex rollout for this cwd after spawn".into());
        r.team = Some("L2 x-eeee".into());
        r.receipt = None;
        r.resume = None;
        r.cwd = None;
        r.trigger = None;
        let p = project("", &[], std::slice::from_ref(&r), "", "", "", "", None);
        let row = p
            .rows
            .iter()
            .find(|row| row.kind == "session_reaped")
            .expect("one reaped row");
        assert_eq!(row.title, "jolly-finch removed");
        assert_eq!(row.actor.as_deref(), Some("fno-py"));
        assert_eq!(
            row.reason.as_deref(),
            Some("no unique codex rollout for this cwd after spawn")
        );
        // The receipt row's team copies verbatim: the receipt is the
        // surviving record, and the team-event rows above are what render
        // the title.
        assert_eq!(row.team.as_deref(), Some("L2 x-eeee"));
        assert_eq!(row.detail, None);

        // AC4: agent_spawned carries provider (not harness), so the fallback
        // is load-bearing.
        let spawns = r#"{"ts":"2026-09-28T16:48:35Z","type":"agent_spawned","source":"python","data":{"cwd":"/repo","model":"gpt-6-sol","name":"jolly-finch","provider":"codex","spawned_by_session":"49a80492-388e-44a3-bd91-017be26bcaa0","substrate":"pane"}}"#;
        let p = project("", &[], &[], spawns, "", "", "", None);
        let row = p
            .rows
            .iter()
            .find(|r| r.kind == "session_spawned")
            .expect("one spawned row");
        assert_eq!(row.title, "jolly-finch spawned (pane)");
        assert_eq!(row.harness.as_deref(), Some("codex"), "provider falls back");
        assert_eq!(row.model.as_deref(), Some("gpt-6-sol"));
        assert_eq!(
            row.parent.as_deref(),
            Some("49a80492-388e-44a3-bd91-017be26bcaa0")
        );
        // A pre-birth refusal used to write nothing, so the feed showed
        // nothing for a launch the operator watched refuse.
        let refused = r#"{"ts":"2026-09-29T20:03:39Z","type":"agent_spawn_refused","source":"daemon","data":{"argv":["agents","spawn","--harness","claude"],"exit_code":2,"reason":"--mux-session is pane-only; substrate 'bg' has no mux session to spawn into"}}"#;
        let p = project("", &[], &[], refused, "", "", "", None);
        let row = p
            .rows
            .iter()
            .find(|r| r.kind == "session_spawn_refused")
            .expect("one refused row");
        assert!(row
            .title
            .starts_with("spawn refused: --mux-session is pane-only"));

        // AC5: the same team pair landing in both journals yields exactly
        // one granted and one vacated row.
        let team = [
            r#"{"ts":"2026-09-28T16:45:58Z","type":"agent_teamed","source":"python","data":{"grantor":"49a80492","level":2,"name":"jolly-finch","scope":"x-eeee"}}"#,
            r#"{"ts":"2026-09-28T16:45:58Z","type":"agent_team_vacated","source":"python","data":{"cause":"succession","grantor":"vellum","holder":"warden","level":2,"scope":"x-eeee","successor":"jolly-finch"}}"#,
        ]
        .join("\n");
        let both = format!("{team}\n{team}");
        let p = project("", &[], &[], "", &both, "", "", None);
        let granted: Vec<_> = p.rows.iter().filter(|r| r.kind == "team_granted").collect();
        let vacated: Vec<_> = p.rows.iter().filter(|r| r.kind == "team_vacated").collect();
        assert_eq!(granted.len(), 1, "granted dedupes");
        assert_eq!(granted[0].title, "jolly-finch teamed L2 x-eeee");
        assert_eq!(granted[0].team.as_deref(), Some("Lead of x-eeee"));
        assert_eq!(granted[0].holder.as_deref(), Some("jolly-finch"));
        assert_eq!(vacated.len(), 1, "vacated dedupes");
        assert_eq!(
            vacated[0].title,
            "warden left L2 x-eeee: succession -> jolly-finch"
        );
        assert_eq!(vacated[0].holder.as_deref(), Some("warden"));

        // AC6: a node in a held team's scope rolls up to the lead; a node
        // whose only tie is a graph parent rolls up to the epic; the team
        // kinds get no owner at all.
        let entries = vec![
            serde_json::json!({
                "id": "x-child", "title": "child node", "parent": "x-epic",
                "created_at": "2026-09-28T17:00:00Z",
            }),
            serde_json::json!({
                "id": "x-epic", "title": "the epic", "created_at": "2026-09-28T16:00:00Z",
            }),
        ];
        let team = r#"{"ts":"2026-09-28T15:30:00Z","type":"agent_teamed","source":"python","data":{"grantor":"s-lead","level":2,"name":"successor","scope":"x-epic"}}"#;
        let p = project("", &entries, &[], "", team, "", "", None);
        let child = p
            .rows
            .iter()
            .find(|r| r.node == Some("x-child".into()))
            .unwrap();
        assert_eq!(child.owner.as_deref(), Some("Lead of x-epic (successor)"));
        // The team row itself renders in the teams band: no owner on it.
        let granted = p
            .rows
            .iter()
            .find(|r| r.kind == "team_granted")
            .expect("the team projects a granted row");
        assert_eq!(granted.owner, None, "team rows carry no owner");
        let epic = p
            .rows
            .iter()
            .find(|r| r.node == Some("x-epic".into()))
            .unwrap();
        assert_eq!(epic.owner.as_deref(), Some("Lead of x-epic (successor)"));
        // Without a team the child rolls up to its epic by the graph parent.
        let p = project("", &entries, &[], "", "", "", "", None);
        let child = p
            .rows
            .iter()
            .find(|r| r.node == Some("x-child".into()))
            .unwrap();
        assert_eq!(child.owner.as_deref(), Some("epic x-epic the epic"));
        let epic = p
            .rows
            .iter()
            .find(|r| r.node == Some("x-epic".into()))
            .unwrap();
        assert_eq!(epic.owner, None);

        // Close rows: a pane_closed row renders its reason verbatim
        // with the bound session, a server_stopped row names its cause, and
        // both order by ts with the rest.
        let closes = concat!(
            r#"{"ts":"2026-09-30T10:00:05Z","type":"pane_closed","source":"daemon","data":{"mux_session":"main","pane":7,"squad":1,"cause":"operator","reason":"closed by operator","name":null,"harness_session":"sess-a","harness":"codex"}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:00:02Z","type":"pane_closed","source":"daemon","data":{"mux_session":"main","pane":9,"squad":1,"cause":"viewer_died","reason":"child exited","name":"w1","harness_session":null,"harness":null}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:00:09Z","type":"server_stopped","source":"daemon","data":{"mux_session":"main","cause":"shutdown","panes":0}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:00:07Z","type":"composer_shell_ran","source":"cli","data":{"mux_session":"main","cwd":"/tmp/p","shell":"/bin/zsh","line":"git status","pane":3}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:00:08Z","type":"composer_shell_refused","source":"cli","data":{"mux_session":"main","cwd":"","line":"git status","reason":"no project chosen; pick one on the Project chip","outcome":"refused"}}"#,
            "\n",
            "{not json",
        );
        let p = project("", &[], &[], "", "", closes, "", None);
        assert_eq!(
            p.skipped_lines, 0,
            "unrelated malformed lines do not count here"
        );
        let closed: Vec<_> = p.rows.iter().filter(|r| r.kind == "pane_closed").collect();
        assert_eq!(closed.len(), 2);
        assert_eq!(closed[0].ts, "2026-09-30T10:00:02Z", "ts ordering");
        assert_eq!(
            closed[0].title, "pane w1 (9) closed: child exited",
            "a named pane titles with the name"
        );
        assert_eq!(closed[0].reason.as_deref(), Some("child exited"));
        assert_eq!(
            closed[1].title, "pane 7 closed: closed by operator",
            "an unnamed pane titles with the pane id"
        );
        assert_eq!(closed[1].session_id.as_deref(), Some("sess-a"));
        assert_eq!(closed[1].harness.as_deref(), Some("codex"));
        let stopped = p
            .rows
            .iter()
            .find(|r| r.kind == "server_stopped")
            .expect("the stop row renders");
        assert_eq!(stopped.title, "mux server stopped: shutdown");
        let ran = p
            .rows
            .iter()
            .find(|r| r.kind == "composer_shell_ran")
            .expect("the composer ran row renders");
        assert_eq!(ran.title, "shell in /tmp/p: git status (pane 3)");
        let refused = p
            .rows
            .iter()
            .find(|r| r.kind == "composer_shell_refused")
            .expect("the composer refused row renders");
        assert_eq!(
            refused.title,
            "shell refused: no project chosen; pick one on the Project chip"
        );
        assert_eq!(
            refused.reason.as_deref(),
            Some("no project chosen; pick one on the Project chip")
        );
    }

    #[test]
    fn the_lead_column_resolves_the_current_team_name_at_render_time() {
        let granted = |ts: &str, holder: &str, scope: &str| TeamEvent {
            ts: ts.into(),
            action: TeamAction::Granted,
            holder: holder.into(),
            scope: scope.into(),
            level: 2,
            cause: None,
            successor: None,
            actor: None,
            vacated_scope: None,
        };
        let row_under = |ts: &str, node: &str| FeedRow {
            ts: ts.into(),
            kind: "question_asked".into(),
            node: Some(node.into()),
            ..FeedRow::default()
        };
        // x-live was granted to "finch" but the team is now named Quill;
        // x-old's grant froze a dispatch slug and no live team covers it;
        // x-gone's team is gone but its stored holder is a person name.
        let mut rows = vec![
            row_under("2026-10-07T10:00:00Z", "x-live"),
            row_under("2026-10-07T10:00:00Z", "x-old"),
            row_under("2026-10-07T10:00:00Z", "x-gone"),
        ];
        let events = vec![
            granted("2026-10-07T09:00:00Z", "finch", "x-live"),
            granted("2026-10-07T09:00:00Z", "king-4d9b-op", "x-old"),
            granted("2026-10-07T09:00:00Z", "candor", "x-gone"),
        ];
        let lead_names = std::collections::BTreeMap::from([("x-live".to_string(), "Quill".into())]);
        assign_owners(
            &mut rows,
            &events,
            &[],
            &Default::default(),
            &lead_names,
            &Default::default(),
        );
        // The renamed team's lead column prints the CURRENT name while the
        // owner string keeps the lead at event time: both names on the row.
        assert_eq!(rows[0].lead.as_deref(), Some("Quill"));
        assert_eq!(rows[0].owner.as_deref(), Some("Lead of x-live (finch)"));
        // A slug holder with no live team prints no name: rank only, lead
        // absent - the placeholder never renders.
        assert_eq!(rows[1].lead, None);
        assert_eq!(rows[1].owner.as_deref(), Some("Lead of x-old"));
        // A person-shaped stored holder is the fallback when no live team
        // covers the node.
        assert_eq!(rows[2].lead.as_deref(), Some("candor"));
        assert_eq!(rows[2].owner.as_deref(), Some("Lead of x-gone (candor)"));
    }
}

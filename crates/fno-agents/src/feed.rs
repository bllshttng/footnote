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
    /// `node_created` | `node_started` | `pr_created` | `node_ended` |
    /// `session_spawned` | `session_reaped` | `crown_granted` |
    /// `crown_vacated` | `day_boundary`
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
    /// `L{level} {scope}` for the crown kinds and a crowned removal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crown: Option<String>,
    /// The king or epic the row rolls up to, set on non-crown rows only:
    /// `king {holder} L{level}` or `epic {parent} {title}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The session that spawned this row's session, from the birth event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
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

/// The pure projection: questions text + graph entries + removals + the
/// agents journal's spawn events + the crown events (both journals) ->
/// ordered rows. Ascending by ts, so a consumer reads history forward and
/// `--limit` trims from the newest end.
pub fn project(
    questions_raw: &str,
    graph_entries: &[Value],
    removals: &[crate::removals::Removal],
    spawns_raw: &str,
    crown_raw: &str,
    closes_raw: &str,
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
            rows.push(FeedRow {
                ts: created,
                kind: "node_created".into(),
                node: Some(node_id.to_string()),
                cwd: s_field(entry, "cwd"),
                session_id: s_field(entry, "source_session_id").filter(|s| is_session_handle(s)),
                harness: s_field(entry, "source_harness"),
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
            actor: removed_by,
            reason: r.reason.clone(),
            crown: r.crown.clone(),
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
    // recorded. The reason rides verbatim; cause is the enum's word, so the
    // row tells the operator WHO closed it (operator vs the death path) and
    // WHY in one line.
    for line in closes_raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let kind = match v.get("type").and_then(Value::as_str) {
            Some(k @ ("pane_closed" | "server_stopped")) => k,
            _ => continue,
        };
        let Some(data) = v.get("data") else { continue };
        let Some(ts) = s_field(&v, "ts") else {
            continue;
        };
        if kind == "server_stopped" {
            let cause = s_field(data, "cause").unwrap_or("unknown");
            rows.push(FeedRow {
                ts: ts.to_string(),
                kind: "server_stopped".into(),
                title: format!("mux server stopped: {cause}"),
                ..FeedRow::default()
            });
            continue;
        }
        let name = s_field(data, "name").unwrap_or("");
        let pane = data
            .get("pane")
            .and_then(Value::as_u64)
            .map(|p| p.to_string())
            .unwrap_or_default();
        let reason = s_field(data, "reason").unwrap_or("no reason recorded");
        let title = if name.is_empty() {
            format!("pane {pane} closed: {reason}")
        } else {
            format!("pane {name} ({pane}) closed: {reason}")
        };
        rows.push(FeedRow {
            ts: ts.to_string(),
            kind: "pane_closed".into(),
            session_id: s_field(data, "harness_session").map(str::to_string),
            harness: s_field(data, "harness").map(str::to_string),
            title,
            reason: Some(reason.to_string()),
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

    // Crown rows from the crown events, which land in BOTH journals, so rows
    // dedupe on (ts, scope, holder, cause). The same parse feeds the owner
    // assignment below.
    let mut crown_events = parse_crown_events(crown_raw);
    crown_events.sort_by_key(|c| ts_key(&c.ts));
    // The themes render once per fold, not once per row: one tolerant store
    // read feeds every crown row's title below.
    let themes = crate::paths::AgentsHome::from_env_opt()
        .map(|home| crate::crown_names::theme_map(&home.crown_names_json()))
        .unwrap_or_default();
    let rank = |level: i64, scope: &str| -> String {
        let theme = themes
            .get(crate::territory::canonical_scope(scope).as_str())
            .cloned();
        crate::crown_names::title(level as u32, scope, theme.as_deref())
    };
    let mut seen_crowns: std::collections::HashSet<(String, String, String, String)> =
        std::collections::HashSet::new();
    for c in &crown_events {
        let key = (
            c.ts.clone(),
            c.scope.clone(),
            c.holder.clone(),
            c.cause.clone().unwrap_or_default(),
        );
        if !seen_crowns.insert(key) {
            continue;
        }
        match c.action {
            CrownAction::Granted => {
                let mut title = format!("{} crowned L{} {}", c.holder, c.level, c.scope);
                if let Some(from) = &c.vacated_scope {
                    title.push_str(&format!(" (moved from {from})"));
                }
                rows.push(FeedRow {
                    ts: c.ts.clone(),
                    kind: "crown_granted".into(),
                    crown: Some(rank(c.level, &c.scope)),
                    actor: c.actor.clone(),
                    title,
                    ..FeedRow::default()
                });
            }
            CrownAction::Vacated => {
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
                    kind: "crown_vacated".into(),
                    crown: Some(rank(c.level, &c.scope)),
                    actor: c.actor.clone(),
                    title,
                    ..FeedRow::default()
                });
            }
        }
    }

    rows.sort_by(|a, b| ts_key(&a.ts).cmp(&ts_key(&b.ts)));
    assign_owners(&mut rows, &crown_events, graph_entries);
    Projection {
        rows,
        skipped_lines,
        skipped_entries,
    }
}

/// One parsed crown event, used twice: for the crown rows and for the owner
/// assignment's scope-to-holder timeline.
#[derive(Debug, Clone)]
struct CrownEvent {
    ts: String,
    action: CrownAction,
    holder: String,
    scope: String,
    level: i64,
    cause: Option<String>,
    successor: Option<String>,
    actor: Option<String>,
    vacated_scope: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CrownAction {
    Granted,
    Vacated,
}

fn parse_crown_events(crown_raw: &str) -> Vec<CrownEvent> {
    let mut out = Vec::new();
    for line in crown_raw.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let Some(ts) = s_field(&v, "ts") else {
            continue;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("agent_crowned") => {
                let Some(name) = s_field(&data, "name") else {
                    continue;
                };
                let Some(level) = data.get("level").and_then(Value::as_i64) else {
                    continue;
                };
                let Some(scope) = s_field(&data, "scope") else {
                    continue;
                };
                out.push(CrownEvent {
                    ts,
                    action: CrownAction::Granted,
                    holder: name.to_string(),
                    scope: scope.to_string(),
                    level,
                    cause: None,
                    successor: None,
                    actor: s_field(&data, "grantor"),
                    vacated_scope: s_field(&data, "vacated_scope"),
                });
            }
            Some("agent_crown_vacated") => {
                let Some(holder) = s_field(&data, "holder") else {
                    continue;
                };
                let Some(level) = data.get("level").and_then(Value::as_i64) else {
                    continue;
                };
                let Some(scope) = s_field(&data, "scope") else {
                    continue;
                };
                out.push(CrownEvent {
                    ts,
                    action: CrownAction::Vacated,
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

/// Assign `owner` to every non-crown row. Crown events folded in time order
/// hold a map from scope to its holder; a removal row whose `crown` is set
/// clears its scope at its own ts. A row whose node, or that node's graph
/// parent, sits in a held scope gets `king {holder} L{level}`; otherwise a
/// row whose node has a graph parent gets `epic {parent} {parent title}`.
fn assign_owners(rows: &mut [FeedRow], crown_events: &[CrownEvent], graph_entries: &[Value]) {
    if rows.is_empty() {
        return;
    }
    // node -> parent / title lookups, one linear scan each (the entry set is
    // the operator's backlog, not a hot path).
    let parent_of = |node: &str| -> Option<String> {
        graph_entries
            .iter()
            .find(|e| graph_store::entry_id(e) == Some(node))
            .and_then(|e| s_field(e, "parent"))
    };
    let title_of = |node: &str| -> Option<String> {
        graph_entries
            .iter()
            .find(|e| graph_store::entry_id(e) == Some(node))
            .and_then(|e| graph_store::s_str(e, "title"))
            .map(str::to_string)
    };
    // The scope timeline in time order: (ts_key, grant?, scope, holder, level).
    // Owned strings, because a clear entry derives from a row's crown string
    // while the walk below borrows `rows` mutably.
    let mut timeline: Vec<((u8, i64), bool, String, String, i64)> = Vec::new();
    for c in crown_events {
        timeline.push((
            ts_key(&c.ts),
            true,
            c.scope.clone(),
            c.holder.clone(),
            c.level,
        ));
    }
    // A removal whose crown is set clears that crown's scope from its own ts:
    // after the heir's removal the territory has no king, so later rows stop
    // rolling up to it.
    for r in rows.iter() {
        if r.kind == "session_reaped" && r.crown.is_some() {
            let scope = r
                .crown
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
        if r.kind == "crown_granted"
            || r.kind == "crown_vacated"
            || (r.kind == "session_reaped" && r.crown.is_some())
        {
            // A crown-band row carries no owner: the band IS its group.
            continue;
        }
        let parent = r.node.as_deref().and_then(parent_of);
        let king = held.iter().find(|(scope, _, _)| {
            r.node.as_deref().is_some_and(|n| scope_holds(scope, n))
                || parent.as_deref().is_some_and(|p| scope_holds(scope, p))
        });
        if let Some((_, holder, level)) = king {
            let scope = held
                .iter()
                .find(|(s, _, _)| {
                    r.node.as_deref().is_some_and(|n| scope_holds(s, n))
                        || parent.as_deref().is_some_and(|p| scope_holds(s, p))
                })
                .map(|(s, _, _)| s.clone())
                .unwrap_or_default();
            let theme = crate::paths::AgentsHome::from_env_opt()
                .and_then(|home| crate::crown_names::theme_for(&home.crown_names_json(), &scope));
            let rank = crate::crown_names::title(*level as u32, &scope, theme.as_deref());
            r.owner = Some(format!("{rank} ({holder})"));
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

/// True when a crown scope (comma-separated node ids) holds `node`.
fn scope_holds(scope: &str, node: &str) -> bool {
    scope.split(',').any(|seg| seg.trim() == node)
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
fn agents_journal(home: &AgentsHome, types: &[&str], since_epoch: Option<u64>) -> String {
    let query = crate::event_store::EventQuery {
        since_ms: since_epoch.map(|s| s as i64 * 1000),
        ..crate::event_store::EventQuery::of_types(types)
    };
    crate::event_store::journal_text_checked(&home.events_jsonl(), &query).unwrap_or_default()
}

/// The crown events, concatenated from BOTH journals so a crown row dedupes
/// on (ts, scope, holder, cause) instead of depending on which store an
/// emitter wrote. Unbounded by the window: a crown is granted before the
/// events it owns.
fn crown_journals(home: &AgentsHome) -> String {
    let types = ["agent_crowned", "agent_crown_vacated"];
    let mut raw = agents_journal(home, &types, None);
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
    let questions_raw = match crate::event_store::journal_text_checked(
        &questions_path,
        &crate::event_store::EventQuery::of_types(&[]),
    ) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!(
                "fno-agents feed: questions store unreadable, skipped: {} ({e})",
                questions_path.display()
            );
            String::new()
        }
    };

    let (graph_entries, graph_note): (Vec<Value>, Option<String>) = {
        // The lifecycle leg reads the store, never the file. An absent store
        // is still a skipped store for the feed's purposes: the operator
        // should know the lifecycle leg is absent, so its absence keeps its
        // own note (AC3) where a present-but-empty store is just empty.
        let path = graph_path(home);
        if !crate::backlog::database_path(&path).exists() {
            (Vec::new(), Some("graph store skipped (absent)".to_string()))
        } else {
            match crate::backlog::api::rows(&crate::backlog::api::Store::new(&path)) {
                Ok(list) => (list, None),
                Err(e) => (Vec::new(), Some(format!("graph store skipped ({})", e.0))),
            }
        }
    };
    if let Some(note) = graph_note {
        eprintln!("fno-agents feed: {note}");
    }

    let (removals, removal_notes) =
        crate::removals::read(home, args.since_epoch.map(|s| s as i64 * 1000));
    for note in &removal_notes {
        eprintln!("fno-agents feed: {note}");
    }
    let spawns_raw = agents_journal(home, &["agent_spawned"], args.since_epoch);
    let crown_raw = crown_journals(home);
    let closes_raw = agents_journal(home, &["pane_closed", "server_stopped"], args.since_epoch);

    let Projection {
        rows,
        skipped_lines,
        skipped_entries,
    } = project(
        &questions_raw,
        &graph_entries,
        &removals,
        &spawns_raw,
        &crown_raw,
        &closes_raw,
    );
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

    // The x-aaaa shape: a blueprint row with only ended_at, a do row and a
    // ship row each with started_at, on a node carrying pr_number and
    // completed_at.
    fn graph_fixture() -> Vec<Value> {
        vec![serde_json::json!({
            "id": "x-aaaa",
            "status": "done",
            "title": "feed marker node",
            "pr_number": 1395,
            "pr_url": "https://github.com/bllshttng/footnote/pull/1395",
            "created_at": "2026-09-01T08:00:00Z",
            "cwd": "/workspace/node-project",
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
            r#"{"ts":"2026-09-02T17:00:00Z","type":"operator_question","source":"target","data":{"question_id":"q-1","question":"line one\nline two","session_id":"s-ask","node":"x-aaaa"}}"#,
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
        let p = project("", &graph_fixture(), &[], "", "", "");
        assert_eq!(
            kinds(&p.rows),
            ["node_created", "node_started", "pr_created", "node_ended"]
        );
        let started = &p.rows[1];
        assert_eq!(started.node.as_deref(), Some("x-aaaa"));
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
        let p = project(&questions_fixture(), &[], &[], "", "", "");
        assert_eq!(
            kinds(&p.rows),
            ["question_asked", "question_closed", "decision_recorded"]
        );
    }

    #[test]
    fn day_boundary_rows_are_projected_without_being_skipped() {
        let questions = r#"{"ts":"2026-09-13T08:00:00Z","type":"day_boundary","source":"operator","data":{"kind":"start","boundary_id":"day-start-20260913-ab12"}}"#;
        let p = project(questions, &[], &[], "", "", "");
        assert_eq!(p.skipped_lines, 0);
        assert_eq!(kinds(&p.rows), ["day_boundary"]);
        assert_eq!(p.rows[0].title, "day start");
        assert_eq!(p.rows[0].r#ref.as_deref(), Some("day-start-20260913-ab12"));
    }

    #[test]
    fn question_rows_carry_ids_and_asker_session() {
        let p = project(&questions_fixture(), &graph_fixture(), &[], "", "", "");
        let asked = p.rows.iter().find(|r| r.kind == "question_asked").unwrap();
        assert_eq!(asked.r#ref.as_deref(), Some("q-1"));
        assert_eq!(asked.session_id.as_deref(), Some("s-ask"));
        assert_eq!(asked.node.as_deref(), Some("x-aaaa"));
        assert_eq!(asked.title, "line one");
        let closed = p.rows.iter().find(|r| r.kind == "question_closed").unwrap();
        assert_eq!(closed.r#ref.as_deref(), Some("q-1"));
        // `s-op` is not a session handle, so it is the ACTOR and the closure
        // offers no attach target. The node comes from the asking row.
        assert_eq!(closed.session_id, None);
        assert_eq!(closed.actor.as_deref(), Some("s-op"));
        assert_eq!(closed.node.as_deref(), Some("x-aaaa"));
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
        let p = project(&questions_fixture(), &graph_fixture(), &[], "", "", "");
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
        let p = project(&questions, &entries, &[], "", "", "");
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
        let p = project(&questions, &[], &[], "", "", "");
        assert_eq!(kinds(&p.rows)[0], "question_asked");
        assert_eq!(p.rows[0].ts, "yesterday-ish");
        let kept = filter_rows(p.rows, None, None, None, Some(1_700_000_000), None);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn filter_node_session_and_limit_from_newest_end() {
        let p = project(&questions_fixture(), &graph_fixture(), &[], "", "", "");
        let node_rows = filter_rows(p.rows.clone(), Some("x-aaaa"), None, None, None, None);
        // The fixture question carries node x-aaaa, so a node filter keeps it
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
        let p = project(&questions, &[], &[], "", "", "");
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
        let p = project(&questions, &[], &[], "", "", "");
        let closed = p.rows.iter().find(|r| r.kind == "question_closed").unwrap();
        assert_eq!(
            closed.session_id.as_deref(),
            Some("20260904T151442Z-cl54345-58af0c")
        );
        assert_eq!(closed.actor, None);
    }

    fn removal_fixture() -> crate::removals::Removal {
        crate::removals::Removal {
            ts: "2026-09-06T10:00:00Z".into(),
            name: "t-d145".into(),
            short_id: Some("d145".into()),
            harness: Some("claude".into()),
            session_id: Some("00847995-e0db-47c2-ab5b-24468ba1a4f5".into()),
            node: Some("x-aaaa".into()),
            removed_by: "gc-sweep".into(),
            verb: None,
            reason: Some("every named node done: x-aaaa".into()),
            cause: Some("agent_row_reaped".into()),
            cause_at: Some("2026-09-06T10:00:00Z".into()),
            crown: None,
            resume: Some("claude --resume 00847995".into()),
            cwd: Some("/tmp/wt".into()),
            trigger: Some("unattended".into()),
            model: None,
            receipt: Some(std::path::PathBuf::from("/tmp/reap-t-d145.json")),
        }
    }

    #[test]
    fn a_receipt_becomes_one_reaped_row_carrying_its_resume_line() {
        let r = removal_fixture();
        let p = project("", &[], std::slice::from_ref(&r), "", "", "");
        let row = p
            .rows
            .iter()
            .find(|row| row.kind == "session_reaped")
            .expect("one reaped row");
        assert!(row.title.contains("t-d145"), "title was {}", row.title);
        // The remover moved to the actor field; the title says what happened.
        assert_eq!(row.title, "t-d145 removed", "title was {}", row.title);
        assert_eq!(row.actor.as_deref(), Some("gc-sweep"));
        assert_eq!(row.reason.as_deref(), Some("every named node done: x-aaaa"));
        assert_eq!(
            row.detail.as_deref(),
            Some("resume: claude --resume 00847995 - cwd /tmp/wt - trigger unattended")
        );
        assert_eq!(row.node.as_deref(), Some("x-aaaa"));
        assert_eq!(
            row.session_id.as_deref(),
            Some("00847995-e0db-47c2-ab5b-24468ba1a4f5")
        );
        let only_reaped = filter_rows(p.rows, None, None, Some("session_reaped"), None, None);
        assert_eq!(only_reaped.len(), 1);
    }

    // A pre-stamp receipt carries no writer. The feed once invented the
    // word `reap` for it; now the title says what happened and the actor
    // stays empty rather than naming a door nobody named.
    #[test]
    fn a_pre_stamp_receipt_reads_unknown_never_reap() {
        let mut r = removal_fixture();
        r.removed_by.clear();
        r.trigger = None;
        let p = project("", &[], std::slice::from_ref(&r), "", "", "");
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
    }

    // The receipt outlives the registry row, so it is the only surviving
    // record of the lane. A provenance view that drops it reports NOT
    // RECORDED for a model the store is holding.
    #[test]
    fn a_reaped_row_keeps_the_model_its_receipt_recorded() {
        let mut r = removal_fixture();
        r.model = Some("glm-5.3-flash[1m]".into());
        let p = project("", &[], std::slice::from_ref(&r), "", "", "");
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
        );
        let bare = p
            .rows
            .iter()
            .find(|row| row.kind == "session_reaped")
            .expect("one reaped row");
        assert_eq!(bare.model, None);
    }

    #[test]
    fn an_absent_receipts_directory_yields_no_rows_and_a_note() {
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

    #[test]
    fn every_graph_entry_yields_a_node_created_row_and_the_lane_it_ran() {
        let p = project("", &graph_fixture(), &[], "", "", "");
        let created = p
            .rows
            .iter()
            .find(|r| r.kind == "node_created")
            .expect("created_at projects with no emitter");
        assert_eq!(created.node.as_deref(), Some("x-aaaa"));
        let wire = serde_json::to_value(created).unwrap();
        assert_eq!(
            wire.get("cwd").and_then(Value::as_str),
            Some("/workspace/node-project")
        );
        let mut cwdless_entry = graph_fixture().remove(0);
        cwdless_entry.as_object_mut().unwrap().remove("cwd");
        let cwdless = project("", &[cwdless_entry], &[], "", "", "");
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
    }

    #[test]
    fn node_without_pr_number_gets_no_pr_row() {
        let mut entry = graph_fixture().remove(0);
        entry
            .as_object_mut()
            .unwrap()
            .remove("pr_number")
            .expect("fixture carries pr_number");
        let p = project("", &[entry], &[], "", "", "");
        assert_eq!(
            kinds(&p.rows),
            ["node_created", "node_started", "node_ended"]
        );
    }

    #[test]
    fn the_feed_reads_a_store_committed_question() {
        // AC12-FEED: a store-only operator_question reaches the questions leg.
        let dir = tempfile::tempdir().unwrap();
        let questions = dir.path().join("questions.jsonl");
        let row = serde_json::json!({
            "ts": "2026-09-17T12:00:00Z", "type": "operator_question", "source": "agent",
            "data": {"question_id": "q-feed-1", "question": "proceed?", "blocks": []}
        });
        crate::event_store::append_envelope(&questions, &row.to_string(), None).unwrap();
        let raw = crate::event_store::journal_text_checked(
            &questions,
            &crate::event_store::EventQuery::of_types(&[]),
        )
        .unwrap();
        assert!(raw.contains("q-feed-1"), "{raw}");
    }

    #[test]
    fn a_crowned_never_bound_removal_projects_with_reason_and_crown() {
        // AC1: the jolly-finch shape, projected. The removal is recovered from
        // its registry_row_removed event; the feed row carries the deeper
        // uncaptured reason, the crown it held, and no detail (a recovered
        // removal has no resume line to hand over).
        let mut r = removal_fixture();
        r.session_id = None;
        r.harness = Some("codex".into());
        r.name = "jolly-finch".into();
        r.removed_by = "fno-py".into();
        r.verb = Some("fno-py agents spawn --substrate pane --crown".into());
        r.reason = Some("no unique codex rollout for this cwd after spawn".into());
        r.crown = Some("L2 x-eeee".into());
        r.receipt = None;
        r.resume = None;
        r.cwd = None;
        r.trigger = None;
        let p = project("", &[], std::slice::from_ref(&r), "", "", "");
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
        // The receipt row's crown copies verbatim: the receipt is the
        // surviving record, and the crown-event rows above are what render
        // the title.
        assert_eq!(row.crown.as_deref(), Some("L2 x-eeee"));
        assert_eq!(row.detail, None);
    }

    #[test]
    fn a_spawn_event_projects_a_session_spawned_row() {
        // AC4: agent_spawned carries provider (not harness), so the fallback
        // is load-bearing.
        let spawns = r#"{"ts":"2026-09-28T16:48:35Z","type":"agent_spawned","source":"python","data":{"cwd":"/repo","model":"gpt-6-sol","name":"jolly-finch","provider":"codex","spawned_by_session":"49a80492-388e-44a3-bd91-017be26bcaa0","substrate":"pane"}}"#;
        let p = project("", &[], &[], spawns, "", "");
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
    }

    #[test]
    fn crown_rows_project_from_both_journals_and_dedupe() {
        // AC5: the same crown pair landing in both journals yields exactly
        // one granted and one vacated row.
        let crown = [
            r#"{"ts":"2026-09-28T16:45:58Z","type":"agent_crowned","source":"python","data":{"grantor":"49a80492","level":2,"name":"jolly-finch","scope":"x-eeee"}}"#,
            r#"{"ts":"2026-09-28T16:45:58Z","type":"agent_crown_vacated","source":"python","data":{"cause":"succession","grantor":"vellum","holder":"warden","level":2,"scope":"x-eeee","successor":"jolly-finch"}}"#,
        ]
        .join("\n");
        let both = format!("{crown}\n{crown}");
        let p = project("", &[], &[], "", &both, "");
        let granted: Vec<_> = p
            .rows
            .iter()
            .filter(|r| r.kind == "crown_granted")
            .collect();
        let vacated: Vec<_> = p
            .rows
            .iter()
            .filter(|r| r.kind == "crown_vacated")
            .collect();
        assert_eq!(granted.len(), 1, "granted dedupes");
        assert_eq!(granted[0].title, "jolly-finch crowned L2 x-eeee");
        assert_eq!(granted[0].crown.as_deref(), Some("Lead of x-eeee"));
        assert_eq!(vacated.len(), 1, "vacated dedupes");
        assert_eq!(
            vacated[0].title,
            "warden left L2 x-eeee: succession -> jolly-finch"
        );
    }

    #[test]
    fn owners_come_from_held_crowns_then_graph_parents() {
        // AC6: a node in a held crown's scope rolls up to the king; a node
        // whose only tie is a graph parent rolls up to the epic; the crown
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
        let crown = r#"{"ts":"2026-09-28T15:30:00Z","type":"agent_crowned","source":"python","data":{"grantor":"s-king","level":2,"name":"heir","scope":"x-epic"}}"#;
        let p = project("", &entries, &[], "", crown);
        let child = p
            .rows
            .iter()
            .find(|r| r.node == Some("x-child".into()))
            .unwrap();
        assert_eq!(child.owner.as_deref(), Some("Lead of x-epic (heir)"));
        // The crown row itself renders in the crowns band: no owner on it.
        let granted = p
            .rows
            .iter()
            .find(|r| r.kind == "crown_granted")
            .expect("the crown projects a granted row");
        assert_eq!(granted.owner, None, "crown rows carry no owner");
        let epic = p
            .rows
            .iter()
            .find(|r| r.node == Some("x-epic".into()))
            .unwrap();
        assert_eq!(epic.owner.as_deref(), Some("Lead of x-epic (heir)"));
        // Without a crown the child rolls up to its epic by the graph parent.
        let p = project("", &entries, &[], "", "", "");
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
    }

    /// Close rows: a pane_closed row renders its reason verbatim
    /// with the bound session, a server_stopped row names its cause, and
    /// both order by ts with the rest.
    #[test]
    fn close_rows_render_reason_and_cause() {
        let closes = concat!(
            r#"{"ts":"2026-09-30T10:00:05Z","type":"pane_closed","source":"daemon","data":{"mux_session":"main","pane":7,"squad":1,"cause":"operator","reason":"closed by operator","name":null,"harness_session":"sess-a","harness":"codex"}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:00:02Z","type":"pane_closed","source":"daemon","data":{"mux_session":"main","pane":9,"squad":1,"cause":"viewer_died","reason":"child exited","name":"w1","harness_session":null,"harness":null}}"#,
            "\n",
            r#"{"ts":"2026-09-30T10:00:09Z","type":"server_stopped","source":"daemon","data":{"mux_session":"main","cause":"shutdown","panes":0}}"#,
            "\n",
            "{not json",
        );
        let p = project("", &[], &[], "", "", closes);
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
    }
}

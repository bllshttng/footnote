//! One removal fold, shared by the feed projection and the `agent.list --all`
//! retired lane.
//!
//! Two readers used to hand-join the removal records separately, and both
//! missed the same rows: feed.rs read only the reap-receipt files, and
//! daemon/list_rows.rs joined those files with the event journal for a cause.
//! But a row with no harness session identity is refused a receipt
//! (`receipt.rs` builds none), so its removal exists ONLY as the
//! `registry_row_removed` event the registry choke point emits. Measured
//! 2026-09-28: a teamed successor dropped out of the feed in exactly that
//! shape - the event sat in the store, the projection never read it.
//!
//! The fold reads the receipts AND the registry events, keys every cause by
//! session id, and recovers a receipt-less removal from its own event. Pure
//! over its inputs; `read` is the store-reading half and never fails.

use crate::event_store::EventQuery;
use crate::paths::AgentsHome;
use crate::receipt::ReapReceipt;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

/// One session removal, whatever record it came from. A receipt-backed
/// removal carries `resume`/`cwd`/`trigger`/`model`/`receipt`; a recovered
/// receipt-less one carries the remover verb and the reason the event
/// recorded, and the live-record fields stay `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct Removal {
    pub ts: String,
    pub name: String,
    pub short_id: Option<String>,
    pub harness: Option<String>,
    pub session_id: Option<String>,
    pub node: Option<String>,
    pub removed_by: String,
    /// The command that wrote the removal, joined from the same-second
    /// `registry_rows_lost` line (`fno-py agents spawn --substrate pane --team`).
    pub verb: Option<String>,
    pub reason: Option<String>,
    pub cause: Option<String>,
    pub cause_at: Option<String>,
    /// `L{level} {scope}` when a team still named this row at removal time.
    pub team: Option<String>,
    pub resume: Option<String>,
    pub cwd: Option<String>,
    pub trigger: Option<String>,
    pub model: Option<String>,
    pub receipt: Option<PathBuf>,
}

/// One parsed journal line, commit order preserved by the vec.
#[derive(Clone)]
struct JEvent {
    ts: String,
    kind: String,
    data: Value,
}

fn parse_events(raw: &str) -> Vec<JEvent> {
    raw.lines()
        .filter_map(|line| {
            let v = serde_json::from_str::<Value>(line.trim()).ok()?;
            Some(JEvent {
                ts: v.get("ts")?.as_str()?.to_string(),
                kind: v.get("type")?.as_str()?.to_string(),
                data: v.get("data").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Sort key for `rfc3339-ish` stamps; unparseable stamps order first.
fn tord(ts: &str) -> (u8, i64) {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(t) => (1, t.timestamp_millis()),
        Err(_) => (0, 0),
    }
}

/// Whole seconds, for the same-second joins. Unparseable stamps fall back to
/// their first 19 characters, so two renderings of one second still collide.
fn tsec(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| t.timestamp().to_string())
        .unwrap_or_else(|_| ts.chars().take(19).collect())
}

fn s_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The newest cause recorded for `sid`: `(type, ts, reason)`, reason taking
/// the event's `basis` first, then its `reason`. Commit order means the last
/// line for a sid is the newest; a later removal over an earlier one is the
/// reading that names why the row is gone NOW.
fn cause_map(agent_events: &[JEvent]) -> HashMap<String, (String, String, Option<String>)> {
    let mut out: HashMap<String, (String, String, Option<String>)> = HashMap::new();
    for e in agent_events {
        if e.kind != "agent_row_reaped" && e.kind != "registry_row_removed" {
            continue;
        }
        let sid = s_str(&e.data, "harness_session_id")
            .or_else(|| s_str(&e.data, "session_id"))
            .unwrap_or("");
        if sid.is_empty() {
            continue;
        }
        let reason = s_str(&e.data, "basis")
            .or_else(|| s_str(&e.data, "reason"))
            .map(str::to_string);
        out.insert(sid.to_string(), (e.kind.clone(), e.ts.clone(), reason));
    }
    out
}

/// `(pid, second) -> verb`, from the `registry_rows_lost` lines: the writer
/// that dropped the rows names itself once per process, not once per row.
fn lost_verbs(agent_events: &[JEvent]) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    for e in agent_events {
        if e.kind != "registry_rows_lost" {
            continue;
        }
        let Some(verb) = s_str(&e.data, "verb") else {
            continue;
        };
        let pid = e
            .data
            .get("pid")
            .and_then(Value::as_i64)
            .map(|p| p.to_string())
            .unwrap_or_default();
        out.insert((pid, tsec(&e.ts)), verb.to_string());
    }
    out
}

/// The team a name still held at `before_ts`: the newest grant at or before
/// it, voided when a vacate names that holder in between.
fn team_at(grants: &[JEvent], vacates: &[JEvent], name: &str, before_ts: &str) -> Option<String> {
    let g = grants
        .iter()
        .filter(|e| {
            e.kind == "agent_teamed"
                && s_str(&e.data, "name") == Some(name)
                && tord(&e.ts) <= tord(before_ts)
        })
        .max_by(|a, b| tord(&a.ts).cmp(&tord(&b.ts)))?;
    let between = |ts: &str| tord(g.ts.clone().as_str()) < tord(ts) && tord(ts) <= tord(before_ts);
    if vacates.iter().any(|v| {
        v.kind == "agent_team_vacated" && s_str(&v.data, "holder") == Some(name) && between(&v.ts)
    }) {
        return None;
    }
    let level = g.data.get("level").and_then(Value::as_i64)?;
    let scope = s_str(&g.data, "scope")?;
    let theme = crate::paths::AgentsHome::from_env_opt()
        .and_then(|home| crate::team_names::theme_for(&home.team_names_json(), scope));
    Some(crate::team_names::title(
        level as u32,
        scope,
        theme.as_deref(),
    ))
}

/// The pure fold. One `Removal` per receipt, plus one per `registry_row_removed`
/// whose `receipt_staged` is false and whose session id holds no receipt.
pub fn fold(
    receipts: &[(PathBuf, ReapReceipt)],
    agent_events: &str,
    global_events: &str,
) -> Vec<Removal> {
    let agents = parse_events(agent_events);
    let global = parse_events(global_events);
    let causes = cause_map(&agents);
    let verbs = lost_verbs(&agents);
    let uncaptured: Vec<&JEvent> = global
        .iter()
        .filter(|e| e.kind == "agent_session_id_uncaptured")
        .collect();
    let grants: Vec<JEvent> = global
        .iter()
        .filter(|e| e.kind == "agent_teamed")
        .cloned()
        .collect();
    let vacates: Vec<JEvent> = global
        .iter()
        .filter(|e| e.kind == "agent_team_vacated")
        .cloned()
        .collect();

    let mut out: Vec<Removal> = Vec::new();
    for (path, r) in receipts {
        let cause = causes.get(&r.harness_session_id);
        let model = r
            .model_provenance
            .as_ref()
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let node = r
            .ledger
            .as_ref()
            .and_then(|l| l.get("graph_node_id").or_else(|| l.get("node")))
            .and_then(Value::as_str)
            .map(str::to_string);
        out.push(Removal {
            ts: r.reaped_at.clone(),
            name: r.row_name.clone(),
            short_id: Some(r.short_id.clone()),
            harness: Some(r.harness.clone()),
            session_id: Some(r.harness_session_id.clone()),
            node,
            removed_by: r.removed_by.clone(),
            verb: None,
            reason: cause.and_then(|(_, _, why)| why.clone()),
            cause: cause.map(|(kind, _, _)| kind.clone()),
            cause_at: cause.map(|(_, ts, _)| ts.clone()),
            team: team_at(&grants, &vacates, &r.row_name, &r.reaped_at),
            resume: Some(r.resume.clone()),
            cwd: Some(r.cwd.clone()),
            trigger: Some(r.removal_trigger.clone()),
            model,
            receipt: Some(path.clone()),
        });
    }

    let receipt_sids: std::collections::HashSet<&str> = receipts
        .iter()
        .map(|(_, r)| r.harness_session_id.as_str())
        .collect();
    for e in &agents {
        if e.kind != "registry_row_removed" {
            continue;
        }
        if e.data.get("receipt_staged").and_then(Value::as_bool) != Some(false) {
            continue;
        }
        let sid = s_str(&e.data, "harness_session_id")
            .or_else(|| s_str(&e.data, "session_id"))
            .unwrap_or("");
        if receipt_sids.contains(sid) {
            continue;
        }
        let Some(name) = s_str(&e.data, "name") else {
            continue;
        };
        let pid = e
            .data
            .get("pid")
            .and_then(Value::as_i64)
            .map(|p| p.to_string())
            .unwrap_or_default();
        // Why the identity was never captured outranks the removal's own
        // one-liner: the uncaptured event is the deeper cause.
        let reason = uncaptured
            .iter()
            .filter(|u| s_str(&u.data, "name") == Some(name) && tord(&u.ts) <= tord(&e.ts))
            .max_by(|a, b| tord(&a.ts).cmp(&tord(&b.ts)))
            .and_then(|u| s_str(&u.data, "reason"))
            .map(str::to_string)
            .or_else(|| s_str(&e.data, "reason").map(str::to_string));
        out.push(Removal {
            ts: e.ts.clone(),
            name: name.to_string(),
            short_id: s_str(&e.data, "short_id").map(str::to_string),
            harness: s_str(&e.data, "harness").map(str::to_string),
            session_id: (!sid.is_empty()).then(|| sid.to_string()),
            node: None,
            removed_by: s_str(&e.data, "remover").unwrap_or("").to_string(),
            verb: verbs.get(&(pid, tsec(&e.ts))).cloned(),
            reason,
            cause: Some(e.kind.clone()),
            cause_at: Some(e.ts.clone()),
            team: team_at(&grants, &vacates, name, &e.ts),
            resume: None,
            cwd: None,
            trigger: None,
            model: None,
            receipt: None,
        });
    }
    out
}

/// Read every removal record under `home`: the receipts directory, the agents
/// journal (bounded by `since_ms`), and the global journal unbounded, because
/// a team is granted before the window opens. Every unreadable store adds
/// one note and never fails.
pub fn read(home: &AgentsHome, since_ms: Option<i64>) -> (Vec<Removal>, Vec<String>) {
    let mut notes = Vec::new();
    let mut receipts: Vec<(PathBuf, ReapReceipt)> = Vec::new();
    let dir = home.root().join("reap-receipts");
    match std::fs::read_dir(&dir) {
        Err(e) => notes.push(format!(
            "reap-receipts store skipped ({e}): {}",
            dir.display()
        )),
        Ok(entries) => {
            let mut skipped = 0usize;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                match crate::receipt::read_reap_receipt(&path) {
                    Ok(r) => receipts.push((path, r)),
                    Err(_) => skipped += 1,
                }
            }
            if skipped > 0 {
                notes.push(format!("skipped {skipped} unreadable reap receipt(s)"));
            }
        }
    }
    let agent_events = read_journal(
        &home.events_jsonl(),
        EventQuery {
            since_ms,
            ..EventQuery::of_types(&[
                "agent_row_reaped",
                "registry_row_removed",
                "registry_rows_lost",
            ])
        },
        &mut notes,
    );
    let global_events = home
        .root()
        .parent()
        .map(|d| d.join("events.jsonl"))
        .map(|p| {
            read_journal(
                &p,
                EventQuery::of_types(&[
                    "agent_teamed",
                    "agent_team_vacated",
                    "agent_session_id_uncaptured",
                ]),
                &mut notes,
            )
        })
        .unwrap_or_default();
    (fold(&receipts, &agent_events, &global_events), notes)
}

fn read_journal(path: &PathBuf, query: EventQuery, notes: &mut Vec<String>) -> String {
    match crate::event_store::journal_text_checked(path, &query) {
        Ok(raw) => raw,
        Err(e) => {
            notes.push(format!("event store skipped ({e}): {}", path.display()));
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::ReapReceipt;

    fn receipt(sid: &str, name: &str, reaped_at: &str) -> (PathBuf, ReapReceipt) {
        (
            PathBuf::from(format!("/tmp/reap-{name}.json")),
            ReapReceipt {
                row_name: name.into(),
                short_id: "d145".into(),
                harness: "claude".into(),
                harness_session_id: sid.into(),
                cwd: "/tmp/wt".into(),
                log_path: None,
                created_at: "2026-09-04T10:00:00Z".into(),
                reaped_at: reaped_at.into(),
                resume: format!("claude --resume {sid}"),
                ledger: Some(serde_json::json!({"graph_node_id": "x-aaaa"})),
                removed_by: "gc-sweep".into(),
                removal_trigger: "unattended".into(),
                schema_version: Some(2),
                identity: None,
                native_locator: None,
                model_provenance: None,
                resume_argv: Vec::new(),
                effects: Vec::new(),
                assignment: None,
                details_expired_at: None,
                writer_build: None,
                retirement_contract: None,
            },
        )
    }

    fn lines(parts: &[&str]) -> String {
        parts.join("\n")
    }

    /// AC1: the jolly-finch shape. A never-bound teamed row, removed by the
    /// spawn that launched it, its identity never captured.
    #[test]
    fn a_never_bound_teamed_removal_folds_to_one_removal_with_team_and_reason() {
        let agent = lines(&[
            r#"{"ts":"2026-09-28T16:48:49Z","type":"registry_rows_lost","source":"python","data":{"lost":[{"name":"jolly-finch","harness_session_id":""}],"pid":40417,"verb":"fno-py agents spawn --substrate pane --team"}}"#,
            r#"{"ts":"2026-09-28T16:48:49Z","type":"registry_row_removed","source":"python","data":{"harness":"codex","harness_session_id":"","name":"jolly-finch","pid":40417,"reason":"row 'jolly-finch': missing harness session identity","receipt_staged":false,"remover":"fno-py","short_id":""}}"#,
        ]);
        let global = lines(&[
            r#"{"ts":"2026-09-28T16:45:58Z","type":"agent_teamed","source":"python","data":{"grantor":"49a80492","level":2,"name":"jolly-finch","scope":"x-eeee,x-4444","vacated_scope":null}}"#,
            r#"{"ts":"2026-09-28T16:45:58Z","type":"agent_session_id_uncaptured","source":"python","data":{"harness":"codex","name":"jolly-finch","reason":"no unique codex rollout for this cwd after spawn"}}"#,
        ]);
        let out = fold(&[], &agent, &global);
        assert_eq!(out.len(), 1, "{out:?}");
        let r = &out[0];
        assert_eq!(r.name, "jolly-finch");
        assert_eq!(r.session_id, None, "never bound: no session id");
        assert_eq!(r.removed_by, "fno-py");
        assert_eq!(
            r.verb.as_deref(),
            Some("fno-py agents spawn --substrate pane --team")
        );
        assert_eq!(
            r.reason.as_deref(),
            Some("no unique codex rollout for this cwd after spawn")
        );
        assert_eq!(r.team.as_deref(), Some("Lead of x-eeee,x-4444"));
        assert_eq!(r.resume, None, "no receipt, no resume line");
    }

    /// AC2 first half: a receipt and its event fold to exactly ONE removal,
    /// taken from the receipt, carrying the event's basis as the reason.
    #[test]
    fn a_receipted_removal_folds_once_with_the_event_basis_as_reason() {
        let (path, r) = receipt(
            "11111111-2222-3333-4444-555555555555",
            "warden",
            "2026-09-25T16:19:49Z",
        );
        let agent = lines(&[
            r#"{"ts":"2026-09-25T16:19:48Z","type":"registry_row_removed","source":"daemon","data":{"session_id":"11111111-2222-3333-4444-555555555555"}}"#,
            r#"{"ts":"2026-09-25T16:19:49Z","type":"agent_row_reaped","source":"daemon","data":{"harness_session_id":"11111111-2222-3333-4444-555555555555","basis":"every named node done: x-node"}}"#,
        ]);
        let out = fold(std::slice::from_ref(&(path, r)), &agent, "");
        assert_eq!(out.len(), 1, "the receipt wins, the event is its cause");
        let out = &out[0];
        assert_eq!(out.cause.as_deref(), Some("agent_row_reaped"));
        assert_eq!(out.reason.as_deref(), Some("every named node done: x-node"));
        assert!(out.receipt.is_some());
        assert_eq!(
            out.resume.as_deref(),
            Some("claude --resume 11111111-2222-3333-4444-555555555555")
        );
    }

    /// AC2 second half: an unreadable agents store degrades to the receipt
    /// rows plus one note naming the store, never a failure.
    #[test]
    fn an_unreadable_agents_store_yields_receipt_rows_and_a_note() {
        let temp = tempfile::tempdir().unwrap();
        let home = AgentsHome::at(temp.path().join("agents"));
        let receipts = home.root().join("reap-receipts");
        std::fs::create_dir_all(&receipts).unwrap();
        let (path, r) = receipt(
            "11111111-2222-3333-4444-555555555555",
            "warden",
            "2026-09-25T16:19:49Z",
        );
        std::fs::write(
            receipts.join("claude-11111111.json"),
            serde_json::to_string(&r).unwrap(),
        )
        .unwrap();
        // Corrupt the sqlite store the journal read opens.
        std::fs::write(home.events_jsonl().with_file_name("events.db"), b"not a db").unwrap();
        let (out, notes) = read(&home, None);
        assert_eq!(out.len(), 1, "the receipt still folds");
        assert_eq!(out[0].name, "warden");
        assert!(
            notes.iter().any(|n| n.contains("event store skipped")),
            "notes were {notes:?}"
        );
        let _ = path;
    }

    /// A team vacated between grant and removal is not held at removal time.
    #[test]
    fn a_team_vacated_before_the_removal_is_not_reported() {
        let global = lines(&[
            r#"{"ts":"2026-09-28T10:00:00Z","type":"agent_teamed","source":"python","data":{"level":1,"name":"quill","scope":"fno"}}"#,
            r#"{"ts":"2026-09-28T11:00:00Z","type":"agent_team_vacated","source":"python","data":{"cause":"stepped_down","holder":"quill","level":1,"scope":"fno"}}"#,
        ]);
        let agent = r#"{"ts":"2026-09-28T12:00:00Z","type":"registry_row_removed","source":"python","data":{"name":"quill","receipt_staged":false,"remover":"daemon"}}"#;
        let out = fold(&[], agent, &global);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].team, None, "the team was vacated first");
    }
}

//! `fno-agents subscribe` -- stream registry state transitions + pane exits as
//! newline-delimited JSON.
//!
//! Client-side and daemon-free by design: the daemon already commits every
//! transition to the events store beside `events.jsonl` (the
//! `inside_leg_report`, `inside_leg_completed`, and `screen_state_change`
//! kinds it emits at the badge transition edges). `subscribe` follows those
//! committed rows by seq and reshapes them into a stable transition schema,
//! rather than threading a broadcast channel through the hot registry-write
//! path. The store is also strictly better substrate for a work-queue
//! consumer than a bounded drop-oldest broadcast: a slow reader never blocks
//! the daemon (the store is the buffer) and never drops the "agent went
//! idle" event it needs -- it just reads it later.
//!
//! ponytail: no per-subscriber bounded queue / lagged marker / rate coalescing.
//! The seq-follow transport does not have the "slow consumer stalls the daemon"
//! problem those solve, and the daemon's own emit cadence bounds the rate. Add
//! them only if a socket-push transport is ever actually required.

use crate::paths::AgentsHome;
use crate::state::{self, Registry};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(250);

const TRANSITION_TYPES: &[&str] = &[
    "inside_leg_report",
    "inside_leg_buffer_flushed",
    "inside_leg_completed",
    "screen_state_change",
];
/// ponytail: a row whose ts lags its commit by more than this is missed; widen it if that shows up.
const FOLLOW_SKEW_MS: i64 = 60_000;

/// Where the store-follow stands: the last delivered row's seq and the
/// connect-time wall clock the window filter keys on.
struct Cursor {
    seq: i64,
    ts_ms: i64,
}

fn transition_query(since_ms: i64) -> crate::event_store::EventQuery {
    crate::event_store::EventQuery {
        types: TRANSITION_TYPES.iter().map(|t| t.to_string()).collect(),
        since_ms: Some(since_ms),
        ..Default::default()
    }
}

/// The committed transition lines after `cursor`, oldest first; advances it.
/// A missing or busy store reads as nothing and is retried on the next tick.
fn poll(journal: &std::path::Path, cursor: &mut Cursor) -> Vec<String> {
    let Ok(rows) =
        crate::event_store::query_events(journal, &transition_query(cursor.ts_ms - FOLLOW_SKEW_MS))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for row in rows {
        if row.seq <= cursor.seq {
            continue;
        }
        cursor.seq = row.seq;
        out.push(row.line);
    }
    out
}

/// The cursor at connect: the newest committed seq among recent transition
/// rows, so the stream starts after connect as today (0 when none).
fn connect_cursor(journal: &std::path::Path, now_ms: i64) -> Cursor {
    let mut cursor = Cursor {
        seq: 0,
        ts_ms: now_ms,
    };
    if let Ok(rows) =
        crate::event_store::query_events(journal, &transition_query(now_ms - FOLLOW_SKEW_MS))
    {
        if let Some(last) = rows.last() {
            cursor.seq = last.seq;
        }
    }
    cursor
}

/// One normalized transition, reshaped from an `events.jsonl` line. `agent` is
/// `None` for `inside_leg_report` (that event carries only `session_id`); the
/// follow loop resolves it against the registry before emitting.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    /// Original event kind (passed through for provenance).
    pub kind: String,
    /// `"state"` (a working/blocked/idle transition) or `"exit"` (pane/turn end).
    pub category: &'static str,
    /// Which authority decided the verdict: `"hook"` or `"screen"`.
    pub authority: &'static str,
    /// Agent name when the source event carries it, else `None` (resolve by sid).
    pub agent: Option<String>,
    /// The report's session id, present only on `inside_leg_report`.
    pub session_id: Option<String>,
    /// New state label.
    pub state: String,
    /// Per-source monotonic sequence, when the event carries one.
    pub seq: Option<u64>,
}

/// Reshape one `events.jsonl` line into a [`Transition`], or `None` for any
/// non-transition kind (spawn/stop/reconcile/daemon-lifecycle/... are ignored).
pub fn classify(v: &Value) -> Option<Transition> {
    // Unified envelope: `type` + payload under `data`. The `kind`/flat
    // fallback covers the mixed-binary window (an old daemon binary keeps writing
    // the retired shape until `fno agents restart`) and rotated events.jsonl.1 history.
    // Removal criterion: drop the `.or_else(kind)`/flat fallback once the daemon
    // fleet has restarted on the post-change binary and no rotated file carries a
    // `kind` line.
    let kind = v
        .get("type")
        .or_else(|| v.get("kind"))
        .and_then(|x| x.as_str())?;
    let payload = v.get("data").unwrap_or(v);
    let str_field = |k: &str| payload.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let seq = payload.get("seq").and_then(|x| x.as_u64());
    match kind {
        // Hook report: {session_id, seq, state} -- no name, resolved later.
        "inside_leg_report" => Some(Transition {
            kind: kind.to_string(),
            category: "state",
            authority: "hook",
            agent: None,
            session_id: str_field("session_id"),
            state: str_field("state").unwrap_or_default(),
            seq,
        }),
        // Early-push flush: a report buffered before its row existed, applied at
        // row creation. Carries {name, session_id, state, seq} and is the ONLY
        // event for that transition, so a subscriber must surface it too.
        "inside_leg_buffer_flushed" => Some(Transition {
            kind: kind.to_string(),
            category: "state",
            authority: "hook",
            agent: str_field("name"),
            session_id: str_field("session_id"),
            state: str_field("state").unwrap_or_default(),
            seq,
        }),
        // Ordered exit / turn-done teardown: {name, session_id, final_state, seq}.
        "inside_leg_completed" => Some(Transition {
            kind: kind.to_string(),
            category: "exit",
            authority: "hook",
            agent: str_field("name"),
            session_id: str_field("session_id"),
            state: str_field("final_state").unwrap_or_else(|| "done".to_string()),
            seq,
        }),
        // Scrape verdict change: {name, state, rule, seq, cleared}. A cleared
        // verdict (badge dropped) reads as idle. The scrape sweep ALSO emits this
        // kind for a manifest PARSE ERROR ({provider, error}, no name/state) --
        // that is not a row transition, so require `name` and skip otherwise.
        "screen_state_change" => {
            let name = payload.get("name").and_then(|x| x.as_str())?;
            let cleared = payload
                .get("cleared")
                .and_then(|c| c.as_bool())
                .unwrap_or(false);
            let state = if cleared {
                "idle".to_string()
            } else {
                str_field("state").unwrap_or_else(|| "idle".to_string())
            };
            Some(Transition {
                kind: kind.to_string(),
                category: "state",
                authority: "screen",
                agent: Some(name.to_string()),
                session_id: None,
                state,
                seq,
            })
        }
        _ => None,
    }
}

/// Resolve a hook report's `session_id` to a row name using the daemon's own
/// matcher (any provider id field), so the mapping never drifts from spawn.
fn resolve_name(reg: &Registry, session_id: &str) -> Option<String> {
    reg.entries
        .iter()
        .find(|e| crate::daemon::entry_holds_session(e, session_id))
        .map(|e| e.name.clone())
}

/// Runtime filters for one subscribe stream.
struct Filters {
    agent: Option<String>,
    want_state: bool,
    want_exit: bool,
}

/// Classify one raw `events.jsonl` line, resolve its agent name, apply the
/// filters, and emit one NDJSON transition. `reg` caches the registry for
/// session_id->name resolution (refreshed on a miss); `last_state` tracks the
/// prior state per agent so each emission carries `old_state`.
fn process_line(
    line: &str,
    home: &AgentsHome,
    filters: &Filters,
    reg: &mut Option<Registry>,
    last_state: &mut HashMap<String, String>,
) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let Some(t) = classify(&v) else { return };
    match t.category {
        "state" if !filters.want_state => return,
        "exit" if !filters.want_exit => return,
        _ => {}
    }
    // Resolve the name (enrich a name-less hook report by session id).
    let agent = match (t.agent.clone(), &t.session_id) {
        (Some(name), _) => Some(name),
        (None, Some(sid)) => {
            let mut found = reg.as_ref().and_then(|r| resolve_name(r, sid));
            if found.is_none() {
                *reg = state::load_registry(&home.registry_json()).ok();
                found = reg.as_ref().and_then(|r| resolve_name(r, sid));
            }
            found
        }
        (None, None) => None,
    };
    // --agent filter: an unresolved agent can't match.
    if let Some(want) = &filters.agent {
        if agent.as_deref() != Some(want.as_str()) {
            return;
        }
    }
    let old = agent.as_ref().and_then(|a| last_state.get(a).cloned());
    let out_line = json!({
        "agent": agent,
        "event": t.category,
        "state": t.state,
        "old_state": old,
        "authority": t.authority,
        "seq": t.seq,
        "kind": t.kind,
    })
    .to_string();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{out_line}");
    let _ = out.flush();
    if let Some(a) = agent {
        last_state.insert(a, t.state);
    }
}

/// `fno-agents subscribe [--agent <name>] [--kinds state,exit] [--json]`
pub async fn run_subscribe(rest: &[String], home: &AgentsHome) -> i32 {
    let mut agent_filter: Option<String> = None;
    let mut want_state = true;
    let mut want_exit = true;
    let mut kinds_set = false;

    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--agent" => match it.next() {
                Some(v) => agent_filter = Some(v.clone()),
                None => {
                    eprintln!("fno-agents: --agent needs a value");
                    return 2;
                }
            },
            "--kinds" => match it.next() {
                Some(v) => {
                    // First --kinds resets to the named subset; unknown names error.
                    want_state = false;
                    want_exit = false;
                    kinds_set = true;
                    for k in v.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                        match k {
                            "state" => want_state = true,
                            "exit" => want_exit = true,
                            other => {
                                eprintln!("fno-agents: subscribe --kinds must be state|exit (got {other})");
                                return 2;
                            }
                        }
                    }
                }
                None => {
                    eprintln!("fno-agents: --kinds needs a value");
                    return 2;
                }
            },
            // --json is the only output shape (a JSON stream), accepted for parity.
            "--json" | "-J" => {}
            other if other.starts_with("--") => {
                eprintln!("fno-agents: subscribe: unknown flag: {other}");
                return 2;
            }
            other => {
                eprintln!("fno-agents: subscribe: unexpected argument: {other}");
                return 2;
            }
        }
    }
    if kinds_set && !want_state && !want_exit {
        eprintln!("fno-agents: subscribe --kinds selected nothing (use state and/or exit)");
        return 2;
    }

    let filters = Filters {
        agent: agent_filter,
        want_state,
        want_exit,
    };
    let path = home.events_jsonl();
    let mut last_state: HashMap<String, String> = HashMap::new();
    // Cache the registry for session_id->name resolution; refresh on a miss.
    let mut reg: Option<Registry> = state::load_registry(&home.registry_json()).ok();

    // Follow the store by seq: start after connect -- subscribe is a push
    // stream of transitions after connect, not a history dump.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut cursor = connect_cursor(&path, now_ms);

    loop {
        for line in poll(&path, &mut cursor) {
            process_line(&line, home, &filters, &mut reg, &mut last_state);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_hook_report_without_name() {
        let t = classify(&json!({
            "type": "inside_leg_report",
            "data": {"session_id": "sid-1", "seq": 3, "state": "blocked"}
        }))
        .unwrap();
        assert_eq!(t.category, "state");
        assert_eq!(t.authority, "hook");
        assert_eq!(t.agent, None);
        assert_eq!(t.session_id.as_deref(), Some("sid-1"));
        assert_eq!(t.state, "blocked");
        assert_eq!(t.seq, Some(3));
    }

    #[test]
    fn classifies_completion_as_exit() {
        let t = classify(&json!({
            "type": "inside_leg_completed",
            "data": {"name": "wkA", "session_id": "sid-1", "final_state": "done", "seq": 9}
        }))
        .unwrap();
        assert_eq!(t.category, "exit");
        assert_eq!(t.agent.as_deref(), Some("wkA"));
        assert_eq!(t.state, "done");
    }

    #[test]
    fn cleared_screen_state_reads_idle() {
        let t = classify(&json!({
            "type": "screen_state_change",
            "data": {"name": "wkA", "state": Value::Null, "rule": Value::Null, "seq": 2, "cleared": true}
        }))
        .unwrap();
        assert_eq!(t.category, "state");
        assert_eq!(t.authority, "screen");
        assert_eq!(t.agent.as_deref(), Some("wkA"));
        assert_eq!(t.state, "idle");
    }

    #[test]
    fn live_screen_state_keeps_verdict() {
        let t = classify(&json!({
            "type": "screen_state_change",
            "data": {"name": "wkA", "state": "blocked", "rule": "menu", "seq": 4, "cleared": false}
        }))
        .unwrap();
        assert_eq!(t.state, "blocked");
    }

    #[test]
    fn ignores_non_transition_kinds() {
        assert!(classify(&json!({"type": "agent_spawned", "data": {"name": "wkA"}})).is_none());
        assert!(classify(&json!({"type": "daemon_started", "data": {"pid": 1}})).is_none());
        assert!(classify(&json!({"no_type": true})).is_none());
    }

    #[test]
    fn classifies_buffer_flush_as_hook_state() {
        // The early-push flush is the ONLY event for that transition; it carries
        // {name, session_id, state, seq}.
        let t = classify(&json!({
            "type": "inside_leg_buffer_flushed",
            "data": {"name": "wkA", "session_id": "sid-1", "state": "working", "seq": 4}
        }))
        .unwrap();
        assert_eq!(t.category, "state");
        assert_eq!(t.authority, "hook");
        assert_eq!(t.agent.as_deref(), Some("wkA"));
        assert_eq!(t.state, "working");
        assert_eq!(t.seq, Some(4));
    }

    #[test]
    fn screen_state_parse_error_variant_is_ignored() {
        // The scrape sweep emits screen_state_change for a manifest parse error
        // with {provider, error} and no name -- not a row transition, must skip.
        assert!(classify(&json!({
            "type": "screen_state_change",
            "data": {"provider": "codex", "error": "bad manifest"}
        }))
        .is_none());
    }

    #[test]
    fn legacy_kind_flat_line_still_classifies_via_fallback() {
        // Mixed-binary/rotated-history window: an old daemon binary emits the
        // retired {kind, <flat fields>} shape. The fallback must still classify
        // it until the fleet has restarted. Delete with the fallback in classify.
        let t = classify(&json!({
            "kind": "inside_leg_report", "session_id": "sid-1", "seq": 3, "state": "blocked"
        }))
        .unwrap();
        assert_eq!(t.category, "state");
        assert_eq!(t.session_id.as_deref(), Some("sid-1"));
        assert_eq!(t.state, "blocked");
        assert_eq!(t.seq, Some(3));
    }

    fn stamp_rfc3339(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    #[test]
    fn poll_streams_a_row_committed_after_the_cursor() {
        // AC8-HP: the connect cursor skips pre-connect rows; one post-connect
        // commit streams once.
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("events.jsonl");
        let now_ms = chrono::Utc::now().timestamp_millis();
        let pre = json!({"ts": stamp_rfc3339(now_ms - 5_000), "type": "inside_leg_report",
            "source": "daemon", "data": {"session_id": "s0", "state": "working"}});
        crate::event_store::append_envelope(&journal, &pre.to_string(), None).unwrap();
        let mut cursor = connect_cursor(&journal, now_ms);
        let post = json!({"ts": stamp_rfc3339(now_ms + 5_000), "type": "inside_leg_report",
            "source": "daemon", "data": {"session_id": "s1", "state": "blocked"}});
        crate::event_store::append_envelope(&journal, &post.to_string(), None).unwrap();
        let lines = poll(&journal, &mut cursor);
        assert_eq!(
            lines.len(),
            1,
            "only the post-connect row streams: {lines:?}"
        );
        assert!(lines[0].contains("s1"), "{lines:?}");
        assert!(
            poll(&journal, &mut cursor).is_empty(),
            "delivered rows never repeat"
        );
    }

    #[test]
    fn poll_survives_a_missing_store_and_recovers() {
        // AC8-ERR
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("events.jsonl");
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut cursor = Cursor {
            seq: 0,
            ts_ms: now_ms,
        };
        assert!(
            poll(&journal, &mut cursor).is_empty(),
            "no store reads as nothing"
        );
        let row = json!({"ts": stamp_rfc3339(now_ms + 1_000), "type": "screen_state_change",
            "source": "daemon", "data": {"name": "wkA", "state": "working"}});
        crate::event_store::append_envelope(&journal, &row.to_string(), None).unwrap();
        let lines = poll(&journal, &mut cursor);
        assert_eq!(
            lines.len(),
            1,
            "the next poll after the store appears: {lines:?}"
        );
    }
}

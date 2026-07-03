//! `fno-agents subscribe` -- stream registry state transitions + pane exits as
//! newline-delimited JSON.
//!
//! Client-side and daemon-free by design: the daemon already writes every
//! transition to its own append-only `events.jsonl` (the `inside_leg_report`,
//! `inside_leg_completed`, and `screen_state_change` kinds it emits at the badge
//! transition edges). `subscribe` follows that file from EOF and reshapes those
//! kinds into a stable transition schema, rather than threading a broadcast
//! channel through the hot registry-write path. The append-only log is also
//! strictly better substrate for a work-queue consumer than a bounded
//! drop-oldest broadcast: a slow reader never blocks the daemon (the file is the
//! buffer) and never drops the "agent went idle" event it needs -- it just reads
//! it later.
//!
//! ponytail: no per-subscriber bounded queue / lagged marker / rate coalescing.
//! The file-follow transport does not have the "slow consumer stalls the daemon"
//! problem those solve, and the daemon's own emit cadence bounds the rate. Add
//! them only if a socket-push transport is ever actually required.

use crate::paths::AgentsHome;
use crate::state::{self, Registry};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(250);

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
    let kind = v.get("kind")?.as_str()?;
    let str_field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let seq = v.get("seq").and_then(|x| x.as_u64());
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
        // verdict (badge dropped) reads as idle.
        "screen_state_change" => {
            let cleared = v.get("cleared").and_then(|c| c.as_bool()).unwrap_or(false);
            let state = if cleared {
                "idle".to_string()
            } else {
                str_field("state").unwrap_or_else(|| "idle".to_string())
            };
            Some(Transition {
                kind: kind.to_string(),
                category: "state",
                authority: "screen",
                agent: str_field("name"),
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

    let path = home.events_jsonl();
    // Start at EOF: subscribe is a PUSH stream of transitions after connect, not
    // a history dump.
    let mut pos: u64 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut carry = String::new();
    let mut last_state: HashMap<String, String> = HashMap::new();
    // Cache the registry for session_id->name resolution; refresh on a miss.
    let mut reg: Option<Registry> = state::load_registry(&home.registry_json()).ok();

    loop {
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        // Rotation/truncation (events.jsonl -> events.jsonl.1, active recreated):
        // the file shrank, so re-read the fresh file from its start.
        if len < pos {
            pos = 0;
            carry.clear();
        }
        if len > pos {
            if let Ok(mut f) = std::fs::File::open(&path) {
                if f.seek(SeekFrom::Start(pos)).is_ok() {
                    let mut buf = String::new();
                    if let Ok(n) = f.read_to_string(&mut buf) {
                        pos += n as u64;
                        carry.push_str(&buf);
                        while let Some(nl) = carry.find('\n') {
                            let line: String = carry.drain(..=nl).collect();
                            let line = line.trim_end();
                            if line.is_empty() {
                                continue;
                            }
                            let Ok(v) = serde_json::from_str::<Value>(line) else {
                                continue;
                            };
                            let Some(t) = classify(&v) else { continue };
                            // Category filter.
                            match t.category {
                                "state" if !want_state => continue,
                                "exit" if !want_exit => continue,
                                _ => {}
                            }
                            // Resolve the name (enrich inside_leg_report by sid).
                            let agent = match (t.agent.clone(), &t.session_id) {
                                (Some(name), _) => Some(name),
                                (None, Some(sid)) => {
                                    let mut found = reg.as_ref().and_then(|r| resolve_name(r, sid));
                                    if found.is_none() {
                                        reg = state::load_registry(&home.registry_json()).ok();
                                        found = reg.as_ref().and_then(|r| resolve_name(r, sid));
                                    }
                                    found
                                }
                                (None, None) => None,
                            };
                            // --agent filter: an unresolved agent can't match.
                            if let Some(want) = &agent_filter {
                                if agent.as_deref() != Some(want.as_str()) {
                                    continue;
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
                    }
                }
            }
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
            "kind": "inside_leg_report", "session_id": "sid-1", "seq": 3, "state": "blocked"
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
            "kind": "inside_leg_completed", "name": "wkA",
            "session_id": "sid-1", "final_state": "done", "seq": 9
        }))
        .unwrap();
        assert_eq!(t.category, "exit");
        assert_eq!(t.agent.as_deref(), Some("wkA"));
        assert_eq!(t.state, "done");
    }

    #[test]
    fn cleared_screen_state_reads_idle() {
        let t = classify(&json!({
            "kind": "screen_state_change", "name": "wkA",
            "state": Value::Null, "rule": Value::Null, "seq": 2, "cleared": true
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
            "kind": "screen_state_change", "name": "wkA",
            "state": "blocked", "rule": "menu", "seq": 4, "cleared": false
        }))
        .unwrap();
        assert_eq!(t.state, "blocked");
    }

    #[test]
    fn ignores_non_transition_kinds() {
        assert!(classify(&json!({"kind": "agent_spawned", "name": "wkA"})).is_none());
        assert!(classify(&json!({"kind": "daemon_started", "pid": 1})).is_none());
        assert!(classify(&json!({"no_kind": true})).is_none());
    }
}

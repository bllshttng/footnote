//! May this `fno agents resume` launch? A resume can bring an exited session
//! back after its node or PR took a different live holder; relaunching then
//! puts a second writer on one branch (measured 2026-09-18 on PR 2187: a
//! `dead` reading reassigned the PR, the resume fast-forwarded the branch
//! beside the replacement worker). One gate answers the question before any
//! launch.

use serde_json::Value;

use crate::claims::{self, AcquireOutcome, ClaimState};
use crate::client_verbs::py_repr_str;
use crate::gc_sweep;
use crate::graph_store::{entry_id, is_open_do_row, work_state_key};
use crate::king_board::prs::{node_pr_refs, nodes_binding_pr};
use crate::king_board::{is_terminal, s_str};
use crate::paths::AgentsHome;
use crate::state;
use crate::truth_probe::{family1_truth_probe_many, TruthProbe};

/// Refusal exit: the session's node or PR now has a different live holder.
/// 17 is unused in the resume family (16 belongs to resume_wake and
/// pane_relaunch).
pub const RESUME_REASSIGNED_EXIT: i32 = 17;

/// The revival window a node reservation holds: long enough to relaunch, and
/// freed early by pid death (the claim is pid-liveness anchored).
const RESERVE_TTL_MS: i64 = 900_000;

/// The other holder the gate found.
pub(crate) struct OtherHolder {
    /// Node whose claim the other holder keeps.
    pub node: String,
    /// The session's own node the resume would collide with.
    pub session_node: String,
    pub holder: String,
    /// The PR at stake: the session node's primary, or the binding that
    /// joined the holder's node to it.
    pub pr: Option<i64>,
}

/// Non-terminal entries with a `sessions[]` row of phase `do` for this
/// session id (case-insensitive).
fn session_nodes<'a>(entries: &'a [Value], session_id: &str) -> Vec<&'a Value> {
    entries
        .iter()
        .filter(|e| !is_terminal(e))
        .filter(|e| {
            e.get("sessions")
                .and_then(Value::as_array)
                .is_some_and(|rows| {
                    rows.iter().any(|r| {
                        s_str(r, "phase").is_some_and(|p| p.eq_ignore_ascii_case("do"))
                            && s_str(r, "session_id")
                                .is_some_and(|s| s.eq_ignore_ascii_case(session_id))
                    })
                })
        })
        .collect()
}

/// Session ids with an open `do` row on `node_id`. A planner's ended
/// blueprint row records who planned the node, never who writes it.
fn open_do_sessions(entries: &[Value], node_id: &str) -> Vec<String> {
    entries
        .iter()
        .filter(|e| entry_id(e) == Some(node_id))
        .filter_map(|e| e.get("sessions").and_then(Value::as_array))
        .flatten()
        .filter(|r| is_open_do_row(r))
        .filter_map(|r| s_str(r, "session_id"))
        .map(|s| work_state_key(s.trim()))
        .collect()
}

fn holder_is_self(holder: &str, session_id: &str) -> bool {
    holder
        .split_once(':')
        .map(|(_, id)| id)
        .unwrap_or(holder)
        .eq_ignore_ascii_case(session_id)
}

/// The first node claim held (Live or Suspect) by someone other than this
/// session: the session's own `do` nodes first, then every node bound to one
/// of their PRs. Pure: `holder_of` answers for one `node:<id>` key.
pub(crate) fn other_holder(
    entries: &[Value],
    session_id: &str,
    holder_of: &dyn Fn(&str) -> Option<String>,
) -> Option<OtherHolder> {
    let mine = session_nodes(entries, session_id);
    for node in &mine {
        let Some(id) = entry_id(node) else {
            continue;
        };
        if let Some(holder) = holder_of(&format!("node:{id}")) {
            if !holder_is_self(&holder, session_id) {
                return Some(OtherHolder {
                    node: id.to_string(),
                    session_node: id.to_string(),
                    holder,
                    pr: node_pr_refs(node).into_iter().next().map(|(n, _)| n),
                });
            }
        }
    }
    for node in &mine {
        for (pr, _) in node_pr_refs(node) {
            for bound in nodes_binding_pr(entries, pr) {
                if mine.iter().any(|n| entry_id(n) == Some(bound)) {
                    continue; // its own claim was already checked above
                }
                if let Some(holder) = holder_of(&format!("node:{bound}")) {
                    if !holder_is_self(&holder, session_id) {
                        return Some(OtherHolder {
                            node: bound.to_string(),
                            session_node: entry_id(node).unwrap_or("").to_string(),
                            holder,
                            pr: Some(pr),
                        });
                    }
                }
            }
        }
    }
    None
}

/// The reachable registry row that currently writes `node_id`, excluding the
/// resuming session itself. The claim-store half is a snapshot: a claim can
/// expire while the worker it held keeps answering. The roster half closes
/// that gap for direct stamps and open `do` rows.
fn roster_holder_with(
    home: &AgentsHome,
    entries: &[Value],
    node_id: &str,
    self_id: &str,
    probe_many: &dyn Fn(&[String]) -> std::collections::HashMap<String, TruthProbe>,
) -> Option<String> {
    let registry = state::load_registry(&home.registry_json()).ok()?;
    let open = open_do_sessions(entries, node_id);
    let me = work_state_key(self_id.trim());
    let mut tokens: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for e in &registry.entries {
        let session_key = e
            .harness_session_id
            .as_deref()
            .map(|sid| work_state_key(sid.trim()));
        let is_self = !me.is_empty() && session_key.as_deref() == Some(me.as_str());
        let stamped = e.node.as_deref() == Some(node_id)
            || session_key.as_ref().is_some_and(|sid| open.contains(sid));
        if !stamped || is_self {
            continue;
        }
        tokens.push(
            e.harness_session_id
                .clone()
                .unwrap_or_else(|| e.name.clone()),
        );
        names.push(e.name.clone());
    }
    let probes = probe_many(&tokens);
    for (token, name) in tokens.iter().zip(&names) {
        if probes
            .get(token)
            .is_some_and(|p| p.reachability.as_deref() == Some("reachable"))
        {
            return Some(name.clone());
        }
    }
    None
}

fn roster_holder(
    home: &AgentsHome,
    entries: &[Value],
    node_id: &str,
    self_id: &str,
) -> Option<String> {
    roster_holder_with(home, entries, node_id, self_id, &family1_truth_probe_many)
}

/// The holder predicate the gate hands to `other_holder`: the claim store
/// first (Live or Suspect), then the roster.
fn gate_holder_of(
    home: &AgentsHome,
    entries: &[Value],
    key: &str,
    self_id: &str,
) -> Option<String> {
    let (claim_state, rec) = claims::status(key, None);
    if matches!(claim_state, ClaimState::Live | ClaimState::Suspect) {
        return rec.map(|r| r.holder).filter(|h| !h.is_empty());
    }
    let node = key.strip_prefix("node:")?;
    roster_holder(home, entries, node, self_id)
}

fn refused_line(
    home: &AgentsHome,
    entries: &[Value],
    session_id: &str,
    row_name: &str,
) -> Option<i32> {
    let Some(hit) = other_holder(entries, session_id, &|key| {
        gate_holder_of(home, entries, key, session_id)
    }) else {
        return None;
    };
    let pr_clause = hit.pr.map(|pr| format!(" (PR #{pr})")).unwrap_or_default();
    let who = holder_handle(&hit.holder);
    eprintln!(
        "fno agents resume: refused: node {sn}{pr_clause} is now held by {holder} on node {node}. \
Resuming {name} would put a second writer on that branch. Stop or hand off that holder first; \
read it with fno agents truth {who}.",
        sn = hit.session_node,
        pr_clause = pr_clause,
        holder = hit.holder,
        node = hit.node,
        name = py_repr_str(row_name),
        who = who,
    );
    Some(RESUME_REASSIGNED_EXIT)
}

/// Gate plus atomic reservation. A dispatch racing this resume is decided by
/// the claim file itself: the reserve acquires `node:<id>` under the
/// resuming session's own holder, and same-holder acquire is idempotent, so
/// the revived session's own claim refreshes the reservation instead of
/// fighting it. `session_id` is always the full session id, including on the
/// claude resume arms whose row handle is only a short id.
pub fn gate_and_reserve(home: &AgentsHome, row_name: &str, session_id: &str) -> Option<i32> {
    let Some(entries) = gc_sweep::read_graph_rows(home) else {
        eprintln!("fno agents resume: warning: graph unreadable, holder check skipped");
        return None;
    };
    if let Some(code) = refused_line(home, &entries, session_id, row_name) {
        return Some(code);
    }
    let holder = format!("target-session:{session_id}");
    reserve_nodes(&entries, session_id, row_name, &holder, None)
}

/// The reserve half: acquire every session node under the resuming session's
/// holder. A dispatch that wins the race holds the claim file; the resume
/// refuses naming it. Root is injectable for tests; production passes the
/// machine claims root.
fn reserve_nodes(
    entries: &[Value],
    session_id: &str,
    row_name: &str,
    holder: &str,
    root: Option<&std::path::Path>,
) -> Option<i32> {
    for node in session_nodes(entries, session_id) {
        let Some(id) = entry_id(node) else {
            continue;
        };
        let opts = claims::AcquireOpts {
            pid: Some(std::process::id()),
            ttl_ms: Some(RESERVE_TTL_MS),
            reason: Some("resume reserve".into()),
            root: root.map(|p| p.to_path_buf()),
            ..Default::default()
        };
        match claims::acquire(&format!("node:{id}"), holder, opts) {
            AcquireOutcome::Acquired(_) => {}
            AcquireOutcome::HeldByOther { holder: other, .. } => {
                let who = holder_handle(&other);
                eprintln!(
                    "fno agents resume: refused: node {id} was just claimed by {other}. \
Resuming {name} would put a second writer on that branch. Stop or hand off that holder first; \
read it with fno agents truth {who}.",
                    id = id,
                    other = other,
                    name = py_repr_str(row_name),
                    who = who,
                );
                return Some(RESUME_REASSIGNED_EXIT);
            }
            AcquireOutcome::Error(e) => {
                // Same posture as an unreadable graph: the reserve degrades
                // to the gate alone, it never blocks the revival.
                eprintln!(
                    "fno agents resume: warning: reserve on node {id} failed ({e}); continuing"
                );
            }
        }
    }
    None
}

fn holder_handle(holder: &str) -> &str {
    holder.split_once(':').map(|(_, id)| id).unwrap_or(holder)
}

/// `run_resume`'s two cwd refusals live here beside the holder check: all
/// three answer "may this resume launch?". Message text is byte-identical to
/// the blocks they moved out of client_verbs.rs.
pub(crate) fn missing_cwd_refusal(name: &str, session_id: &str) -> i32 {
    if !session_id.is_empty() {
        eprintln!(
            "fno agents resume: agent {} has no recorded cwd. \
             the row is the resume handle and still carries the session id: the harness \
             itself can reach the session directly (e.g. claude --resume <id>). To re-drive \
             it under fno, rm this row and `fno agents adopt <id>` rebinds a fresh one \
             with a live cwd - that pair spends the recorded route bindings. rm alone just \
             deletes the handle.",
            py_repr_str(name)
        );
    } else {
        // Neither cwd nor session id: nothing to resume and nothing to
        // rebind. Do not claim an id the row does not carry.
        eprintln!(
            "fno agents resume: agent {} has no recorded cwd and no session id. \
             The row holds nothing resumable; rm it and re-spawn is the honest cleanup, \
             and nothing is lost.",
            py_repr_str(name)
        );
    }
    13
}

pub(crate) fn gone_cwd_refusal(cwd: &str, name: &str) -> i32 {
    eprintln!(
        "fno agents resume: cwd {} for {} is no longer reachable. Check whether the path \
         is recoverable first (renamed worktree base, unmounted volume): \
         the row is the resume handle. To re-drive the session under fno from a live cwd, \
         rm this row and `fno agents adopt <id>` rebinds a fresh one; rm alone deletes the \
         handle and the session binding with it. rm is for a path that is gone for good.",
        py_repr_str(cwd),
        py_repr_str(name)
    );
    13
}

/// Ask the spawn gate before a revival launches. A revival counts against the
/// revived row's own parent, never whoever runs the verb.
pub(crate) fn admit_revival(
    home: &AgentsHome,
    verb: &str,
    row_name: &str,
    worker_cwd: &std::path::Path,
) -> Result<crate::spawn_gate::GateGuard, i32> {
    admit_revival_with(home, verb, row_name, |input| {
        crate::spawn_gate::run_gate(worker_cwd, &home.registry_json(), input)
    })
}

pub(crate) fn release_revival_claims(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    let session_key = format!("session:{session_id}");
    let session_holder = format!("resume:{}", std::process::id());
    let _ = claims::release(&session_key, &session_holder, None, None);

    let node_holder = format!("target-session:{session_id}");
    if let Ok(records) = claims::list(Some("node:"), None, true) {
        for record in records
            .into_iter()
            .filter(|record| record.holder == node_holder)
        {
            let _ = claims::release(&record.key, &node_holder, None, None);
        }
    }
}

pub(crate) fn admit_revival_with<G>(
    home: &AgentsHome,
    verb: &str,
    row_name: &str,
    gate: G,
) -> Result<crate::spawn_gate::GateGuard, i32>
where
    G: FnOnce(
        crate::spawn_gate::GateInput,
    ) -> Result<crate::spawn_gate::GateGuard, crate::spawn_gate::Refusal>,
{
    let row = state::load_registry(&home.registry_json())
        .ok()
        .and_then(|registry| {
            registry
                .entries
                .into_iter()
                .find(|entry| entry.name == row_name)
        });
    let input = crate::spawn_gate::GateInput {
        name: row_name.to_string(),
        substrate: "bg".to_string(),
        account: row.as_ref().and_then(|entry| entry.launch_account.clone()),
        caller_session: row
            .as_ref()
            .and_then(|entry| entry.spawned_by_session.clone()),
        ..Default::default()
    };
    match gate(input) {
        Ok(guard) => Ok(guard),
        Err(refusal) => {
            if let Some(receipt) = &refusal.receipt {
                println!("{receipt}");
            }
            eprintln!(
                "fno agents {verb}: the spawn gate refused reviving {row_name}; nothing was launched. FNO_SPAWN_GATE=0 skips the gate for one run."
            );
            let mut fields: Vec<(String, Value)> = refusal.event.into_iter().collect();
            fields.extend([
                ("name".to_string(), Value::String(row_name.to_string())),
                ("verb".to_string(), Value::String(verb.to_string())),
                ("substrate".to_string(), Value::String("bg".to_string())),
                ("gate".to_string(), Value::String("revival".to_string())),
                ("exit_code".to_string(), Value::from(refusal.exit_code)),
            ]);
            let event_fields: Vec<(&str, Value)> = fields
                .iter()
                .map(|(key, value)| (key.as_str(), value.clone()))
                .collect();
            crate::client_verbs::append_agents_event(
                &crate::client_verbs::trace_events_path(home),
                "spawn_gate_refused",
                &event_fields,
            );
            Err(refusal.exit_code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn holder_of_staged<'a>(
        map: HashMap<&'a str, &'a str>,
    ) -> impl Fn(&str) -> Option<String> + 'a {
        move |key: &str| map.get(key).map(|h| h.to_string())
    }

    fn do_entry(id: &str, session_id: &str) -> Value {
        json!({
            "id": id,
            "status": "in_progress",
            "sessions": [{"phase": "do", "session_id": session_id}],
        })
    }

    // AC3-HP, the 2026-09-18 shape: the session's node free, the new node
    // bound to its PR through additional_prs, live holder there.
    #[test]
    fn binding_held_by_another_node_refuses() {
        let sid = "8c58eaf1-old";
        let entries = vec![
            json!({
                "id": "x-aaaa",
                "status": "in_progress",
                "pr_number": 2187,
                "sessions": [{"phase": "do", "session_id": sid}],
            }),
            json!({
                "id": "x-bbbb",
                "status": "in_progress",
                "additional_prs": [{"number": 2187, "url": "https://example.com/pr/2187"}],
                "sessions": [{"phase": "do", "session_id": "fbf271b1-new"}],
            }),
        ];
        let holder_of = holder_of_staged(HashMap::from([(
            "node:x-bbbb",
            "target-session:fbf271b1-ee03",
        )]));
        let hit = other_holder(&entries, sid, &holder_of).expect("must refuse");
        assert_eq!(hit.node, "x-bbbb");
        assert_eq!(hit.session_node, "x-aaaa");
        assert_eq!(hit.pr, Some(2187));
        assert_eq!(hit.holder, "target-session:fbf271b1-ee03");
    }

    // AC3-HP: the session's own node claim taken over directly.
    #[test]
    fn own_node_claim_taken_over_refuses() {
        let sid = "8c58eaf1-old";
        let entries = vec![json!({
            "id": "x-aaaa",
            "status": "in_progress",
            "pr_number": 2187,
            "sessions": [{"phase": "do", "session_id": sid}],
        })];
        let holder_of = holder_of_staged(HashMap::from([(
            "node:x-aaaa",
            "target-session:fbf271b1-ee03",
        )]));
        let hit = other_holder(&entries, sid, &holder_of).expect("must refuse");
        assert_eq!(hit.node, "x-aaaa");
        assert_eq!(hit.pr, Some(2187));
    }

    // AC3-HP: a suspect holder (TTL unexpired, pid unproven) refuses too.
    #[test]
    fn suspect_holder_refuses() {
        let sid = "8c58eaf1-old";
        let entries = vec![do_entry("x-aaaa", sid)];
        let holder_of = holder_of_staged(HashMap::from([(
            "node:x-aaaa",
            "target-session:suspect-one",
        )]));
        assert!(other_holder(&entries, sid, &holder_of).is_some());
    }

    // AC4-EDGE: the only live holder is this same session.
    #[test]
    fn own_session_holder_does_not_refuse() {
        let sid = "8c58eaf1-old";
        let entries = vec![do_entry("x-aaaa", sid)];
        let holder_of = holder_of_staged(HashMap::from([(
            "node:x-aaaa",
            "target-session:8c58eaf1-old",
        )]));
        assert!(other_holder(&entries, sid, &holder_of).is_none());
    }

    #[test]
    fn holder_handle_prints_the_whole_handle() {
        assert_eq!(holder_handle("king-fno-g6"), "king-fno-g6");
        assert_eq!(
            holder_handle("target-session:01a0c61c-c000-70c0-8dd4-dcb7cd9e27d4"),
            "01a0c61c-c000-70c0-8dd4-dcb7cd9e27d4"
        );
    }

    // AC4-EDGE: no node carries a do-row for this session.
    #[test]
    fn no_session_node_does_not_refuse() {
        let entries = vec![do_entry("x-aaaa", "someone-else")];
        let holder_of = holder_of_staged(HashMap::from([(
            "node:x-aaaa",
            "target-session:fbf271b1-ee03",
        )]));
        assert!(other_holder(&entries, "8c58eaf1-old", &holder_of).is_none());
    }

    // AC4-EDGE: a terminal node is not a session node, however fresh its row.
    #[test]
    fn terminal_session_node_does_not_refuse() {
        let sid = "8c58eaf1-old";
        let mut entry = do_entry("x-aaaa", sid);
        entry["status"] = json!("done");
        let holder_of = holder_of_staged(HashMap::from([(
            "node:x-aaaa",
            "target-session:fbf271b1-ee03",
        )]));
        assert!(other_holder(&[entry], sid, &holder_of).is_none());
    }

    // AC5-ERR: an unreadable graph warns and never blocks.
    #[test]
    fn unreadable_graph_skips_the_check() {
        let dir = std::env::temp_dir().join(format!(
            "fno-resume-gate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        std::fs::write(dir.join("graph.json"), b"{not json").unwrap();
        let home = AgentsHome::at(dir.join("agents"));
        assert_eq!(gate_and_reserve(&home, "row", "sid"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn registry_home(tag: &str) -> (AgentsHome, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "fno-resume-gate-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        (AgentsHome::at(dir.join("agents")), dir)
    }

    fn reachable_probe() -> TruthProbe {
        TruthProbe {
            state: "working".to_string(),
            provider_refusal: None,
            harness_title: None,
            reachability: Some("reachable".to_string()),
            basis: Some("transcript".to_string()),
            last_activity_age_s: Some(30.0),
            last_activity_basis: None,
            last_event_at: None,
            last_message: None,
            observed_model: Value::Null,
        }
    }

    // P1 (PR 2249 codex review): the claim store is a snapshot. A replacement
    // worker whose node claim expired but whose transcript still answers
    // reachable is still the holder; the roster half of the predicate must
    // find it.
    #[test]
    fn roster_answers_when_the_claim_expired() {
        let (home, dir) = registry_home("roster");
        crate::state::update_registry(&home.registry_json(), |r| {
            r.entries.push(
                serde_json::from_str(
                    r#"{"name":"t-other","harness":"claude","harness_session_id":"uuid-other",
                    "node":"x-aaaa","status":"live","cwd":"/tmp/x",
                    "created_at":"2026-09-01T00:00:00Z"}"#,
                )
                .unwrap(),
            )
        })
        .unwrap();
        let entries = vec![do_entry("x-aaaa", "8c58eaf1-old")];
        let probes = HashMap::from([("uuid-other".to_string(), reachable_probe())]);
        let hit = roster_holder_with(&home, &entries, "x-aaaa", "8c58eaf1-old", &|toks| {
            toks.iter()
                .filter_map(|t| probes.get(t).cloned().map(|p| (t.clone(), p)))
                .collect()
        });
        assert_eq!(hit.as_deref(), Some("t-other"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn add_registry_row(home: &AgentsHome, name: &str, session_id: &str, node: Option<&str>) {
        crate::state::update_registry(&home.registry_json(), |r| {
            r.entries.push(
                serde_json::from_value(json!({
                    "name": name,
                    "harness": "claude",
                    "harness_session_id": session_id,
                    "node": node,
                    "status": "live",
                    "cwd": "/tmp/x",
                    "created_at": "2026-09-01T00:00:00Z"
                }))
                .unwrap(),
            )
        })
        .unwrap();
    }

    fn open_do_entry(id: &str, session_id: &str) -> Value {
        json!({
            "id": id,
            "status": "in_progress",
            "sessions": [{
                "phase": "do",
                "harness": "codex",
                "session_id": session_id,
                "started_at": "2026-09-22T10:00:00Z"
            }]
        })
    }

    #[test]
    fn ended_blueprint_row_is_not_a_holder() {
        let (home, dir) = registry_home("ended-blueprint");
        add_registry_row(&home, "king-fno-g6", "uuid-king", None);
        let entries = vec![json!({
            "id": "x-aaaa",
            "status": "in_progress",
            "sessions": [
                {
                    "phase": "blueprint",
                    "harness": "claude",
                    "session_id": "uuid-king",
                    "started_at": "2026-09-21T22:00:00Z",
                    "ended_at": "2026-09-21T22:31:41Z"
                },
                {
                    "phase": "do",
                    "harness": "codex",
                    "session_id": "uuid-resuming",
                    "started_at": "2026-09-22T10:00:00Z"
                }
            ]
        })];
        let probes = HashMap::from([("uuid-king".to_string(), reachable_probe())]);
        let hit = roster_holder_with(&home, &entries, "x-aaaa", "uuid-resuming", &|toks| {
            toks.iter()
                .filter_map(|t| probes.get(t).cloned().map(|p| (t.clone(), p)))
                .collect()
        });
        assert_eq!(hit, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_do_row_from_another_session_holds() {
        let (home, dir) = registry_home("open-do");
        add_registry_row(&home, "t-other", "uuid-other", None);
        let entries = vec![open_do_entry("x-aaaa", "uuid-other")];
        let probes = HashMap::from([("uuid-other".to_string(), reachable_probe())]);
        let hit = roster_holder_with(&home, &entries, "x-aaaa", "uuid-resuming", &|toks| {
            toks.iter()
                .filter_map(|t| probes.get(t).cloned().map(|p| (t.clone(), p)))
                .collect()
        });
        assert_eq!(hit.as_deref(), Some("t-other"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ended_do_row_is_not_a_holder() {
        let (home, dir) = registry_home("ended-do");
        add_registry_row(&home, "t-other", "uuid-other", None);
        let mut entry = open_do_entry("x-aaaa", "uuid-other");
        entry["sessions"][0]["ended_at"] = json!("2026-09-22T10:30:00Z");
        let probes = HashMap::from([("uuid-other".to_string(), reachable_probe())]);
        let hit = roster_holder_with(&home, &[entry], "x-aaaa", "uuid-resuming", &|toks| {
            toks.iter()
                .filter_map(|t| probes.get(t).cloned().map(|p| (t.clone(), p)))
                .collect()
        });
        assert_eq!(hit, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resuming_session_row_never_holds_against_itself() {
        let (home, dir) = registry_home("self-row");
        add_registry_row(&home, "t-self", "uuid-self", Some("x-aaaa"));
        add_registry_row(&home, "t-other", "uuid-other", Some("x-aaaa"));
        let entries = vec![open_do_entry("x-aaaa", "uuid-self")];
        let probes = HashMap::from([
            ("uuid-self".to_string(), reachable_probe()),
            ("uuid-other".to_string(), reachable_probe()),
        ]);
        let hit = roster_holder_with(&home, &entries, "x-aaaa", "uuid-self", &|toks| {
            toks.iter()
                .filter_map(|t| probes.get(t).cloned().map(|p| (t.clone(), p)))
                .collect()
        });
        assert_eq!(hit.as_deref(), Some("t-other"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The reserve half: a dispatch that wins the race holds the claim file,
    // and the resume refuses naming it. A quiet run acquires the seat under
    // the resuming session's own holder.
    #[test]
    fn reserve_refuses_when_a_dispatch_wins_the_race() {
        let (_, dir) = registry_home("reserve");
        let entries = vec![do_entry("x-aaaa", "8c58eaf1-old")];
        let claims_root = dir.join("claims-root");
        std::fs::create_dir_all(&claims_root).unwrap();
        let opts = crate::claims::AcquireOpts {
            ttl_ms: Some(60_000),
            root: Some(claims_root.clone()),
            ..Default::default()
        };
        match crate::claims::acquire("node:x-aaaa", "target-session:the-dispatch", opts) {
            crate::claims::AcquireOutcome::Acquired(_) => {}
            other => panic!("fixture claim failed: {other:?}"),
        }
        let refused = reserve_nodes(
            &entries,
            "8c58eaf1-old",
            "t-old-row",
            "target-session:8c58eaf1-old",
            Some(&claims_root),
        );
        assert_eq!(refused, Some(RESUME_REASSIGNED_EXIT));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reserve_acquires_the_seat_and_a_second_pass_reenters() {
        let (_, dir) = registry_home("reserve-ok");
        let entries = vec![do_entry("x-aaaa", "8c58eaf1-old")];
        let claims_root = dir.join("claims-root");
        std::fs::create_dir_all(&claims_root).unwrap();
        let holder = "target-session:8c58eaf1-old";
        assert_eq!(
            reserve_nodes(
                &entries,
                "8c58eaf1-old",
                "t-old-row",
                holder,
                Some(&claims_root)
            ),
            None
        );
        let (st, rec) = crate::claims::status("node:x-aaaa", Some(&claims_root));
        assert!(matches!(
            st,
            crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect
        ));
        assert_eq!(rec.map(|r| r.holder), Some(holder.to_string()));
        // Same-holder acquire is idempotent: the revived session's own claim
        // refreshes the reservation instead of fighting it.
        assert_eq!(
            reserve_nodes(
                &entries,
                "8c58eaf1-old",
                "t-old-row",
                holder,
                Some(&claims_root)
            ),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn admit_revival_charges_the_row_parent_and_account() {
        let (home, dir) = registry_home("admit-input");
        crate::state::update_registry(&home.registry_json(), |registry| {
            registry.entries.push(crate::state::RegistryEntry {
                name: "w1".to_string(),
                harness: Some("claude".to_string()),
                harness_session_id: Some("sess-uuid".to_string()),
                launch_account: Some("makers".to_string()),
                spawned_by_session: Some("k1-parent".to_string()),
                ..Default::default()
            });
        })
        .unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = seen.clone();
        assert!(admit_revival_with(&home, "resume", "w1", move |input| {
            *capture.lock().unwrap() = Some(input);
            Ok(crate::spawn_gate::GateGuard::default())
        })
        .is_ok());
        let input = seen.lock().unwrap().take().unwrap();
        assert_eq!(input.name, "w1");
        assert_eq!(input.substrate, "bg");
        assert_eq!(input.account.as_deref(), Some("makers"));
        assert_eq!(input.caller_session.as_deref(), Some("k1-parent"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn admit_revival_asks_without_parent_when_row_is_missing() {
        let (home, dir) = registry_home("admit-missing");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let capture = seen.clone();
        assert!(admit_revival_with(&home, "recover", "w1", move |input| {
            *capture.lock().unwrap() = Some(input);
            Ok(crate::spawn_gate::GateGuard::default())
        })
        .is_ok());
        let input = seen.lock().unwrap().take().unwrap();
        assert_eq!(input.caller_session, None);
        assert_eq!(input.account, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn admit_revival_records_refusal_and_returns_gate_code() {
        let (home, dir) = registry_home("admit-refusal");
        let code = admit_revival_with(&home, "resume", "w1", |_| {
            Err(crate::spawn_gate::Refusal::code(83).ev("axis", Value::String("slots".to_string())))
        })
        .unwrap_err();
        assert_eq!(code, 83);
        let events =
            std::fs::read_to_string(crate::client_verbs::trace_events_path(&home)).unwrap();
        assert!(events.contains("\"kind\":\"spawn_gate_refused\""));
        assert!(events.contains("\"name\":\"w1\""));
        assert!(events.contains("\"verb\":\"resume\""));
        assert!(events.contains("\"gate\":\"revival\""));
        let _ = std::fs::remove_dir_all(dir);
    }
}

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
use crate::graph_store::{entry_id, sessions_index};
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

/// The gate `run_resume` consults. `None` means "may launch": either no other
/// holder, or the graph could not be read (one warning, never a block).
/// The claim-store half is a snapshot: a claim can expire while the worker it
/// held keeps answering. The roster half closes that gap - registry rows
/// stamped (directly, or through a graph session row) to this node whose
/// transcript probe answers reachable. The probe is the liveness instrument;
/// the batch answers for every candidate in ONE process.
fn roster_holder_with(
    home: &AgentsHome,
    entries: &[Value],
    node_id: &str,
    probe_many: &dyn Fn(&[String]) -> std::collections::HashMap<String, TruthProbe>,
) -> Option<String> {
    let registry = state::load_registry(&home.registry_json()).ok()?;
    let index = sessions_index(entries);
    let mut tokens: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for e in &registry.entries {
        let stamped = e.node.as_deref() == Some(node_id)
            || e.harness_session_id.as_deref().is_some_and(|sid| {
                index
                    .get(sid.trim())
                    .is_some_and(|rows| rows.iter().any(|(n, _)| n == node_id))
            });
        if !stamped {
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

fn roster_holder(home: &AgentsHome, entries: &[Value], node_id: &str) -> Option<String> {
    roster_holder_with(home, entries, node_id, &family1_truth_probe_many)
}

/// The holder predicate the gate hands to `other_holder`: the claim store
/// first (Live or Suspect), then the roster.
fn gate_holder_of(home: &AgentsHome, entries: &[Value], key: &str) -> Option<String> {
    let (claim_state, rec) = claims::status(key, None);
    if matches!(claim_state, ClaimState::Live | ClaimState::Suspect) {
        return rec.map(|r| r.holder).filter(|h| !h.is_empty());
    }
    let node = key.strip_prefix("node:")?;
    roster_holder(home, entries, node)
}

fn refused_line(
    home: &AgentsHome,
    entries: &[Value],
    session_id: &str,
    row_name: &str,
) -> Option<i32> {
    let Some(hit) = other_holder(entries, session_id, &|key| {
        gate_holder_of(home, entries, key)
    }) else {
        return None;
    };
    let pr_clause = hit.pr.map(|pr| format!(" (PR #{pr})")).unwrap_or_default();
    let who = holder_short(&hit.holder);
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

/// The gate `run_resume` consults. `None` means "may launch": either no other
/// holder, or the graph could not be read (one warning, never a block).
pub fn refuse_if_reassigned(home: &AgentsHome, session_id: &str, row_name: &str) -> Option<i32> {
    let Some(entries) = gc_sweep::read_graph_rows(home) else {
        eprintln!("fno agents resume: warning: graph unreadable, holder check skipped");
        return None;
    };
    refused_line(home, &entries, session_id, row_name)
}

/// Gate plus atomic reservation. A dispatch racing this resume is decided by
/// the claim file itself: the reserve acquires `node:<id>` under the
/// resuming session's own holder, and same-holder acquire is idempotent, so
/// the revived session's own claim refreshes the reservation instead of
/// fighting it. `gate_id` is the holder id part (`claim_uuid` on the
/// relaunch arm, else the row's session id).
pub fn gate_and_reserve(
    home: &AgentsHome,
    session_id: &str,
    row_name: &str,
    gate_id: &str,
) -> Option<i32> {
    let Some(entries) = gc_sweep::read_graph_rows(home) else {
        eprintln!("fno agents resume: warning: graph unreadable, holder check skipped");
        return None;
    };
    if let Some(code) = refused_line(home, &entries, session_id, row_name) {
        return Some(code);
    }
    let holder = format!("target-session:{gate_id}");
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
                let who = holder_short(&other);
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

fn holder_short(holder: &str) -> &str {
    let id = holder.split_once(':').map(|(_, id)| id).unwrap_or(holder);
    id.get(..8).unwrap_or(id)
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
        assert_eq!(refuse_if_reassigned(&home, "sid", "row"), None);
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
        let hit = roster_holder_with(&home, &entries, "x-aaaa", &|toks| {
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
}

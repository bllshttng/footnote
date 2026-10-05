//! `fno-agents worked-nodes`: the worked-overlay join, ported from Python's
//! `live_worked_node_ids` (cli/src/fno/graph/statuses.py).
//!
//! One JSON payload on stdin (or `--rows-file <path>`, `-` = stdin):
//!   {"rows": [{"name": str, "label": str, "session": str,
//!              "node": str|null}]}
//! `label` is the display string Python formatted after its transcript
//! liveness pass (reachability already applied); `session` is "" for rows no
//! harness session id names (the unmeasurable attribution fold).
//!
//! One JSON reply on stdout:
//!   {"worked": {"<node-id>": ["<label>", ...]}}
//!
//! The gate this verb owns (the crown ruling on the dispatch false refusal):
//!   - a crowned lead row is never a node worker (registry `crown_level`);
//!   - a seat record alone is a witness: a seated session joins its node
//!     only when the row's own attribution names the node or the session
//!     holds the live/suspect `node:<id>` claim (the gate the reap keep got
//!     first);
//!   - an attributed row joins through its attribution, never closed, never
//!     crowned;
//!   - the unmeasurable fold joins through attribution, never for a crown.
//!
//! Graph semantics ported byte-for-byte from the Python join: terminal rungs
//! `done`/`superseded` skip; a ship row is a link event, never occupancy; a
//! session is open while any non-ship phase row lacks `ended_at`; a session
//! whose every row closed is finished with the node and admits nothing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;

use serde_json::Value;

/// Terminal rungs, mirrored from `TERMINAL_RUNGS` in
/// `cli/src/fno/graph/statuses.py:43`.
const TERMINAL_RUNGS: [&str; 2] = ["done", "superseded"];

/// One payload row after parsing. `node` is the row's own dispatch
/// attribution (registry birth fact, worktree name, or ledger), which the
/// provenance gate compares against the seat's node.
struct GateRow {
    name: String,
    label: String,
    session: String,
    node: Option<String>,
}

/// The seat facts one graph node carries: open non-ship session ids, the
/// closed-minus-open receipt that skips a finished session, in row order.
struct NodeSeats {
    open: Vec<String>,
    closed_minus_open: HashSet<String>,
}

fn parse_rows(payload: &Value) -> Result<Vec<GateRow>, String> {
    let items = payload
        .get("rows")
        .and_then(|v| v.as_array())
        .ok_or("payload carries no rows array")?;
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let obj = item.as_object().ok_or("payload row is not an object")?;
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let label = obj
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or(&name)
            .to_string();
        let session = obj
            .get("session")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let node = obj
            .get("node")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        rows.push(GateRow {
            name,
            label,
            session,
            node,
        });
    }
    Ok(rows)
}

/// One valid phase row shape check, ported from `is_open_phase_row`:
/// phase/harness/session_id/started_at non-empty strings, `ended_at` absent
/// or null counts open. Ship rows never reach here (the caller skips them).
fn open_row(row: &Value) -> Option<String> {
    let ended_open = row.get("ended_at").map(|v| v.is_null()).unwrap_or(true);
    let (phase, harness, sid, started) = (
        row.get("phase").and_then(|v| v.as_str()),
        row.get("harness").and_then(|v| v.as_str()),
        row.get("session_id").and_then(|v| v.as_str()),
        row.get("started_at").and_then(|v| v.as_str()),
    );
    match (phase, harness, sid, started) {
        (Some(p), Some(h), Some(s), Some(t))
            if !p.is_empty() && !h.is_empty() && !s.is_empty() && !t.is_empty() && ended_open =>
        {
            Some(s.to_string())
        }
        _ => None,
    }
}

fn node_seats(entry: &Value) -> Option<(String, NodeSeats)> {
    let id = entry.get("id").and_then(|v| v.as_str())?;
    if id.is_empty() {
        return None;
    }
    let status = entry.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if TERMINAL_RUNGS.contains(&status) {
        return None;
    }
    let mut open = Vec::new();
    let mut closed: HashSet<String> = HashSet::new();
    for row in entry.get("sessions").and_then(|v| v.as_array())? {
        // The outer guard of Python's closed_worker_session_ids: a dict row
        // carrying string session_id and phase fields, ship rows skipped as
        // link events.
        let Some(sid) = row.get("session_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(phase) = row.get("phase").and_then(|v| v.as_str()) else {
            continue;
        };
        if phase.is_empty() || sid.is_empty() || phase == "ship" {
            continue;
        }
        if open_row(row).is_some() {
            open.push(sid.to_string());
        } else {
            // Any non-ship row that is not open-shaped closes its session:
            // the receipt is "no open row anywhere", not ended_at alone.
            closed.insert(sid.to_string());
        }
    }
    let closed_minus_open: HashSet<String> = closed
        .iter()
        .filter(|sid| !open.contains(sid))
        .cloned()
        .collect();
    Some((
        id.to_string(),
        NodeSeats {
            open,
            closed_minus_open,
        },
    ))
}

/// The join: seat records, crown exclusion, provenance, attribution fold.
/// Deterministic: nodes in graph row order, seats in session-row order, the
/// attribution fold in payload order, labels deduped per node.
fn join_worked(
    rows: &[GateRow],
    graph: &[Value],
    crowned: &HashSet<String>,
    claims: &HashMap<String, HashSet<String>>,
) -> BTreeMap<String, Vec<String>> {
    let by_session: HashMap<&str, &GateRow> = rows
        .iter()
        .filter(|r| !r.session.is_empty())
        .map(|r| (r.session.as_str(), r))
        .collect();
    let mut worked: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in graph {
        let Some((node_id, seats)) = node_seats(entry) else {
            continue;
        };
        let claimed = claims.get(&node_id);
        let admit = |label: &str, worked: &mut BTreeMap<String, Vec<String>>| {
            let list = worked.entry(node_id.clone()).or_default();
            if !list.iter().any(|l| l == label) {
                list.push(label.to_string());
            }
        };
        for sid in &seats.open {
            if seats.closed_minus_open.contains(sid) {
                continue;
            }
            let Some(row) = by_session.get(sid.as_str()) else {
                continue;
            };
            if crowned.contains(&row.name) {
                // A crowned lead row is never a node worker.
                continue;
            }
            let attributed = row.node.as_deref() == Some(node_id.as_str());
            let claim_held = claimed.is_some_and(|sids| sids.contains(sid));
            if !attributed && !claim_held {
                // A bare seat record is a witness, never ownership.
                continue;
            }
            admit(&row.label, &mut worked);
        }
        for row in rows {
            if row.node.as_deref() != Some(node_id.as_str()) {
                continue;
            }
            if crowned.contains(&row.name) {
                continue;
            }
            if !row.session.is_empty() && seats.closed_minus_open.contains(&row.session) {
                continue;
            }
            admit(&row.label, &mut worked);
        }
    }
    worked
}

fn crowned_names(registry_path: &PathBuf) -> Result<HashSet<String>, String> {
    let entries = crate::client_verbs::load_registry_entries(registry_path)?;
    Ok(entries
        .iter()
        .filter(|e| e.get("crown_level").map(|v| !v.is_null()).unwrap_or(false))
        .filter_map(|e| e.get("name").and_then(|v| v.as_str()))
        .filter(|n| !n.is_empty())
        .map(|n| n.to_string())
        .collect())
}

fn claim_sessions() -> HashMap<String, HashSet<String>> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    let Some(dir) = crate::claims_root::global_claims_dir() else {
        return out;
    };
    // Live and suspect only (`include_stale = false`); a claims outage drops
    // the provenance supplement, never fails the gate - attribution is the
    // primary half.
    let records = crate::claims::list_in(&[dir], Some("node:"), false).unwrap_or_default();
    for rec in records {
        let Some(node) = rec.key.strip_prefix("node:") else {
            continue;
        };
        let Some(sid) = rec.session_id.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        out.entry(node.to_string())
            .or_default()
            .insert(sid.to_string());
    }
    out
}

/// `fno-agents worked-nodes`: payload on stdin, reply on stdout.
/// Exit 0 on a reply; exit 2 on an unreadable payload; exit 3 on a graph or
/// registry read failure (fail closed: a blank answer would free occupied
/// nodes).
pub fn run_worked_nodes(args: &[String]) -> i32 {
    let mut rows_text = String::new();
    let mut from_file: Option<&String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--rows-file" {
            from_file = iter.next();
        }
    }
    match from_file {
        Some(path) if path != "-" => {
            rows_text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("worked-nodes: rows file unreadable: {e}");
                    return 2;
                }
            };
        }
        _ => {
            if std::io::stdin().read_to_string(&mut rows_text).is_err() {
                eprintln!("worked-nodes: stdin unreadable");
                return 2;
            }
        }
    }
    let payload: Value = match serde_json::from_str(&rows_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("worked-nodes: payload is not valid JSON: {e}");
            return 2;
        }
    };
    let rows = match parse_rows(&payload) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("worked-nodes: {e}");
            return 2;
        }
    };
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        eprintln!("worked-nodes: agents home unresolved");
        return 3;
    };
    let Some(graph) = crate::gc_sweep::read_graph_rows(&home) else {
        eprintln!("worked-nodes: graph read failed; refusing to answer free");
        return 3;
    };
    let registry_path = home.registry_json();
    let crowned = match crowned_names(&registry_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("worked-nodes: {e}; refusing to answer free");
            return 3;
        }
    };
    let claims = claim_sessions();
    let worked = join_worked(&rows, &graph, &crowned, &claims);
    let reply = serde_json::json!({ "worked": worked });
    println!("{}", reply);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, session: &str, node: Option<&str>) -> GateRow {
        GateRow {
            name: name.to_string(),
            label: name.to_string(),
            session: session.to_string(),
            node: node.map(|n| n.to_string()),
        }
    }

    fn node(id: &str, status: &str, sessions: Value) -> Value {
        serde_json::json!({"id": id, "status": status, "sessions": sessions})
    }

    fn open(sid: &str) -> Value {
        serde_json::json!({
            "phase": "execute", "harness": "claude", "session_id": sid,
            "started_at": "2026-10-05T00:00:00Z"
        })
    }

    fn closed(sid: &str) -> Value {
        serde_json::json!({
            "phase": "execute", "harness": "claude", "session_id": sid,
            "started_at": "2026-10-05T00:00:00Z",
            "ended_at": "2026-10-05T01:00:00Z"
        })
    }

    fn crowns(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn claims(map: &[(&str, &[&str])]) -> HashMap<String, HashSet<String>> {
        map.iter()
            .map(|(n, sids)| (n.to_string(), sids.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[test]
    fn a_seated_uncrowned_worker_with_attribution_joins() {
        let graph = vec![node("x-1", "in_progress", serde_json::json!([open("s-1")]))];
        let rows = vec![row("t-w", "s-1", Some("x-1"))];
        let worked = join_worked(&rows, &graph, &HashSet::new(), &HashMap::new());
        assert_eq!(worked.get("x-1").unwrap(), &vec!["t-w".to_string()]);
    }

    #[test]
    fn a_crowned_lead_is_never_the_worker_at_any_fold() {
        let graph = vec![node(
            "x-1",
            "in_progress",
            serde_json::json!([open("crown-s"), open("s-2")]),
        )];
        // The same crown at all three admissions: a seated attributed row,
        // an unseated attributed row, and the sessionless unmeasurable name.
        let rows = vec![
            row("finch", "crown-s", Some("x-1")),
            row("finch", "", Some("x-1")),
            row("finch", "", Some("x-1")),
            row("t-w", "s-2", Some("x-1")),
        ];
        let worked = join_worked(&rows, &graph, &crowns(&["finch"]), &HashMap::new());
        assert_eq!(worked.get("x-1").unwrap(), &vec!["t-w".to_string()]);
    }

    #[test]
    fn a_bare_seat_record_without_provenance_is_a_witness() {
        let graph = vec![node("x-1", "in_progress", serde_json::json!([open("s-1")]))];
        let rows = vec![row("drifter", "s-1", None)];
        let worked = join_worked(&rows, &graph, &HashSet::new(), &HashMap::new());
        assert!(worked.is_empty());
    }

    #[test]
    fn the_node_claim_proves_a_seat_record() {
        let graph = vec![node("x-1", "in_progress", serde_json::json!([open("s-1")]))];
        let rows = vec![row("drifter", "s-1", None)];
        let worked = join_worked(
            &rows,
            &graph,
            &HashSet::new(),
            &claims(&[("x-1", &["s-1"])]),
        );
        assert_eq!(worked.get("x-1").unwrap(), &vec!["drifter".to_string()]);
    }

    #[test]
    fn an_attribution_without_a_seat_still_joins_its_node() {
        let graph = vec![node("x-1", "in_progress", serde_json::json!([]))];
        let rows = vec![row("t-fold", "s-9", Some("x-1"))];
        let worked = join_worked(&rows, &graph, &HashSet::new(), &HashMap::new());
        assert_eq!(worked.get("x-1").unwrap(), &vec!["t-fold".to_string()]);
    }

    #[test]
    fn a_ship_row_is_a_link_event_never_occupancy() {
        let ship = serde_json::json!([{
            "phase": "ship", "harness": "claude",
            "session_id": "s-1", "started_at": "2026-10-05T00:00:00Z"
        }]);
        let graph = vec![node("x-1", "in_review", ship)];
        let rows = vec![row("t-w", "s-1", None)];
        let worked = join_worked(&rows, &graph, &HashSet::new(), &HashMap::new());
        assert!(worked.is_empty());
    }

    #[test]
    fn a_closed_session_frees_the_node() {
        let graph = vec![node(
            "x-1",
            "in_progress",
            serde_json::json!([closed("s-1")]),
        )];
        let rows = vec![row("t-w", "s-1", Some("x-1"))];
        let worked = join_worked(&rows, &graph, &HashSet::new(), &HashMap::new());
        assert!(worked.is_empty());
    }

    #[test]
    fn a_terminal_rung_never_joins() {
        let graph = vec![node("x-1", "done", serde_json::json!([open("s-1")]))];
        let rows = vec![row("t-w", "s-1", Some("x-1"))];
        let worked = join_worked(&rows, &graph, &HashSet::new(), &HashMap::new());
        assert!(worked.is_empty());
    }

    #[test]
    fn labels_pass_through_and_dedupe_per_node() {
        let graph = vec![node(
            "x-1",
            "in_progress",
            serde_json::json!([open("s-1"), open("s-2")]),
        )];
        let mut unmeasured = row("t-u", "s-2", Some("x-1"));
        unmeasured.label = "t-u (unmeasurable: no positive liveness evidence)".to_string();
        let rows = vec![row("t-w", "s-1", Some("x-1")), unmeasured];
        let worked = join_worked(&rows, &graph, &HashSet::new(), &HashMap::new());
        assert_eq!(
            worked.get("x-1").unwrap(),
            &vec![
                "t-w".to_string(),
                "t-u (unmeasurable: no positive liveness evidence)".to_string(),
            ]
        );
    }
}

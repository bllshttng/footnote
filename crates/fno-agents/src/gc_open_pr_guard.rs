//! The open-PR reap guard and the keep reader it leans on. The
//! commit-time backstop for a node whose PR is still open: the driver row
//! is kept and resumed rather than reaped without a recorded termination,
//! and every reap that does go through on an open-PR node files the alert
//! naming the PR. The decision-phase open-PR keep (`open_pr_verdict`)
//! lives here too, so one module answers the whole open-PR question.

use crate::events::EventEmitter;
use crate::gc_sweep::{GraphRead, RetireOrder};
use crate::node_route;
use crate::paths::AgentsHome;
use crate::state;
use serde_json::json;
use std::collections::BTreeMap;

/// The GitHub read Locked Decision 2 pays: `Some(true)` = PR still open,
/// `Some(false)` = merged or closed, `None` = the read failed. The caller
/// owns the per-pass cache; the verdict itself stays network-free.
pub type PrStateRead<'a> = &'a mut dyn FnMut(u64, &str) -> Option<bool>;

/// The open-PR question, asked once for both sweeps. The graph record
/// names the candidate; the PR itself settles it.
#[derive(Debug)]
pub enum OpenPrVerdict {
    /// The PR reads open: hold, and name it.
    Holds { node: String, pr: u64 },
    /// The PR reads merged or closed: this session has nothing left to
    /// drive, and the row falls through to the grace gate.
    Settled { node: String, pr: u64 },
    /// The read failed, or no reader was supplied: hold, and say so.
    Unread { node: String, pr: u64 },
    /// No candidate: no pr_number, a recorded merge, or this session
    /// never drove the node.
    None,
}

/// The candidate conjuncts are exactly the three the open-PR keep has
/// always used: the node carries `pr_number`, its recorded `merge_status`
/// is not `merged`, and this session has a `do` row on it. The attribution
/// gate is unchanged, so a session that never drove the PR pays no read.
/// `quiet_past_grace` is the read gate: a row inside the grace window is
/// kept by `Active` anyway and a row with no age by `TranscriptUnresolved`
/// anyway, so the read would change no verdict - pass `false` and every
/// candidate holds, which is the keep's behavior before it learned to ask.
pub fn open_pr_verdict(
    graph: &GraphRead,
    sid: &str,
    node: &str,
    cwd: &str,
    quiet_past_grace: bool,
    mut pr_read: Option<PrStateRead>,
) -> OpenPrVerdict {
    let pr = match graph.pr_number.get(node).copied().flatten() {
        Some(pr) => pr,
        None => return OpenPrVerdict::None,
    };
    let merged = graph
        .pr_state
        .get(node)
        .and_then(|(merge_status, _, _)| merge_status.clone())
        .as_deref()
        == Some("merged");
    let drives = graph
        .do_nodes
        .get(&sid.to_ascii_lowercase())
        .is_some_and(|set| set.contains(node));
    if merged || !drives {
        return OpenPrVerdict::None;
    }
    if !quiet_past_grace {
        return OpenPrVerdict::Holds {
            node: node.to_string(),
            pr,
        };
    }
    match pr_read.as_mut().map(|f| f(pr, cwd)) {
        Some(Some(false)) => OpenPrVerdict::Settled {
            node: node.to_string(),
            pr,
        },
        Some(Some(true)) => OpenPrVerdict::Holds {
            node: node.to_string(),
            pr,
        },
        // No reader, or the reader itself failed: both are an unread answer.
        _ => OpenPrVerdict::Unread {
            node: node.to_string(),
            pr,
        },
    }
}

/// Does the fno graph hold an OPEN PR on any node this row's session names?
/// The liveness sweeps' busy read: a worker whose node carries a
/// recorded `pr_number` with no recorded merge is waiting on a CI run or a
/// merge, and its transcript silence is the watching protocol, not idle
/// death. Read from the graph rows (the row-verdict path), never from
/// transcript quiet time; an unrecorded `merge_status` holds, because the
/// ship stamp can lag the open PR. A recorded terminal state settles:
/// `merged`, and `closed` - the writer's terminal spelling for a PR closed
/// unmerged, which this path has no live read to override. A headless row
/// is a one-shot run, not a seated worker: its normal exit must stay
/// observable, so the hold never covers it.
pub fn row_has_open_pr(graph: &GraphRead, e: &state::RegistryEntry) -> bool {
    if e.substrate.as_deref() == Some("headless") {
        return false;
    }
    let sid = e
        .harness_session_id
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if sid.is_empty() {
        return false;
    }
    graph
        .index
        .get(&crate::graph_store::work_state_key(sid))
        .is_some_and(|nodes| {
            nodes.iter().any(|(node, _)| {
                graph.pr_number.get(node).copied().flatten().is_some()
                    && !matches!(
                        graph.pr_state.get(node).and_then(|(m, _, _)| m.as_deref()),
                        Some("merged") | Some("closed")
                    )
            })
        })
}

/// The node's recorded primary PR when the graph does not say merged:
/// `None` leaves the guard and the alert silent for that node.
fn recorded_open_pr(graph: &GraphRead, node: &str) -> Option<u64> {
    let pr = graph.pr_number.get(node).cloned().flatten()?;
    let merged = graph
        .pr_state
        .get(node)
        .and_then(|(merge_status, _, _)| merge_status.clone())
        .as_deref()
        == Some("merged");
    (!merged).then_some(pr)
}

/// Refuse every order whose node still carries an open PR while the
/// session never recorded a dispatch termination: the row is kept, a
/// resume task filed, and the refusal returned as `(id, reason)` pairs
/// for the caller's kept report. An explicit operator release and a
/// live-settled PR read outrank the guard.
pub(crate) fn hold_open_pr_reaps(
    home: &AgentsHome,
    emitter: &EventEmitter,
    caller: &str,
    entries: &[state::RegistryEntry],
    to_retire: &mut BTreeMap<String, RetireOrder>,
    graph: &GraphRead,
) -> Vec<(String, String)> {
    let mut kept = Vec::new();

    for name in to_retire.keys().cloned().collect::<Vec<_>>() {
        let Some(order) = to_retire.get(&name) else {
            continue;
        };
        if order.released || order.via_release || order.pr_settled_live {
            continue;
        }
        let order_id = order.id.clone();
        let Some(e) = entries.iter().find(|e| e.name == name) else {
            continue;
        };
        let node = match crate::daemon::dispatch_node_id(&e.name) {
            Some(node) => node,
            None => {
                let sid = e.harness_session_id.as_deref().unwrap_or("").trim();
                match node_route::resolve(e, sid, &graph, None).node {
                    Some(node) => node,
                    None => continue,
                }
            }
        };
        let Some(pr) = recorded_open_pr(graph, &node) else {
            continue;
        };
        match crate::daemon::dispatch_termination(home, e, &node) {
            crate::daemon::DispatchTermination::Found(_) => continue,
            crate::daemon::DispatchTermination::Unknown(err) => {
                to_retire.remove(&name);
                kept.push((order_id, format!("termination unread ({err}); row kept")));
                continue;
            }
            crate::daemon::DispatchTermination::Absent(_) => {}
        }
        to_retire.remove(&name);
        let sid = e.harness_session_id.clone().unwrap_or_default();
        // The resume task rides the row's own dispatch identity: a driver
        // row names its target loop, so `fno agents resume` lands on the
        // session that owns the PR. A helper row that merely resolves to
        // the node (a wake spawn, a keyed peer) has no loop to resume, so
        // it is kept and named in the report without a task.
        let dispatch_named = crate::daemon::dispatch_node_id(&e.name).is_some();
        if dispatch_named {
            let text = format!(
                "reap-keep: node {node} still has open PR {pr} but its driver \
                 row {name} was about to be reaped with no termination event. \
                 The row is kept; resume the session so the PR keeps its owner."
            );
            // The event rides the task's own dedupe: a duplicate filing is
            // the row's earlier refusal already on record, so the identical
            // event is not re-emitted every sweep pass until the PR settles.
            match crate::fleet_task::file_once(
                &crate::provider_cap::questions_path(home),
                "reap-keep",
                &node,
                &e.cwd,
                &text,
                (!sid.is_empty())
                    .then(|| format!("fno agents resume {sid}"))
                    .as_deref(),
                Some(&node),
            ) {
                Ok(crate::fleet_task::Filed::New(_)) => {
                    let _ = emitter.emit(
                        "worker_reap_refused",
                        &json!({
                            "name": name,
                            "node": node,
                            "pr": pr,
                            "session_id": sid,
                            "caller": caller,
                        }),
                    );
                }
                Ok(crate::fleet_task::Filed::Duplicate(_)) => {}
                Err(err) => {
                    eprintln!("gc-sweep: reap-keep task refused: {err}");
                }
            }
        }
        kept.push((
            order_id,
            if dispatch_named {
                format!("open pr {pr} with no termination event; row kept, resume filed")
            } else {
                format!("open pr {pr} with no termination event; helper row kept")
            },
        ));
    }
    kept
}

/// File the alert for a reap that went through while the node still
/// carries an open PR: the PR lost its driver row, and the loss is named
/// the hour it happens, not when a human notices a CONFLICTING check.
pub(crate) fn alert_open_pr_reap(
    home: &AgentsHome,
    receipt_node: Option<&str>,
    graph_join: Option<&GraphRead>,
    e: &state::RegistryEntry,
) {
    if let Some(node) = receipt_node {
        let pr_open = graph_join.and_then(|g| recorded_open_pr(g, node));
        if let Some(pr) = pr_open {
            let text = format!(
                "reap-alert: node {node}'s PR {pr} lost its driver row ({}) \
                 to the reap sweep while the PR is still open. Watch the PR: \
                 it can go CONFLICTING with no owner.",
                e.name
            );
            if let Err(err) = crate::fleet_task::file_once(
                &crate::provider_cap::questions_path(home),
                "reap-alert",
                node,
                &e.cwd,
                &text,
                Some(&format!("fno do pr status {pr}")),
                Some(node),
            ) {
                eprintln!("gc-sweep: reap-alert task refused: {err}");
            }
        }
    }
}

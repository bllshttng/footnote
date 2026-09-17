//! The mux server's launch surface : node dispatch (extracted from
//! `server.rs`) and the sideline launcher's correlated attempts.
//!
//! Node dispatch (prefix+g / card click) keeps its exact behavior: board
//! selection + the one door shell-out + a one-line notice. The launcher adds
//! a correlated, in-flight-deduplicated exchange so a popup launch can be
//! tracked to a birth fact without a second spawn implementation: one
//! `AgentLaunchRequest` = one canonical `fno agents spawn` attempt, decoded
//! by the shared [`crate::dispatch_launch`] decoder.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use crate::dispatch_launch::{
    decode_launch_outcome, launch_spawn_argv, run_fno_captured, run_fno_captured_with_stdin,
    LaunchOutcome,
};
use crate::proto::agent_launch::{AgentLaunchRequest, AgentLaunchUpdate, LaunchState};
use crate::proto::ServerMsg;

/// Node dispatch's off-loop body (extracted verbatim from `server.rs`,
/// move). Selection + spawn crosses subprocesses and a mux socket
/// round-trip, so the budget is seconds, not the digest's 800ms; a hung
/// dispatch still fails open to a notice rather than wedging.
pub(crate) async fn run_dispatch_one(
    session: &str,
    node: Option<&str>,
    account: Option<&str>,
) -> String {
    let dispatch_timeout = crate::dispatch_launch::dispatch_timeout();
    let deadline = tokio::time::Instant::now() + dispatch_timeout;
    let fno = super::fno_bin().display().to_string();

    // Steps 1-2 (change 3): resolve the node identity. A targeted node
    // (a clicked work-queue card) pins its id and reads through
    // `fno backlog get`; without it the board's own order picks (`fno backlog
    // next`, `null` or empty output on an empty bench).
    let picked = if let Some(pinned) = node {
        let argv = [fno.as_str(), "backlog", "get", pinned];
        let answer = match run_fno_captured(&argv, dispatch_timeout, deadline).await {
            Some((true, out, _)) => crate::dispatch_launch::node_identity(&out)
                .ok_or_else(|| "grab work failed: the node record carries no id".to_string()),
            _ => Err("grab work failed: the node read produced no answer".to_string()),
        };
        answer
    } else {
        let argv = [fno.as_str(), "backlog", "next"];
        let answer = match run_fno_captured(&argv, dispatch_timeout, deadline).await {
            Some((true, out, _)) => crate::dispatch_launch::node_identity(&out),
            _ => None,
        };
        answer.ok_or_else(|| "no ready work".to_string())
    };
    let (node_id, slug, parent) = match picked {
        Ok(identity) => identity,
        Err(notice) => return notice,
    };

    // Step 3: the door launches. The argv builder is pure and unit-pinned; no
    // --harness/--model/--route and no message ride, so the grid picks the
    // lane and the door renders the seed.
    let argv = crate::dispatch_launch::dispatch_spawn_argv(
        &fno,
        &node_id,
        session,
        account,
        parent.as_deref(),
    );
    let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
    // Step 4: the outcome maps to the operator's one-liner. Both streams are
    // captured - the door's refusal receipt lives on stderr.
    match crate::dispatch_launch::run_fno_captured(&borrowed, dispatch_timeout, deadline).await {
        None => "grab work: timed out".to_string(),
        Some((exit_ok, out, err)) => crate::dispatch_launch::dispatch_notice(
            exit_ok,
            &out,
            &err,
            &node_id,
            slug.as_deref().unwrap_or(""),
        ),
    }
}

/// In-flight + finished attempt memory for the launcher .
///
/// The desk answers the plan's two hard rules: one request id = ONE spawn
/// attempt (a duplicate submission replays the attempt in flight or its
/// terminal update, never a second process), and a finished attempt's
/// outcome outlives the client round-trip so a reopened popup can show what
/// happened. Bounded: the last [`DESK_RETENTION`] finished attempts are
/// kept, oldest evicted.
#[derive(Default)]
pub(crate) struct LaunchDesk {
    /// Request ids currently running off-loop. A duplicate id here replays
    /// `Starting` and starts nothing.
    pending: HashSet<u64>,
    /// Terminal updates, insertion-ordered for eviction.
    finished: HashMap<u64, AgentLaunchUpdate>,
    order: VecDeque<u64>,
}

/// Finished attempts remembered for replay. Small: a popup session rarely
/// exceeds a handful, and eviction only costs a reopened popup its history.
const DESK_RETENTION: usize = 64;

impl LaunchDesk {
    fn in_flight_or_done(&self, request_id: u64) -> Option<AgentLaunchUpdate> {
        if let Some(update) = self.finished.get(&request_id) {
            return Some(update.clone());
        }
        self.pending
            .contains(&request_id)
            .then(|| AgentLaunchUpdate {
                request_id,
                state: LaunchState::Starting,
            })
    }

    fn mark_started(&mut self, request_id: u64) {
        self.pending.insert(request_id);
    }

    /// Read-only replay of an attempt's terminal state (test reader).
    #[cfg(test)]
    pub(super) fn settled_state(&self, request_id: u64) -> Option<LaunchState> {
        self.finished.get(&request_id).map(|u| u.state.clone())
    }

    fn settle(&mut self, update: AgentLaunchUpdate) {
        self.pending.remove(&update.request_id);
        if !self.finished.contains_key(&update.request_id) {
            self.order.push_back(update.request_id);
        }
        while self.order.len() > DESK_RETENTION {
            let evict = self.order.pop_front().expect("order nonempty");
            self.finished.remove(&evict);
        }
        self.finished.insert(update.request_id, update);
    }
}

/// The launcher's pre-birth validation: everything checkable WITHOUT the
/// spawn door, refused before any effect. The door itself stays the
/// authority on harness support, routing, capacity and permissions.
fn validate_launch_request(req: &AgentLaunchRequest) -> Result<(), String> {
    if req.harness.trim().is_empty() {
        return Err("no harness selected".to_string());
    }
    if !matches!(req.substrate.as_str(), "pane" | "thread") {
        return Err(format!("unsupported substrate {:?}", req.substrate));
    }
    let cwd = std::path::Path::new(&req.cwd);
    if !cwd.is_absolute() {
        return Err(format!("project path {:?} is not absolute", req.cwd));
    }
    if !cwd.is_dir() {
        return Err(format!("project path {:?} does not exist", req.cwd));
    }
    if req.message.chars().count() > crate::proto::MAX_MAIL_TEXT {
        return Err(format!(
            "message too long (max {} chars)",
            crate::proto::MAX_MAIL_TEXT
        ));
    }
    Ok(())
}

/// One attempt's budget. Same shape as node dispatch's: the door can take
/// seconds, never minutes.
fn launch_timeout() -> Duration {
    crate::dispatch_launch::dispatch_timeout()
}

impl super::Core {
    /// "Grab work" (prefix+g): dispatch the next ready backlog node into
    /// a new pane. Board selection is `fno backlog next`; the launch is the door
    /// (`fno agents spawn`), shelled OFF the core loop in a detached
    /// task so a slow backlog read never stalls a pane. The launched pane
    /// appears through the existing registry reader; the outcome (dispatched /
    /// no-work / refusal / failure) routes back as `DispatchResult` for a
    /// one-line notice. (move from `server.rs`.)
    pub(super) fn dispatch_next(&mut self, id: u64, node: Option<String>, account: Option<String>) {
        let session = self.session_name.clone();
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice = run_dispatch_one(&session, node.as_deref(), account.as_deref()).await;
            let _ = core_tx
                .send(super::CoreMsg::DispatchResult { id, notice })
                .await;
        });
    }

    /// One sideline launcher request : validate pre-birth, answer
    /// duplicates with the SAME attempt, then run exactly one canonical
    /// spawn off the core loop. Closing/reopening the popup or losing the
    /// client never cancels a running attempt - the desk owns the truth, so
    /// a lost reply reads `Unknown` client-side instead of becoming a
    /// second process.
    pub(super) fn agent_launch(&mut self, id: u64, req: AgentLaunchRequest) {
        if let Some(update) = self.launch_desk.in_flight_or_done(req.request_id) {
            self.send_launch_update(id, update);
            return;
        }
        if let Err(reason) = validate_launch_request(&req) {
            let update = AgentLaunchUpdate {
                request_id: req.request_id,
                state: LaunchState::Refused { reason },
            };
            self.launch_desk.settle(update.clone());
            self.send_launch_update(id, update);
            return;
        }
        self.launch_desk.mark_started(req.request_id);
        self.send_launch_update(
            id,
            AgentLaunchUpdate {
                request_id: req.request_id,
                state: LaunchState::Starting,
            },
        );
        let session = self.session_name.clone();
        let core_tx = self.self_tx.clone();
        let request_id = req.request_id;
        tokio::spawn(async move {
            let timeout = launch_timeout();
            let deadline = tokio::time::Instant::now() + timeout;
            let fno = super::fno_bin().display().to_string();
            let argv = launch_spawn_argv(&fno, &req, &session);
            let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
            let state = match run_fno_captured_with_stdin(
                &borrowed,
                req.message.as_bytes(),
                timeout,
                deadline,
            )
            .await
            {
                // A timed-out door is the ambiguous case: the child was
                // running, so whether a worker was born is unresolved.
                None => LaunchState::Unknown {
                    reason: "launch timed out; a worker may have been born".to_string(),
                },
                Some((ok, out, err)) => match decode_launch_outcome(ok, &out, &err) {
                    LaunchOutcome::Launched {
                        name,
                        pane,
                        seed_delivered,
                    } => LaunchState::Launched {
                        name,
                        pane,
                        seed_delivered,
                    },
                    LaunchOutcome::Refused(reason) => LaunchState::Refused { reason },
                    LaunchOutcome::Unknown(reason) => LaunchState::Unknown { reason },
                },
            };
            let _ = core_tx
                .send(super::CoreMsg::AgentLaunchUpdate {
                    id,
                    update: AgentLaunchUpdate { request_id, state },
                })
                .await;
        });
    }

    /// The off-loop attempt's terminal update landed: settle the desk and
    /// answer the requesting client.
    pub(super) fn agent_launch_update(&mut self, id: u64, update: AgentLaunchUpdate) {
        self.launch_desk.settle(update.clone());
        self.send_launch_update(id, update);
    }

    fn send_launch_update(&self, id: u64, update: AgentLaunchUpdate) {
        if let Some(c) = self.clients.iter().find(|c| c.id == id) {
            let _ = c.reliable_tx.try_send(ServerMsg::AgentLaunch(update));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: u64) -> AgentLaunchRequest {
        AgentLaunchRequest {
            request_id: id,
            revision: 1,
            cwd: "/tmp".to_string(),
            harness: "claude".to_string(),
            substrate: "pane".to_string(),
            model: None,
            effort: None,
            permission_mode: None,
            placement: None,
            message: String::new(),
        }
    }

    #[test]
    fn validation_refuses_pre_birth() {
        assert!(validate_launch_request(&req(1)).is_ok());
        // A relative project path never spawns.
        let mut r = req(2);
        r.cwd = "relative/path".into();
        assert!(validate_launch_request(&r).is_err());
        // A nonexistent project path never spawns.
        let mut r = req(3);
        r.cwd = "/definitely/not/a/dir/".into();
        assert!(validate_launch_request(&r).is_err());
        // Only the canonical substrates pass; the door owns the rest.
        let mut r = req(4);
        r.substrate = "headless".into();
        assert!(validate_launch_request(&r).is_err());
        // An over-cap message refuses with the limit named.
        let mut r = req(5);
        r.message = "x".repeat(crate::proto::MAX_MAIL_TEXT + 1);
        assert!(validate_launch_request(&r).is_err());
    }

    #[test]
    fn launch_desk_replays_one_attempt_per_request_id() {
        let mut desk = LaunchDesk::default();
        // Unknown id: nothing replayed.
        assert!(desk.in_flight_or_done(7).is_none());
        // Started: a duplicate id reads Starting and must not re-spawn.
        desk.mark_started(7);
        assert_eq!(
            desk.in_flight_or_done(7),
            Some(AgentLaunchUpdate {
                request_id: 7,
                state: LaunchState::Starting
            })
        );
        // Settled: the terminal update replays verbatim.
        let terminal = AgentLaunchUpdate {
            request_id: 7,
            state: LaunchState::Refused {
                reason: "no capacity".into(),
            },
        };
        desk.settle(terminal.clone());
        assert_eq!(desk.in_flight_or_done(7), Some(terminal));
        // A second settle for the same id never resurrects an evicted order
        // slot twice.
        desk.settle(AgentLaunchUpdate {
            request_id: 7,
            state: LaunchState::Unknown { reason: "x".into() },
        });
        assert_eq!(desk.order.len(), 1);
    }

    #[test]
    fn launch_desk_eviction_stays_bounded() {
        let mut desk = LaunchDesk::default();
        for i in 0..(DESK_RETENTION as u64 + 10) {
            desk.settle(AgentLaunchUpdate {
                request_id: i,
                state: LaunchState::Refused { reason: "r".into() },
            });
        }
        assert!(desk.finished.len() <= DESK_RETENTION);
        // The newest survives; the oldest was evicted.
        assert!(desk.in_flight_or_done(DESK_RETENTION as u64 + 9).is_some());
        assert!(desk.in_flight_or_done(0).is_none());
    }
}

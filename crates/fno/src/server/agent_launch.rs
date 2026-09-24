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
    plan: bool,
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
    // lane and the door renders the seed. A plan spawn pins the architect
    // sub-agent and the blueprint message on the SAME door flags.
    let argv = if plan {
        crate::dispatch_launch::plan_spawn_argv(&fno, &node_id, session, account, parent.as_deref())
    } else {
        crate::dispatch_launch::dispatch_spawn_argv(
            &fno,
            &node_id,
            session,
            account,
            parent.as_deref(),
        )
    };
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
    /// Attempts currently running off-loop. A duplicate key here replays
    /// `Starting` and starts nothing. Keyed by (client id, request id):
    /// request ids are client-minted and per-client, so the client id is
    /// what makes one attempt's key distinct from another client's.
    pending: HashSet<(u64, u64)>,
    /// Terminal updates, insertion-ordered for eviction.
    finished: HashMap<(u64, u64), AgentLaunchUpdate>,
    order: VecDeque<(u64, u64)>,
}

/// Finished attempts remembered for replay. Small: a popup session rarely
/// exceeds a handful, and eviction only costs a reopened popup its history.
const DESK_RETENTION: usize = 64;

/// The terminal update's bounded redelivery: ~5s of 200ms ticks before the
/// update is dropped and the desk settled anyway.
const UPDATE_SEND_RETRIES: u8 = 25;
const UPDATE_SEND_BACKOFF: Duration = Duration::from_millis(200);

impl LaunchDesk {
    fn in_flight_or_done(&self, client: u64, request_id: u64) -> Option<AgentLaunchUpdate> {
        let key = (client, request_id);
        if let Some(update) = self.finished.get(&key) {
            return Some(update.clone());
        }
        self.pending.contains(&key).then(|| AgentLaunchUpdate {
            request_id,
            state: LaunchState::Starting,
        })
    }

    fn mark_started(&mut self, client: u64, request_id: u64) {
        self.pending.insert((client, request_id));
    }

    /// Read-only replay of an attempt's terminal state (test reader).
    #[cfg(test)]
    pub(super) fn settled_state(&self, client: u64, request_id: u64) -> Option<LaunchState> {
        self.finished
            .get(&(client, request_id))
            .map(|u| u.state.clone())
    }

    fn settle(&mut self, client: u64, update: AgentLaunchUpdate) {
        let key = (client, update.request_id);
        self.pending.remove(&key);
        if !self.finished.contains_key(&key) {
            self.order.push_back(key);
        }
        while self.order.len() > DESK_RETENTION {
            let evict = self.order.pop_front().expect("order nonempty");
            self.finished.remove(&evict);
        }
        self.finished.insert(key, update);
    }
}

/// The launcher's pre-birth validation: everything checkable WITHOUT the
/// spawn door, refused before any effect. The door itself stays the
/// authority on harness support, routing, capacity and permissions.
fn validate_launch_request(req: &AgentLaunchRequest) -> Result<(), String> {
    if req.harness.trim().is_empty() {
        return Err("no harness selected".to_string());
    }
    // EMPTY = the door's default (thread where the harness seats one); an
    // explicit lane names `pane` or `thread`. `headless` is never offered by
    // the dock and is refused here before any effect.
    if !matches!(req.substrate.as_str(), "" | "pane" | "thread") {
        return Err(format!("unsupported substrate {:?}", req.substrate));
    }
    if let Some(dir) = &req.split {
        if !matches!(dir.as_str(), "left" | "right" | "up" | "down") {
            return Err(format!("unsupported split direction {dir:?}"));
        }
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
    /// One targeted card launch - dispatch or plan - the shared gate both
    /// wire commands run. Readiness re-checks against the server's OWN
    /// backlog snapshot (codex peer review): the client gates its confirm to
    /// a ready card, but the server's snapshot is fresher, so a card that
    /// went blocked/in-flight between publish and click is refused or routed
    /// here, never started down a path prefix+g would skip. A stale
    /// DISPATCH routes to the live work (focus/attach); a stale PLAN refuses
    /// plainly - the routing arms answer "where is the work", and a
    /// blueprint for worked work is not asked for twice.
    pub(super) fn dispatch_card(
        &mut self,
        client_id: u64,
        node: String,
        account: Option<String>,
        plan: bool,
    ) -> super::Flow {
        // The plan branch refuses ONLY an in-flight node: the blueprint
        // spawn door accepts an idea node (nothing to route to yet), and a
        // card being worked already must not fork a second plan.
        if plan {
            if let Some(refusal) = self.plan_refusal(&node) {
                self.notice(client_id, refusal);
            } else {
                self.dispatch_next(client_id, Some(node), account, true);
            }
            return super::Flow::Continue;
        }
        if super::card_ready_to_dispatch(&self.backlog, &node) {
            self.dispatch_next(client_id, Some(node), account, plan);
        } else if plan {
            self.notice(client_id, "card not ready to dispatch");
        } else if let Some(route) = self.inflight_route(&node) {
            // The client's Layout was stale, but the server can route it:
            // focus/attach instead of refusing (AC2-ERR). The recursion
            // reuses the FocusPane/AttachAgent gates verbatim (catalog
            // membership, jobId shape), so this adds no second spawn path.
            return self.command(client_id, route);
        } else if let Some(hint) = self.inflight_hint(&node) {
            // In flight but unroutable: say where the work is, the same
            // copy a routed v18 card click would show.
            self.notice(client_id, hint);
        } else {
            self.notice(client_id, "card not ready to dispatch");
        }
        super::Flow::Continue
    }

    /// The plan-spawn door's refusal: a node the server feed shows in
    /// flight names its session instead of getting a second blueprint.
    /// Everything else passes - an idea node (no feed card at all), an
    /// unranked or blocked card: the model decides readiness, the spawn
    /// door answers, and a blueprint for worked work is not asked twice.
    pub(super) fn plan_refusal(&self, node: &str) -> Option<String> {
        self.inflight_hint(node).map(|hint| {
            format!("{node} is already being worked ({hint}); open its session instead")
        })
    }

    /// "Grab work" (prefix+g): dispatch the next ready backlog node into    /// a new pane. Board selection is `fno backlog next`; the launch is the door
    /// (`fno agents spawn`), shelled OFF the core loop in a detached
    /// task so a slow backlog read never stalls a pane. The launched pane
    /// appears through the existing registry reader; the outcome (dispatched /
    /// no-work / refusal / failure) routes back as `DispatchResult` for a
    /// one-line notice. (move from `server.rs`.)
    pub(super) fn dispatch_next(
        &mut self,
        id: u64,
        node: Option<String>,
        account: Option<String>,
        plan: bool,
    ) {
        let session = self.session_name.clone();
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            let notice =
                run_dispatch_one(&session, node.as_deref(), account.as_deref(), plan).await;
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
        if let Some(update) = self.launch_desk.in_flight_or_done(id, req.request_id) {
            self.send_launch_update(id, update);
            return;
        }
        if let Err(reason) = validate_launch_request(&req) {
            let update = AgentLaunchUpdate {
                request_id: req.request_id,
                state: LaunchState::Refused { reason },
            };
            self.launch_desk.settle(id, update.clone());
            self.send_launch_update(id, update);
            return;
        }
        self.launch_desk.mark_started(id, req.request_id);
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
                    retry: 0,
                })
                .await;
        });
    }

    /// The off-loop attempt's terminal update landed: settle the desk and
    /// answer the requesting client. A client whose reliable channel is
    /// momentarily full gets the update re-queued on a short bounded backoff
    /// instead of a silent drop: the desk stays pending while retrying, so
    /// a duplicate still replays `Starting`, and a lost terminal update can
    /// never strand the client's disabled button.
    pub(super) fn agent_launch_update(&mut self, id: u64, update: AgentLaunchUpdate, retry: u8) {
        if self.send_launch_update(id, update.clone()) {
            self.launch_desk.settle(id, update);
            return;
        }
        if retry >= UPDATE_SEND_RETRIES {
            // Bounded best effort: settle so the desk stays truthful about
            // the attempt, and let the update go. The client still has the
            // Submitting escape (dismiss).
            self.launch_desk.settle(id, update);
            return;
        }
        let core_tx = self.self_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(UPDATE_SEND_BACKOFF).await;
            let _ = core_tx
                .send(super::CoreMsg::AgentLaunchUpdate {
                    id,
                    update,
                    retry: retry + 1,
                })
                .await;
        });
    }

    /// True when the update was delivered or the client is gone (nothing
    /// left to answer); false only on a full client channel.
    fn send_launch_update(&self, id: u64, update: AgentLaunchUpdate) -> bool {
        match self.clients.iter().find(|c| c.id == id) {
            None => true,
            Some(c) => c
                .reliable_tx
                .try_send(ServerMsg::AgentLaunch(update))
                .is_ok(),
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
            model_names_harness: false,
            effort: None,
            permission_mode: None,
            placement: None,
            portal: None,
            split: None,
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
        // Unknown key: nothing replayed.
        assert!(desk.in_flight_or_done(1, 7).is_none());
        // Started: a duplicate key reads Starting and must not re-spawn.
        desk.mark_started(1, 7);
        assert_eq!(
            desk.in_flight_or_done(1, 7),
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
        desk.settle(1, terminal.clone());
        assert_eq!(desk.in_flight_or_done(1, 7), Some(terminal));
        // A second settle for the same key never resurrects an evicted order
        // slot twice.
        desk.settle(
            1,
            AgentLaunchUpdate {
                request_id: 7,
                state: LaunchState::Unknown { reason: "x".into() },
            },
        );
        assert_eq!(desk.order.len(), 1);
    }

    #[test]
    fn launch_desk_keys_attempts_by_client_not_request_id_alone() {
        // Two clients minting the same client-local request id are DISTINCT
        // attempts: one's terminal state must never replay to the other.
        let mut desk = LaunchDesk::default();
        desk.mark_started(1, 1);
        assert!(
            desk.in_flight_or_done(2, 1).is_none(),
            "client 2's id 1 is a fresh attempt"
        );
        desk.settle(
            1,
            AgentLaunchUpdate {
                request_id: 1,
                state: LaunchState::Launched {
                    name: "a".into(),
                    pane: Some(9),
                    seed_delivered: Some(true),
                },
            },
        );
        assert!(
            desk.in_flight_or_done(2, 1).is_none(),
            "A's birth never replays to B"
        );
    }

    #[test]
    fn launch_desk_eviction_stays_bounded() {
        let mut desk = LaunchDesk::default();
        for i in 0..(DESK_RETENTION as u64 + 10) {
            desk.settle(
                1,
                AgentLaunchUpdate {
                    request_id: i,
                    state: LaunchState::Refused { reason: "r".into() },
                },
            );
        }
        assert!(desk.finished.len() <= DESK_RETENTION);
        // The newest survives; the oldest was evicted.
        assert!(desk
            .in_flight_or_done(1, DESK_RETENTION as u64 + 9)
            .is_some());
        assert!(desk.in_flight_or_done(1, 0).is_none());
    }
}

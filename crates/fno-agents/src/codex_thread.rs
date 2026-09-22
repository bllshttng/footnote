//! Shared-daemon app-server driver for persistent Codex threads.
//!
//! This module is a WebSocket CLIENT of the one `codex app-server daemon`
//! running on the machine. It owns no process: the daemon owns the thread, so
//! a thread outlives this driver's connection, the mux, and `fno-agents-daemon`
//! itself, and stays visible to `codex agents`, `codex resume`, `codex fork`
//! and Remote Control, all of which are scoped to that shared daemon.
//!
//! It used to fork a PRIVATE `codex app-server` per worker and speak
//! newline-delimited JSON to its stdin/stdout. That made fno the owner of the
//! process and forfeited every vendor verb above at once, with no symptom
//! beyond a thing the operator expected to work not working.
//!
//! The transport is the ONLY thing that changed. The single-owner actor below
//! stays: its whole-turn-exclusion reasoning is about handle types, not pipes.
//!
//! # The rule this module must keep
//!
//! fno never renders a harness interface. Viewing a thread is codex's own
//! declared attach form (`codex resume <id> --remote unix://`)
//! EXEC'd in a pane; the frames read here drive
//! turns and never paint a screen. A future change that reads frames to draw
//! something has rebuilt the layer this lane deleted, and the process tree is
//! how you tell: no `codex app-server` may have `fno-agents-daemon` as its
//! parent, and no `fno` process may sit between a viewer terminal and `codex`.

use crate::codex_inject::{
    connect_app_server, parse_review_start_response, review_start_request_json, AppServerSink,
    AppServerStream, ReviewDelivery, ReviewTarget,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// Total budget for one id-matched request/response exchange (handshake,
/// turn/start, steer, interrupt, review). Frames unrelated to the id arrive
/// interleaved and are parked, so the budget bounds the whole exchange, not
/// any single frame.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Total budget for one whole turn. Codex streams a turn as an unbounded
/// burst of notification frames with quiet gaps while the model thinks, so
/// this deadline is the ONLY bound on the wait for `turn/completed`.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);
/// How long the daemon's `ask` waits on a submitter's reply before answering
/// `in_flight`. Comfortably under the client's 120s `RESPONSE_DEADLINE`
/// (crates/fno-agents/src/bin/client.rs) so a bounded receipt, not a silent
/// transport failure, is what the caller sees. Env-overridable so tests can
/// exercise the expiry path without sleeping 90s.
pub fn ask_wait() -> Duration {
    std::env::var("FNO_CODEX_ASK_WAIT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map_or(Duration::from_secs(90), Duration::from_millis)
}
/// Bound on waiting for a turn's `turn/completed` AFTER an interrupt ack (or a
/// steer-precondition failure): the same outer-deadline shape as the daemon's
/// switchboard drive (request budget + grace) backstopping a wedged turn.
const TURN_SETTLE_TIMEOUT: Duration = Duration::from_secs(65);
/// Outer bound around the daemon's WHOLE stop exchange (interrupt RPC + turn
/// settle), kept just under the client's 120s `RESPONSE_DEADLINE` so a wedged
/// turn yields a bounded receipt, not a dead socket.
pub fn stop_settle_bound() -> Duration {
    Duration::from_secs(115)
}
/// Shared bound on the ACTOR side of one interrupt: the RPC ack wait and the
/// settle wait split one deadline instead of stacking (`REQUEST_TIMEOUT` +
/// `TURN_SETTLE_TIMEOUT` once totaled 125s, past the daemon's 115s outer
/// bound, whose expiry left `shutdown()` blocked on the interrupt tail until
/// after the client's 120s deadline). Under `stop_settle_bound()` so the
/// actor always answers `Interrupt` before that outer bound can fire, which
/// is what keeps the shutdown ack fast. Env-overridable for the same reason
/// as `ask_wait`.
pub fn interrupt_total_bound() -> Duration {
    std::env::var("FNO_CODEX_INTERRUPT_BOUND_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map_or(Duration::from_secs(110), Duration::from_millis)
}
/// Parked-but-unclaimed turn receipts are telemetry only (the rollout on disk
/// is the durable record), so the map is capped and overflow drops entries.
const COMPLETED_PARK_CAP: usize = 8;
const THREAD_CHANNEL_CAP: usize = 32;

/// What the driver's own turn state says, fired at the transitions the actor
/// already observes. The daemon maps these onto inside-leg reports:
/// the ack and every refresh write `working`, the completion writes `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadTurnPhase {
    /// A turn was accepted (or is still driving at a keepalive tick).
    Working,
    /// The turn routed `turn/completed`; the thread is at its prompt.
    Done,
}

/// Keepalive cadence for the `working` report: half the reader TTL, the same
/// convention the claude inside-leg hook uses (`hooks/inside-leg-report.sh`).
/// Env-overridable so tests can exercise the refresh without sleeping 45s.
pub fn thread_turn_refresh() -> Duration {
    std::env::var("FNO_THREAD_TURN_REFRESH_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(
            Duration::from_millis(crate::state::THREAD_TURN_TTL_MS / 2),
            Duration::from_millis,
        )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadStartError {
    InvalidResponse,
    NotConfirmed,
    Server(String),
}

impl std::fmt::Display for ThreadStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidResponse => f.write_str("invalid thread response"),
            Self::NotConfirmed => f.write_str("thread identity was not confirmed"),
            Self::Server(message) => write!(f, "server-error: {message}"),
        }
    }
}

impl std::error::Error for ThreadStartError {}

/// Driver failures. There is deliberately no spawn variant: this driver forks
/// nothing, so "the child would not start" is not a state it can reach. A
/// daemon that will not boot surfaces as [`ThreadDriverError::Protocol`] with
/// the boot error, which is a different remedy (repair the shared daemon)
/// than a failed fork ever was.
#[derive(Debug, thiserror::Error)]
pub enum ThreadDriverError {
    #[error("codex app-server I/O failed: {0}")]
    Io(#[source] io::Error),
    #[error("codex app-server response timed out")]
    Timeout,
    #[error("codex app-server protocol: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnResult {
    pub turn_id: String,
    pub status: String,
    pub text: String,
    pub raw: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeGoal {
    pub objective: String,
    pub status: GoalStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewResult {
    pub turn_id: String,
    pub review_thread_id: String,
}

/// Build the `thread/start` request with the unattended permission posture.
/// The nested `result.thread.id` is the only accepted identity shape.
pub fn thread_start_request_json(cwd: &str, approval_policy: &str) -> String {
    json!({
        "id": 1,
        "method": "thread/start",
        "params": {
            "cwd": cwd,
            "sandbox": "workspace-write",
            "approvalPolicy": approval_policy,
        }
    })
    .to_string()
}

/// Build the `thread/archive` request (task 3): the history-preserving
/// active-surface removal. The stored conversation survives; `thread/resume`
/// searches the archived store, and `thread/unarchive` (the matching builder
/// below) puts the same id back before a resume that needs it live.
pub fn thread_archive_request_json(id: u64) -> String {
    json!({
        "id": id,
        "method": "thread/archive",
        "params": {}
    })
    .to_string()
}

/// Build the `thread/unarchive` request for a named thread id.
pub fn thread_unarchive_request_json(id: u64, thread_id: &str) -> String {
    json!({
        "id": id,
        "method": "thread/unarchive",
        "params": {
            "threadId": thread_id,
        }
    })
    .to_string()
}

/// Build the `thread/resume` request. The full thread id and cwd are both
/// required so recovery cannot silently move a worker onto the canonical repo.
pub fn thread_resume_request_json(thread_id: &str, cwd: &str, approval_policy: &str) -> String {
    json!({
        "id": 1,
        "method": "thread/resume",
        "params": {
            "threadId": thread_id,
            "cwd": cwd,
            "sandbox": "workspace-write",
            "approvalPolicy": approval_policy,
        }
    })
    .to_string()
}

/// Build the typed native compaction action. The caller must prove completion
/// from the same thread's `contextCompaction` lifecycle item; this frame is
/// only the submit half of that transaction.
pub fn thread_compact_start_request_json(id: u64, thread_id: &str) -> String {
    json!({
        "id": id,
        "method": "thread/compact/start",
        "params": { "threadId": thread_id }
    })
    .to_string()
}

/// Read the native goal for one exact full thread id.
pub fn thread_goal_get_request_json(id: u64, thread_id: &str) -> String {
    json!({
        "id": id,
        "method": "thread/goal/get",
        "params": { "threadId": thread_id }
    })
    .to_string()
}

/// Set or pause a native goal without exposing a clear operation. The
/// controller preserves the objective and usage across a pause.
pub fn thread_goal_set_request_json(
    id: u64,
    thread_id: &str,
    objective: &str,
    status: &str,
) -> String {
    thread_goal_set_request_json_with_owner(id, thread_id, objective, status, None)
}

pub fn thread_goal_set_request_json_with_owner(
    id: u64,
    thread_id: &str,
    objective: &str,
    status: &str,
    continuation_owner: Option<&str>,
) -> String {
    let mut params = json!({
        "threadId": thread_id,
        "goal": objective,
        "status": status,
    });
    if let Some(owner) = continuation_owner.filter(|owner| !owner.trim().is_empty()) {
        params["continuationOwner"] = json!(owner);
    }
    json!({
        "id": id,
        "method": "thread/goal/set",
        "params": params,
    })
    .to_string()
}

pub fn reign_objective(scope: &str) -> String {
    format!("$fno:reign {}", scope.trim())
}

pub fn parse_goal_response(raw: &str) -> Result<Option<NativeGoal>, ThreadDriverError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| ThreadDriverError::Protocol("invalid thread/goal response".into()))?;
    parse_goal_value(&value)
}

pub fn parse_goal_value(value: &Value) -> Result<Option<NativeGoal>, ThreadDriverError> {
    if let Some(error) = value.get("error") {
        return Err(ThreadDriverError::Protocol(server_error(error)));
    }
    let goal = value.pointer("/result/goal").or_else(|| value.get("goal"));
    let Some(goal) = goal else {
        return Ok(None);
    };
    if goal.is_null() {
        return Ok(None);
    }
    let object = goal.as_object().ok_or_else(|| {
        ThreadDriverError::Protocol("thread/goal response carried a non-object goal".into())
    })?;
    let objective = object
        .get("objective")
        .or_else(|| object.get("goal"))
        .and_then(Value::as_str)
        .filter(|objective| !objective.trim().is_empty())
        .ok_or_else(|| ThreadDriverError::Protocol("thread/goal response has no objective".into()))?
        .to_string();
    let status = match object
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("active")
    {
        "active" => GoalStatus::Active,
        "paused" => GoalStatus::Paused,
        "completed" | "done" => GoalStatus::Completed,
        other => {
            return Err(ThreadDriverError::Protocol(format!(
                "thread/goal response has unknown status {other:?}"
            )))
        }
    };
    let continuation_owner = object
        .get("continuationOwner")
        .or_else(|| object.get("continuation_owner"))
        .or_else(|| object.get("owner"))
        .and_then(Value::as_str)
        .filter(|owner| !owner.trim().is_empty())
        .map(str::to_string);
    Ok(Some(NativeGoal {
        objective,
        status,
        continuation_owner,
    }))
}

pub fn ensure_reign_goal(
    current: Option<&NativeGoal>,
    scope: &str,
    continuation_owner: &str,
) -> Result<GoalStatus, ThreadDriverError> {
    let expected = reign_objective(scope);
    if let Some(goal) = current {
        if goal.status == GoalStatus::Active && goal.objective != expected {
            return Err(ThreadDriverError::Protocol(format!(
                "refusing active native goal {:?}; expected {:?}; continuation owner {}",
                goal.objective, expected, continuation_owner
            )));
        }
    }
    Ok(GoalStatus::Active)
}

/// Build a `turn/start` request for the held driver.
pub fn turn_start_request_json_with_id(id: u64, thread_id: &str, text: &str) -> String {
    turn_start_request_json_with_effort(id, thread_id, text, None)
}

pub fn turn_start_request_json_with_effort(
    id: u64,
    thread_id: &str,
    text: &str,
    effort: Option<&str>,
) -> String {
    // No resolved posture and the bounded default request: the minimal frame.
    turn_start_request_json_full(
        id,
        thread_id,
        text,
        effort,
        &[],
        None,
        &CodexPosture::bounded(),
    )
}

/// `turn/start` with the optional state-root grant.
///
/// `turn/start` is the carrier, and that is a MEASUREMENT rather than a
/// reading of the protocol docs. Against the live app-server on 2026-08-28,
/// `thread/start` accepted a `sandboxPolicy` object without complaint and
/// IGNORED it, falling back to the machine's configured default; only the
/// scalar `sandbox` enum reaches it. `turn/start` honors
/// `sandboxPolicy.writableRoots`: with the state root named, a shell command
/// in that turn created a file under it, and with the root withheld the same
/// command was denied while still writing inside cwd.
///
/// The grant rides EVERY turn, not just the first. A turn-level override
/// becomes the thread's default for later turns, so once would be enough on a
/// thread that is never resumed - and a resumed thread re-resolves its
/// posture, which would silently drop the grant with a slower fuse. Sending it
/// per turn makes resume carry it for free.
///
/// The turn's policy echoes the resolved posture with `writableRoots` widened
/// and `networkAccess` forced on. The keeper socket and `gh` egress share one
/// seatbelt switch, so a thread worker without network cannot claim a node
/// even with every directory granted (measured 2026-09-14, live app-server).
/// The roots are ADDITIVE to the workspace - a bounded thread reports
/// `writableRoots: []` and can still write its own cwd - so naming the state
/// root does not take the worktree away.
///
/// Widen a RESOLVED workspaceWrite posture with `state_dirs` and network
/// access. Additive and order-stable: the posture's own roots come first and
/// a root it already names is not repeated, so the turn widens the policy and
/// narrows nothing. Every other resolved posture is echoed unchanged by
/// [`turn_policy`]; this builder only ever sees the workspaceWrite echo,
/// because roots mean nothing under a wider posture and naming them on a
/// narrower one would be a widening of its own.
pub(crate) fn sandbox_policy_with_roots(resolved: &Value, state_dirs: &[String]) -> Value {
    let mut policy = resolved.clone();
    let mut roots: Vec<String> = posture_roots(&policy);
    for dir in state_dirs {
        if !roots.iter().any(|root| root == dir) {
            roots.push(dir.clone());
        }
    }
    policy["writableRoots"] = json!(roots);
    policy["networkAccess"] = json!(true);
    policy
}

/// The `writableRoots` a policy object already carries, as owned strings.
fn posture_roots(policy: &Value) -> Vec<String> {
    policy
        .get("writableRoots")
        .and_then(Value::as_array)
        .map(|existing| {
            existing
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The `turn/start` sandboxPolicy: the thread's RESOLVED posture echoed with
/// its roots widened, or - when the server named no sandbox at all - the
/// recorded REQUEST built into a policy, marked `requested` so no reader can
/// mistake a replayed request for a server answer. The return pairs the
/// policy with its source (`resolved` / `requested`); `None` sends no policy
/// key at all, which keeps a bounded request with no roots on today's exact
/// frame.
///
/// This is where the old frame narrowed a full-access thread: the resolved
/// posture was filtered to `workspaceWrite` and a missing one was fabricated
/// as `workspaceWrite` from nothing whenever any root existed, so a
/// `dangerFullAccess` thread received a `workspaceWrite` policy on every
/// single turn. The echo is now unfiltered, and the from-request build never
/// invents a posture name - it builds the posture the request named.
pub(crate) fn turn_policy(
    resolved: Option<&Value>,
    requested: &CodexPosture,
    state_dirs: &[String],
) -> Option<(Value, &'static str)> {
    if let Some(resolved) = resolved {
        return Some(match resolved.get("type").and_then(Value::as_str) {
            Some("workspaceWrite") => (sandbox_policy_with_roots(resolved, state_dirs), "resolved"),
            // Full access and read-only echo unchanged: roots mean nothing
            // under full access, and adding roots (or network) to read-only
            // would be a widening of its own.
            _ => (resolved.clone(), "resolved"),
            // The server's own value, whatever it names: never a hand-built
            // substitute for an unknown posture.
        });
    }
    if requested.is_full_access() {
        return Some((
            json!({"type": requested.sandbox.as_policy_type()}),
            "requested",
        ));
    }
    if state_dirs.is_empty() {
        return None;
    }
    let policy = json!({
        "type": requested.sandbox.as_policy_type(),
        "writableRoots": state_dirs,
        "networkAccess": true,
    });
    Some((policy, "requested"))
}

/// The roots the thread lane carries onto every `turn/start`: the caller's
/// state dirs plus the repo's git common dir, resolved by the same resolver
/// the exec lane grants with. Both postures carry them. A `yolo` thread asks
/// for `danger-full-access` on `thread/start`, but the server keeps its
/// workspaceWrite default, and withholding the policy does not lift a sandbox -
/// it keeps whatever the server already had, `.git` read-only included.
///
/// Fail-open like the exec lane's grant: an unresolvable root is skipped, so
/// resolution can never break the spawn.
fn granted_roots(cwd: &Path, state_dirs: &[String]) -> Vec<String> {
    let mut roots = state_dirs.to_vec();
    if let Some(git_dir) = crate::provider::git_common_dir(cwd) {
        if !roots.iter().any(|root| root == &git_dir) {
            roots.push(git_dir);
        }
    }
    roots
}

/// `turn/start` takes a whole `sandboxPolicy` object, never a `writableRoots`
/// delta, so the policy is built FROM the thread's own resolved posture
/// (`resolved`), or from the recorded REQUEST when the server named no
/// sandbox ([`turn_policy`]). The source is carried BESIDE the policy on the
/// row (`turn_policy_source`), not inside the frame: the app-server owns the
/// params' shape, and a non-protocol key would be a second vocabulary for one
/// fact.
pub fn turn_start_request_json_full(
    id: u64,
    thread_id: &str,
    text: &str,
    effort: Option<&str>,
    state_dirs: &[String],
    resolved: Option<&Value>,
    requested: &CodexPosture,
) -> String {
    let mut params = json!({
        "threadId": thread_id,
        "input": [{"type": "text", "text": text}],
    });
    if let Some(effort) = effort.filter(|effort| !effort.is_empty()) {
        params["effort"] = json!(effort);
    }
    if let Some((policy, _source)) = turn_policy(resolved, requested, state_dirs) {
        params["sandboxPolicy"] = policy;
    }
    json!({
        "id": id,
        "method": "turn/start",
        "params": params,
    })
    .to_string()
}

/// Build a `turn/steer` request with the server-enforced expected-turn
/// precondition. Keystrokes cannot provide this identity check.
pub fn turn_steer_request_json(
    id: u64,
    thread_id: &str,
    expected_turn_id: &str,
    text: &str,
) -> String {
    json!({
        "id": id,
        "method": "turn/steer",
        "params": {
            "threadId": thread_id,
            "expectedTurnId": expected_turn_id,
            "input": [{"type": "text", "text": text}],
        }
    })
    .to_string()
}

/// Build a `turn/interrupt` request against a full turn id.
pub fn turn_interrupt_request_json(id: u64, thread_id: &str, turn_id: &str) -> String {
    json!({
        "id": id,
        "method": "turn/interrupt",
        "params": {"threadId": thread_id, "turnId": turn_id},
    })
    .to_string()
}

/// The sandbox posture the server RESOLVED for this thread, as it reports it.
///
/// Read so a per-turn override can be built FROM it. `turn/start` takes a whole
/// `sandboxPolicy` object rather than a `writableRoots` delta, so a
/// hand-built object silently replaces every sibling field - `networkAccess`,
/// the tmp exclusions, any roots the posture already carried - with whatever
/// default the server applies. Echoing the resolved posture back with only
/// `writableRoots` widened keeps the turn's policy equal to the thread's in
/// every other respect.
pub fn parse_resolved_sandbox(raw: &str) -> Option<Value> {
    serde_json::from_str::<Value>(raw)
        .ok()?
        .pointer("/result/sandbox")
        .cloned()
}

pub use crate::codex_posture::{resolve_thread_posture, CodexPosture};

/// The posture name the server reported, read WITHOUT the workspaceWrite
/// filter [`parse_resolved_sandbox`] applies.
///
/// That filter answers `None` for two different worlds - a full-access thread
/// and a response that carried no `sandbox` at all - which is fine where it is
/// used (there is nothing to echo either way) and wrong for a RECORD. A row
/// that omits the posture leaves the reader inferring which world it was, and
/// this lane already cost one investigation a day on exactly that ambiguity.
/// So the record gets the name the server used, or [`SANDBOX_POSTURE_UNKNOWN`].
pub fn parse_resolved_sandbox_type(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw)
        .ok()?
        .pointer("/result/sandbox/type")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Recorded when `thread/start` reported no sandbox at all. An explicit value,
/// never an absent key: absence is not evidence.
pub const SANDBOX_POSTURE_UNKNOWN: &str = "unknown";

pub fn parse_thread_start_response(raw: &str) -> Result<(String, String), ThreadStartError> {
    let value: Value = serde_json::from_str(raw).map_err(|_| ThreadStartError::InvalidResponse)?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or("unknown app-server error");
        return Err(ThreadStartError::Server(message.to_string()));
    }
    let thread = value
        .pointer("/result/thread")
        .and_then(Value::as_object)
        .ok_or(ThreadStartError::NotConfirmed)?;
    let id = thread
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(ThreadStartError::NotConfirmed)?;
    let path = thread
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or(ThreadStartError::NotConfirmed)?;
    Ok((id.to_string(), path.to_string()))
}

pub fn parse_turn_start_response(raw: &str) -> Result<String, ThreadDriverError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| ThreadDriverError::Protocol("invalid turn/start response".into()))?;
    parse_turn_start_response_value(&value)
}

/// Value-based twin of [`parse_turn_start_response`] so the actor parses each
/// inbound frame exactly once instead of re-serializing it to parse again.
pub fn parse_turn_start_response_value(value: &Value) -> Result<String, ThreadDriverError> {
    if let Some(error) = value.get("error") {
        return Err(ThreadDriverError::Protocol(server_error(error)));
    }
    value
        .pointer("/result/turn/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ThreadDriverError::Protocol("turn id was not confirmed".into()))
}

/// Parse a `turn/completed` notification. The app-server has emitted both a
/// nested `params.turn` shape and a flattened params shape across versions;
/// accept either while requiring the positive notification marker.
pub fn parse_turn_completed_notification(raw: &str) -> Option<TurnResult> {
    let value: Value = serde_json::from_str(raw).ok()?;
    parse_turn_completed_value(&value)
}

/// Value-based twin of [`parse_turn_completed_notification`]; see
/// [`parse_turn_start_response_value`] for why it exists.
pub fn parse_turn_completed_value(value: &Value) -> Option<TurnResult> {
    if value.get("method").and_then(Value::as_str) != Some("turn/completed") {
        return None;
    }
    let params = value.get("params")?;
    let turn = params.get("turn").unwrap_or(params);
    let turn_id = turn
        .get("id")
        .or_else(|| params.get("turnId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?
        .to_string();
    let status = turn
        .get("status")
        .or_else(|| params.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("completed")
        .to_string();
    let items = turn
        .get("items")
        .or_else(|| params.get("items"))
        .and_then(Value::as_array);
    let text = items
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    Some(TurnResult {
        turn_id,
        status,
        text,
        raw: value.clone(),
    })
}

pub struct CodexThread {
    /// The write half of this driver's connection to the SHARED daemon. There
    /// is no child handle beside it, and that absence is the point: dropping
    /// this struct closes a socket, it does not end a thread.
    sink: AppServerSink,
    /// `None` once [`CodexThread::into_actor`] moved the read half into the
    /// actor's read pump; the legacy read paths below then refuse rather than
    /// spin.
    stream: Option<AppServerStream>,
    /// The shared app-server daemon's pid, recorded at connect. This is NOT
    /// the registry row's `pid`: the row carries `pid: None` for a codex
    /// thread worker (codex_thread_entry.rs - one always-alive shared pid on
    /// every thread row broke `derive_liveness` and gc). The daemon pid travels
    /// into [`CodexThreadActor`]; ownership is provable from the control
    /// socket and `thread/loaded/list`, not from a row pid.
    daemon_pid: Option<u32>,
    pending: VecDeque<Value>,
    /// `turn/completed` notifications parsed once at push, keyed by turn id
    /// (Change 10: `take_completed` used to re-serialize every parked entry on
    /// each check). Capped? No: the legacy path claims its entry on match and
    /// `drive_turn` is one turn at a time, so the map holds only the
    /// completions that arrived while a DIFFERENT frame class was awaited.
    completed_turns: HashMap<String, TurnResult>,
    next_id: u64,
    thread_id: String,
    rollout_path: PathBuf,
    cwd: PathBuf,
    effort: Option<String>,
    /// Explicit context-window settings belong to this thread, not the shared
    /// Codex daemon. The registry copies this request so a daemon restart can
    /// replay the same values on `thread/resume`.
    context_window_request: Option<crate::context_window::ContextWindowRequest>,
    /// The fno state roots this thread is granted, spent on every `turn/start`
    ///. Per THREAD, never per daemon: the daemon is shared and owns
    /// every thread on the box, so a grant applied at daemon scope would widen
    /// every other worker's sandbox at once. Empty for a yolo thread, which is
    /// already `danger-full-access` and would be NARROWED by a workspaceWrite
    /// policy, and empty when the seam published nothing.
    state_dirs: Vec<String>,
    /// The sandbox posture the server resolved for this thread, echoed back on
    /// every per-turn override so the grant widens the roots and changes
    /// nothing else. `None` when the thread is not `workspaceWrite`.
    resolved_sandbox: Option<Value>,
    /// The posture the spawn (or resume) REQUESTED, spelled onto the per-turn
    /// policy when the server resolved nothing. Per THREAD, set once at
    /// `thread/start` or `thread/resume`.
    requested: CodexPosture,
    /// The posture name the server reported, unfiltered, for the registry row.
    /// `None` only until `thread/start` answers; recorded as
    /// [`SANDBOX_POSTURE_UNKNOWN`] when the response named no sandbox.
    resolved_sandbox_type: Option<String>,
    current_turn_id: Option<String>,
}

/// Park one inbound frame: id-matched responses stay `Value`s in the deque,
/// `turn/completed` notifications are parsed ONCE here and keyed by turn id.
/// Pure so the parse-once contract is unit-testable without a child process.
fn park_frame(
    pending: &mut VecDeque<Value>,
    completed: &mut HashMap<String, TurnResult>,
    frame: Value,
) {
    match parse_turn_completed_value(&frame) {
        Some(turn) => {
            completed.insert(turn.turn_id.clone(), turn);
        }
        None => pending.push_back(frame),
    }
}

impl CodexThread {
    pub async fn start(
        cwd: impl Into<PathBuf>,
        model: Option<&str>,
        posture: &CodexPosture,
        effort: Option<&str>,
    ) -> Result<Self, ThreadDriverError> {
        Self::start_with_state_dirs(cwd, model, posture, effort, &[], None).await
    }

    /// [`CodexThread::start`] plus the roots this thread carries on every turn
    /// ([`granted_roots`]). Both postures carry them; only the `thread/start`
    /// scalar differs.
    pub async fn start_with_state_dirs(
        cwd: impl Into<PathBuf>,
        model: Option<&str>,
        posture: &CodexPosture,
        effort: Option<&str>,
        state_dirs: &[String],
        config: Option<&serde_json::Map<String, Value>>,
    ) -> Result<Self, ThreadDriverError> {
        let cwd = cwd.into();
        // `launch` completes the app-server handshake as part of connecting,
        // so the driver is protocol-ready the moment it exists.
        let mut driver = Self::launch(cwd.clone()).await?;
        // Project assignment: the thread rolls up under its repo's
        // ChatGPT Project instead of a cwd-keyed bucket. Resolution is
        // fail-open and bounded; a None below drops the key and the request
        // stays byte-identical to the unassigned form.
        let project_id = crate::codex_inject::ensure_project_for_cwd(&cwd).await;
        let request = thread_start_request_with_options(
            1,
            &cwd,
            model,
            posture,
            project_id.as_deref(),
            config,
        );
        let response = driver.request(1, request).await?;
        let (thread_id, rollout_path) = parse_thread_start_response(&response)
            .map_err(|error| ThreadDriverError::Protocol(error.to_string()))?;
        driver.thread_id = thread_id;
        driver.rollout_path = PathBuf::from(rollout_path);
        driver.effort = effort
            .filter(|effort| !effort.is_empty())
            .map(str::to_string);
        driver.context_window_request =
            crate::context_window::ContextWindowRequest::from_config(config);
        driver.state_dirs = granted_roots(&cwd, state_dirs);
        driver.requested = posture.clone();
        driver.resolved_sandbox = parse_resolved_sandbox(&response);
        driver.resolved_sandbox_type = parse_resolved_sandbox_type(&response);
        Ok(driver)
    }

    /// Resume an existing thread by its app-server id.
    ///
    /// The state-root grant cannot be reconstructed on this path, and
    /// the loss is announced rather than taken quietly. The roots reach a
    /// spawn from the Python seam's `FNO_WORKER_ADD_DIRS`, which the long-lived
    /// shared daemon does not have, and Rust deliberately runs no second copy
    /// of the resolver (`writable_dirs.published_worker_writable_dirs`: one
    /// published value, two readers). Reading the daemon's own env instead
    /// would grant whatever shell started it, which is wrong in a more
    /// dangerous direction.
    ///
    /// In practice the grant usually survives: a turn-level `sandboxPolicy`
    /// becomes the thread's default server-side, so a thread still loaded by
    /// the codex app-server keeps it across an `fno-agents-daemon` restart. It
    /// is lost only when the app-server itself restarted and reloaded the
    /// thread from its rollout. That worker is then mute again, so the caller
    /// gets an event instead of silence. The durable fix is a granted-roots
    /// receipt on the registry row, which belongs to the sibling node that
    /// owns that schema.
    ///
    /// Emitted only for a thread that COULD have lost something. A yolo thread
    /// is `danger-full-access` and needs no grant, and a resume that succeeds
    /// says nothing on its own, so the event fires after the resume and only
    /// for a bounded thread. An unconditional emit on every cache miss reports
    /// a loss that never happened, which is the kind of telemetry an operator
    /// learns to ignore.
    pub async fn resume(
        cwd: impl Into<PathBuf>,
        thread_id: &str,
        model: Option<&str>,
        posture: &CodexPosture,
        effort: Option<&str>,
        config: Option<&serde_json::Map<String, Value>>,
    ) -> Result<Self, ThreadDriverError> {
        Self::resume_with_state_dirs(cwd, thread_id, model, posture, effort, &[], config).await
    }

    /// [`CodexThread::resume`] plus the state-root grant. A resumed thread
    /// re-resolves its posture server-side, so a resume that forgot the roots
    /// would be the same silent defect with a slower fuse.
    pub async fn resume_with_state_dirs(
        cwd: impl Into<PathBuf>,
        thread_id: &str,
        model: Option<&str>,
        posture: &CodexPosture,
        effort: Option<&str>,
        state_dirs: &[String],
        config: Option<&serde_json::Map<String, Value>>,
    ) -> Result<Self, ThreadDriverError> {
        if thread_id.trim().is_empty() {
            return Err(ThreadDriverError::Protocol(
                "harness_session_id is required for codex resume".into(),
            ));
        }
        let cwd = cwd.into();
        // `launch` completes the app-server handshake as part of connecting,
        // so the driver is protocol-ready the moment it exists.
        let mut driver = Self::launch(cwd.clone()).await?;
        let request =
            thread_resume_request_with_options(1, thread_id, &cwd, model, posture, config);
        let response = driver.request(1, request).await?;
        let (confirmed_id, rollout_path) = parse_thread_start_response(&response)
            .map_err(|error| ThreadDriverError::Protocol(error.to_string()))?;
        if confirmed_id != thread_id {
            return Err(ThreadDriverError::Protocol(format!(
                "thread/resume returned {confirmed_id}, expected {thread_id}"
            )));
        }
        driver.thread_id = confirmed_id;
        driver.rollout_path = PathBuf::from(rollout_path);
        driver.effort = effort
            .filter(|effort| !effort.is_empty())
            .map(str::to_string);
        driver.context_window_request =
            crate::context_window::ContextWindowRequest::from_config(config);
        driver.state_dirs = granted_roots(&cwd, state_dirs);
        driver.requested = posture.clone();
        driver.resolved_sandbox = parse_resolved_sandbox(&response);
        driver.resolved_sandbox_type = parse_resolved_sandbox_type(&response);
        Ok(driver)
    }

    /// Connect this driver to the shared app-server daemon.
    ///
    /// The daemon is ensured FIRST, every time. Spawn-time health does not
    /// survive to the next connect: a shared daemon measured up at 00:46 was
    /// gone by 15:48 on the same machine, and the socket file outlives the
    /// process that served it, so its presence proves nothing.
    ///
    /// `cwd` stops being a process attribute here and travels as a
    /// `thread/start` parameter, where the protocol already carried it.
    async fn launch(cwd: PathBuf) -> Result<Self, ThreadDriverError> {
        // `ensure_codex_daemon` is synchronous and can block for up to 15s
        // booting the daemon, and its health probe joins a helper thread.
        // Running it inline would stall the calling runtime for that whole
        // window, which on a single-threaded executor also stalls whatever it
        // is waiting for.
        let ensured = tokio::task::spawn_blocking(crate::codex_inject::ensure_codex_daemon)
            .await
            .map_err(|error| {
                ThreadDriverError::Protocol(format!("daemon-ensure task failed: {error}"))
            })?
            .map_err(|error| {
                ThreadDriverError::Protocol(format!("codex app-server daemon unavailable: {error}"))
            })?;
        let socket = crate::codex_inject::codex_app_server_socket_path();
        let (sink, stream) = connect_app_server(&socket).await.map_err(|error| {
            ThreadDriverError::Protocol(format!(
                "codex app-server daemon at {} refused the connection: {error}",
                socket.display()
            ))
        })?;
        Ok(Self {
            sink,
            stream: Some(stream),
            daemon_pid: ensured.state.pid,
            pending: VecDeque::new(),
            completed_turns: HashMap::new(),
            next_id: 2,
            thread_id: String::new(),
            rollout_path: PathBuf::new(),
            cwd,
            effort: None,
            context_window_request: None,
            state_dirs: Vec::new(),
            requested: CodexPosture::bounded(),
            resolved_sandbox: None,
            resolved_sandbox_type: None,
            current_turn_id: None,
        })
    }

    async fn request(&mut self, id: u64, request: String) -> Result<String, ThreadDriverError> {
        self.request_value(id, request)
            .await
            .map(|value| value.to_string())
    }

    async fn request_value(
        &mut self,
        id: impl Into<Value>,
        request: String,
    ) -> Result<Value, ThreadDriverError> {
        self.write_frame(&request).await?;
        let id = id.into();
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ThreadDriverError::Timeout);
            }
            let value = self.read_value(remaining).await?;
            if value.get("id") == Some(&id) {
                return Ok(value);
            }
            park_frame(&mut self.pending, &mut self.completed_turns, value);
        }
    }

    /// Read one JSON frame, bounded by `read_timeout`. Callers that wait on a
    /// turn pass their remaining whole-turn budget: a fixed per-frame timeout
    /// would fire during the app-server's quiet gaps and abort a turn that was
    /// still running.
    async fn read_value(&mut self, read_timeout: Duration) -> Result<Value, ThreadDriverError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(ThreadDriverError::Protocol(
                "actor owns the read pump; legacy reads are unavailable".into(),
            ));
        };
        let deadline = Instant::now() + read_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ThreadDriverError::Timeout);
            }
            let frame = tokio::time::timeout(remaining, stream.next())
                .await
                .map_err(|_| ThreadDriverError::Timeout)?;
            match frame {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(text.trim()).map_err(|_| {
                        ThreadDriverError::Protocol("app-server emitted a non-JSON frame".into())
                    })
                }
                // Ping/pong/binary carry no protocol payload; keep reading
                // inside the SAME budget rather than charging the caller a
                // fresh one per control frame.
                Some(Ok(_)) => continue,
                // A closed socket means the SHARED DAEMON went away, not that
                // a child of ours exited. There is no exit status to report
                // and the remedy is different: the thread is still on disk
                // and a re-ensured daemon can resume it.
                Some(Err(error)) => {
                    return Err(ThreadDriverError::Protocol(format!(
                        "codex app-server daemon closed the connection: {error}"
                    )))
                }
                None => {
                    return Err(ThreadDriverError::Protocol(
                        "codex app-server daemon closed the connection".into(),
                    ))
                }
            }
        }
    }

    async fn take_completed(&mut self, turn_id: &str) -> Result<TurnResult, ThreadDriverError> {
        // Parse-once (Change 10): completions were parked as raw Values and
        // re-serialized per check; they are parsed once at push and claimed
        // from the map here. No entry is ever re-serialized.
        if let Some(turn) = self.completed_turns.remove(turn_id) {
            return Ok(turn);
        }
        // The turn budget is the only ceiling. A frame-count bound aborts
        // turns larger than the constant after the app-server already ran
        // them, and a per-frame timeout aborts turns with quiet gaps.
        let deadline = Instant::now() + TURN_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ThreadDriverError::Timeout);
            }
            let value = self.read_value(remaining).await?;
            park_frame(&mut self.pending, &mut self.completed_turns, value);
            if let Some(turn) = self.completed_turns.remove(turn_id) {
                return Ok(turn);
            }
        }
    }

    pub async fn drive_turn(&mut self, text: &str) -> Result<TurnResult, ThreadDriverError> {
        let request_id = self.next_id;
        self.next_id += 1;
        let request = turn_start_request_json_full(
            request_id,
            &self.thread_id,
            text,
            self.effort.as_deref(),
            &self.state_dirs,
            self.resolved_sandbox.as_ref(),
            &self.requested,
        );
        let response = self.request(request_id, request).await?;
        let turn_id = parse_turn_start_response(&response)?;
        self.current_turn_id = Some(turn_id.clone());
        let result = self.take_completed(&turn_id).await?;
        // INVARIANT (pinned, do not "fix"): `current_turn_id` is cleared ONLY
        // on this success path. After a client-side timeout the id is the only
        // interrupt handle left, and the actor's Interrupt command depends on
        // it surviving. The actor keeps the same rule: its driving turn id
        // survives a timed-out wait and dies only when the completion actually
        // routes or the child is dropped.
        self.current_turn_id = None;
        Ok(result)
    }

    /// Write-only `turn/start`: returns the request id so the actor can await
    /// the response on its own read pump. Does not touch `current_turn_id`;
    /// the caller pairs this with [`CodexThread::note_turn_started`] once the
    /// response confirms the turn id.
    pub async fn send_turn_start(&mut self, text: &str) -> Result<u64, ThreadDriverError> {
        let request_id = self.next_id;
        self.next_id += 1;
        let request = turn_start_request_json_full(
            request_id,
            &self.thread_id,
            text,
            self.effort.as_deref(),
            &self.state_dirs,
            self.resolved_sandbox.as_ref(),
            &self.requested,
        );
        self.write_frame(&request).await?;
        Ok(request_id)
    }

    /// Write-only `turn/steer` with the server-enforced precondition.
    pub async fn send_steer(
        &mut self,
        expected_turn_id: &str,
        text: &str,
    ) -> Result<u64, ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_frame(&turn_steer_request_json(
            id,
            &self.thread_id,
            expected_turn_id,
            text,
        ))
        .await?;
        Ok(id)
    }

    /// Write-only `turn/interrupt` against a full turn id.
    pub async fn send_interrupt(&mut self, turn_id: &str) -> Result<u64, ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_frame(&turn_interrupt_request_json(id, &self.thread_id, turn_id))
            .await?;
        Ok(id)
    }

    /// Write-only `review/start`.
    pub async fn send_review(
        &mut self,
        target: &ReviewTarget,
        delivery: ReviewDelivery,
    ) -> Result<u64, ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_frame(&crate::codex_inject::review_start_request_json_with_id(
            id,
            &self.thread_id,
            target,
            delivery,
        ))
        .await?;
        Ok(id)
    }

    /// Write one JSON-RPC frame to the shared daemon. One WebSocket text
    /// frame per request; the newline the stdio transport needed is not part
    /// of this protocol.
    async fn write_frame(&mut self, request: &str) -> Result<(), ThreadDriverError> {
        self.sink
            .send(Message::Text(request.to_string().into()))
            .await
            .map_err(|error| {
                ThreadDriverError::Protocol(format!(
                    "codex app-server daemon closed the connection while writing: {error}"
                ))
            })
    }

    /// Record the turn the driver is currently driving (actor path; the
    /// legacy path sets this inside `drive_turn`).
    pub fn note_turn_started(&mut self, turn_id: &str) {
        self.current_turn_id = Some(turn_id.to_string());
    }

    /// Clear `current_turn_id` ONLY when the named turn actually completed,
    /// preserving the drive_turn survivor invariant for the actor path too.
    pub fn note_turn_completed(&mut self, turn_id: &str) {
        if self.current_turn_id.as_deref() == Some(turn_id) {
            self.current_turn_id = None;
        }
    }

    /// Archive this thread history-preservingly (task 3): the codex
    /// app-server's `thread/archive` removes the thread from the ACTIVE
    /// surface while the stored conversation survives and `thread/resume`
    /// (which searches active and archived stores) still opens it. An error
    /// is returned, never swallowed: the caller records a partial outcome
    /// and retries rather than claiming retirement.
    pub async fn archive(&mut self) -> Result<(), ThreadDriverError> {
        let request = thread_archive_request_json(1);
        let _answer = self.request(1, request).await?;
        // The archive answer is a submit receipt, not a completion promise:
        // accept the ack shape and let the caller's own loaded-list check
        // prove the effect.
        Ok(())
    }

    pub async fn compact(&mut self) -> Result<Value, ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        let receipt = self
            .request_value(id, thread_compact_start_request_json(id, &self.thread_id))
            .await
            .and_then(provider_response)?;
        crate::context_window::verify_compaction_receipt(&receipt, &self.thread_id).map_err(
            |error| {
                ThreadDriverError::Protocol(format!("unverified compaction receipt: {error:?}"))
            },
        )
    }

    pub async fn goal_get(&mut self) -> Result<Value, ThreadDriverError> {
        let id = self.next_id;
        self.next_id += 1;
        self.request_value(id, thread_goal_get_request_json(id, &self.thread_id))
            .await
            .and_then(provider_response)
    }

    pub async fn goal_set(
        &mut self,
        objective: &str,
        status: &str,
    ) -> Result<Value, ThreadDriverError> {
        if objective.trim().is_empty() || !matches!(status, "active" | "paused") {
            return Err(ThreadDriverError::Protocol(
                "goal set needs a non-empty objective and active|paused status".into(),
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.request_value(
            id,
            thread_goal_set_request_json(id, &self.thread_id, objective, status),
        )
        .await
        .and_then(provider_response)
    }

    pub async fn goal_get_typed(&mut self) -> Result<Option<NativeGoal>, ThreadDriverError> {
        let value = self.goal_get().await?;
        parse_goal_value(&value)
    }

    pub async fn goal_set_typed(
        &mut self,
        objective: &str,
        status: GoalStatus,
        continuation_owner: Option<&str>,
    ) -> Result<NativeGoal, ThreadDriverError> {
        let status_wire = match status {
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Completed => "completed",
        };
        if objective.trim().is_empty() || matches!(status, GoalStatus::Completed) {
            return Err(ThreadDriverError::Protocol(
                "typed goal set needs a non-empty active or paused objective".into(),
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        let value = self
            .request_value(
                id,
                thread_goal_set_request_json_with_owner(
                    id,
                    &self.thread_id,
                    objective,
                    status_wire,
                    continuation_owner,
                ),
            )
            .await
            .and_then(provider_response)?;
        parse_goal_value(&value)?.ok_or_else(|| {
            ThreadDriverError::Protocol("thread/goal/set returned no typed goal".into())
        })
    }

    pub async fn ensure_reign_goal_typed(
        &mut self,
        scope: &str,
        continuation_owner: &str,
    ) -> Result<NativeGoal, ThreadDriverError> {
        let current = self.goal_get_typed().await?;
        ensure_reign_goal(current.as_ref(), scope, continuation_owner)?;
        let objective = reign_objective(scope);
        match current {
            Some(goal) if goal.status == GoalStatus::Active => Ok(goal),
            _ => {
                self.goal_set_typed(&objective, GoalStatus::Active, Some(continuation_owner))
                    .await
            }
        }
    }

    /// Unarchive this thread id so `thread/resume` finds it in the same
    /// live store it left. History-preserving in both directions.
    pub async fn unarchive(&mut self, thread_id: &str) -> Result<(), ThreadDriverError> {
        let request = thread_unarchive_request_json(1, thread_id);
        let _answer = self.request(1, request).await?;
        Ok(())
    }

    pub async fn steer(
        &mut self,
        expected_turn_id: &str,
        text: &str,
    ) -> Result<String, ThreadDriverError> {
        let id = self.send_steer(expected_turn_id, text).await?;
        let response = self.wait_for_response(id).await?;
        parse_turn_start_response_value(&response)
    }

    pub async fn interrupt(&mut self, turn_id: &str) -> Result<(), ThreadDriverError> {
        let id = self.send_interrupt(turn_id).await?;
        let response = self.wait_for_response(id).await?;
        if let Some(error) = response.get("error") {
            return Err(ThreadDriverError::Protocol(server_error(error)));
        }
        Ok(())
    }

    /// Await an id-matched response on the LEGACY in-struct read path (the
    /// write already happened in the `send_*` call).
    async fn wait_for_response(&mut self, id: u64) -> Result<Value, ThreadDriverError> {
        let id = Value::from(id);
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ThreadDriverError::Timeout);
            }
            let value = self.read_value(remaining).await?;
            if value.get("id") == Some(&id) {
                return Ok(value);
            }
            park_frame(&mut self.pending, &mut self.completed_turns, value);
        }
    }

    pub async fn review(
        &mut self,
        target: &ReviewTarget,
        delivery: ReviewDelivery,
    ) -> Result<ReviewResult, ThreadDriverError> {
        let response = self
            .request_value(
                1,
                review_start_request_json(&self.thread_id, target, delivery),
            )
            .await?;
        let (turn_id, review_thread_id) = parse_review_start_response(&response.to_string())
            .map_err(|error| ThreadDriverError::Protocol(error.to_string()))?;
        Ok(ReviewResult {
            turn_id,
            review_thread_id,
        })
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn rollout_path(&self) -> &Path {
        &self.rollout_path
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The posture the server RESOLVED for this thread, for the registry row.
    /// Distinct from the posture the spawn REQUESTED: a `yolo` thread asks for
    /// `danger-full-access` and the app-server can still keep its
    /// workspaceWrite default, so the two disagree and the row must not report
    /// the request as if it were the outcome.
    pub fn resolved_sandbox_posture(&self) -> &str {
        self.resolved_sandbox_type
            .as_deref()
            .unwrap_or(SANDBOX_POSTURE_UNKNOWN)
    }

    /// The writable roots this thread carries onto every `turn/start`.
    pub fn granted_writable_roots(&self) -> &[String] {
        &self.state_dirs
    }

    /// The posture this thread's start or resume REQUESTED. One source for the
    /// registry row and the per-turn policy replay: the entry builder reads it
    /// instead of taking a parallel copy of the same answer.
    pub fn requested_posture(&self) -> &CodexPosture {
        &self.requested
    }

    pub fn context_window_request(&self) -> Option<&crate::context_window::ContextWindowRequest> {
        self.context_window_request.as_ref()
    }

    /// Where the CURRENT turn's sandboxPolicy comes from: `resolved` when the
    /// server reported a posture the turn echoes, `requested` when the server
    /// named no sandbox and the row's own request is replayed instead. Two
    /// different worlds behind one policy object, named so a record can tell
    /// them apart.
    pub fn turn_policy_source(&self) -> &'static str {
        if self.resolved_sandbox.is_some() {
            "resolved"
        } else {
            "requested"
        }
    }

    /// The pid of the app-server SERVING this thread, which is the shared
    /// daemon. The registry row does NOT record it (thread rows carry
    /// `pid: None`; codex_thread_entry.rs), so the ownership claim ("a codex
    /// thread worker's app-server is the shared daemon's") is provable from
    /// the control socket and `thread/loaded/list`, with no inference from a
    /// process count.
    pub fn pid(&self) -> Option<u32> {
        self.daemon_pid
    }

    pub fn current_turn_id(&self) -> Option<&str> {
        self.current_turn_id.as_deref()
    }

    /// Convert this driver into a single-owner actor and hand back the cheap
    /// command handle. This replaces the old `Arc<Mutex<CodexThread>>` shape,
    /// where `drive_turn` held the guard across a whole (up to `TURN_TIMEOUT`)
    /// turn and every follow-up verb queued behind it. One task now owns the
    /// driver exclusively; the connection's read half moves into a dedicated
    /// read pump (a `select!` arm on a frame read is not cancel-safe:
    /// cancellation mid-frame drops buffered bytes and corrupts the stream).
    /// While a turn is
    /// driving, follow-ups STEER rather than block; only the wait for
    /// `turn/completed` is long, and that wait is the actor's idle loop, not a
    /// lock.
    ///
    /// `on_turn_done` fires once per completed turn, from the actor task, for
    /// every submitter class (ask, seed, mail steer) - the daemon uses it for
    /// the `agent_ask_done` event and the `last_message_at` bump.
    ///
    /// `on_turn_phase` fires at the driver's own turn transitions:
    /// [`ThreadTurnPhase::Working`] at the ack and on every keepalive tick
    /// that lands while a turn drives, [`ThreadTurnPhase::Done`] at the
    /// completion. The daemon maps these onto the row's inside-leg report, so
    /// a thread row's status comes from its driver with no pane attached.
    pub fn into_actor(
        mut self,
        on_turn_done: Arc<dyn Fn(TurnReceipt) + Send + Sync>,
        on_turn_phase: Arc<dyn Fn(ThreadTurnPhase) + Send + Sync>,
    ) -> CodexThreadActor {
        let pid = self.pid();
        let (cmd_tx, cmd_rx) = mpsc::channel(THREAD_CHANNEL_CAP);
        let (frame_tx, frame_rx) = mpsc::channel(THREAD_CHANNEL_CAP);
        let shared = Arc::new(ActorShared {
            turn_id: std::sync::Mutex::new(self.current_turn_id.clone()),
        });
        let stream = self
            .stream
            .take()
            .expect("the read half is only taken here, once, at actor birth");
        tokio::spawn(read_pump(stream, frame_tx));
        tokio::spawn(actor_task(
            self,
            frame_rx,
            cmd_rx,
            Arc::clone(&shared),
            on_turn_done,
            on_turn_phase,
        ));
        CodexThreadActor {
            tx: cmd_tx,
            pid,
            shared,
        }
    }
}

/// The actor's outbound command surface. Cloning is cheap; the daemon holds
/// one per registry name in `ctx.codex_threads`.
#[derive(Clone)]
pub struct CodexThreadActor {
    tx: mpsc::Sender<ThreadCommand>,
    pid: Option<u32>,
    shared: Arc<ActorShared>,
}

/// Turn-id cell shared between the actor task (writer) and handle holders
/// (readers): the in_flight ask receipt needs the turn id even after the
/// submitter's 90s wait expired. A std Mutex is fine - no await inside.
struct ActorShared {
    turn_id: std::sync::Mutex<Option<String>>,
}

impl ActorShared {
    fn set_turn_id(&self, turn_id: Option<String>) {
        *self.turn_id.lock().expect("turn-id cell poisoned") = turn_id;
    }

    fn current_turn_id(&self) -> Option<String> {
        self.turn_id.lock().expect("turn-id cell poisoned").clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReceipt {
    pub turn_id: String,
    pub status: String,
    pub text: String,
}

impl From<TurnResult> for TurnReceipt {
    fn from(result: TurnResult) -> Self {
        Self {
            turn_id: result.turn_id,
            status: result.status,
            text: result.text,
        }
    }
}

pub enum ThreadCommand {
    /// Drive (or steer into) a turn; `reply` resolves at completion and the
    /// optional `accept` resolves at ACCEPTANCE (the turn/start ack when idle,
    /// the steer ack when driving) - the protocol's own delivery receipt, for
    /// callers like the switchboard that must not wait out a whole turn.
    Submit {
        body: String,
        reply: oneshot::Sender<Result<TurnReceipt, String>>,
        accept: Option<oneshot::Sender<Result<String, String>>>,
    },
    /// Interrupt the in-flight turn and report its terminal state.
    Interrupt {
        ack: oneshot::Sender<InterruptOutcome>,
    },
    /// Start a review turn; the reply is the review receipt.
    Review {
        target: ReviewTarget,
        delivery: ReviewDelivery,
        reply: oneshot::Sender<Result<ReviewResult, String>>,
    },
    /// End the actor task and close this driver's connection to the shared
    /// daemon. The ack fires only AFTER the driver is dropped, so a caller
    /// that waits for it never reads a still-connected driver as stopped.
    /// Closing the connection does NOT end the thread: the daemon owns it and
    /// it stays resumable, which is the durability this lane promises.
    Shutdown { ack: oneshot::Sender<()> },
}

type SubmitReplyTx = oneshot::Sender<Result<TurnReceipt, String>>;
type AcceptTx = oneshot::Sender<Result<String, String>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptOutcome {
    /// No turn was in flight; nothing to interrupt, safe to drop the child.
    NoTurnInFlight,
    /// The interrupted turn reached a terminal status (normally `interrupted`).
    Interrupted(TurnReceipt),
    /// The bounded settle wait expired with the turn still driving. The
    /// caller must not report a clean stop over a live turn. Dropping the
    /// actor closes this driver's connection but does NOT end the turn: the
    /// shared daemon owns the thread and keeps running it, so the honest
    /// report is that the turn was left running, never that a child was
    /// killed.
    Timeout,
}

impl CodexThreadActor {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The turn currently driving, if any - the interrupt handle that
    /// survives a caller-side timeout (the pinned drive_turn invariant).
    pub fn current_turn_id(&self) -> Option<String> {
        self.shared.current_turn_id()
    }

    /// Queue a submit; the returned receiver resolves with the turn receipt at
    /// completion. The CALLER bounds the wait (the daemon's ask uses
    /// [`ask_wait`] and answers `in_flight` on expiry).
    pub async fn submit(
        &self,
        body: String,
    ) -> Result<oneshot::Receiver<Result<TurnReceipt, String>>, String> {
        self.submit_with_accept(body, None).await
    }

    /// [`CodexThreadActor::submit`] with an acceptance channel: `accept`
    /// resolves Ok(turn_id) the moment the actor accepts the body (start or
    /// steer ack) and Err if the turn is refused.
    pub async fn submit_with_accept(
        &self,
        body: String,
        accept: Option<AcceptTx>,
    ) -> Result<oneshot::Receiver<Result<TurnReceipt, String>>, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ThreadCommand::Submit {
                body,
                reply: reply_tx,
                accept,
            })
            .await
            .map_err(|_| "codex thread actor is gone".to_string())?;
        Ok(reply_rx)
    }

    pub async fn interrupt(&self) -> Result<InterruptOutcome, String> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(ThreadCommand::Interrupt { ack: ack_tx })
            .await
            .map_err(|_| "codex thread actor is gone".to_string())?;
        ack_rx
            .await
            .map_err(|_| "codex thread actor is gone".to_string())
    }

    pub async fn review(
        &self,
        target: ReviewTarget,
        delivery: ReviewDelivery,
    ) -> Result<ReviewResult, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ThreadCommand::Review {
                target,
                delivery,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "codex thread actor is gone".to_string())?;
        reply_rx
            .await
            .map_err(|_| "codex thread actor is gone".to_string())
            .and_then(|inner| inner)
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.tx
            .send(ThreadCommand::Shutdown { ack: ack_tx })
            .await
            .map_err(|_| "codex thread actor is gone".to_string())?;
        ack_rx
            .await
            .map_err(|_| "codex thread actor is gone".to_string())
    }
}

/// Sequential frame pump from the daemon connection's read half into the
/// actor. Owns the reader so the actor's `select!` never holds a cancellable
/// frame read. Channel close (actor gone) or a closed socket ends the task; a
/// closed socket means the SHARED DAEMON went away, and the thread it owns
/// survives that on disk.
async fn read_pump(mut stream: AppServerStream, tx: mpsc::Sender<Value>) {
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                let Ok(value) = serde_json::from_str::<Value>(text.trim()) else {
                    continue;
                };
                if tx.send(value).await.is_err() {
                    break;
                }
            }
            // Ping/pong/binary carry no protocol payload.
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => break,
        }
    }
}

struct Driving {
    turn_id: String,
    waiters: Vec<oneshot::Sender<Result<TurnReceipt, String>>>,
}

struct ActorCtx {
    driver: CodexThread,
    /// Inbound frames routed while awaiting a response for another id.
    pending: HashMap<u64, Value>,
    /// Completed turns keyed by id: claimed by waiters on match, parked
    /// (capped) when their waiter is gone, claimable by `await_turn_end`.
    completed: HashMap<String, TurnReceipt>,
    driving: Option<Driving>,
    shared: Arc<ActorShared>,
    on_turn_done: Arc<dyn Fn(TurnReceipt) + Send + Sync>,
    on_turn_phase: Arc<dyn Fn(ThreadTurnPhase) + Send + Sync>,
}

async fn actor_task(
    driver: CodexThread,
    mut frames: mpsc::Receiver<Value>,
    mut cmds: mpsc::Receiver<ThreadCommand>,
    shared: Arc<ActorShared>,
    on_turn_done: Arc<dyn Fn(TurnReceipt) + Send + Sync>,
    on_turn_phase: Arc<dyn Fn(ThreadTurnPhase) + Send + Sync>,
) {
    let mut ctx = ActorCtx {
        driver,
        pending: HashMap::new(),
        completed: HashMap::new(),
        driving: None,
        shared,
        on_turn_done,
        on_turn_phase,
    };
    // The driver-status keepalive lives as a select arm, NOT a
    // separate task: a separate task would hold a `cmd_tx` clone forever, and
    // `cmds.recv()` would then never answer None - the "every handle dropped"
    // exit the arm below promises would be unreachable. A turn drives while
    // the actor sits at this loop (the completion routes through the frame
    // arm), so the tick is observable exactly while it matters; only the
    // bounded interrupt-settle wait pauses it.
    let mut next_keepalive = tokio::time::Instant::now() + thread_turn_refresh();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next_keepalive) => {
                next_keepalive = tokio::time::Instant::now() + thread_turn_refresh();
                if ctx.driving.is_some() {
                    // Rewrite `working` while a turn drives so a turn longer
                    // than the report's ttl never ages to Unmeasured. The
                    // check runs in the actor loop, so a tick landing after
                    // the completion fires nothing.
                    (ctx.on_turn_phase)(ThreadTurnPhase::Working);
                }
            }
            cmd = cmds.recv() => {
                match cmd {
                    None => {
                        // Every handle dropped; end the actor and close the
                        // connection. The daemon-owned thread outlives this.
                        ctx.fail_waiters("codex thread actor handles all dropped");
                        return;
                    }
                    Some(ThreadCommand::Shutdown { ack }) => {
                        ctx.fail_waiters("codex thread actor shut down");
                        // Closes this driver's connection to the shared
                        // daemon, before the ack, so a caller that waits for
                        // the ack never reads a still-connected driver as
                        // stopped. The THREAD survives: the daemon owns it.
                        drop(ctx.driver);
                        let _ = ack.send(());
                        return;
                    }
                    Some(cmd) => ctx.handle_command(cmd, &mut frames).await,
                }
            }
            frame = frames.recv() => {
                match frame {
                    None => {
                        // The connection closed: the SHARED daemon is gone.
                        // The thread itself is still on disk and resumable.
                        ctx.fail_waiters("codex app-server daemon closed the connection");
                        return;
                    }
                    Some(frame) => ctx.route_frame(frame),
                }
            }
        }
    }
}

enum TurnEnd {
    /// The turn reached a terminal state; waiters were resolved by routing.
    Ended,
    /// The bound expired with the turn still driving.
    TimedOut,
}

impl ActorCtx {
    /// File one inbound frame without parsing anything twice: id-matched
    /// responses park by id; `turn/completed` notifications settle the driving
    /// turn (or park, capped, when nobody waits on it).
    fn route_frame(&mut self, frame: Value) {
        if frame.get("id").is_some() {
            if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                self.pending.insert(id, frame);
            }
            return;
        }
        if let Some(result) = parse_turn_completed_value(&frame) {
            self.complete_turn(result);
        }
        // Every other notification (turn/event noise) is dropped: the rollout
        // on disk is the durable record.
    }

    fn complete_turn(&mut self, result: TurnResult) {
        let turn_id = result.turn_id.clone();
        let receipt: TurnReceipt = result.into();
        self.park_completed(turn_id.clone(), receipt.clone());
        // take(), not take()-then-filter: a completion for a turn nobody
        // drives (the review lane's own turn id, or a stale completion racing
        // the steer-precondition retry) must stay parked telemetry above -
        // stripping the driving value here dropped its waiters unresolved
        // and left the shared turn id stale.
        let Some(driving) = self.driving.take_if(|driving| driving.turn_id == turn_id) else {
            return;
        };
        self.driver.note_turn_completed(&turn_id);
        self.shared.set_turn_id(None);
        for waiter in driving.waiters {
            let _ = waiter.send(Ok(receipt.clone()));
        }
        (self.on_turn_phase)(ThreadTurnPhase::Done);
        (self.on_turn_done)(receipt);
    }

    fn park_completed(&mut self, turn_id: String, receipt: TurnReceipt) {
        if self.completed.len() >= COMPLETED_PARK_CAP {
            // Drop an arbitrary parked entry: an unclaimed completion is
            // telemetry (the rollout holds the real record), and an unbounded
            // map would grow on every waiter-gone completion.
            if let Some(oldest) = self.completed.keys().next().cloned() {
                self.completed.remove(&oldest);
            }
        }
        self.completed.insert(turn_id, receipt);
    }

    fn fail_waiters(&mut self, message: &str) {
        if let Some(driving) = self.driving.take() {
            self.shared.set_turn_id(None);
            for waiter in driving.waiters {
                // The error names the turn and reads as a RESTART of the
                // daemon-owned thread, never as a failed turn: the thread
                // and its transcript survive, and a sender that believed
                // the turn itself failed would report a lie upstream.
                let _ = waiter.send(Err(format!(
                    "{message} (turn {turn}; the daemon-owned thread and its transcript survive)",
                    turn = driving.turn_id,
                )));
            }
        }
    }

    fn turn_ended(&self, turn_id: &str) -> bool {
        match &self.driving {
            Some(driving) => driving.turn_id != turn_id,
            None => true,
        }
    }

    /// Await one id-matched response, routing every frame that arrives first
    /// (a completion for the driving turn still resolves its waiters).
    async fn await_response(
        &mut self,
        id: u64,
        frames: &mut mpsc::Receiver<Value>,
    ) -> Result<Value, String> {
        self.await_response_bounded(id, frames, REQUEST_TIMEOUT)
            .await
    }

    /// [`Self::await_response`] under a caller-supplied bound, so one shared
    /// deadline can span a response wait and the turn settle that follows it.
    async fn await_response_bounded(
        &mut self,
        id: u64,
        frames: &mut mpsc::Receiver<Value>,
        bound: Duration,
    ) -> Result<Value, String> {
        if let Some(value) = self.pending.remove(&id) {
            return Ok(value);
        }
        let deadline = Instant::now() + bound;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("codex app-server response timed out".into());
            }
            let frame = match tokio::time::timeout(remaining, frames.recv()).await {
                Ok(Some(frame)) => frame,
                Ok(None) => return Err("codex app-server closed stdout".into()),
                Err(_) => return Err("codex app-server response timed out".into()),
            };
            self.route_frame(frame);
            if let Some(value) = self.pending.remove(&id) {
                return Ok(value);
            }
        }
    }

    /// Await a turn's terminal state. `Ended` means routing already resolved
    /// the waiters; the receipt itself stays claimable from the completed map.
    async fn await_turn_end(
        &mut self,
        turn_id: &str,
        bound: Duration,
        frames: &mut mpsc::Receiver<Value>,
    ) -> TurnEnd {
        if self.turn_ended(turn_id) {
            return TurnEnd::Ended;
        }
        let deadline = Instant::now() + bound;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return TurnEnd::TimedOut;
            }
            match tokio::time::timeout(remaining, frames.recv()).await {
                Ok(Some(frame)) => {
                    self.route_frame(frame);
                    if self.turn_ended(turn_id) {
                        return TurnEnd::Ended;
                    }
                }
                // Channel closed: no further frame can end the turn.
                Ok(None) => {
                    return if self.turn_ended(turn_id) {
                        TurnEnd::Ended
                    } else {
                        TurnEnd::TimedOut
                    }
                }
                // Elapsed slice with no frame; loop re-checks the deadline.
                Err(_) => continue,
            }
        }
    }

    async fn handle_command(&mut self, cmd: ThreadCommand, frames: &mut mpsc::Receiver<Value>) {
        match cmd {
            ThreadCommand::Submit {
                body,
                reply,
                accept,
            } => self.handle_submit(body, reply, accept, frames).await,
            ThreadCommand::Interrupt { ack } => {
                let outcome = self.handle_interrupt(frames).await;
                let _ = ack.send(outcome);
            }
            ThreadCommand::Review {
                target,
                delivery,
                reply,
            } => {
                let result = match self.driver.send_review(&target, delivery).await {
                    Ok(id) => self.await_response(id, frames).await.and_then(|response| {
                        parse_review_start_response(&response.to_string())
                            .map(|(turn_id, review_thread_id)| ReviewResult {
                                turn_id,
                                review_thread_id,
                            })
                            .map_err(|error| error.to_string())
                    }),
                    Err(error) => Err(error.to_string()),
                };
                let _ = reply.send(result);
            }
            ThreadCommand::Shutdown { .. } => unreachable!("handled in the select arm"),
        }
    }

    async fn handle_submit(
        &mut self,
        body: String,
        reply: SubmitReplyTx,
        accept: Option<AcceptTx>,
        frames: &mut mpsc::Receiver<Value>,
    ) {
        let driving_turn = self.driving.as_ref().map(|driving| driving.turn_id.clone());
        match driving_turn {
            // Idle: drive a fresh turn. The reply resolves when the completion
            // routes in the main loop.
            None => self.start_turn(body, reply, accept, frames).await,
            Some(expected) => {
                // Driving: steer into the in-flight turn instead of queueing
                // behind it. The steer ack returns in milliseconds; the
                // submitter rides the shared completion.
                let sent = self.driver.send_steer(&expected, &body).await;
                let ack = match sent {
                    Ok(id) => self.await_response(id, frames).await,
                    Err(error) => Err(error.to_string()),
                };
                // An error PAYLOAD is an Err here: a failed expectedTurnId
                // precondition arrives as a well-formed response with an
                // `error` body, not as a transport failure.
                let ack = ack.and_then(|response| {
                    if response.get("error").is_some() {
                        Err("turn/steer was refused".to_string())
                    } else {
                        Ok(response)
                    }
                });
                match ack {
                    Ok(_) => {
                        // The turn may have COMPLETED while the steer exchange
                        // ran (routing resolved the waiters and cleared
                        // `driving`); only attach the waiter to a live turn.
                        let still_driving = self
                            .driving
                            .as_mut()
                            .filter(|driving| driving.turn_id == expected);
                        match still_driving {
                            Some(driving) => {
                                if let Some(accept) = accept {
                                    let _ = accept.send(Ok(expected));
                                }
                                driving.waiters.push(reply);
                            }
                            None => self.start_turn(body, reply, accept, frames).await,
                        }
                    }
                    Err(_) => {
                        // Steer failed its expectedTurnId precondition: the
                        // turn completed in the race window. Drain the old
                        // completion (bounded) so its waiters resolve, then
                        // retry ONCE as a fresh turn/start - the submitter
                        // still gets a reply either way.
                        match self
                            .await_turn_end(&expected, TURN_SETTLE_TIMEOUT, frames)
                            .await
                        {
                            TurnEnd::Ended => {}
                            TurnEnd::TimedOut => {
                                let message = format!(
                                    "turn {expected} completed without a receipt and the \
                                     completion never arrived"
                                );
                                if let Some(driving) = self.driving.take() {
                                    self.shared.set_turn_id(None);
                                    for waiter in driving.waiters {
                                        let _ = waiter.send(Err(message.clone()));
                                    }
                                }
                            }
                        }
                        self.start_turn(body, reply, accept, frames).await;
                    }
                }
            }
        }
    }

    async fn start_turn(
        &mut self,
        body: String,
        reply: SubmitReplyTx,
        accept: Option<AcceptTx>,
        frames: &mut mpsc::Receiver<Value>,
    ) {
        let sent = self.driver.send_turn_start(&body).await;
        let response = match sent {
            Ok(id) => self.await_response(id, frames).await,
            Err(error) => Err(error.to_string()),
        };
        match response
            .and_then(|value| parse_turn_start_response_value(&value).map_err(|e| e.to_string()))
        {
            Ok(turn_id) => {
                if let Some(accept) = accept {
                    let _ = accept.send(Ok(turn_id.clone()));
                }
                self.driver.note_turn_started(&turn_id);
                self.shared.set_turn_id(Some(turn_id.clone()));
                self.driving = Some(Driving {
                    turn_id,
                    waiters: vec![reply],
                });
                (self.on_turn_phase)(ThreadTurnPhase::Working);
            }
            Err(error) => {
                if let Some(accept) = accept {
                    let _ = accept.send(Err(error.clone()));
                }
                let _ = reply.send(Err(error));
            }
        }
    }

    async fn handle_interrupt(&mut self, frames: &mut mpsc::Receiver<Value>) -> InterruptOutcome {
        let Some(turn_id) = self.driving.as_ref().map(|driving| driving.turn_id.clone()) else {
            return InterruptOutcome::NoTurnInFlight;
        };
        // See the pinned drive_turn invariant: `turn_id` here IS the surviving
        // interrupt handle, also after any caller-side timeout.
        // The RPC ack wait and the settle wait share one deadline: stacked
        // full bounds once let the actor outlive the daemon's outer stop
        // bound, which stalled the shutdown ack past the client's deadline.
        let deadline = Instant::now() + interrupt_total_bound();
        let sent = self.driver.send_interrupt(&turn_id).await;
        let ack = match sent {
            Ok(id) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                self.await_response_bounded(id, frames, remaining).await
            }
            Err(error) => Err(error.to_string()),
        };
        match ack {
            Ok(response) if response.get("error").is_none() => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.await_turn_end(&turn_id, remaining, frames).await {
                    TurnEnd::Ended => {
                        let receipt = self.completed.remove(&turn_id).unwrap_or(TurnReceipt {
                            turn_id: turn_id.clone(),
                            status: "interrupted".into(),
                            text: String::new(),
                        });
                        InterruptOutcome::Interrupted(receipt)
                    }
                    TurnEnd::TimedOut => InterruptOutcome::Timeout,
                }
            }
            _ => {
                // The interrupt RPC failed - commonly because the turn
                // completed and routing already ended it mid-exchange.
                if self.turn_ended(&turn_id) {
                    let receipt = self.completed.remove(&turn_id).unwrap_or(TurnReceipt {
                        turn_id: turn_id.clone(),
                        status: "completed".into(),
                        text: String::new(),
                    });
                    InterruptOutcome::Interrupted(receipt)
                } else {
                    InterruptOutcome::Timeout
                }
            }
        }
    }
}

/// What a fenced-token list carries onto the codex thread lane.
pub struct HarnessCarry {
    /// `-c`/`--config key=value` pairs. Keys stay dotted strings (the
    /// app-server owns their meaning); values parse as TOML like the codex
    /// CLI, and a non-TOML value stays a string.
    pub config: serde_json::Map<String, Value>,
    /// `--add-dir` roots, appended to the state-root grant.
    pub add_dirs: Vec<String>,
}

/// Parse the fenced `--` tokens a spawn routed to this lane. Only the
/// spellings the codex CLI itself takes are accepted: `-c`/`--config
/// key=value` and `--add-dir <dir>`, two-token or `=`-joined on the long
/// forms. Anything else is a front-door contract break - the spawn front
/// door demotes a flag this lane cannot carry to the pane, so a stray token
/// here is a router bug - and is refused by name rather than dropped
/// silently.
pub fn parse_harness_args(tokens: &[String]) -> Result<HarnessCarry, String> {
    fn toml_value(raw: &str) -> Value {
        // `toml::from_str` reads a DOCUMENT, so a bare value like `true`
        // needs a key to hang off; the wrap is local to this parse.
        toml::from_str::<toml::Table>(&format!("v = {raw}"))
            .ok()
            .and_then(|table| table.get("v").cloned())
            .and_then(|parsed| serde_json::to_value(parsed).ok())
            .unwrap_or_else(|| json!(raw))
    }
    let mut carry = HarnessCarry {
        config: serde_json::Map::new(),
        add_dirs: Vec::new(),
    };
    let mut values = tokens.iter();
    while let Some(token) = values.next() {
        let (flag, inline): (&str, Option<&str>) = match token.split_once('=') {
            Some((flag, rest)) if flag == "--config" || flag == "--add-dir" => (flag, Some(rest)),
            _ => (token.as_str(), None),
        };
        match (flag, inline) {
            ("--add-dir", Some(dir)) => carry.add_dirs.push(dir.to_string()),
            ("--add-dir", None) => match values.next() {
                Some(dir) => carry.add_dirs.push(dir.clone()),
                None => return Err("--add-dir arrives with no directory".into()),
            },
            ("-c", None) | ("--config", None) => {
                let Some(pair) = values.next() else {
                    return Err(format!("config flag {flag} arrives with no key=value pair"));
                };
                let Some((key, raw)) = pair.split_once('=') else {
                    return Err(format!("config value {pair:?} is not key=value"));
                };
                carry.config.insert(key.to_string(), toml_value(raw));
            }
            ("--config", Some(rest)) => {
                let Some((key, raw)) = rest.split_once('=') else {
                    return Err(format!("config value {rest:?} is not key=value"));
                };
                carry.config.insert(key.to_string(), toml_value(raw));
            }
            _ => {
                return Err(format!(
                    "harness token {token:?} is not one the codex thread lane carries \
                     (-c/--config key=value, --add-dir dir); the spawn front door owns \
                     the -- fence and should have routed it to the pane"
                ));
            }
        }
    }
    Ok(carry)
}

fn thread_start_request_with_options(
    id: u64,
    cwd: &Path,
    model: Option<&str>,
    posture: &CodexPosture,
    project_id: Option<&str>,
    config: Option<&serde_json::Map<String, Value>>,
) -> String {
    let mut params = json!({
        "cwd": cwd,
        "sandbox": posture.sandbox.as_scalar(),
        "approvalPolicy": posture.approval.as_str(),
    });
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        params["model"] = json!(model);
    }
    if let Some(project_id) = project_id.filter(|id| !id.is_empty()) {
        params["projectId"] = json!(project_id);
    }
    // Absent or empty keeps the frame byte-identical to the pre-config form.
    if let Some(config) = config.filter(|config| !config.is_empty()) {
        params["config"] = Value::Object(config.clone());
    }
    json!({"id": id, "method": "thread/start", "params": params}).to_string()
}

fn thread_resume_request_with_options(
    id: u64,
    thread_id: &str,
    cwd: &Path,
    model: Option<&str>,
    posture: &CodexPosture,
    config: Option<&serde_json::Map<String, Value>>,
) -> String {
    let mut params = json!({
        "threadId": thread_id,
        "cwd": cwd,
        "sandbox": posture.sandbox.as_scalar(),
        "approvalPolicy": posture.approval.as_str(),
    });
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        params["model"] = json!(model);
    }
    if let Some(config) = config.filter(|config| !config.is_empty()) {
        params["config"] = Value::Object(config.clone());
    }
    json!({"id": id, "method": "thread/resume", "params": params}).to_string()
}

fn provider_response(value: Value) -> Result<Value, ThreadDriverError> {
    if let Some(error) = value.get("error") {
        return Err(ThreadDriverError::Protocol(server_error(error)));
    }
    Ok(value)
}

fn server_error(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .unwrap_or("unknown app-server error")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_resume_carries_full_id_and_worktree() {
        let value: Value = serde_json::from_str(&thread_resume_request_json(
            "thread-1",
            "/tmp/worktree",
            "never",
        ))
        .unwrap();
        assert_eq!(value["method"], "thread/resume");
        assert_eq!(value["params"]["threadId"], "thread-1");
        assert_eq!(value["params"]["cwd"], "/tmp/worktree");
    }

    #[test]
    fn native_provider_actions_pin_the_full_thread_id() {
        let compact: Value = serde_json::from_str(&thread_compact_start_request_json(
            7,
            "01a0acac-9c64-72f3-99b5-e26620eb1c6d",
        ))
        .unwrap();
        assert_eq!(compact["method"], "thread/compact/start");
        assert_eq!(
            compact["params"]["threadId"],
            "01a0acac-9c64-72f3-99b5-e26620eb1c6d"
        );

        let goal: Value = serde_json::from_str(&thread_goal_set_request_json(
            8,
            "thread-full",
            "$fno:reign x-0000",
            "active",
        ))
        .unwrap();
        assert_eq!(goal["method"], "thread/goal/set");
        assert_eq!(goal["params"]["status"], "active");
        assert_eq!(goal["params"]["goal"], "$fno:reign x-0000");
    }

    #[test]
    fn steer_requires_expected_turn_id() {
        let value: Value = serde_json::from_str(&turn_steer_request_json(
            7, "thread-1", "turn-1", "continue",
        ))
        .unwrap();
        assert_eq!(value["params"]["expectedTurnId"], "turn-1");
    }

    /// A spawn that spells its posture as `permission_mode` reaches the same
    /// frame a `yolo` bool reaches. Asserted THROUGH the frame rather than on
    /// the resolver's answer alone: the typed posture is an implementation
    /// detail and the wire field is what the app-server reads.
    #[test]
    fn permission_mode_yolo_reaches_a_full_access_frame() {
        let yolo = resolve_thread_posture(None, Some("yolo")).expect("yolo maps");
        let frame: Value = serde_json::from_str(&thread_start_request_with_options(
            1,
            std::path::Path::new("/tmp/w"),
            None,
            &yolo,
            None,
            None,
        ))
        .unwrap();
        assert_eq!(frame["params"]["sandbox"], "danger-full-access");
        assert_eq!(frame["params"]["approvalPolicy"], "never");

        // The explicit pair form resolves off its halves, not its name.
        let paired =
            resolve_thread_posture(None, Some("danger-full-access:never")).expect("pair maps");
        let frame: Value = serde_json::from_str(&thread_start_request_with_options(
            1,
            std::path::Path::new("/tmp/w"),
            None,
            &paired,
            None,
            None,
        ))
        .unwrap();
        assert_eq!(frame["params"]["sandbox"], "danger-full-access");
        assert_eq!(frame["params"]["approvalPolicy"], "never");
    }

    /// AC11: the resume request carries the recorded posture, so a daemon
    /// restart cannot silently demote a yolo worker to workspace-write. The
    /// read-only spelling proves the halves are typed, not a bool.
    #[test]
    fn thread_resume_carries_the_recorded_sandbox_posture() {
        let full = CodexPosture::full_access();
        let full: Value = serde_json::from_str(&thread_resume_request_with_options(
            1,
            "thread-p",
            std::path::Path::new("/tmp/w"),
            None,
            &full,
            None,
        ))
        .unwrap();
        assert_eq!(full["params"]["sandbox"], "danger-full-access");
        assert_eq!(full["params"]["approvalPolicy"], "never");
        let read_only = CodexPosture::from_record(Some("read-only:on-request"), None);
        let read_only: Value = serde_json::from_str(&thread_resume_request_with_options(
            1,
            "thread-p",
            std::path::Path::new("/tmp/w"),
            None,
            &read_only,
            None,
        ))
        .unwrap();
        assert_eq!(read_only["params"]["sandbox"], "read-only");
        assert_eq!(read_only["params"]["approvalPolicy"], "on-request");
    }

    /// An empty config map is the absent form: the key is omitted so the
    /// frame stays byte-identical to the pre-config shape (AC2-EDGE).
    #[test]
    fn thread_requests_omit_an_empty_config_map() {
        let empty = serde_json::Map::new();
        let start: Value = serde_json::from_str(&thread_start_request_with_options(
            1,
            std::path::Path::new("/tmp/w"),
            None,
            &CodexPosture::bounded(),
            None,
            Some(&empty),
        ))
        .unwrap();
        let resume: Value = serde_json::from_str(&thread_resume_request_with_options(
            1,
            "thread-p",
            std::path::Path::new("/tmp/w"),
            None,
            &CodexPosture::bounded(),
            Some(&empty),
        ))
        .unwrap();
        assert!(start["params"].get("config").is_none());
        assert!(resume["params"].get("config").is_none());

        let mut config = serde_json::Map::new();
        config.insert(
            "sandbox_workspace_write.network_access".to_string(),
            json!(true),
        );
        let start: Value = serde_json::from_str(&thread_start_request_with_options(
            1,
            std::path::Path::new("/tmp/w"),
            None,
            &CodexPosture::bounded(),
            None,
            Some(&config),
        ))
        .unwrap();
        assert_eq!(
            start["params"]["config"]["sandbox_workspace_write.network_access"],
            json!(true),
            "a populated map rides thread/start verbatim"
        );
    }

    /// The fenced-token parser: TOML-typed values, string fallback, and the
    /// by-name refusal that names the front door.
    #[test]
    fn harness_args_parse_into_config_and_add_dirs() {
        let tokens: Vec<String> = [
            "-c",
            "sandbox_workspace_write.network_access=true",
            "--add-dir",
            "/tmp/x",
            "--config=model_reasoning_effort=high",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let carry = parse_harness_args(&tokens).unwrap();
        assert_eq!(
            carry.config["sandbox_workspace_write.network_access"],
            json!(true),
            "a TOML boolean parses as a boolean"
        );
        assert_eq!(
            carry.config["model_reasoning_effort"],
            json!("high"),
            "a non-TOML scalar stays a string, as the codex CLI reads it"
        );
        assert_eq!(carry.add_dirs, vec!["/tmp/x".to_string()]);

        let error = parse_harness_args(&["-p".to_string(), "profile".to_string()])
            .err()
            .expect("an unrouted token refuses");
        assert!(error.contains("-p"), "the refusal names the token: {error}");
        assert!(
            error.contains("front door"),
            "the refusal names the router, not a silent drop: {error}"
        );

        let error = parse_harness_args(&["--add-dir".to_string()])
            .err()
            .expect("a dangling flag refuses");
        assert!(
            error.contains("--add-dir"),
            "the refusal names the flag: {error}"
        );
    }

    #[test]
    fn turn_start_carries_reasoning_effort_when_requested() {
        let value: Value = serde_json::from_str(&turn_start_request_json_with_effort(
            7,
            "thread-1",
            "continue",
            Some("high"),
        ))
        .unwrap();
        assert_eq!(value["params"]["effort"], "high");
    }

    /// The grant rides `turn/start`, and only there. Measured against the live
    /// app-server on 2026-08-28: `thread/start` ignores a `sandboxPolicy`
    /// object without erroring, `turn/start` honors it.
    #[test]
    fn turn_start_carries_the_state_root_grant() {
        let roots = vec!["/Users/x/.fno".to_string()];
        let value: Value = serde_json::from_str(&turn_start_request_json_full(
            7,
            "thread-1",
            "go",
            None,
            &roots,
            None,
            &CodexPosture::bounded(),
        ))
        .unwrap();
        assert_eq!(value["params"]["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(
            value["params"]["sandboxPolicy"]["writableRoots"],
            json!(["/Users/x/.fno"])
        );
        // Network rides with the grant: the keeper socket and gh egress share
        // one seatbelt switch, so a directory grant without network still
        // cannot claim a node.
        assert_eq!(value["params"]["sandboxPolicy"]["networkAccess"], true);
    }

    /// The turn replaces the WHOLE policy, so a bounded thread whose server
    /// posture reads network off would keep starving: its `false` would stand.
    /// The force-on must overwrite exactly that field and echo the rest.
    #[test]
    fn turn_start_forces_network_on_a_network_off_posture() {
        let resolved = json!({
            "type": "workspaceWrite",
            "writableRoots": ["/repo/already-granted"],
            "networkAccess": false,
            "excludeSlashTmp": false,
            "excludeTmpdirEnvVar": true,
        });
        let roots = vec!["/Users/x/.fno".to_string()];
        let value: Value = serde_json::from_str(&turn_start_request_json_full(
            7,
            "thread-1",
            "go",
            None,
            &roots,
            Some(&resolved),
            &CodexPosture::bounded(),
        ))
        .unwrap();
        let policy = &value["params"]["sandboxPolicy"];
        assert_eq!(policy["networkAccess"], true);
        assert_eq!(
            policy["writableRoots"],
            json!(["/repo/already-granted", "/Users/x/.fno"])
        );
        assert_eq!(policy["excludeSlashTmp"], false);
        assert_eq!(policy["excludeTmpdirEnvVar"], true);
    }

    /// A resolved bounded posture carries the policy on its own, roots or
    /// none: network is part of what a bounded worker needs, and a non-repo
    /// cwd with no published state dirs grants no roots at all.
    #[test]
    fn turn_start_sends_the_policy_for_a_resolved_posture_with_no_roots() {
        let resolved = json!({
            "type": "workspaceWrite",
            "writableRoots": [],
            "networkAccess": false,
        });
        let value: Value = serde_json::from_str(&turn_start_request_json_full(
            7,
            "thread-1",
            "go",
            None,
            &[],
            Some(&resolved),
            &CodexPosture::bounded(),
        ))
        .unwrap();
        let policy = &value["params"]["sandboxPolicy"];
        assert_eq!(policy["type"], "workspaceWrite");
        assert_eq!(policy["networkAccess"], true);
        assert_eq!(policy["writableRoots"], json!([]));
    }

    #[test]
    fn linked_worktree_grant_uses_the_repository_common_dir() {
        let canonical = tempfile::tempdir().unwrap();
        let run = |args: &[&str], cwd: &Path| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .status()
                .unwrap();
            assert!(status.success(), "git command failed: {args:?}");
        };
        run(&["init", "--quiet"], canonical.path());
        run(
            &["config", "user.email", "test@example.com"],
            canonical.path(),
        );
        run(&["config", "user.name", "Test"], canonical.path());
        std::fs::write(canonical.path().join("README"), "base\n").unwrap();
        run(&["add", "README"], canonical.path());
        run(&["commit", "--quiet", "-m", "base"], canonical.path());
        let linked = canonical.path().join("linked");
        run(
            &[
                "worktree", "add", "--quiet", "-b", "linked", "linked", "HEAD",
            ],
            canonical.path(),
        );
        assert!(linked.join(".git").is_file());

        let common = crate::provider::git_common_dir(&linked).unwrap();
        let linked_git = linked.join(".git").to_string_lossy().into_owned();
        let roots = granted_roots(&linked, &[]);
        let value: Value = serde_json::from_str(&turn_start_request_json_full(
            7,
            "thread-linked",
            "go",
            None,
            &roots,
            None,
            &CodexPosture::bounded(),
        ))
        .unwrap();
        let wired = value["params"]["sandboxPolicy"]["writableRoots"]
            .as_array()
            .unwrap();
        assert!(wired
            .iter()
            .any(|root| root.as_str() == Some(common.as_str())));
        assert!(!wired
            .iter()
            .any(|root| root.as_str() == Some(linked_git.as_str())));
    }

    /// An ungranted spawn must build TODAY's frame, byte for byte. A new
    /// always-on field would change the posture of every existing lane.
    #[test]
    fn turn_start_without_roots_is_byte_identical_to_today() {
        let with_helper = turn_start_request_json_with_effort(7, "thread-1", "go", Some("high"));
        let with_empty = turn_start_request_json_full(
            7,
            "thread-1",
            "go",
            Some("high"),
            &[],
            None,
            &CodexPosture::bounded(),
        );
        assert_eq!(with_helper, with_empty);
        let value: Value = serde_json::from_str(&with_empty).unwrap();
        assert!(value["params"].get("sandboxPolicy").is_none());
    }

    /// The turn sends a WHOLE policy object, so a hand-built one silently
    /// replaces every sibling field of the thread's posture with a server
    /// default. Echoing the resolved posture back keeps the turn equal to the
    /// thread in every respect except the roots it widens.
    #[test]
    fn turn_start_preserves_the_threads_own_posture_fields() {
        // The shape the live daemon reports for a bounded thread.
        let resolved = json!({
            "type": "workspaceWrite",
            "writableRoots": ["/repo/already-granted"],
            "networkAccess": true,
            "excludeSlashTmp": true,
            "excludeTmpdirEnvVar": false,
        });
        let roots = vec!["/Users/x/.fno".to_string()];
        let value: Value = serde_json::from_str(&turn_start_request_json_full(
            7,
            "thread-1",
            "go",
            None,
            &roots,
            Some(&resolved),
            &CodexPosture::bounded(),
        ))
        .unwrap();
        let policy = &value["params"]["sandboxPolicy"];
        // Widened, and nothing else touched.
        assert_eq!(
            policy["writableRoots"],
            json!(["/repo/already-granted", "/Users/x/.fno"])
        );
        assert_eq!(policy["networkAccess"], true);
        assert_eq!(policy["excludeSlashTmp"], true);
        assert_eq!(policy["excludeTmpdirEnvVar"], false);
    }

    /// A root the posture already names is not repeated.
    #[test]
    fn turn_start_does_not_duplicate_a_root_the_posture_already_has() {
        let resolved = json!({
            "type": "workspaceWrite",
            "writableRoots": ["/Users/x/.fno"],
        });
        let roots = vec!["/Users/x/.fno".to_string()];
        let value: Value = serde_json::from_str(&turn_start_request_json_full(
            7,
            "thread-1",
            "go",
            None,
            &roots,
            Some(&resolved),
            &CodexPosture::bounded(),
        ))
        .unwrap();
        assert_eq!(
            value["params"]["sandboxPolicy"]["writableRoots"],
            json!(["/Users/x/.fno"])
        );
    }

    /// The resolved posture is read UNFILTERED from the thread/start
    /// response (AC2-HP): a full-access thread is echoed what the server
    /// resolved, never handed a fabricated workspaceWrite object; read-only
    /// reads as itself (AC2-EDGE). Only a response with NO sandbox is None.
    #[test]
    fn resolved_sandbox_is_read_unfiltered() {
        let bounded =
            r#"{"id":1,"result":{"sandbox":{"type":"workspaceWrite","writableRoots":[]}}}"#;
        assert_eq!(
            parse_resolved_sandbox(bounded).unwrap()["type"],
            "workspaceWrite"
        );
        let full = r#"{"id":1,"result":{"sandbox":{"type":"dangerFullAccess"}}}"#;
        assert_eq!(
            parse_resolved_sandbox(full).unwrap()["type"],
            "dangerFullAccess"
        );
        assert_eq!(
            parse_resolved_sandbox(r#"{"id":1,"result":{"sandbox":{"type":"readOnly"}}}"#).unwrap()
                ["type"],
            "readOnly"
        );
        assert!(parse_resolved_sandbox(r#"{"id":1,"result":{}}"#).is_none());
    }

    /// The RECORD's read is unfiltered, and that is the whole difference from
    /// `parse_resolved_sandbox` above. That one answers `None` for a
    /// full-access thread AND for a response naming no sandbox, which is fine
    /// where it is used (nothing to echo either way) and useless in a row: the
    /// reader cannot tell which world produced the blank. Asserting the
    /// full-access spelling POSITIVELY is the point - a test that only checked
    /// the workspaceWrite arm would pass against the filtered reader too.
    #[test]
    fn resolved_sandbox_type_is_read_unfiltered_for_the_record() {
        let bounded =
            r#"{"id":1,"result":{"sandbox":{"type":"workspaceWrite","writableRoots":[]}}}"#;
        assert_eq!(
            parse_resolved_sandbox_type(bounded).as_deref(),
            Some("workspaceWrite")
        );
        let full = r#"{"id":1,"result":{"sandbox":{"type":"dangerFullAccess"}}}"#;
        assert_eq!(
            parse_resolved_sandbox_type(full).as_deref(),
            Some("dangerFullAccess")
        );
        // Only a response that named NO sandbox is genuinely unknown.
        assert_eq!(parse_resolved_sandbox_type(r#"{"id":1,"result":{}}"#), None);
        assert_eq!(
            parse_resolved_sandbox_type(r#"{"id":1,"result":{"sandbox":{"type":""}}}"#),
            None
        );
    }

    /// `thread/start` keeps the SCALAR field and its exact spelling. The
    /// app-server rejects the docs' `workspaceWrite` spelling outright
    /// (`-32600 unknown variant`), so the code is right and the doc is wrong;
    /// this pins the code against a well-meaning "fix" toward the doc.
    #[test]
    fn thread_start_keeps_the_scalar_sandbox_spelling_the_server_accepts() {
        let value: Value = serde_json::from_str(&thread_start_request_with_options(
            1,
            std::path::Path::new("/tmp/w"),
            None,
            &CodexPosture::bounded(),
            None,
            None,
        ))
        .unwrap();
        assert_eq!(value["params"]["sandbox"], "workspace-write");
        assert!(value["params"].get("sandboxPolicy").is_none());
    }

    /// The assigned form carries `params.projectId`: the thread lane's
    /// whole side of the assignment contract is one conditional key on the map
    /// it already builds for `model`.
    #[test]
    fn thread_start_carries_the_resolved_project_id() {
        let value: Value = serde_json::from_str(&thread_start_request_with_options(
            1,
            std::path::Path::new("/tmp/w"),
            None,
            &CodexPosture::bounded(),
            Some("proj-1"),
            None,
        ))
        .unwrap();
        assert_eq!(value["params"]["projectId"], "proj-1");
    }

    /// No resolvable project -> NO key at all, never an empty string: the
    /// protocol reads an explicit empty projectId as a CLEAR, so an absent
    /// resolution must omit the field the way today's request does.
    #[test]
    fn thread_start_without_a_project_omits_the_key_entirely() {
        let value: Value = serde_json::from_str(&thread_start_request_with_options(
            1,
            std::path::Path::new("/tmp/w"),
            None,
            &CodexPosture::bounded(),
            None,
            None,
        ))
        .unwrap();
        assert!(value["params"].get("projectId").is_none());
    }

    #[test]
    fn completion_notification_extracts_positive_turn_marker_and_text() {
        let result = parse_turn_completed_notification(
            r#"{"method":"turn/completed","params":{"turn":{"id":"turn-1","status":"completed","items":[{"type":"agentMessage","text":"recalled TOKEN"}]}}}"#,
        )
        .unwrap();
        assert_eq!(result.turn_id, "turn-1");
        assert_eq!(result.text, "recalled TOKEN");
    }

    /// AC18: completed turns are parsed ONCE at push and claimed from a map.
    /// The old `take_completed` re-serialized every parked entry
    /// (`value.to_string()`) on each completed-turn check; this pins the
    /// replacement structure - two completions parked while a third is awaited
    /// come back from the map by turn id, and no parked entry is a
    /// `turn/completed` Value at all.
    #[test]
    fn parked_frames_parse_completions_once_and_claim_by_turn_id() {
        let mut pending = VecDeque::new();
        let mut completed = HashMap::new();
        for turn in ["turn-1", "turn-2", "turn-3"] {
            park_frame(
                &mut pending,
                &mut completed,
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": turn, "status": "completed",
                                        "items": [{"type": "agentMessage", "text": "done"}]}}
                }),
            );
        }
        park_frame(
            &mut pending,
            &mut completed,
            serde_json::json!({"id": 9, "result": {"turn": {"id": "turn-9"}}}),
        );
        assert_eq!(completed.len(), 3, "each completion parsed once at push");
        assert_eq!(
            pending.len(),
            1,
            "id-matched responses stay Values in the deque"
        );
        assert!(
            !pending
                .iter()
                .any(|value| value.get("method").is_some_and(|m| m == "turn/completed")),
            "no completion is parked as a raw Value to re-serialize later"
        );
        let claimed = completed.remove("turn-2").expect("claimed from the map");
        assert_eq!(claimed.text, "done");
        assert_eq!(claimed.status, "completed");
        assert!(
            completed.contains_key("turn-3"),
            "unclaimed turns stay parked"
        );
    }
}

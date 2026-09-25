//! The live retask seams: `fno mux` pane reads/sends under bounded
//! timeouts, the succession poll over the shared registry, the
//! rename-with-node registry transaction, the verified-tier projection, the
//! graph-join source preflight, the claude transcript's live permission
//! mode, and the stdin-payload door on the `rename` client action.
//!
//! Every subprocess rides the crate's bounded, group-killed budget the way
//! the Python transport's `subprocess.run(timeout=...)` did; a budget death
//! maps to the same named refusal the Python transport raised.

use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use super::{
    execute_retask, refused_receipt, RetaskRow, RetaskSeams, RetaskTarget, TransportFailure,
};
use crate::bounded_cmd::output_with_timeout_result;
use crate::state::{classify_session_transition, rename_agent, update_registry, RegistryEntry};
use crate::{claude_drive, graph_store};

/// How long each pane op may run, mirroring the Python transport's budgets.
const PANE_READ_SECS: u64 = 10;
const PANE_WAIT_SECS: u64 = 15;
const PANE_SEND_SECS: u64 = 15;
const PR_STATUS_SECS: u64 = 60;
/// The succession poll: 40 reads at 250 ms, the Python cadence.
const RESTAMP_POLLS: usize = 40;
const RESTAMP_SLEEP_MS: u64 = 250;
const TRANSCRIPT_TAIL_BYTES: u64 = 1024 * 1024;

fn fno_bin() -> String {
    std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".to_string())
}

fn timeout_reason(argv: &[String]) -> String {
    match argv.get(2).map(String::as_str) {
        Some("wait") => "pane_wait_timeout",
        Some("send") => "pane_send_timeout",
        _ => "pane_read_timeout",
    }
    .to_string()
}

/// The stdin-payload door: `fno-agents rename` with no argv reads one JSON
/// payload and runs the transaction. Exit 0 with the receipt on stdout for
/// both `retasked` and `refused`; exit 2 with stderr naming the problem for
/// any usage or payload error.
pub fn run_payload() -> i32 {
    let fail = |message: String| {
        eprintln!("retask: {message}");
        2
    };
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return fail("could not read the payload on stdin".to_string());
    }
    let payload: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => return fail(format!("bad payload: {error}")),
    };
    match payload.get("op").and_then(Value::as_str) {
        Some("retask") => {}
        other => {
            return fail(format!(
                "unsupported op {}; only \"retask\" rides the rename payload",
                other.unwrap_or("(missing)")
            ))
        }
    }
    let worker = match payload.get("worker").and_then(Value::as_str) {
        Some(worker) if !worker.is_empty() => worker.to_string(),
        _ => return fail("payload needs worker".to_string()),
    };
    let node = match payload.get("node").and_then(Value::as_str) {
        Some(node) if !node.is_empty() => node.to_string(),
        _ => return fail("payload needs node".to_string()),
    };
    let target = match payload.get("target") {
        Some(value) => match RetaskTarget::from_payload(value) {
            Ok(target) => target,
            Err(message) => return fail(message),
        },
        None => return fail("payload needs target".to_string()),
    };
    let target_command = match payload.get("target_command").and_then(Value::as_str) {
        Some(command) if !command.is_empty() => command.to_string(),
        _ => return fail("payload needs target_command".to_string()),
    };
    let mux_ref = match payload.get("mux") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let session = value.get("session").and_then(Value::as_str).unwrap_or("");
            let pane_id = value.get("pane_id").and_then(Value::as_u64).unwrap_or(0);
            if session.is_empty() || pane_id == 0 {
                return fail("payload mux needs a session and a nonzero pane_id".to_string());
            }
            Some((session.to_string(), pane_id))
        }
    };

    let registry_path = crate::paths::AgentsHome::from_env().registry_json();
    let rows = match crate::client_verbs::load_registry_entries(&registry_path) {
        Ok(rows) => rows,
        Err(message) => return fail(format!("registry read failed: {message}")),
    };
    let row_value = match crate::client_verbs::find_agent_entry(&rows, &worker) {
        Ok(row) => row.clone(),
        Err(error) => return fail(format!("cannot resolve worker {worker:?}: {error:?}")),
    };
    let entry: RegistryEntry = match serde_json::from_value(row_value) {
        Ok(entry) => entry,
        Err(error) => return fail(format!("registry row {worker:?} does not decode: {error}")),
    };
    let row = retask_row_from_entry(&entry);

    let mut seams = LiveSeams::live(
        row.clone(),
        mux_ref,
        node,
        registry_path,
        crate::graph_get::default_graph_path(),
    );
    let live_mode = live_permission_mode(&entry);
    let node = seams.node.clone();
    let receipt = match execute_retask(
        &row,
        &target,
        &node,
        &target_command,
        &mut seams,
        live_mode.as_deref(),
    ) {
        Ok(receipt) => receipt,
        Err(failure) => refused_receipt_from_transport(&row, &seams, failure),
    };
    println!(
        "{}",
        serde_json::to_string(&receipt).unwrap_or_else(|_| "{}".into())
    );
    0
}

/// The refusal for a pane transport death: the partial transaction state is
/// preserved, exactly as the Python transport's except arm did.
fn refused_receipt_from_transport(
    row: &RetaskRow,
    seams: &LiveSeams,
    failure: TransportFailure,
) -> Value {
    let restamped = seams.restamped_session() != row.harness_session_id;
    let mut receipt = refused_receipt(&failure.reason);
    let obj = receipt.as_object_mut().expect("refusal is an object");
    obj.insert("cleared".into(), json!(seams.clear_sent || restamped));
    obj.insert("session_restamped".into(), json!(restamped));
    if let Some(detail) = failure.detail {
        obj.insert("detail".into(), json!(detail));
    }
    if let Some(renamed) = seams.renamed_name() {
        if renamed != row.name {
            obj.insert("registry_name".into(), json!(renamed));
        }
    }
    receipt
}

/// The row facts the transaction reads, off the registry row resolved at run
/// time.
fn retask_row_from_entry(entry: &RegistryEntry) -> RetaskRow {
    RetaskRow {
        name: entry.name.clone(),
        harness: entry.harness.clone().unwrap_or_default(),
        provider: entry.provider.clone(),
        model: entry.model.clone(),
        effort: entry.effort.clone(),
        substrate: entry.substrate.clone(),
        status_live: entry.status == crate::AgentStatus::Live,
        harness_session_id: entry.harness_session_id.clone(),
        launch_account: entry.launch_account.clone(),
        mux: entry
            .mux
            .as_ref()
            .map(|mux| (mux.session.clone(), mux.pane_id)),
        thread_id: entry.fno_id.clone(),
    }
}

/// The live seam set over one pane. The `run` indirection is the single
/// subprocess seam the tests inject, the way the Python tests monkeypatched
/// `subprocess.run`.
pub(crate) struct LiveSeams {
    session: String,
    pane: String,
    node: String,
    registry_path: PathBuf,
    graph_path: PathBuf,
    worker: RetaskRow,
    // Partial transaction state, so a transport death can report the pane's
    // true state instead of claiming it is untouched.
    clear_sent: bool,
    restamped: Option<String>,
    renamed: Option<String>,
    /// The one subprocess seam: argv, budget seconds, and the cwd the child
    /// runs in (Some for the pr-status call, which resolves in the source
    /// node's project space).
    pub(crate) run: RunSeam,
}

/// The seam's type, named so the struct field stays readable.
type RunSeam =
    Box<dyn FnMut(&[String], u64, Option<&str>) -> Result<std::process::Output, TransportFailure>>;

impl LiveSeams {
    /// The production seam set: one bounded `fno` invocation per op.
    pub(crate) fn live(
        worker: RetaskRow,
        mux_ref: Option<(String, u64)>,
        node: String,
        registry_path: PathBuf,
        graph_path: PathBuf,
    ) -> Self {
        Self {
            session: mux_ref
                .as_ref()
                .map(|(session, _)| session.clone())
                .unwrap_or_default(),
            pane: mux_ref
                .as_ref()
                .map(|(_, pane)| pane.to_string())
                .unwrap_or_default(),
            node,
            registry_path,
            graph_path,
            clear_sent: false,
            restamped: worker.harness_session_id.clone(),
            renamed: None,
            worker,
            run: Box::new(|argv, secs, cwd| {
                let mut cmd = Command::new(fno_bin());
                cmd.args(argv);
                if let Some(cwd) = cwd {
                    cmd.current_dir(cwd);
                }
                let output =
                    output_with_timeout_result(cmd, secs).map_err(|error| TransportFailure {
                        reason: timeout_reason(argv),
                        detail: Some(error.to_string()),
                    })?;
                // The bounded runner's only signal death is its own deadline
                // SIGKILL, so a code-less failed status IS the timeout.
                if !output.status.success() && output.status.code().is_none() {
                    return Err(TransportFailure {
                        reason: timeout_reason(argv),
                        detail: None,
                    });
                }
                Ok(output)
            }),
        }
    }

    fn bounded(
        &mut self,
        argv: Vec<String>,
        secs: u64,
    ) -> Result<std::process::Output, TransportFailure> {
        (self.run)(&argv, secs, None)
    }

    pub(crate) fn restamped_session(&self) -> Option<String> {
        self.restamped.clone()
    }

    pub(crate) fn renamed_name(&self) -> Option<String> {
        self.renamed.clone()
    }
}

impl RetaskSeams for LiveSeams {
    fn read_frame(&mut self) -> Result<String, TransportFailure> {
        let output = self.bounded(
            vec![
                "mux".into(),
                "pane".into(),
                "read".into(),
                "--server".into(),
                self.session.clone(),
                self.pane.clone(),
                "--lines".into(),
                "80".into(),
            ],
            PANE_READ_SECS,
        )?;
        Ok(if output.status.success() {
            String::from_utf8_lossy(&output.stdout).to_string()
        } else {
            String::new()
        })
    }

    fn settle(&mut self) -> Result<(), TransportFailure> {
        self.bounded(
            vec![
                "mux".into(),
                "pane".into(),
                "wait".into(),
                "--server".into(),
                self.session.clone(),
                self.pane.clone(),
                "--quiet-ms".into(),
                "400".into(),
                "--timeout".into(),
                "8".into(),
            ],
            PANE_WAIT_SECS,
        )?;
        Ok(())
    }

    fn send(&mut self, text: &str, submit: bool) -> Result<bool, TransportFailure> {
        let mut argv = vec![
            "mux".into(),
            "pane".into(),
            "send".into(),
            "--server".into(),
            self.session.clone(),
            self.pane.clone(),
            "--text".into(),
            text.to_string(),
            "--raw".into(),
        ];
        if submit {
            argv.push("--submit".into());
        }
        let output = self.bounded(argv, PANE_SEND_SECS)?;
        let code = output.status.code();
        if code == Some(23) {
            // EXIT_TARGET_IDENTITY_MISMATCH (mux_cli.rs). The server's
            // portal-refusal text names the left session; other identity
            // refusals keep the family name with the truthful detail.
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let detail: Option<String> = stderr
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_string);
            let reason = if detail
                .as_deref()
                .is_some_and(|line| line.contains("the viewer left that session"))
            {
                "view_left_worker"
            } else {
                "identity_refused"
            };
            return Err(TransportFailure {
                reason: reason.to_string(),
                detail,
            });
        }
        if text == "/clear" && submit && code == Some(0) {
            self.clear_sent = true;
        }
        Ok(code == Some(0))
    }

    fn restamp(&mut self) -> Result<Value, TransportFailure> {
        let clear_frame = {
            self.settle()?;
            self.read_frame()?
        };
        let predecessor = self.worker.harness_session_id.clone().unwrap_or_default();
        let codex_receipt =
            regex::Regex::new(r"To continue this session, run codex resume (?P<predecessor>\S+)")
                .expect("static codex receipt regex");
        let named = codex_receipt
            .captures(&clear_frame)
            .and_then(|captures| captures.name("predecessor"))
            .map(|matched| matched.as_str().to_string());
        if named.as_deref().is_some_and(|name| name != predecessor) {
            return Ok(json!({
                "classification": "deferred",
                "reason": "clear_predecessor_mismatch",
                "predecessor_session_id": named,
            }));
        }
        for _ in 0..RESTAMP_POLLS {
            let registry = crate::state::load_registry(&self.registry_path).map_err(|error| {
                TransportFailure {
                    reason: "pane_read_timeout".to_string(),
                    detail: Some(error.to_string()),
                }
            })?;
            let entries = &registry.entries;
            // A branch row forked from the predecessor ends the poll first:
            // a fork is never the succession a retask needs.
            let branch = entries.iter().find(|candidate| {
                candidate.harness.as_deref() == Some(self.worker.harness.as_str())
                    && candidate.forked_from_session_id.as_deref() == Some(predecessor.as_str())
                    && candidate
                        .harness_session_id
                        .as_deref()
                        .is_some_and(|id| !id.is_empty())
                    && classify_session_transition(
                        &predecessor,
                        candidate.harness_session_id.as_deref().unwrap_or(""),
                        Some(true),
                    ) == crate::state::SessionTransition::Branch
            });
            if let Some(branch) = branch {
                return Ok(json!({
                    "classification": "branch",
                    "reason": "session_transition_not_succession",
                    "predecessor_session_id": predecessor,
                    "current_session_id": branch.harness_session_id,
                }));
            }
            let mut successor: Option<String> = None;
            for candidate in entries.iter() {
                if candidate.name != self.worker.name {
                    continue;
                }
                let Some(session) = candidate
                    .harness_session_id
                    .clone()
                    .filter(|id| !id.is_empty())
                else {
                    continue;
                };
                if session == predecessor {
                    continue;
                }
                if named.is_none() {
                    return Ok(json!({
                        "classification": "deferred",
                        "reason": "clear_predecessor_unconfirmed",
                        "predecessor_session_id": predecessor,
                        "current_session_id": session,
                    }));
                }
                self.restamped = Some(session.clone());
                if !candidate
                    .predecessor_session_ids
                    .iter()
                    .any(|id| id == &predecessor)
                {
                    return Ok(json!({
                        "classification": "deferred",
                        "reason": "successor_lineage_unrecorded",
                        "predecessor_session_id": predecessor,
                        "current_session_id": session,
                    }));
                }
                if classify_session_transition(&predecessor, &session, Some(false))
                    != crate::state::SessionTransition::Succession
                {
                    return Ok(json!({
                        "classification": "deferred",
                        "reason": "session_transition_not_succession",
                        "predecessor_session_id": predecessor,
                        "current_session_id": session,
                    }));
                }
                successor = Some(session);
                break;
            }
            if let Some(session) = successor {
                let rows = entries
                    .iter()
                    .filter(|row| {
                        row.harness.as_deref() == Some(self.worker.harness.as_str())
                            && row.harness_session_id.as_deref() == Some(session.as_str())
                    })
                    .count();
                return Ok(json!({
                    "classification": "succession",
                    "predecessor_session_id": predecessor,
                    "current_session_id": session,
                    "registry_rows": rows,
                    "lineage_recorded": true,
                }));
            }
            std::thread::sleep(std::time::Duration::from_millis(RESTAMP_SLEEP_MS));
        }
        Ok(Value::Null)
    }

    fn rename(&mut self, new_name: &str) -> Option<String> {
        match rename_agent(
            &self.registry_path,
            &self.worker.name,
            new_name,
            Some(&self.node),
        ) {
            Ok((_old, new)) => {
                self.renamed = Some(new.clone());
                Some(new)
            }
            Err(_) => None,
        }
    }

    fn project_tier(&mut self, model: &str, effort: &str) -> Result<(), String> {
        let renamed = self
            .renamed
            .clone()
            .unwrap_or_else(|| self.worker.name.clone());
        let session = self.restamped.clone().unwrap_or_default();
        let registry_path = self.registry_path.clone();
        match update_registry(&registry_path, |registry| {
            let target = registry.entries.iter_mut().find(|row| {
                row.name == renamed && row.harness_session_id.as_deref() == Some(session.as_str())
            });
            let Some(target) = target else {
                return Err(format!(
                    "registry row {renamed:?} was not restamped to session {session:?}"
                ));
            };
            target.model = Some(model.to_string());
            target.model_basis = Some("verified".to_string());
            target.effort = Some(effort.to_string());
            Ok(())
        }) {
            Ok(inner) => inner,
            Err(error) => Err(error.to_string()),
        }
    }

    fn ready_frame(&mut self, frame: &str) -> Option<Value> {
        let osc_title = self.pane_osc_title();
        crate::manifest::evaluate_screen_json_with_osc(
            &self.worker.harness,
            frame,
            osc_title.as_deref(),
            None,
        )
        .ok()
    }

    fn source_preflight(&mut self) -> Option<Value> {
        Some(self.source_preflight_inner())
    }
}

impl LiveSeams {
    /// The OSC title off the pane listing. A portal view of a live claude
    /// reads None, and the manifest's grid rules carry the verdict alone, so
    /// an unreadable title is passed when readable but never required.
    fn pane_osc_title(&mut self) -> Option<String> {
        let output = (self.run)(
            &[
                "mux".to_string(),
                "pane".to_string(),
                "ls".to_string(),
                "--server".to_string(),
                self.session.clone(),
                "--json".to_string(),
            ],
            PANE_READ_SECS,
            None,
        )
        .ok()?;
        if !output.status.success() {
            return None;
        }
        let rows: Value = serde_json::from_slice(&output.stdout).ok()?;
        let pane: u64 = self.pane.parse().ok()?;
        rows.as_array()?
            .iter()
            .find(|row| row.get("pane_id").and_then(Value::as_u64) == Some(pane))
            .and_then(|row| row.get("title"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// The source/PR authorization: join the worker's harness session onto
    /// exactly one graph node, then prove the node's PR is green or closed
    /// before any pane mutation.
    fn source_preflight_inner(&mut self) -> Value {
        fn refused(reason: &str, extra: Value) -> Value {
            let mut base = refused_receipt(reason);
            if let Some(obj) = base.as_object_mut() {
                if let Some(extra) = extra.as_object() {
                    for (key, value) in extra {
                        obj.insert(key.clone(), value.clone());
                    }
                }
                // Preflight receipts carry "status", the contract execute reads.
                obj.insert("status".into(), json!("refused"));
            }
            base
        }
        let Some(session) = self
            .worker
            .harness_session_id
            .clone()
            .filter(|id| !id.is_empty())
        else {
            return refused("source_node_unresolved", json!({}));
        };
        let rows = match graph_store::read_rows(&self.graph_path) {
            Ok(rows) => rows,
            // Unreadable graph evidence cannot authorize clear.
            Err(error) => {
                return refused(
                    "source_node_unresolved",
                    json!({ "error": error.to_string() }),
                )
            }
        };
        let mut matches: Vec<&Value> = Vec::new();
        for node in rows.iter() {
            let hit = node
                .get("sessions")
                .and_then(Value::as_array)
                .map(|sessions| {
                    sessions.iter().any(|session_row| {
                        session_row.get("harness").and_then(Value::as_str)
                            == Some(self.worker.harness.as_str())
                            && session_row.get("session_id").and_then(Value::as_str)
                                == Some(session.as_str())
                    })
                })
                .unwrap_or(false);
            if hit {
                matches.push(node);
            }
        }
        if matches.len() > 1 {
            return refused("source_node_ambiguous", json!({}));
        }
        let Some(source) = matches.first() else {
            return refused("source_node_unresolved", json!({}));
        };
        let source_node_id = source.get("id").cloned().unwrap_or(Value::Null);
        let pr_number = source.get("pr_number").and_then(Value::as_i64);
        let closed = source.get("status").and_then(Value::as_str) == Some("superseded")
            || (source.get("status").and_then(Value::as_str) == Some("done")
                && source.get("merge_status").and_then(Value::as_str) == Some("merged"));
        if pr_number.is_none() || closed {
            return json!({ "status": "ready", "source_node_id": source_node_id });
        }
        let pr_number = pr_number.expect("checked above");
        let argv = [
            "do".to_string(),
            "pr".to_string(),
            "status".to_string(),
            pr_number.to_string(),
        ];
        let payload = match (self.run)(
            &argv,
            PR_STATUS_SECS,
            source.get("cwd").and_then(Value::as_str),
        ) {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                stdout
                    .trim()
                    .lines()
                    .next_back()
                    .and_then(|line| serde_json::from_str::<Value>(line).ok())
                    .unwrap_or_else(|| json!({ "error": "pr status output was not JSON" }))
            }
            Err(_) => json!({ "error": "pr status timed out" }),
        };
        let state = payload
            .get("pr_state")
            .or_else(|| payload.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_uppercase();
        if state == "MERGED"
            || state == "CLOSED"
            || (state == "OPEN" && payload.get("green") == Some(&json!(true)))
        {
            return json!({
                "status": "ready",
                "source_node_id": source_node_id,
                "source_pr": pr_number,
                "pr_state": state,
            });
        }
        if state == "OPEN" {
            return refused(
                "source_pr_not_green",
                json!({
                    "source_node_id": source_node_id,
                    "pr": pr_number,
                    "head": payload
                        .get("head_sha")
                        .or_else(|| payload.get("head"))
                        .cloned()
                        .unwrap_or(Value::Null),
                    "verdict": payload.get("verdict").cloned().unwrap_or(Value::Null),
                    "blockers": payload.get("checks").cloned().unwrap_or(Value::Null),
                }),
            );
        }
        refused(
            "source_pr_status_unknown",
            json!({
                "source_node_id": source_node_id,
                "pr": pr_number,
                "verdict": payload.get("verdict").cloned().unwrap_or(Value::Null),
                "error": payload.get("error").cloned().unwrap_or(Value::Null),
            }),
        )
    }
}

/// The live permission mode from the worker's own transcript: the last
/// `permission-mode` record wins. `None` (other harness, missing transcript,
/// no record) fails closed at the caller.
pub(crate) fn live_permission_mode(entry: &RegistryEntry) -> Option<String> {
    if entry.harness.as_deref() != Some("claude") {
        return None;
    }
    let transcript = claude_drive::find_transcript(entry.harness_session_id.as_deref()?)?;
    let mut handle = std::fs::File::open(&transcript).ok()?;
    let size = handle.metadata().ok()?.len();
    let start = size.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    use std::io::Seek;
    handle.seek(std::io::SeekFrom::Start(start)).ok()?;
    let mut tail = String::new();
    handle.read_to_string(&mut tail).ok()?;
    if !tail.is_empty() && !tail.ends_with('\n') {
        // A concurrently appended torn record does not decide; complete
        // records do.
        let cut = tail.rfind('\n').map(|at| at + 1).unwrap_or(0);
        tail.truncate(cut);
    }
    let mut mode: Option<String> = None;
    for line in tail.lines() {
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if record.get("type").and_then(Value::as_str) == Some("permission-mode") {
            if let Some(value) = record.get("permissionMode").and_then(Value::as_str) {
                if !value.is_empty() {
                    mode = Some(value.to_string());
                }
            }
        }
    }
    mode
}

#[cfg(test)]
mod transport_tests;

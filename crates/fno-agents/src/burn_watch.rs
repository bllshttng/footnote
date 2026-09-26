//! The burn arm: spend against progress, per live worker.
//!
//! A worker can burn hours and dollars while its node's `touched_at` and its
//! branch sit flat; nothing else in the fleet reads cost against progress.
//! This arm samples every live session carrying an open `do` phase: ledger
//! spend, branch head and commit count (`git` in the row's own cwd), and the
//! node's `touched_at` (graph). Progress on any axis resets the ladder; a
//! flat sample where spend grew or the node aged past the idle ceiling wakes
//! the worker, and three unanswered wakes file ONE fleet task through the
//! same store the pr-nudge ladder escalates through.
//!
//! Scope is open-do sessions on purpose: a blueprint or think worker's
//! deliverable is a plan document, so commits are not its progress signal.
//! No transcript is read - a doom loop writes its transcript constantly,
//! which is exactly the shape this arm exists to catch, so quietness gates
//! nothing here. An unreadable input (no ledger, no git, unparsable
//! `touched_at`) is never evidence: the arm it would feed stays silent.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths::AgentsHome;

pub const BURN_WATCH_INTERVAL_S: u64 = 900;
pub const DEFAULT_IDLE_S: i64 = 7200;
pub const DEFAULT_SPEND_USD: f64 = 0.01;
const SENDER: &str = "burn-watch";
const SENDER_LINE: &str = "Automatic notice from the fno daemon burn-watch arm, not a person. Your operator's hold outranks it.";

const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const RESUME_RUN_TIMEOUT: Duration = Duration::from_secs(180);

pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

impl Default for Arm {
    fn default() -> Self {
        Self {
            last_tick: Mutex::new(None),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// One sampled worker: what progress and spend read this pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sample {
    pub cost_usd: Option<f64>,
    pub head: Option<String>,
    pub commits: Option<u64>,
    /// Node `touched_at` as epoch seconds, when the node row carried a
    /// parsable stamp.
    pub touched_at: Option<i64>,
}

/// Per-session ladder state, one JSON file under `burn-watch/`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BurnState {
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub commits: Option<u64>,
    #[serde(default)]
    pub touched_at: Option<i64>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub escalated: bool,
    #[serde(default)]
    pub last_wake_at: Option<i64>,
    /// The filed fleet task's identity, carried so a later pass (or the
    /// scope-exit sweep) can close the task it actually filed.
    #[serde(default)]
    pub task_key: Option<String>,
    #[serde(default)]
    pub task_cwd: Option<String>,
}

/// What this pass does with one in-scope session.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// No prior sample: record one and wait an interval. A worker with no
    /// history can never fire on its first sighting.
    FirstSight,
    /// Progress landed on some axis, or the burn evidence withdrew: reset
    /// the ladder, close any filed task.
    StandDown,
    /// Flat sample, budget not spent: wake the worker.
    Wake(String),
    /// Flat sample, budget spent, operator not yet asked: file one task.
    Escalate(String),
    /// Flat sample, already escalated: wait for progress or the operator.
    Hold,
}

/// The pure decision. Progress is ANY change on the sampled axes; a flat
/// sample burns when spend grew past `spend_min` or the node's
/// `touched_at` aged past `idle_s`. Unknown inputs never fire.
pub fn decide(
    prev: Option<&BurnState>,
    sample: &Sample,
    now_epoch: i64,
    idle_s: i64,
    spend_min: f64,
) -> (Decision, BurnState) {
    let mut next = BurnState {
        cost_usd: sample.cost_usd,
        head: sample.head.clone(),
        commits: sample.commits,
        touched_at: sample.touched_at,
        ..BurnState::default()
    };
    let Some(prev) = prev else {
        return (Decision::FirstSight, next);
    };
    next.task_key = prev.task_key.clone();
    next.task_cwd = prev.task_cwd.clone();
    let flat = prev.head == sample.head
        && prev.commits == sample.commits
        && prev.touched_at == sample.touched_at;
    if !flat {
        return (Decision::StandDown, next);
    }
    let spend_grew = match (prev.cost_usd, sample.cost_usd) {
        (Some(before), Some(now)) => now - before >= spend_min,
        _ => false,
    };
    let node_aged = sample
        .touched_at
        .is_some_and(|t| now_epoch.saturating_sub(t) >= idle_s);
    let reason = if spend_grew {
        Some(format!(
            "spend grew to ${:.2} with no new commit and no node touch",
            sample.cost_usd.unwrap_or(0.0)
        ))
    } else if node_aged {
        Some(format!(
            "node untouched {}h with no new commit",
            now_epoch.saturating_sub(sample.touched_at.unwrap_or(0)) / 3600
        ))
    } else {
        None
    };
    next.attempts = prev.attempts;
    next.escalated = prev.escalated;
    next.last_wake_at = prev.last_wake_at;
    match reason {
        None => (Decision::StandDown, next),
        Some(reason) => {
            if prev.attempts < crate::pr_nudge::MAX_ATTEMPTS {
                (Decision::Wake(reason), next)
            } else if !prev.escalated {
                next.escalated = true;
                (Decision::Escalate(reason), next)
            } else {
                (Decision::Hold, next)
            }
        }
    }
}

/// Sessions with an open `do` phase, joined to their node: the arm's scope.
fn scope_sessions(rows: &[Value]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for row in rows {
        if !crate::graph_store::is_open_phase_row(row, "do") {
            continue;
        }
        if let (Some(sid), Some(node)) = (
            row.get("session_id").and_then(Value::as_str),
            row.get("node").and_then(Value::as_str),
        ) {
            out.insert(sid.to_string(), node.to_string());
        }
    }
    out
}

/// Open nodes by id: `(status, touched_at epoch)`; unparsable stamps read
/// None and can never feed the age arm.
fn node_facts(rows: &[Value]) -> HashMap<String, (String, Option<i64>)> {
    rows.iter()
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_str)?;
            let status = row.get("status").and_then(Value::as_str)?.to_string();
            let touched = row
                .get("touched_at")
                .and_then(Value::as_str)
                .and_then(|raw| {
                    chrono::DateTime::parse_from_rfc3339(raw)
                        .ok()
                        .map(|t| t.timestamp())
                });
            Some(id.to_string()).zip(Some((status, touched)))
        })
        .collect()
}

/// The production runner every arm shares with the pr-nudge ladder.
pub type Runner<'a> = &'a mut dyn FnMut(&[String], &str) -> (i32, String, String);

fn production_run(argv: &[String], cwd: &str) -> (i32, String, String) {
    let bin = argv[0].clone();
    let refs: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
    let dir = if cwd.is_empty() {
        std::path::PathBuf::from(".")
    } else {
        std::path::PathBuf::from(cwd)
    };
    let timeout = if argv.len() > 2 && argv[2] == "resume" {
        RESUME_RUN_TIMEOUT
    } else {
        RUN_TIMEOUT
    };
    match crate::loopcheck::bounded_read(bin.as_ref(), &refs, &dir, SENDER, timeout) {
        Ok(out) => (
            if out.status.success() {
                0
            } else {
                out.status.code().unwrap_or(1)
            },
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr_tail).into_owned(),
        ),
        Err(e) => (
            1,
            String::new(),
            crate::loopcheck::bounded_read_diagnostic(SENDER, &e),
        ),
    }
}

/// Branch progress at the row's own checkout: `(head, commit count)`.
/// Either read failing reads None and can never count as progress.
fn git_progress(cwd: &str, runner: Runner) -> (Option<String>, Option<u64>) {
    let head = runner(
        &[
            "git".into(),
            "log".into(),
            "-1".into(),
            "--format=%H".into(),
        ],
        cwd,
    )
    .1
    .lines()
    .rev()
    .find(|l| !l.trim().is_empty())
    .map(|l| l.trim().to_string());
    let count = runner(
        &[
            "git".into(),
            "rev-list".into(),
            "--count".into(),
            "HEAD".into(),
        ],
        cwd,
    )
    .1
    .lines()
    .rev()
    .find(|l| !l.trim().is_empty())
    .and_then(|l| l.trim().parse().ok());
    (head, count)
}

fn state_path(home: &AgentsHome, sid: &str) -> Option<std::path::PathBuf> {
    if sid.is_empty() || sid.len() > 64 || !sid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    {
        return None;
    }
    Some(home.burn_watch_dir().join(format!("{sid}.json")))
}

fn load_state(home: &AgentsHome, sid: &str) -> Option<BurnState> {
    let path = state_path(home, sid)?;
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn save_state(home: &AgentsHome, sid: &str, state: &BurnState) {
    let Some(path) = state_path(home, sid) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(state) {
        let _ = std::fs::write(path, bytes);
    }
}

fn wake_text(node: &str, reason: &str) -> String {
    format!(
        "{SENDER_LINE} node {node}: {reason}. Commit what the work produced, or move the \
         node (fno backlog note) so progress is visible; a flat branch while spend \
         climbs escalates to your operator."
    )
}

fn escalation_text(node: &str, sid: &str, reason: &str, attempts: u32) -> String {
    format!(
        "burn-watch: burning worker on {node}, session {sid}: {reason} across \
         {attempts} wakes. Resume it with `fno agents resume {sid}`, stop it, or \
         requeue the node; the fleet task closes itself when progress lands."
    )
}

/// One wake: mail the report; when the lane could not take it, fall back to
/// the content-confirmed resume. A durable receipt means the message lands
/// at the worker's next turn, so a mid-turn worker never takes a resume
/// typed over its turn.
fn wake(sid: &str, node: &str, busy: bool, reason: &str, runner: Runner) -> (bool, &'static str) {
    let text = wake_text(node, reason);
    let mail_argv = vec![
        "fno".to_string(),
        "agents".to_string(),
        "mail".to_string(),
        "send".to_string(),
        "--from-name".to_string(),
        SENDER.to_string(),
        "--origin".to_string(),
        "scheduler".to_string(),
        sid.to_string(),
        text.clone(),
    ];
    let (code, stdout, _) = runner(&mail_argv, "");
    if crate::mail_inject::mail_send_accepted(code, &stdout) {
        return (true, "mail");
    }
    let durable = crate::mail_inject::mail_send_receipt(&stdout).contains("durable");
    if durable && busy {
        return (false, "durable");
    }
    let mut resume_argv = vec![
        "fno".to_string(),
        "agents".to_string(),
        "resume".to_string(),
        sid.to_string(),
        "--message".to_string(),
        text,
    ];
    if durable {
        resume_argv.insert(4, "--message-already-queued".to_string());
    }
    let (code, _, _) = runner(&resume_argv, "");
    (code == 0, "resume")
}

fn sample_session(
    sid: &str,
    node: &str,
    cwd: &str,
    facts: &HashMap<String, (String, Option<i64>)>,
    runner: Runner,
) -> Sample {
    let (head, commits) = git_progress(cwd, runner);
    Sample {
        cost_usd: Some(crate::loopcheck::session_cost_from_ledger(
            &crate::paths::ledger_path(Path::new(cwd)),
            sid,
        )),
        head,
        commits,
        touched_at: facts.get(node).and_then(|(_, t)| *t),
    }
}

/// The pass body, behind `maybe_tick`'s cadence gate. `runner` and
/// `busy_for` are injected so tests stage the world.
fn run_pass(
    home: &AgentsHome,
    emitter: &crate::events::EventEmitter,
    runner: Runner,
    busy_for: &dyn Fn(&str) -> bool,
    now_epoch: i64,
    idle_s: i64,
    spend_min: f64,
) {
    let graph = crate::gc_sweep::graph_path(home);
    let store = crate::backlog::api::Store::new(&graph);
    let Ok(rows) = crate::backlog::api::rows(&store) else {
        return;
    };
    let scope = scope_sessions(&rows);
    let facts = node_facts(&rows);
    let mut warnings = Vec::new();
    let mut live: BTreeMap<String, crate::state::RegistryEntry> = BTreeMap::new();
    for entry in crate::spawn_gate::live_rows(&home.registry_json(), &mut warnings) {
        if let Some(sid) = entry
            .harness_session_id
            .clone()
            .or(entry.session_id.clone())
        {
            live.entry(sid).or_insert(entry);
        }
    }
    let mut acted = 0u64;
    let mut skip: Option<String> = None;
    let mut sampled: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (sid, node) in &scope {
        let Some(entry) = live.get(sid) else { continue };
        let Some(cwd) = (!entry.cwd.is_empty()).then_some(entry.cwd.as_str()) else {
            continue;
        };
        // A node that left the open set is the reaper's question, not ours.
        if !facts
            .get(node)
            .map(|(status, _)| is_open_status(status))
            .unwrap_or(false)
        {
            continue;
        }
        sampled.insert(sid.clone());
        let prev = load_state(home, sid);
        let sample = sample_session(sid, node, cwd, &facts, runner);
        let (decision, mut next) = decide(prev.as_ref(), &sample, now_epoch, idle_s, spend_min);
        match decision {
            Decision::FirstSight => {}
            Decision::StandDown => {
                if let (Some(key), Some(task_cwd)) = (next.task_key.clone(), next.task_cwd.clone())
                {
                    if let Err(e) = crate::fleet_task::close(
                        &crate::provider_cap::questions_path(home),
                        SENDER,
                        &key,
                        &task_cwd,
                        "progress",
                        SENDER,
                    ) {
                        eprintln!("burn-watch: task close refused: {e}");
                    }
                    next.task_key = None;
                    next.task_cwd = None;
                }
                next.attempts = 0;
                next.escalated = false;
            }
            Decision::Wake(reason) => {
                let (landed, via) = wake(sid, node, busy_for(sid), &reason, runner);
                next.attempts += 1;
                next.last_wake_at = Some(now_epoch);
                let _ = emitter.emit(
                    "burn_watch_wake",
                    &serde_json::json!({
                        "session_id": sid, "node": node,
                        "attempt": next.attempts, "delivered": landed, "via": via,
                        "reason": reason,
                    }),
                );
                acted += 1;
            }
            Decision::Escalate(reason) => {
                let key = format!("burning worker on {node}");
                if let Err(e) = crate::fleet_task::file_once(
                    &crate::provider_cap::questions_path(home),
                    SENDER,
                    &key,
                    cwd,
                    &escalation_text(node, sid, &reason, next.attempts),
                    Some(&format!("fno agents resume {sid}")),
                    Some(node),
                ) {
                    eprintln!("burn-watch: task refused: {e}");
                }
                next.task_key = Some(key);
                next.task_cwd = Some(cwd.to_string());
                let _ = emitter.emit(
                    "burn_watch_escalated",
                    &serde_json::json!({
                        "session_id": sid, "node": node, "reason": reason,
                    }),
                );
                acted += 1;
            }
            Decision::Hold => skip = Some("holding".into()),
        }
        save_state(home, sid, &next);
    }
    // A session that left the scope loses its ladder and its filed task,
    // exactly as the pr-nudge ladder drops state beside its rows.
    if let Ok(entries) = std::fs::read_dir(home.burn_watch_dir()) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(sid) = name.strip_suffix(".json") else {
                continue;
            };
            if sampled.contains(sid) {
                continue;
            }
            if let Ok(Some(state)) =
                std::fs::read(entry.path()).map(|b| serde_json::from_slice::<BurnState>(&b).ok())
            {
                if let (Some(key), Some(task_cwd)) = (state.task_key, state.task_cwd) {
                    if let Err(e) = crate::fleet_task::close(
                        &crate::provider_cap::questions_path(home),
                        SENDER,
                        &key,
                        &task_cwd,
                        "row left the open-do scope",
                        SENDER,
                    ) {
                        eprintln!("burn-watch: task close refused: {e}");
                    }
                }
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let journal = crate::loop_runtime::Journal::new_raw(
        home.events_jsonl(),
        crate::daemon::global_events_path(home),
    );
    crate::tick_ledger::emit_tick(
        &journal,
        "burn_watch",
        crate::tick_ledger::SCHED_DAEMON,
        acted,
        skip.as_deref(),
        Some(&format!("scope {} sessions", scope.len())),
        BURN_WATCH_INTERVAL_S,
    );
}

fn is_open_status(status: &str) -> bool {
    !matches!(status, "done" | "superseded" | "deferred")
}

/// A claude row reading `working` is mid-turn: the roster snapshot's word,
/// keyed by the job short id (`sessionId[:8]`).
fn claude_busy(sid: &str) -> bool {
    let snapshot = crate::claude_roster::read_all_agents();
    let short = sid.split('-').next().unwrap_or(sid);
    snapshot
        .find(short)
        .is_some_and(|row| row.state.as_deref() == Some("working"))
}

pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    let interval = Duration::from_secs(BURN_WATCH_INTERVAL_S);
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < interval)
            || arm.in_flight.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let cwd = std::env::current_dir().unwrap_or_default();
        if crate::agents_config::config_lookup(&cwd, &["burn_watch", "enabled"])
            .and_then(|v| v.as_bool())
            == Some(false)
        {
            return;
        }
        let idle_s = crate::agents_config::config_lookup(&cwd, &["burn_watch", "idle_threshold_s"])
            .and_then(|v| v.as_integer())
            .map(i64::from)
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_IDLE_S);
        let spend_min =
            crate::agents_config::config_lookup(&cwd, &["burn_watch", "spend_threshold_usd"])
                .and_then(|v| v.as_float())
                .filter(|v| *v >= 0.0)
                .unwrap_or(DEFAULT_SPEND_USD);
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
        let mut runner: Runner = &mut production_run;
        run_pass(
            &home,
            &emitter,
            &mut runner,
            &claude_busy,
            crate::daemon::now_epoch_secs(),
            idle_s,
            spend_min,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(cost: Option<f64>, head: Option<&str>, touched: Option<i64>) -> Sample {
        Sample {
            cost_usd: cost,
            head: head.map(str::to_string),
            commits: head.map(|_| 1),
            touched_at: touched,
        }
    }

    #[test]
    fn spend_growth_on_a_flat_sample_burns() {
        let prev = BurnState {
            cost_usd: Some(1.0),
            head: Some("a".into()),
            commits: Some(1),
            touched_at: Some(1000),
            ..Default::default()
        };
        let (d, _) = decide(
            Some(&prev),
            &sample(Some(2.0), Some("a"), Some(1000)),
            2000,
            7200,
            0.01,
        );
        assert!(matches!(d, Decision::Wake(r) if r.contains("spend grew to $2.00")));
    }

    #[test]
    fn a_stale_node_on_a_flat_sample_burns_past_the_ceiling() {
        let prev = BurnState {
            cost_usd: Some(1.0),
            head: Some("a".into()),
            commits: Some(1),
            touched_at: Some(1000),
            ..Default::default()
        };
        let (d, _) = decide(
            Some(&prev),
            &sample(Some(1.0), Some("a"), Some(1000)),
            1000 + 7200,
            7200,
            0.01,
        );
        assert!(matches!(d, Decision::Wake(r) if r.contains("node untouched 2h")));
    }

    #[test]
    fn moving_progress_never_burns_and_resets() {
        let prev = BurnState {
            cost_usd: Some(1.0),
            head: Some("a".into()),
            commits: Some(1),
            touched_at: Some(1000),
            attempts: 2,
            escalated: true,
            ..Default::default()
        };
        let (d, next) = decide(
            Some(&prev),
            &sample(Some(9.0), Some("b"), Some(1500)),
            9000,
            7200,
            0.01,
        );
        assert_eq!(d, Decision::StandDown);
        assert_eq!((next.attempts, next.escalated), (0, false));
    }

    #[test]
    fn unknown_inputs_never_fire_either_arm() {
        let prev = BurnState {
            cost_usd: None,
            head: None,
            commits: None,
            touched_at: None,
            ..Default::default()
        };
        // No ledger cost history and no parsable touch: nothing may fire,
        // however old the sample looks.
        let (d, _) = decide(Some(&prev), &Sample::default(), 99_999, 7200, 0.01);
        assert_eq!(d, Decision::StandDown);
    }

    #[test]
    fn the_budget_escalates_once_then_holds() {
        let flat = sample(Some(2.0), Some("a"), Some(1000));
        let prev = BurnState {
            cost_usd: Some(1.0),
            head: Some("a".into()),
            commits: Some(1),
            touched_at: Some(1000),
            ..Default::default()
        };
        let mut state = prev;
        for attempt in 1..=crate::pr_nudge::MAX_ATTEMPTS {
            let (d, mut next) = decide(Some(&state), &flat, 9000, 7200, 0.01);
            assert!(matches!(d, Decision::Wake(_)), "attempt {attempt}");
            next.attempts = attempt;
            state = next;
        }
        let (d, escalated) = decide(Some(&state), &flat, 9000, 7200, 0.01);
        assert!(matches!(d, Decision::Escalate(_)));
        assert!(escalated.escalated);
        let (d, _) = decide(Some(&escalated), &flat, 9000, 7200, 0.01);
        assert_eq!(d, Decision::Hold);
    }

    #[test]
    fn a_first_sighting_only_records() {
        let (d, next) = decide(
            None,
            &sample(Some(9.0), Some("a"), Some(1)),
            99_999,
            7200,
            0.01,
        );
        assert_eq!(d, Decision::FirstSight);
        assert_eq!(next.cost_usd, Some(9.0));
    }

    #[test]
    fn scope_reads_open_do_rows_and_open_nodes_only() {
        let rows = vec![
            json!({"id": "x-1", "status": "in_progress", "touched_at": "2026-09-16T17:04:00+00:00"}),
            json!({"id": "x-2", "status": "done", "touched_at": "2026-09-16T17:04:00+00:00"}),
            json!({"id": "x-3", "status": "in_progress", "touched_at": "garbage"}),
            json!({"phase": "do", "session_id": "s-1", "node": "x-1", "harness": "claude", "started_at": "2026-09-16T09:00:00+00:00"}),
            json!({"phase": "do", "session_id": "s-2", "node": "x-2", "harness": "claude", "started_at": "2026-09-16T09:00:00+00:00"}),
            json!({"phase": "blueprint", "session_id": "s-3", "node": "x-1", "harness": "claude", "started_at": "2026-09-16T09:00:00+00:00"}),
        ];
        let scope = scope_sessions(&rows);
        assert_eq!(scope.get("s-1").map(String::as_str), Some("x-1"));
        assert!(
            !scope.contains_key("s-3"),
            "blueprint rows are out of scope"
        );
        let facts = node_facts(&rows);
        assert_eq!(facts["x-1"].1, Some(1_789_578_240));
        assert_eq!(
            facts["x-3"].1, None,
            "a garbage stamp is None, never a guess"
        );
        assert_eq!(facts["x-1"].0, "in_progress");
    }

    #[test]
    fn a_durable_receipt_never_types_a_resume_over_a_busy_turn() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let mut runner: Runner = &mut |argv: &[String], _cwd: &str| {
            calls.push(argv.to_vec());
            if argv[2] == "mail" {
                (
                    0,
                    "queued (durable): will deliver at the next turn".into(),
                    String::new(),
                )
            } else {
                (0, String::new(), String::new())
            }
        };
        let (landed, via) = wake("s-1", "x-1", true, "spend grew", &mut runner);
        assert!(!landed);
        assert_eq!(via, "durable");
        assert_eq!(
            calls.len(),
            1,
            "no resume when the leg is durable and the row is busy"
        );
    }

    #[test]
    fn a_refused_mail_falls_back_to_the_resume() {
        let mut calls: Vec<Vec<String>> = Vec::new();
        let mut runner: Runner = &mut |argv: &[String], _cwd: &str| {
            calls.push(argv.to_vec());
            if argv[2] == "mail" {
                (1, String::new(), String::new())
            } else {
                (0, String::new(), String::new())
            }
        };
        let (landed, via) = wake("s-1", "x-1", false, "spend grew", &mut runner);
        assert!(landed);
        assert_eq!(via, "resume");
        assert_eq!(calls[1][2], "resume");
    }
}

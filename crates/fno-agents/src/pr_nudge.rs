//! The nudge ladder for sessions the retirement sweep keeps on an open PR.
//!
//! The keep itself lives in the retire policy: a row whose session has a
//! `do` row on an open node carrying an unmerged `pr_number` never retires,
//! because a retirement here strands the PR with nothing left to drive it.
//! The ladder is the other half of that law: the kept session is quiet, so
//! it gets a nudge to drive the PR to merge, on a ladder that escalates to
//! the operator instead of nudging forever.
//!
//! The decision is pure; every effect (the PR-state read, the mail, the
//! resume, the operator ask, the state file) rides an injected runner or a
//! file seam, so tests stage the world. The ladder fires on the daemon's
//! retire arm only. A manual `reap` verb never nudges; its dry run prints
//! the plan (`would nudge <id> (<action>)`) and takes no effect.

use crate::events::EventEmitter;
use crate::gc_sweep::OpenPrRow;
use crate::paths::AgentsHome;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// Failed nudges before the ladder escalates to the operator and waits for
/// activity.
pub const MAX_ATTEMPTS: u32 = 3;

/// The bounded subprocess budget, shared by the PR-status read and every
/// mail/resume/ask effect.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// The paused hold re-emits at most once per hour per session, so a stuck
/// merge order names itself without flooding the event log.
const PAUSE_EMIT_FLOOR_S: i64 = 3600;

/// Per-session ladder state, one JSON file beside the terminal-stop markers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LadderState {
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub last_nudge_at: Option<i64>,
    #[serde(default)]
    pub escalated: bool,
    #[serde(default)]
    pub last_pause_emit_at: Option<i64>,
}

/// What this pass does with one open-PR row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeAction {
    /// Nothing is due: the transcript is fresh, the last nudge is young, or
    /// the ladder already escalated and is waiting for activity.
    Wait,
    /// A live merge order holds the session: no nudge, the pause names
    /// itself in the event log at most once per hour.
    Pause,
    /// The attempt budget is spent: file one operator question and wait for
    /// activity.
    Escalate,
    /// The session is live: mail it.
    Mail,
    /// The process is gone: relaunch the same conversation and mail.
    Resume,
}

impl NudgeAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            NudgeAction::Wait => "wait",
            NudgeAction::Pause => "paused",
            NudgeAction::Escalate => "escalate",
            NudgeAction::Mail => "mail",
            NudgeAction::Resume => "resume",
        }
    }
}

/// The pure ladder decision. Order is the law: reset, wait, pause,
/// escalate, mail, resume.
pub fn decide(input: &NudgeInput) -> NudgeAction {
    let mut input = input.clone();
    // 1. Reset: activity the ladder did not cause - a transcript write
    // newer than the last nudge - clears the budget. The session answered;
    // it is driving again.
    if let (Some(activity), Some(nudged)) = (input.last_activity_at, input.state.last_nudge_at) {
        if activity > nudged {
            input.state.attempts = 0;
            input.state.escalated = false;
        }
    }
    // 2. Wait: the transcript is inside the grace, or the last nudge is
    // younger than the grace. Either way the session has had no fair chance
    // to answer yet.
    let quiet_long_enough = input
        .transcript_age_s
        .is_some_and(|age| age > input.grace_secs);
    let nudged_long_ago = input
        .state
        .last_nudge_at
        .is_none_or(|t| input.now.saturating_sub(t) >= input.grace_secs);
    if !quiet_long_enough || !nudged_long_ago {
        return NudgeAction::Wait;
    }
    // 3. Pause: a live merge order is the only allowed hold. The row stays
    // and the pause names itself, at most once per hour.
    if input.merge_order_hold {
        return NudgeAction::Pause;
    }
    // 4. Escalate: the budget is spent and the operator has not heard yet.
    // After this fires once, the ladder waits for activity - no more
    // nudges, no repeat asks.
    if input.state.attempts >= MAX_ATTEMPTS {
        if !input.state.escalated {
            return NudgeAction::Escalate;
        }
        return NudgeAction::Wait;
    }
    // 5/6. A live session reads its mail; a dead process needs a resume.
    if input.live {
        NudgeAction::Mail
    } else {
        NudgeAction::Resume
    }
}

/// Everything the decision reads, cloned cheaply so `decide` can normalize
/// the reset in place.
#[derive(Debug, Clone)]
pub struct NudgeInput {
    pub state: LadderState,
    /// Transcript-quiet seconds for the row, when the seam answered.
    pub transcript_age_s: Option<i64>,
    /// Unix seconds of the transcript's last write, when the age answered.
    pub last_activity_at: Option<i64>,
    /// A live `merge-order:<node>:after:<lead>` decision names this node
    /// and the lead is not done.
    pub merge_order_hold: bool,
    pub grace_secs: i64,
    pub now: i64,
    pub live: bool,
}

/// The production effect: one bounded subprocess, returning the exit code
/// and stdout. Tests stage a fake.
pub type Runner<'a> = &'a mut dyn FnMut(&[String], &str) -> (i32, String);

/// The daemon arm: run the ladder over every open-PR row this sweep kept,
/// then drop the state files of sessions that no longer carry one.
pub fn run_ladder(home: &AgentsHome, emitter: &EventEmitter, rows: &[OpenPrRow], grace_secs: i64) {
    let now = crate::daemon::now_epoch_secs();
    let holds: Vec<bool> = rows
        .iter()
        .map(|row| merge_order_hold(home, &row.node))
        .collect();
    for (row, held) in rows.iter().zip(holds) {
        let state = load_state(home, &row.session_id);
        let mut runner = |argv: &[String], cwd: &str| -> (i32, String) {
            let bin = argv[0].clone();
            let rest: Vec<String> = argv[1..].to_vec();
            let refs: Vec<&str> = rest.iter().map(String::as_str).collect();
            match crate::loopcheck::bounded_read(
                bin.as_ref(),
                &refs,
                std::path::Path::new(cwd),
                "pr-nudge",
                RUN_TIMEOUT,
            ) {
                Ok(out) => (
                    if out.status.success() {
                        0
                    } else {
                        out.status.code().unwrap_or(1)
                    },
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                ),
                Err(_) => (1, String::new()),
            }
        };
        apply(
            home,
            emitter,
            row,
            &state,
            held,
            grace_secs,
            now,
            &mut runner,
        );
    }
    cleanup_state_files(home, rows);
}

/// One row through the decision and its effects. Public so tests can stage
/// the runner instead of the world.
#[allow(clippy::too_many_arguments)]
pub fn apply(
    home: &AgentsHome,
    emitter: &EventEmitter,
    row: &OpenPrRow,
    state: &LadderState,
    merge_order_hold: bool,
    grace_secs: i64,
    now: i64,
    runner: Runner,
) {
    let mut state = state.clone();
    let last_activity_at = row.transcript_age_s.map(|age| now.saturating_sub(age));
    let input = NudgeInput {
        state: state.clone(),
        transcript_age_s: row.transcript_age_s,
        last_activity_at,
        merge_order_hold,
        grace_secs,
        now,
        live: row.live,
    };
    let action = decide(&input);
    match action {
        NudgeAction::Wait => {
            if input.state.attempts != state.attempts || input.state.escalated != state.escalated {
                save_state(home, &row.session_id, &state);
            }
        }
        NudgeAction::Pause => {
            let due = state
                .last_pause_emit_at
                .is_none_or(|t| now.saturating_sub(t) >= PAUSE_EMIT_FLOOR_S);
            if due {
                let _ = emitter.emit(
                    "pr_nudge_paused",
                    &serde_json::json!({
                        "session_id": row.session_id,
                        "node": row.node,
                        "pr": row.pr,
                    }),
                );
                state.last_pause_emit_at = Some(now);
                save_state(home, &row.session_id, &state);
            }
        }
        NudgeAction::Escalate => {
            let marker = format!("pr-nudge: PR #{} on {}", row.pr, row.node);
            if !open_questions_mention(marker.as_str()) {
                let text = format!(
                    "{marker}, session {}: {MAX_ATTEMPTS} nudges drew no activity. \
                     The row is kept. Resume it with `fno agents resume {sid}`, \
                     or record a merge order.",
                    row.session_id,
                    sid = row.session_id,
                );
                let argv = vec![
                    "fno".to_string(),
                    "inbox".to_string(),
                    "outstanding".to_string(),
                    "ask".to_string(),
                    text,
                    "--node".to_string(),
                    row.node.clone(),
                ];
                let _ = runner(&argv, "");
            }
            state.escalated = true;
            save_state(home, &row.session_id, &state);
            let _ = emitter.emit(
                "pr_nudge_escalated",
                &serde_json::json!({
                    "session_id": row.session_id,
                    "node": row.node,
                    "pr": row.pr,
                }),
            );
        }
        NudgeAction::Mail | NudgeAction::Resume => {
            let text = nudge_text(row, runner);
            let argv = match action {
                NudgeAction::Mail => vec![
                    "fno".to_string(),
                    "agents".to_string(),
                    "mail".to_string(),
                    "send".to_string(),
                    row.session_id.clone(),
                    text,
                ],
                _ => vec![
                    "fno".to_string(),
                    "agents".to_string(),
                    "resume".to_string(),
                    row.session_id.clone(),
                    "--message".to_string(),
                    text,
                ],
            };
            let (code, _) = runner(&argv, "");
            state.attempts += 1;
            state.last_nudge_at = Some(now);
            save_state(home, &row.session_id, &state);
            let _ = emitter.emit(
                "pr_nudge_sent",
                &serde_json::json!({
                    "session_id": row.session_id,
                    "node": row.node,
                    "pr": row.pr,
                    "action": action.as_str(),
                    "attempt": state.attempts,
                    "delivered": code == 0,
                }),
            );
        }
    }
}

/// The nudge body: the order to drive, plus the PR's own verdict line so
/// the session sees the state without a round trip.
fn nudge_text(row: &OpenPrRow, runner: Runner) -> String {
    let argv = vec![
        "fno".to_string(),
        "do".to_string(),
        "pr".to_string(),
        "status".to_string(),
        row.pr.to_string(),
    ];
    let (code, stdout) = runner(&argv, &row.cwd);
    let line = if code == 0 {
        stdout
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default()
            .to_string()
    } else {
        format!("pr status unread (exit {code})")
    };
    format!(
        "continue: PR #{pr} on node {node} is open and not merged. Drive it to merge. \
         fno do pr status {pr}: {line}",
        pr = row.pr,
        node = row.node,
    )
}

/// Does an already-open operator question carry this marker? Same read the
/// heal arm makes, so dedupe cannot drift from the board.
fn open_questions_mention(marker: &str) -> bool {
    let out = crate::loopcheck::bounded_read(
        "fno".as_ref(),
        &["inbox", "outstanding", "--json"],
        std::path::Path::new("."),
        "pr-nudge",
        RUN_TIMEOUT,
    );
    let Ok(out) = out else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
        return false;
    };
    v.get("questions")
        .and_then(Value::as_array)
        .is_some_and(|qs| {
            qs.iter().any(|q| {
                q.get("question")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains(marker))
            })
        })
}

/// The merge-order pause (the only allowed one): a live decision with the
/// subject `merge-order:<node>:after:<lead>` names this node, and the lead
/// node is not done yet. A retracted or superseded decision does not count.
fn merge_order_hold(home: &AgentsHome, node: &str) -> bool {
    let subject_prefix = format!("merge-order:{node}:after:");
    let mut newest: Option<(String, String, String)> = None; // (ts, decision_id, lead)
    let mut retired: std::collections::HashSet<String> = Default::default();
    let path = home.root().join("decisions.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    for line in text.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = row.get("type").and_then(Value::as_str).unwrap_or("");
        let data = row.get("data").cloned().unwrap_or(Value::Null);
        if kind == "decision_retracted" {
            if let Some(target) = data.get("target_decision_id").and_then(Value::as_str) {
                retired.insert(target.to_string());
            }
            continue;
        }
        let subject = data.get("subject").and_then(Value::as_str).unwrap_or("");
        if !subject.starts_with(&subject_prefix) {
            continue;
        }
        let did = data
            .get("decision_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(superseded) = data.get("supersedes").and_then(Value::as_str) {
            if !superseded.is_empty() {
                retired.insert(superseded.to_string());
            }
        }
        let ts = row.get("ts").and_then(Value::as_str).unwrap_or("");
        let take = match &newest {
            None => true,
            Some((best_ts, _, _)) => ts >= best_ts.as_str(),
        };
        if take {
            newest = Some((
                ts.to_string(),
                did.to_string(),
                subject[subject_prefix.len()..].to_string(),
            ));
        }
    }
    let Some((_, did, lead)) = newest else {
        return false;
    };
    if retired.contains(&did) {
        return false;
    }
    if lead.is_empty() {
        return false;
    }
    // The lead must still be open work for the hold to stand.
    !crate::gc_sweep::read_graph_entries(home)
        .and_then(|g| g.statuses.get(&lead).cloned())
        .is_some_and(|status| status == "done")
}

/// The session id is the state filename: hex and dashes only, so a stray
/// registry value can never write outside the dir.
fn valid_sid(sid: &str) -> bool {
    !sid.is_empty() && sid.len() <= 64 && sid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn state_path(home: &AgentsHome, sid: &str) -> Option<std::path::PathBuf> {
    if !valid_sid(sid) {
        return None;
    }
    Some(home.pr_nudge_dir().join(format!("{sid}.json")))
}

fn load_state(home: &AgentsHome, sid: &str) -> LadderState {
    let Some(path) = state_path(home, sid) else {
        return LadderState::default();
    };
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_state(home: &AgentsHome, sid: &str, state: &LadderState) {
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

/// Drop the state files of sessions no longer carrying an open PR: the
/// ladder left the row, so its budget must not survive to a later
/// assignment of the same session id.
fn cleanup_state_files(home: &AgentsHome, rows: &[OpenPrRow]) {
    let dir = home.pr_nudge_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let live: std::collections::HashSet<String> = rows
        .iter()
        .map(|r| format!("{}.json", r.session_id))
        .collect();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.ends_with(".json") || live.contains(name) {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// The DRY-RUN plan: decisions only, no effect, no state write. The action
/// strings match [`NudgeAction::as_str`] so a rehearsal reads like the real
/// arm's events.
pub fn plan(home: &AgentsHome, rows: &[OpenPrRow], grace_secs: i64) -> Vec<(String, String)> {
    let now = crate::daemon::now_epoch_secs();
    rows.iter()
        .map(|row| {
            let state = load_state(home, &row.session_id);
            let held = merge_order_hold(home, &row.node);
            let input = NudgeInput {
                state: state.clone(),
                transcript_age_s: row.transcript_age_s,
                last_activity_at: row.transcript_age_s.map(|age| now.saturating_sub(age)),
                merge_order_hold: held,
                grace_secs,
                now,
                live: row.live,
            };
            let action = decide(&input);
            (row.id.clone(), action.as_str().to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(live: bool) -> OpenPrRow {
        OpenPrRow {
            id: "worker-a".into(),
            session_id: "11111111-2222-3333-4444-555555555555".into(),
            harness: "claude".into(),
            node: "x-node".into(),
            pr: 1943,
            cwd: "/tmp/wt".into(),
            transcript_age_s: Some(1000),
            live,
        }
    }

    fn input(state: LadderState, live: bool) -> NudgeInput {
        NudgeInput {
            state,
            transcript_age_s: Some(1000),
            last_activity_at: Some(900),
            merge_order_hold: false,
            grace_secs: 900,
            now: 1900,
            live,
        }
    }

    #[test]
    fn quiet_live_row_gets_mail() {
        let live_row = row(true);
        let mut text_runner_calls: Vec<Vec<String>> = Vec::new();
        let mut runner = |argv: &[String], _cwd: &str| -> (i32, String) {
            text_runner_calls.push(argv.to_vec());
            if argv.contains(&"do".to_string()) {
                (0, "1943 OPEN pending\n".into())
            } else {
                (0, String::new())
            }
        };
        let home = AgentsHome::at(std::env::temp_dir().join("fno-pn-mail"));
        let emitter = EventEmitter::new(home.events_jsonl(), "test");
        let _ = live_row;
        apply(
            &home,
            &emitter,
            &row(true),
            &LadderState::default(),
            false,
            900,
            1900,
            &mut runner,
        );
        assert_eq!(text_runner_calls.len(), 2);
        assert!(text_runner_calls[0].contains(&"do".to_string()));
        let mail = &text_runner_calls[1];
        assert_eq!(mail[1], "agents");
        assert_eq!(mail[3], "send");
        assert!(mail[5].contains("PR #1943"));
        assert!(mail[5].contains("1943 OPEN pending"));
        let saved = load_state(&home, &row(true).session_id);
        assert_eq!(saved.attempts, 1);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("fno-pn-mail"));
    }

    #[test]
    fn stopped_row_gets_resume() {
        let r = row(false);
        let mut saw_resume = false;
        let mut runner = |argv: &[String], _cwd: &str| -> (i32, String) {
            if argv.contains(&"resume".to_string()) {
                saw_resume = true;
            }
            (0, String::new())
        };
        let home = AgentsHome::at(std::env::temp_dir().join("fno-pn-resume"));
        let emitter = EventEmitter::new(home.events_jsonl(), "test");
        apply(
            &home,
            &emitter,
            &r,
            &LadderState::default(),
            false,
            900,
            1900,
            &mut runner,
        );
        assert!(saw_resume);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("fno-pn-resume"));
    }

    #[test]
    fn fresh_transcript_waits_and_does_not_nudge() {
        let mut inp = input(LadderState::default(), true);
        inp.transcript_age_s = Some(10);
        inp.last_activity_at = Some(1890);
        assert_eq!(decide(&inp), NudgeAction::Wait);
    }

    #[test]
    fn recent_nudge_waits() {
        let st = LadderState {
            attempts: 1,
            last_nudge_at: Some(1880),
            ..Default::default()
        };
        assert_eq!(decide(&input(st, true)), NudgeAction::Wait);
    }

    #[test]
    fn merge_order_pauses() {
        let mut inp = input(LadderState::default(), true);
        inp.merge_order_hold = true;
        assert_eq!(decide(&inp), NudgeAction::Pause);
    }

    #[test]
    fn third_failed_nudge_escalates_once_then_waits() {
        let st = LadderState {
            attempts: 3,
            last_nudge_at: Some(900),
            escalated: false,
            ..Default::default()
        };
        assert_eq!(decide(&input(st.clone(), true)), NudgeAction::Escalate);
        let st = LadderState {
            attempts: 3,
            last_nudge_at: Some(900),
            escalated: true,
            ..Default::default()
        };
        assert_eq!(decide(&input(st, true)), NudgeAction::Wait);
    }

    #[test]
    fn activity_resets_the_budget() {
        let st = LadderState {
            attempts: 3,
            last_nudge_at: Some(100),
            escalated: true,
            ..Default::default()
        };
        let mut inp = input(st, true);
        // The transcript was rewritten AFTER the last nudge.
        inp.last_activity_at = Some(1500);
        assert_eq!(decide(&inp), NudgeAction::Mail);
    }

    #[test]
    fn state_file_roundtrip_and_sid_guard() {
        let home = AgentsHome::at(std::env::temp_dir().join("fno-pn-state"));
        let _ = std::fs::remove_dir_all(home.root().to_path_buf());
        let st = LadderState {
            attempts: 2,
            last_nudge_at: Some(1234),
            escalated: true,
            ..Default::default()
        };
        save_state(&home, "abc-123", &st);
        assert_eq!(load_state(&home, "abc-123"), st);
        save_state(&home, "../evil", &st);
        assert_eq!(load_state(&home, "../evil"), LadderState::default());
        let _ = std::fs::remove_dir_all(home.root().to_path_buf());
    }
}

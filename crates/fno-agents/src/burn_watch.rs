//! The burn arm: spend against progress, per live worker.
//!
//! A worker can burn hours and dollars while its node's `touched_at` and its
//! branch sit flat; nothing else in the fleet reads cost against progress.
//! This arm samples every live session carrying an open execute phase: ledger
//! spend, branch head, commit count and newest commit time (`git` in the
//! row's own cwd), and the node's `touched_at` (graph). Progress on any axis
//! resets the ladder; a flat sample where spend grew, or where BOTH the node
//! and the branch aged past the idle ceiling, wakes the worker (a fresh
//! commit is progress even when the node row is ancient - the row only moves
//! on backlog commands, never on a push), and three unanswered wakes file
//! ONE fleet task through the same store the pr-nudge ladder escalates
//! through.
//!
//! Scope is open execute sessions on purpose: a blueprint or think worker's
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
const ESCALATE_DEFAULT_RED_ROUNDS: u32 = 3;
const ESCALATE_DEFAULT_CONFLICT_MERGES: u32 = 2;
const ESCALATE_DEFAULT_DIFF_LINES: u64 = 2500;
const ESCALATE_DEFAULT_HOURS: i64 = 24;
const ESCALATE_DEFAULT_SLOT_WAIT_MIN: i64 = 60;
const FIRST_EDIT_DEFAULT_MIN: i64 = 20;
const SENDER: &str = "fno/burn-watch";
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
    /// Newest commit time on the sampled HEAD as epoch seconds, read from
    /// the same probe as `head`.
    pub last_commit_at: Option<i64>,
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
    pub last_commit_at: Option<i64>,
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
    /// The capability escalation note went to the team (or its board) for
    /// this worker; every later pass skips the counter reads.
    #[serde(default)]
    pub escalation_noted: bool,
    /// The first-edit note went to the lead for this worker; sent once.
    #[serde(default)]
    pub first_edit_noted: bool,
}

/// What the five capability counters read this pass. None is unread, and an
/// unread counter never trips.
#[derive(Debug, Clone, Default, PartialEq)]
struct Counters {
    red: Option<(String, u32)>,
    conflict_merges: Option<u32>,
    diff_lines: Option<u64>,
    hours: Option<i64>,
    slot_wait_min: Option<i64>,
}

/// The `burn_watch.escalate_*` thresholds. A 0 turns that counter off; a
/// negative or wrong-typed value takes the default.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Thresholds {
    red_rounds: u32,
    conflict_merges: u32,
    diff_lines: u64,
    hours: i64,
    slot_wait_minutes: i64,
    first_edit_minutes: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            red_rounds: ESCALATE_DEFAULT_RED_ROUNDS,
            conflict_merges: ESCALATE_DEFAULT_CONFLICT_MERGES,
            diff_lines: ESCALATE_DEFAULT_DIFF_LINES,
            hours: ESCALATE_DEFAULT_HOURS,
            slot_wait_minutes: ESCALATE_DEFAULT_SLOT_WAIT_MIN,
            first_edit_minutes: FIRST_EDIT_DEFAULT_MIN,
        }
    }
}

/// What this pass does with one in-scope session.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// No prior sample: record one and wait an interval. A worker with no
    /// history can never fire on its first sighting.
    FirstSight,
    /// Progress landed on a sampled axis: reset the ladder, close any
    /// filed task.
    StandDown,
    /// Flat sample, budget not spent: wake the worker.
    Wake(String),
    /// Flat sample, budget spent, operator not yet asked: file one task.
    Escalate(String),
    /// Wait: an already-escalated flat row, or a probe that died this pass
    /// (git hung, ledger unreadable). The ladder and the last good sample
    /// are carried; nothing resets and nothing fires on unknown.
    Hold,
}

/// The pure decision. Progress is POSITIVE change on a sampled axis; a flat
/// sample burns when spend grew past `spend_min` or the node's
/// `touched_at` aged past `idle_s`. A probe that died carries the last
/// good sample and decides nothing. Unknown inputs never fire.
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
        last_commit_at: sample.last_commit_at,
        ..BurnState::default()
    };
    let Some(prev) = prev else {
        return (Decision::FirstSight, next);
    };
    next.task_key = prev.task_key.clone();
    next.task_cwd = prev.task_cwd.clone();
    // A probe that died this pass flips an axis Some -> None: carry the
    // last good sample and hold the ladder, never decide on unknown.
    let probe_died = matches!((prev.cost_usd, sample.cost_usd), (Some(_), None))
        || matches!((prev.commits, sample.commits), (Some(_), None))
        || matches!((&prev.head, &sample.head), (Some(_), None));
    if probe_died {
        next = BurnState {
            cost_usd: prev.cost_usd,
            head: prev.head.clone(),
            commits: prev.commits,
            touched_at: prev.touched_at,
            last_commit_at: prev.last_commit_at,
            attempts: prev.attempts,
            escalated: prev.escalated,
            last_wake_at: prev.last_wake_at,
            task_key: prev.task_key.clone(),
            task_cwd: prev.task_cwd.clone(),
            escalation_noted: prev.escalation_noted,
            first_edit_noted: prev.first_edit_noted,
        };
        return (Decision::Hold, next);
    }
    // Progress is POSITIVE change: (Some, Some) that differ, or an axis
    // APPEARING (None -> Some) where real work became measurable.
    let moved = |before: &Option<String>, after: &Option<String>| match (before, after) {
        (Some(a), Some(b)) => a != b,
        (None, Some(_)) => true,
        _ => false,
    };
    let stringified = |n: Option<i64>| n.map(|t| t.to_string());
    let head_moved = moved(&prev.head, &sample.head);
    let count_moved = moved(
        &prev.commits.map(|c| c.to_string()),
        &sample.commits.map(|c| c.to_string()),
    );
    let touch_moved = moved(
        &stringified(prev.touched_at),
        &stringified(sample.touched_at),
    );
    if head_moved || count_moved || touch_moved {
        return (Decision::StandDown, next);
    }
    // Flat, probes read: the two burn arms.
    let spend_grew = match (prev.cost_usd, sample.cost_usd) {
        (Some(before), Some(now)) => now - before >= spend_min,
        _ => false,
    };
    let node_aged = sample
        .touched_at
        .is_some_and(|t| now_epoch.saturating_sub(t) >= idle_s);
    // Both flat, or no alarm: a fresh commit on the branch IS progress even
    // when the node row's touched_at is ancient, because the row only moves
    // on backlog commands, never on a push. An unreadable commit time stays
    // silent, matching the unparsable-touched_at rule.
    let branch_aged = sample
        .last_commit_at
        .is_some_and(|t| now_epoch.saturating_sub(t) >= idle_s);
    let reason = if spend_grew {
        Some(format!(
            "spend grew to ${:.2} with no new commit and no node touch",
            sample.cost_usd.unwrap_or(0.0)
        ))
    } else if node_aged && branch_aged {
        Some(format!(
            "node untouched {}h with no new commit in {}h",
            now_epoch.saturating_sub(sample.touched_at.unwrap_or(0)) / 3600,
            now_epoch.saturating_sub(sample.last_commit_at.unwrap_or(0)) / 3600
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

/// One in-scope worker: its node, the model the graph observed, and when its
/// own execute row opened.
#[derive(Debug, Clone, PartialEq)]
struct ScopedSession {
    node: String,
    observed_model: Option<String>,
    started_at: Option<i64>,
}

/// What the pass knows about one node.
#[derive(Debug, Clone, Default)]
struct NodeFacts {
    status: String,
    touched_at: Option<i64>,
    pr_number: Option<u64>,
    first_execute_at: Option<i64>,
}

/// The model a session row observed, when the keeper wrote one: the
/// `observed` kind carries the live model; other kinds carry nothing.
fn session_model(row: &Value) -> Option<String> {
    let observed = row.get("observed_model")?;
    if observed.get("kind").and_then(Value::as_str) != Some("observed") {
        return None;
    }
    observed
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
        .map(str::to_string)
}

/// Open execute sessions on open nodes, joined to their node: the arm's
/// scope. The store returns node rows; the workers sit under each node's
/// `sessions` array, so the walk goes inside.
fn scope_sessions(rows: &[Value]) -> BTreeMap<String, ScopedSession> {
    let mut out = BTreeMap::new();
    for row in rows {
        if row
            .get("status")
            .and_then(Value::as_str)
            .map(is_open_status)
            != Some(true)
        {
            continue;
        }
        let Some(node) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(sessions) = row.get("sessions").and_then(Value::as_array) else {
            continue;
        };
        for session in sessions {
            if !crate::graph_store::is_open_do_row(session) {
                continue;
            }
            if let Some(sid) = session.get("session_id").and_then(Value::as_str) {
                out.insert(
                    sid.to_string(),
                    ScopedSession {
                        node: node.to_string(),
                        observed_model: session_model(session),
                        started_at: session
                            .get("started_at")
                            .and_then(Value::as_str)
                            .and_then(parse_epoch),
                    },
                );
            }
        }
    }
    out
}

fn parse_epoch(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.timestamp())
}

/// Open nodes by id; unparsable stamps read None and can never feed the age
/// arm.
fn node_facts(rows: &[Value]) -> HashMap<String, NodeFacts> {
    let mut out: HashMap<String, NodeFacts> = HashMap::new();
    for row in rows {
        let (Some(id), Some(status)) = (
            row.get("id").and_then(Value::as_str),
            row.get("status").and_then(Value::as_str),
        ) else {
            continue;
        };
        let touched_at = row
            .get("touched_at")
            .and_then(Value::as_str)
            .and_then(parse_epoch);
        let first_execute_at = row
            .get("sessions")
            .and_then(Value::as_array)
            .map(|sessions| {
                sessions
                    .iter()
                    .filter(|s| s.get("phase").and_then(Value::as_str) == Some("execute"))
                    .filter_map(|s| {
                        s.get("started_at")
                            .and_then(Value::as_str)
                            .and_then(parse_epoch)
                    })
                    .min()
            })
            .unwrap_or(None);
        out.insert(
            id.to_string(),
            NodeFacts {
                status: status.to_string(),
                touched_at,
                pr_number: row.get("pr_number").and_then(Value::as_u64),
                first_execute_at,
            },
        );
    }
    out
}

/// The production runner every arm shares with the pr-nudge ladder.
pub type Runner<'a> = &'a mut dyn FnMut(&[String], &str) -> (i32, String, String);

pub(crate) fn run_command(argv: &[String], cwd: &str) -> (i32, String, String) {
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

/// Branch progress at the row's own checkout: `(head, commit count, newest
/// commit time)`. A failed read reads None and can never count as progress
/// or feed an age arm.
fn git_progress(cwd: &str, runner: Runner) -> (Option<String>, Option<u64>, Option<i64>) {
    let head_line = runner(
        &[
            "git".into(),
            "log".into(),
            "-1".into(),
            "--format=%H %ct".into(),
        ],
        cwd,
    )
    .1
    .lines()
    .rev()
    .find(|l| !l.trim().is_empty())
    .map(|l| l.trim().to_string());
    let (head, last_commit_at) = head_line
        .map(|line| {
            let mut parts = line.split_whitespace();
            (
                parts.next().map(str::to_string),
                parts.next().and_then(|t| t.parse().ok()),
            )
        })
        .unwrap_or((None, None));
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
    (head, count, last_commit_at)
}

/// The last non-empty stdout line, trimmed: a git word like a branch or a sha.
fn first_line(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

/// The workflow with the most distinct failed heads on a branch, from a
/// GitHub Actions failure listing. A run whose latest attempt passed left
/// the failure listing before this fold saw it, and the bucket check drops
/// it again should a caller pass a mixed listing; nothing failed reads None.
fn red_rounds(runs: &[Value]) -> Option<(String, u32)> {
    let mut heads: BTreeMap<&str, std::collections::BTreeSet<&str>> = BTreeMap::new();
    for run in runs {
        if crate::pr_push::rest_bucket(run) != "fail" {
            continue;
        }
        if let (Some(name), Some(sha)) = (
            run.get("name").and_then(Value::as_str),
            run.get("head_sha").and_then(Value::as_str),
        ) {
            heads.entry(name).or_default().insert(sha);
        }
    }
    heads
        .into_iter()
        .max_by_key(|(_, shas)| shas.len())
        .map(|(name, shas)| (name.to_string(), shas.len() as u32))
}

/// Red rounds for the row's PR branch, when the branch has a PR and the gh
/// budget allows the read. A failed read is None and trips nothing.
fn red_rounds_for(cwd: &str, runner: Runner, now_ms: i64) -> Option<(String, u32)> {
    if crate::gh_budget::snapshot(&crate::gh_budget::ledger_path(), now_ms).backoff_remaining_s != 0
    {
        return None;
    }
    let branch = first_line(
        &runner(
            &[
                "git".into(),
                "rev-parse".into(),
                "--abbrev-ref".into(),
                "HEAD".into(),
            ],
            cwd,
        )
        .1,
    )?;
    let (code, out, _) = runner(
        &[
            "gh".into(),
            "api".into(),
            format!(
                "repos/{{owner}}/{{repo}}/actions/runs?branch={branch}&status=failure&per_page=100"
            ),
        ],
        cwd,
    );
    if code != 0 {
        return None;
    }
    let page: Value = serde_json::from_str(&out).ok()?;
    let runs = page.get("workflow_runs")?.as_array()?;
    red_rounds(runs)
}

/// Merges of `base` into the branch whose `git show --remerge-diff` answer is
/// non-empty: each is one merge that needed a resolution. At most the newest
/// 20 merges are opened; a failed read is None.
fn conflict_merges(cwd: &str, base: &str, runner: Runner) -> Option<u32> {
    let (code, out, _) = runner(
        &[
            "git".into(),
            "rev-list".into(),
            "--merges".into(),
            "--max-count=20".into(),
            format!("{base}..HEAD"),
        ],
        cwd,
    );
    if code != 0 {
        return None;
    }
    let mut count = 0u32;
    for sha in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (_, show, _) = runner(
            &[
                "git".into(),
                "show".into(),
                "--remerge-diff".into(),
                "--format=".into(),
                "--name-only".into(),
                sha.to_string(),
            ],
            cwd,
        );
        if !show.trim().is_empty() {
            count += 1;
        }
    }
    Some(count)
}

/// Insertions plus deletions between `base` and HEAD. git omits a zero
/// field, so an absent word reads 0; a failed read is None.
fn diff_lines(cwd: &str, base: &str, runner: Runner) -> Option<u64> {
    let (code, out, _) = runner(
        &[
            "git".into(),
            "diff".into(),
            "--shortstat".into(),
            format!("{base}...HEAD"),
        ],
        cwd,
    );
    if code != 0 {
        return None;
    }
    let field = |word: &str| -> u64 {
        out.split(word)
            .next()
            .and_then(|head| {
                let digits: String = head
                    .trim_end()
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if digits.is_empty() {
                    None
                } else {
                    digits.chars().rev().collect::<String>().parse().ok()
                }
            })
            .unwrap_or(0)
    };
    Some(field("insertion") + field("deletion"))
}

/// The five counters for one in-scope worker's checkout. `base` unset, a
/// probe that dies, an unreadable listing: each reads None and never trips.
fn sample_counters(cwd: &str, facts: &NodeFacts, runner: Runner, now_epoch: i64) -> Counters {
    let base = first_line(
        &runner(
            &[
                "git".into(),
                "rev-parse".into(),
                "--abbrev-ref".into(),
                "origin/HEAD".into(),
            ],
            cwd,
        )
        .1,
    );
    let now_ms = now_epoch.saturating_mul(1000);
    Counters {
        red: facts
            .pr_number
            .filter(|pr| *pr > 0)
            .and_then(|_| red_rounds_for(cwd, runner, now_ms)),
        conflict_merges: base
            .as_deref()
            .and_then(|b| conflict_merges(cwd, b, runner)),
        diff_lines: base.as_deref().and_then(|b| diff_lines(cwd, b, runner)),
        hours: facts.first_execute_at.map(|t| (now_epoch - t) / 3600),
        slot_wait_min: crate::test_run::build_wait_since_ms(Path::new(cwd))
            .map(|since| (now_ms - since) / 60_000),
    }
}

/// Whether the checkout holds no work yet: zero commits past the base and a
/// clean tree. Both reads must answer, and a zero count must print as a zero:
/// an empty or failed listing reads None and never fires the deadline.
fn no_edits_yet(cwd: &str, base: &str, runner: Runner) -> Option<bool> {
    let (code, out, _) = runner(
        &[
            "git".into(),
            "rev-list".into(),
            "--count".into(),
            format!("{base}..HEAD"),
        ],
        cwd,
    );
    let ahead: u64 = (code == 0)
        .then(|| first_line(&out))
        .flatten()?
        .parse()
        .ok()?;
    if ahead > 0 {
        return Some(false);
    }
    let (code, out, _) = runner(&["git".into(), "status".into(), "--porcelain".into()], cwd);
    (code == 0).then(|| out.trim().is_empty())
}

/// The first-edit deadline: a worker whose own execute row opened at least
/// `minutes` ago and whose checkout still holds no work. The node's earliest
/// execute start would fire at once for a fresh worker on a resumed node.
/// Returns the note naming the gate the arm can read without a transcript.
fn first_edit_overdue(
    node: &str,
    sid: &str,
    harness: &str,
    cwd: &str,
    started_at: Option<i64>,
    minutes: i64,
    runner: Runner,
    now_epoch: i64,
) -> Option<String> {
    if minutes <= 0 {
        return None;
    }
    let age_min = (now_epoch - started_at?) / 60;
    if age_min < minutes {
        return None;
    }
    let base = first_line(
        &runner(
            &[
                "git".into(),
                "rev-parse".into(),
                "--abbrev-ref".into(),
                "origin/HEAD".into(),
            ],
            cwd,
        )
        .1,
    )?;
    if !no_edits_yet(cwd, &base, runner)? {
        return None;
    }
    let gate = match crate::test_run::build_wait_since_ms(Path::new(cwd)) {
        Some(since) => format!(
            "{}m waiting at the cargo build door",
            (now_epoch.saturating_mul(1000) - since) / 60_000
        ),
        None => "none measurable without the transcript".to_string(),
    };
    Some(format!(
        "{SENDER_LINE} Stuck: node {node}, session {sid} on {harness} has made no edit \
         {age_min}m after its execute phase opened (no commit past {base}, clean tree in \
         {cwd}). Gate it is on: {gate}. Read it with `fno agents logs {sid}`. The deadline \
         is {minutes}m; this note is sent once per worker."
    ))
}

/// One note to the node's owning lead. A refused send, or a node with no
/// owner, files one fleet task instead so the note is never lost.
fn note_owner(
    home: &AgentsHome,
    runner: Runner,
    owner: Option<String>,
    text: &str,
    cwd: &str,
    node: &str,
    task_key: &str,
    run_line: &str,
) {
    let delivered = owner.is_some_and(|lead_scope| {
        let argv = vec![
            "fno".to_string(),
            "agents".to_string(),
            "mail".to_string(),
            "send".to_string(),
            "--from-name".to_string(),
            SENDER.to_string(),
            "--origin".to_string(),
            "scheduler".to_string(),
            "--to-lead".to_string(),
            lead_scope,
            text.to_string(),
        ];
        let (code, stdout, _) = runner(&argv, "");
        crate::mail_inject::mail_send_accepted(code, &stdout)
    });
    if !delivered {
        if let Err(e) = crate::fleet_task::file_once(
            &crate::provider_cap::questions_path(home),
            SENDER,
            task_key,
            cwd,
            text,
            Some(run_line),
            Some(node),
        ) {
            eprintln!("burn-watch: task refused: {e}");
        }
    }
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

/// One evidence phrase per counter at or past its non-zero threshold.
fn escalation_trips(counters: &Counters, t: &Thresholds) -> Vec<String> {
    let mut trips = Vec::new();
    if t.red_rounds > 0 {
        if let Some((workflow, heads)) = &counters.red {
            if *heads >= t.red_rounds {
                trips.push(format!(
                    "{workflow} red on {heads} heads (threshold {})",
                    t.red_rounds
                ));
            }
        }
    }
    if t.conflict_merges > 0 {
        if let Some(n) = counters.conflict_merges {
            if n >= t.conflict_merges {
                trips.push(format!(
                    "{n} merges of the base needed a resolution (threshold {})",
                    t.conflict_merges
                ));
            }
        }
    }
    if t.diff_lines > 0 {
        if let Some(n) = counters.diff_lines {
            if n >= t.diff_lines {
                trips.push(format!(
                    "{n} changed lines against the base (threshold {})",
                    t.diff_lines
                ));
            }
        }
    }
    if t.hours > 0 {
        if let Some(h) = counters.hours {
            if h >= t.hours {
                trips.push(format!("{h}h on the node (threshold {}h)", t.hours));
            }
        }
    }
    if t.slot_wait_minutes > 0 {
        if let Some(m) = counters.slot_wait_min {
            if m >= t.slot_wait_minutes {
                trips.push(format!(
                    "{m}m waiting at the cargo build door (threshold {}m)",
                    t.slot_wait_minutes
                ));
            }
        }
    }
    trips
}

/// The once-per-worker note: what tripped, what did not, and the non-goal.
fn escalation_note_text(
    node: &str,
    sid: &str,
    harness: &str,
    model: &str,
    counters: &Counters,
    tripped: &[String],
) -> String {
    let reading = |named: bool, text: String| {
        if named {
            None
        } else {
            Some(text)
        }
    };
    let mut untripped: Vec<String> = Vec::new();
    match &counters.red {
        Some((workflow, heads)) => {
            if let Some(text) = reading(
                tripped.iter().any(|t| t.contains(workflow)),
                format!("{workflow} red on {heads} heads"),
            ) {
                untripped.push(text);
            }
        }
        None => untripped.push("red rounds unread".into()),
    }
    if let Some(n) = counters.conflict_merges {
        if let Some(text) = reading(
            tripped.iter().any(|t| t.contains("merges of the base")),
            format!("{n} merges of the base"),
        ) {
            untripped.push(text);
        }
    }
    if let Some(n) = counters.diff_lines {
        if let Some(text) = reading(
            tripped.iter().any(|t| t.contains("changed lines")),
            format!("{n} changed lines"),
        ) {
            untripped.push(text);
        }
    }
    if let Some(h) = counters.hours {
        if let Some(text) = reading(
            tripped.iter().any(|t| t.contains("h on the node")),
            format!("{h}h on the node"),
        ) {
            untripped.push(text);
        }
    }
    if let Some(m) = counters.slot_wait_min {
        if let Some(text) = reading(
            tripped.iter().any(|t| t.contains("cargo build door")),
            format!("{m}m at the cargo build door"),
        ) {
            untripped.push(text);
        }
    }
    let untripped_word = if untripped.is_empty() {
        "no other reading".to_string()
    } else {
        untripped.join("; ")
    };
    format!(
        "{SENDER_LINE} node {node}, session {sid} on {harness} / {model}: {}. \
         Other readings: {untripped_word}. The team chooses the destination: \
         `skills/target/scripts/handoff.sh --harness <harness> --model <model>` \
         (docs/architecture/target-self-handoff.md). Nothing was moved. \
         This note is sent once per worker.",
        tripped.join("; ")
    )
}

/// The node's owning team scope, resolved once per pass and cached.
/// An unreadable registry reads as no owner.
fn node_owner(
    config_cwd: &Path,
    registry_path: &Path,
    rows: &[Value],
    cache: &mut Option<HashMap<String, String>>,
    node: &str,
) -> Option<String> {
    if cache.is_none() {
        let teams = crate::territory::live_teams(registry_path).ok()?;
        let (map, _) = crate::territory::node_owners(
            &teams,
            rows,
            &Ok(crate::org_board::project_map(config_cwd).unwrap_or_default()),
        );
        *cache = Some(map);
    }
    cache.as_ref()?.get(node).cloned()
}

/// One wake: mail the report; when the lane could not take it, fall back to
/// the content-confirmed resume. A durable receipt means the message lands
/// at the worker's next turn, so a mid-turn worker never takes a resume
/// typed over its turn.
fn wake(sid: &str, node: &str, busy: bool, reason: &str, runner: Runner) -> (bool, &'static str) {
    wake_with_text(sid, busy, &wake_text(node, reason), SENDER, runner)
}

pub(crate) fn wake_with_text(
    sid: &str,
    busy: bool,
    text: &str,
    sender: &str,
    runner: Runner,
) -> (bool, &'static str) {
    let mail_argv = vec![
        "fno".to_string(),
        "agents".to_string(),
        "mail".to_string(),
        "send".to_string(),
        "--from-name".to_string(),
        sender.to_string(),
        "--origin".to_string(),
        "scheduler".to_string(),
        sid.to_string(),
        text.to_string(),
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
        text.to_string(),
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
    facts: &HashMap<String, NodeFacts>,
    home: &AgentsHome,
    runner: Runner,
) -> Sample {
    let (head, commits, last_commit_at) = git_progress(cwd, runner);
    Sample {
        cost_usd: session_cost_exact(
            &home.otel_dir().join("otel.db"),
            &crate::paths::ledger_path(Path::new(cwd)),
            sid,
        ),
        head,
        commits,
        last_commit_at,
        touched_at: facts.get(node).and_then(|f| f.touched_at),
    }
}

/// OTel rows are exact cost; the transcript-parsed ledger estimate answers
/// only when no row exists for the session.
pub(crate) fn session_cost_exact(otel_db: &Path, ledger: &Path, sid: &str) -> Option<f64> {
    crate::otel_ingest::session_cost_usd(otel_db, sid)
        .or_else(|| Some(crate::loopcheck::session_cost_from_ledger(ledger, sid)))
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
    config_cwd: &Path,
    thresholds: &Thresholds,
) {
    let graph = crate::gc_sweep::graph_path(home);
    let store = crate::backlog::api::Store::new(&graph);
    let Ok(rows) = crate::graph_store::read_rows_where(
        &store.graph,
        &crate::backlog::RowQuery {
            fields: Some(
                [
                    "deferred_kind",
                    "id",
                    "type",
                    "parent",
                    "project",
                    "cwd",
                    "status",
                    "touched_at",
                    "sessions",
                    "pr_number",
                    "completed_at",
                    "superseded_by",
                    "deferred_at",
                ]
                .into_iter()
                .map(str::to_string)
                .collect(),
            ),
            with_blockers: true,
            ..Default::default()
        },
    )
    .map_err(|error| crate::backlog::api::ApiError(error.to_string())) else {
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
    let mut notes = 0u64;
    let mut skip: Option<String> = None;
    let mut sampled: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut owners: Option<HashMap<String, String>> = None;
    for (sid, scoped) in &scope {
        let node = scoped.node.as_str();
        let Some(entry) = live.get(sid) else { continue };
        let Some(cwd) = (!entry.cwd.is_empty()).then_some(entry.cwd.as_str()) else {
            continue;
        };
        // A node that left the open set is the reaper's question, not ours.
        if !facts
            .get(node)
            .map(|f| is_open_status(&f.status))
            .unwrap_or(false)
        {
            continue;
        }
        sampled.insert(sid.clone());
        let prev = load_state(home, sid);
        let sample = sample_session(sid, node, cwd, &facts, home, runner);
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
        // The capability note rides its own flag, copied from prev because
        // decide rebuilds the state and a stand-down must not clear it.
        next.escalation_noted = prev.as_ref().map(|p| p.escalation_noted).unwrap_or(false);
        if !next.escalation_noted {
            let counters = sample_counters(
                cwd,
                facts.get(node).unwrap_or(&NodeFacts::default()),
                runner,
                now_epoch,
            );
            let tripped = escalation_trips(&counters, thresholds);
            if !tripped.is_empty() {
                let harness = entry.harness.as_deref().unwrap_or("unknown");
                let model = scoped
                    .observed_model
                    .as_deref()
                    .or(entry.model.as_deref())
                    .or(entry.requested_model.as_deref())
                    .unwrap_or("unknown");
                let text = escalation_note_text(node, sid, harness, model, &counters, &tripped);
                let owner = node_owner(config_cwd, &home.registry_json(), &rows, &mut owners, node);
                note_owner(
                    home,
                    runner,
                    owner,
                    &text,
                    cwd,
                    node,
                    &format!("capability escalation for {node}"),
                    "skills/target/scripts/handoff.sh",
                );
                next.escalation_noted = true;
                notes += 1;
            }
        }
        // The first-edit note rides its own flag for the same reason.
        next.first_edit_noted = prev.as_ref().map(|p| p.first_edit_noted).unwrap_or(false);
        if !next.first_edit_noted {
            let harness = entry.harness.as_deref().unwrap_or("unknown");
            if let Some(text) = first_edit_overdue(
                node,
                sid,
                harness,
                cwd,
                scoped.started_at,
                thresholds.first_edit_minutes,
                runner,
                now_epoch,
            ) {
                let owner = node_owner(config_cwd, &home.registry_json(), &rows, &mut owners, node);
                note_owner(
                    home,
                    runner,
                    owner,
                    &text,
                    cwd,
                    node,
                    &format!("no first edit on {node}"),
                    &format!("fno agents logs {sid}"),
                );
                next.first_edit_noted = true;
                notes += 1;
            }
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
                        "row left the open execute scope",
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
        Some(&format!(
            "scope {} sessions, escalation notes {}",
            scope.len(),
            notes
        )),
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
        let thresholds = read_thresholds(&cwd);
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
        let mut runner: Runner = &mut run_command;
        run_pass(
            &home,
            &emitter,
            &mut runner,
            &claude_busy,
            crate::daemon::now_epoch_secs(),
            idle_s,
            spend_min,
            &cwd,
            &thresholds,
        );
    });
}

/// The `burn_watch.escalate_*` reads. A 0 turns the counter off; a negative
/// or wrong-typed value takes the default.
fn read_thresholds(cwd: &Path) -> Thresholds {
    let unsigned = |name: &str, default: u64| -> u64 {
        crate::agents_config::config_lookup(cwd, &["burn_watch", name])
            .and_then(|v| v.as_integer())
            .filter(|v| *v >= 0)
            .map(|v| v as u64)
            .unwrap_or(default)
    };
    let signed = |name: &str, default: i64| -> i64 {
        crate::agents_config::config_lookup(cwd, &["burn_watch", name])
            .and_then(|v| v.as_integer())
            .filter(|v| *v >= 0)
            .unwrap_or(default)
    };
    Thresholds {
        red_rounds: unsigned(
            "escalate_red_rounds",
            u64::from(ESCALATE_DEFAULT_RED_ROUNDS),
        ) as u32,
        conflict_merges: unsigned(
            "escalate_conflict_merges",
            u64::from(ESCALATE_DEFAULT_CONFLICT_MERGES),
        ) as u32,
        diff_lines: unsigned("escalate_diff_lines", ESCALATE_DEFAULT_DIFF_LINES),
        hours: signed("escalate_hours", ESCALATE_DEFAULT_HOURS),
        slot_wait_minutes: signed("escalate_slot_wait_minutes", ESCALATE_DEFAULT_SLOT_WAIT_MIN),
        first_edit_minutes: signed("first_edit_minutes", FIRST_EDIT_DEFAULT_MIN),
    }
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
            last_commit_at: touched,
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
    fn the_age_arm_fires_only_when_node_and_branch_are_both_stale() {
        let prev = BurnState {
            cost_usd: Some(1.0),
            head: Some("a".into()),
            commits: Some(1),
            touched_at: Some(1000),
            ..Default::default()
        };
        // Both aged past the ceiling: wake.
        let (d, _) = decide(
            Some(&prev),
            &sample(Some(1.0), Some("a"), Some(1000)),
            1000 + 7200,
            7200,
            0.01,
        );
        assert!(matches!(d, Decision::Wake(r) if r.contains("node untouched 2h")));
        // Regression shape: the role was woken twice over a node whose PR
        // branch took a commit 30 minutes earlier. The node row's touched_at
        // is not the branch: it moves on backlog commands, never on a push.
        // A flat sample whose newest commit is younger than the idle ceiling
        // stands down.
        let aged_prev = BurnState {
            last_commit_at: Some(1000),
            attempts: 1,
            ..prev
        };
        let now = 1000 + 7 * 3600; // the node row reads 7h stale
        let flat = Sample {
            cost_usd: Some(1.0),
            head: Some("a".into()),
            commits: Some(1),
            touched_at: Some(1000),
            last_commit_at: Some(now - 1800), // a commit landed 30 minutes ago
        };
        let (d, next) = decide(Some(&aged_prev), &flat, now, 7200, 0.01);
        assert_eq!(d, Decision::StandDown);
        assert_eq!(next.last_commit_at, Some(now - 1800));
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
        // First sighting records the sample and never fires.
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
    fn a_probe_that_died_carries_the_last_good_sample_and_holds() {
        let prev = BurnState {
            cost_usd: Some(1.0),
            head: Some("a".into()),
            commits: Some(1),
            touched_at: Some(1000),
            last_commit_at: Some(1000),
            attempts: 3,
            escalated: true,
            task_key: Some("burning worker on x-1".into()),
            task_cwd: Some("/w".into()),
            ..Default::default()
        };
        // git and the ledger die this pass; the graph probe still reads.
        let sample = Sample {
            cost_usd: None,
            head: None,
            commits: None,
            touched_at: Some(1000),
            last_commit_at: None,
        };
        let (d, next) = decide(Some(&prev), &sample, 2000, 1000, 0.01);
        assert_eq!(d, Decision::Hold);
        assert_eq!(next.attempts, 3);
        assert!(next.escalated);
        assert_eq!(next.head.as_deref(), Some("a"));
        assert_eq!(next.cost_usd, Some(1.0));
        assert_eq!(next.last_commit_at, Some(1000));
        assert_eq!(next.task_key.as_deref(), Some("burning worker on x-1"));
    }

    #[test]
    fn an_axis_appearing_after_a_dead_first_sight_is_progress() {
        let prev = BurnState {
            cost_usd: Some(1.0),
            head: None,
            commits: None,
            touched_at: Some(1000),
            last_commit_at: None,
            attempts: 2,
            ..Default::default()
        };
        let (d, next) = decide(
            Some(&prev),
            &sample(Some(1.0), Some("b"), Some(1000)),
            2000,
            7200,
            0.01,
        );
        assert_eq!(d, Decision::StandDown);
        assert_eq!(next.attempts, 0);
    }

    #[test]
    fn scope_reads_open_do_rows_and_open_nodes_only() {
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp();
        let rows = vec![
            json!({
                "id": "x-1", "status": "in_progress",
                "touched_at": "2026-09-16T17:04:00+00:00", "pr_number": 2842,
                "sessions": [
                    {"phase": "execute", "session_id": "s-1", "harness": "claude",
                     "started_at": "2026-09-16T09:00:00+00:00",
                     "observed_model": {"kind": "observed", "model": "glm-5.3-flash"}},
                    {"phase": "execute", "session_id": "s-4", "harness": "claude",
                     "started_at": "2026-09-16T07:00:00+00:00",
                     "ended_at": "2026-09-16T08:00:00+00:00"},
                    {"phase": "blueprint", "session_id": "s-3", "harness": "claude",
                     "started_at": "2026-09-16T06:00:00+00:00"}
                ]
            }),
            json!({
                "id": "x-2", "status": "done",
                "touched_at": "2026-09-16T17:04:00+00:00",
                "sessions": [
                    {"phase": "execute", "session_id": "s-2", "harness": "claude",
                     "started_at": "2026-09-16T09:00:00+00:00"}
                ]
            }),
            json!({"id": "x-3", "status": "in_progress", "touched_at": "garbage"}),
        ];
        let scope = scope_sessions(&rows);
        assert_eq!(scope.get("s-1").map(|s| s.node.as_str()), Some("x-1"));
        assert_eq!(
            scope["s-1"].observed_model.as_deref(),
            Some("glm-5.3-flash")
        );
        assert!(
            !scope.contains_key("s-2"),
            "rows on done nodes are out of scope"
        );
        assert!(
            !scope.contains_key("s-3"),
            "blueprint rows are out of scope"
        );
        assert!(!scope.contains_key("s-4"), "ended rows are out of scope");
        let facts = node_facts(&rows);
        assert_eq!(facts["x-1"].touched_at, Some(1_789_578_240));
        assert_eq!(
            facts["x-3"].touched_at, None,
            "a garbage stamp is None, never a guess"
        );
        assert_eq!(facts["x-1"].status, "in_progress");
        assert_eq!(facts["x-1"].pr_number, Some(2842));
        assert_eq!(
            facts["x-1"].first_execute_at,
            Some(at("2026-09-16T07:00:00+00:00")),
            "the ended row's earlier start"
        );
    }

    #[test]
    fn a_refused_mail_falls_back_to_the_resume() {
        // One table over the two mail answers: a durable receipt never types
        // a resume over a busy turn; a refused send falls back to the resume.
        for (code, stdout, busy, expect_calls, expect_landed, expect_via) in [
            (
                0,
                "queued (durable): will deliver at the next turn",
                true,
                1,
                false,
                "durable",
            ),
            (1, "", false, 2, true, "resume"),
        ] {
            let mut calls: Vec<Vec<String>> = Vec::new();
            let mut runner: Runner = &mut |argv: &[String], _cwd: &str| {
                calls.push(argv.to_vec());
                if argv[2] == "mail" {
                    (code, stdout.into(), String::new())
                } else {
                    (0, String::new(), String::new())
                }
            };
            let (landed, via) = wake("s-1", "x-1", busy, "spend grew", &mut runner);
            assert_eq!(landed, expect_landed);
            assert_eq!(via, expect_via);
            assert_eq!(calls.len(), expect_calls);
        }

        let watch = crate::watch_expiry::Watch {
            event_id: "watch-1".into(),
            seq: 1,
            session_id: "s-1".into(),
            pr: None,
            node: "x-1".into(),
            blocker: "local".into(),
            task_id: Some("task-7".into()),
            reason: Some("local build".into()),
            expires_at_ms: 100,
            ts_ms: 50,
        };
        assert!(!crate::watch_expiry::should_wake(&watch, 99, &[]));
        assert!(crate::watch_expiry::should_wake(&watch, 100, &[]));
        let paired_idle = crate::watch_expiry::Evidence {
            event_id: "loop-check-watch".into(),
            seq: 2,
            ts_ms: 51,
            kind: "loop_check".into(),
            session_id: Some("s-1".into()),
            data: serde_json::json!({"intent": "watching"}),
        };
        assert!(crate::watch_expiry::should_wake(
            &watch,
            100,
            &[paired_idle]
        ));
        let acted = crate::watch_expiry::Evidence {
            event_id: "activity-1".into(),
            seq: 2,
            ts_ms: 101,
            kind: "loop_check".into(),
            session_id: Some("s-1".into()),
            data: serde_json::json!({"intent": "plain"}),
        };
        assert!(!crate::watch_expiry::should_wake(&watch, 101, &[acted]));
        let renewed = crate::watch_expiry::Evidence {
            event_id: "watch-2".into(),
            seq: 3,
            ts_ms: 102,
            kind: "loop_check_watch_idle".into(),
            session_id: Some("s-1".into()),
            data: serde_json::json!({}),
        };
        assert!(!crate::watch_expiry::should_wake(&watch, 102, &[renewed]));
        let receipt = crate::watch_expiry::Evidence {
            event_id: "wake-1".into(),
            seq: 4,
            ts_ms: 103,
            kind: "loop_check_watch_expiry_wake".into(),
            session_id: Some("s-1".into()),
            data: serde_json::json!({"watch_event_id": "watch-1"}),
        };
        assert!(crate::watch_expiry::is_current_watch(
            &watch,
            &[receipt.clone()]
        ));
        assert!(!crate::watch_expiry::should_wake(&watch, 103, &[receipt]));

        let mut review_watch = watch.clone();
        review_watch.reason = Some("review".into());
        let review_message = crate::watch_expiry::message(&review_watch);
        assert!(!review_message.contains("local watch expired"));
        assert!(review_message.contains("Your review watch expired"));
        assert!(review_message.contains("reason: review"));

        let _env_guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        struct RestoreClaimsRoot(Option<std::ffi::OsString>);
        impl Drop for RestoreClaimsRoot {
            fn drop(&mut self) {
                if let Some(value) = self.0.take() {
                    std::env::set_var("FNO_CLAIMS_ROOT", value);
                } else {
                    std::env::remove_var("FNO_CLAIMS_ROOT");
                }
            }
        }
        let _restore_claims_root = RestoreClaimsRoot(std::env::var_os("FNO_CLAIMS_ROOT"));
        let temp = tempfile::tempdir().unwrap();
        let claims_root = temp.path().join("claims-root");
        std::fs::create_dir_all(&claims_root).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        let home = crate::paths::AgentsHome::at(temp.path().join("agents"));
        std::fs::create_dir_all(home.root()).unwrap();
        let mut legacy_entry = crate::state::RegistryEntry::default();
        legacy_entry.name = "legacy-watch-owner".into();
        legacy_entry.session_id = Some("s-legacy".into());
        legacy_entry.status = crate::AgentStatus::Live;
        let mut registry = crate::state::Registry::default();
        registry.entries.push(legacy_entry.clone());
        crate::registry_store::seed_raw(
            &home.registry_json(),
            serde_json::to_vec(&registry).unwrap(),
        );
        let acquired = crate::claims::acquire(
            "node:x-legacy",
            "target-session:s-legacy",
            crate::claims::AcquireOpts {
                pid: Some(std::process::id()),
                identity: Some(("s-legacy".into(), "codex".into())),
                root: None,
                events_dir: Some(temp.path().join("claim-events")),
                ..Default::default()
            },
        );
        let legacy_claims = crate::watch_expiry::current_node_claims(&home).unwrap();
        let legacy_owner = legacy_claims.get("s-legacy").cloned().unwrap();

        let global_events = crate::daemon::global_events_path(&home);
        std::fs::write(
            &global_events,
            format!(
                "{}\n",
                serde_json::json!({
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "type": "loop_check_watch_idle",
                    "source": "hook",
                    "data": {
                        "session_id": "s-legacy",
                        "node": "x-legacy",
                        "blocker": "local",
                        "reason": "local build",
                        "task_id": "task-7",
                        "expires_at_ms": 0,
                        "lease_ms": 1000
                    }
                })
            ),
        )
        .unwrap();

        let overdue = crate::watch_expiry::overdue(&home).unwrap();
        assert_eq!(overdue.len(), 1);
        assert_eq!(overdue[0].session_id, "s-legacy");
        assert!(overdue[0].overdue_ms > 0);

        // A genuine watch woken before recovery keeps its receipt: the copy
        // batch must not change the identity that receipt names.
        let mut genuine_entry = crate::state::RegistryEntry::default();
        genuine_entry.name = "genuine-watch-owner".into();
        genuine_entry.harness_session_id = Some("s-genuine".into());
        genuine_entry.status = crate::AgentStatus::Live;
        registry.entries.push(genuine_entry);
        crate::registry_store::seed_raw(
            &home.registry_json(),
            serde_json::to_vec(&registry).unwrap(),
        );
        let acquired_genuine = crate::claims::acquire(
            "node:x-genuine",
            "target-session:s-genuine",
            crate::claims::AcquireOpts {
                pid: Some(std::process::id()),
                identity: Some(("s-genuine".into(), "claude".into())),
                root: None,
                events_dir: Some(temp.path().join("claim-events")),
                ..Default::default()
            },
        );
        assert!(matches!(
            acquired_genuine,
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        let genuine_watch = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "type": "loop_check_watch_idle",
            "source": "hook",
            "data": {
                "session_id": "s-genuine",
                "node": "x-genuine",
                "blocker": "ci",
                "expires_at_ms": 0
            }
        })
        .to_string();
        std::io::Write::write_all(
            &mut std::fs::OpenOptions::new()
                .append(true)
                .open(&global_events)
                .unwrap(),
            format!("{genuine_watch}\n").as_bytes(),
        )
        .unwrap();
        crate::events::EventEmitter::new(global_events.clone(), "daemon")
            .emit(
                crate::watch_expiry::WAKE_EVENT,
                &serde_json::json!({
                    "session_id": "s-genuine",
                    "node": "x-genuine",
                    "watch_event_id": format!(
                        "sha256:{:x}",
                        <sha2::Sha256 as sha2::Digest>::digest(genuine_watch.as_bytes())
                    ),
                    "blocker": "ci",
                    "task_id": null,
                    "expires_at_ms": 0,
                    "delivered": true,
                    "via": "mail",
                    "reason": "watch deadline expired"
                }),
            )
            .unwrap();

        // Copied-store replay contract: a watch row known only through the
        // recovered store must never wake, while a fresh live watch still
        // wakes exactly once.
        crate::event_store::sync(&global_events).unwrap();
        let replay_store = crate::event_store::store_path(&global_events);
        let db = rusqlite::Connection::open(&replay_store).unwrap();
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS recovery_history(event_id TEXT PRIMARY KEY, batch TEXT NOT NULL);\n             INSERT INTO recovery_history SELECT event_id, 'copy-batch' FROM events WHERE session_id = 's-legacy';",
        )
        .unwrap();
        drop(db);
        let mut fresh_entry = crate::state::RegistryEntry::default();
        fresh_entry.name = "fresh-watch-owner".into();
        fresh_entry.harness_session_id = Some("s-fresh".into());
        fresh_entry.status = crate::AgentStatus::Live;
        registry.entries.push(fresh_entry);
        crate::registry_store::seed_raw(
            &home.registry_json(),
            serde_json::to_vec(&registry).unwrap(),
        );
        let acquired_fresh = crate::claims::acquire(
            "node:x-fresh",
            "target-session:s-fresh",
            crate::claims::AcquireOpts {
                pid: Some(std::process::id()),
                identity: Some(("s-fresh".into(), "claude".into())),
                root: None,
                events_dir: Some(temp.path().join("claim-events")),
                ..Default::default()
            },
        );
        assert!(matches!(
            acquired_fresh,
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        let fresh_emitter = crate::events::EventEmitter::new(global_events.clone(), "hook");
        fresh_emitter
            .emit(
                "loop_check_watch_idle",
                &serde_json::json!({
                    "session_id": "s-fresh",
                    "node": "x-fresh",
                    "pr": null,
                    "blocker": "ci",
                    "lease_ms": 60_000,
                    "expires_at_ms": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64
                        - 1_000,
                }),
            )
            .unwrap();
        let mut replay_mails: Vec<Vec<String>> = Vec::new();
        let woke = {
            let replay_runner: crate::burn_watch::Runner = &mut |argv: &[String], _: &str| {
                if argv.len() > 2 && argv[2] == "mail" {
                    replay_mails.push(argv.to_vec());
                }
                (0, "msg-1 delivered (hosted)".to_string(), String::new())
            };
            crate::watch_expiry::run_pass_with(&home, replay_runner).unwrap();
            crate::watch_expiry::run_pass_with(&home, replay_runner).unwrap();
            replay_mails
                .iter()
                .map(|argv| argv[8].clone())
                .collect::<Vec<String>>()
        };
        assert!(
            !woke.contains(&"s-genuine".to_string()),
            "a receipt predating recovery_history still names its genuine watch"
        );
        assert_eq!(
            woke,
            vec!["s-fresh"],
            "copied-store replay: recovered history stays silent, only the fresh watch wakes once"
        );
        let replay_text =
            crate::event_store::journal_text(&global_events, &["loop_check_watch_idle"]);
        assert!(
            replay_text.contains("s-legacy"),
            "recovered watch history stays readable"
        );

        // The error-path sections below need a due watch to reach the claims
        // read; the passes above consumed the replayed watches' receipts.
        let errorpath_emitter = crate::events::EventEmitter::new(global_events.clone(), "hook");
        errorpath_emitter
            .emit(
                "loop_check_watch_idle",
                &serde_json::json!({
                    "session_id": "s-fresh",
                    "node": "x-fresh",
                    "pr": null,
                    "blocker": "ci",
                    "lease_ms": 60_000,
                    "expires_at_ms": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as i64
                        - 1_000,
                }),
            )
            .unwrap();

        std::fs::remove_file(home.registry_json()).unwrap();
        std::fs::create_dir(home.registry_json()).unwrap();
        let registry_error = crate::watch_expiry::run_pass(&home);
        std::fs::remove_dir(home.registry_json()).unwrap();

        legacy_entry.harness_session_id = Some("s-legacy".into());
        registry.entries = vec![legacy_entry];
        crate::registry_store::seed_raw(
            &home.registry_json(),
            serde_json::to_vec(&registry).unwrap(),
        );
        let broken_claims_root = temp.path().join("claims-root-file");
        std::fs::write(&broken_claims_root, "unreadable claims root").unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &broken_claims_root);
        let claims_error = crate::watch_expiry::run_pass(&home);

        assert!(matches!(
            acquired,
            crate::claims::AcquireOutcome::Acquired(_)
        ));
        assert_eq!(legacy_owner, Ok(Some("x-legacy".into())));
        assert!(
            registry_error.is_err(),
            "registry read failure must surface"
        );
        assert!(claims_error.is_err(), "claim read failure must surface");
    }

    #[test]
    fn red_rounds_count_failed_heads_per_workflow_and_never_a_passed_rerun() {
        let run = |name: &str, sha: &str, status: &str, conclusion: &str, attempt: u32| {
            json!({
                "name": name, "head_sha": sha, "status": status,
                "conclusion": conclusion, "run_attempt": attempt
            })
        };
        // PR 2428: cli-ci failed on 3 heads, attempt 1 each.
        let pr2428 = vec![
            run("cli-ci", "04d227cb0e", "completed", "failure", 1),
            run("cli-ci", "79471b2e86", "completed", "failure", 1),
            run("cli-ci", "af26bd1c9e", "completed", "failure", 1),
        ];
        assert_eq!(red_rounds(&pr2428), Some(("cli-ci".into(), 3)));
        // A live listing: cli-ci on 4 heads, guards on 2, internal-refs on 1.
        let x59e1 = vec![
            run("cli-ci", "310291d161", "completed", "failure", 1),
            run("cli-ci", "fcfe590eae", "completed", "failure", 1),
            run("cli-ci", "5cb6b3d028", "completed", "failure", 2),
            run("cli-ci", "7c3191a5dc", "completed", "failure", 1),
            run("guards", "g1", "completed", "failure", 1),
            run("guards", "g2", "completed", "failure", 1),
            run("internal-refs", "i1", "completed", "failure", 7),
        ];
        assert_eq!(red_rounds(&x59e1), Some(("cli-ci".into(), 4)));
        // Tonight's flake: the run left the failure listing once its second
        // attempt passed; a listing carrying the passed rerun drops it too.
        let flake = vec![run("guards", "f5c155fb6e", "completed", "success", 2)];
        assert_eq!(red_rounds(&flake), None);
        assert_eq!(red_rounds(&[]), None);
        assert_eq!(
            red_rounds(&[run("w", "s", "completed", "cancelled", 1)]),
            None
        );
    }

    /// One scenario: a fresh tree, a store row of the real shape, a team
    /// over `proj-a`, a staged world, one run_pass.
    #[allow(clippy::type_complexity)]
    fn escalation_scenario(
        trip: &str,
        thresholds: Thresholds,
        mail_ok: bool,
        passes: u32,
    ) -> (
        Vec<Vec<String>>,
        crate::paths::AgentsHome,
        tempfile::TempDir,
        Vec<crate::fleet_task::Task>,
    ) {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                for (key, value) in self.0.iter() {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
        let _restore = RestoreEnv(vec![
            ("FNO_STATE_DIR", std::env::var_os("FNO_STATE_DIR")),
            ("FNO_CLAIMS_ROOT", std::env::var_os("FNO_CLAIMS_ROOT")),
            ("FNO_AGENTS_HOME", std::env::var_os("FNO_AGENTS_HOME")),
        ]);
        let td = tempfile::tempdir().unwrap();
        let state_dir = td.path().join("state");
        std::fs::create_dir_all(state_dir.join("locks")).unwrap();
        std::env::set_var("FNO_STATE_DIR", &state_dir);
        let claims_root = td.path().join("claims");
        let waiters = claims_root.join(".fno/claim-aux/build-waiters");
        std::fs::create_dir_all(&waiters).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        let workdir = td.path().join("workdir");
        std::fs::create_dir_all(&workdir).unwrap();
        let workdir = std::fs::canonicalize(&workdir).unwrap();
        let now_ms = crate::claims::now_ms();
        let now_epoch = now_ms / 1000;
        if trip == "slot" {
            // The holder must predate the marker: anchor the marker at pid 1's
            // real creation, because a fresh CI runner is younger than any
            // fixed 70-minute offset. The wait then reads the host's uptime
            // past the row's one-minute threshold on any machine.
            let since_ms = match crate::claims::probe_pid(1) {
                crate::claims::PidProbe::Created(created) => created,
                _ => now_epoch * 1000 - 70 * 60_000,
            };
            std::fs::write(
                waiters.join(format!(
                    "{}.json",
                    crate::claims::encode_key(&workdir.to_string_lossy())
                )),
                serde_json::to_string(&json!({
                    "pid": 1,
                    "since_ms": since_ms,
                    "holder": "cargo:/other:42"
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let home = crate::paths::AgentsHome::at(td.path().join("agents"));
        std::fs::create_dir_all(home.root()).unwrap();
        std::env::set_var("FNO_AGENTS_HOME", home.root());
        let store = crate::backlog::api::Store::new(&crate::gc_sweep::graph_path(&home));
        crate::backlog::api::node_create(
            &store,
            crate::backlog::api::NodeCreateInput {
                id: "x-esc".into(),
                title: "burn me".into(),
                status: Some("in_progress".into()),
                project: Some("proj-a".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let started = chrono::DateTime::from_timestamp(
            now_ms / 1000 - if trip == "hours" { 25 * 3600 } else { 3600 },
            0,
        )
        .unwrap()
        .to_rfc3339();
        assert!(crate::backlog::api::session_open_parked(
            &store,
            "x-esc",
            "execute",
            "claude",
            "5e5c-aaaa-bbbb-cccc-000000000001",
            None,
            None,
            &started
        )
        .unwrap());
        crate::backlog::api::pull_request_attach(
            &store,
            "x-esc",
            crate::backlog::api::PullRequestInput {
                number: 2842,
                url: None,
                note: None,
            },
        )
        .unwrap();
        let proj = td.path().join("proj");
        std::fs::create_dir_all(proj.join(".fno")).unwrap();
        std::fs::write(
            proj.join(".fno/config.toml"),
            "[work.workspaces.ws]\n[[work.workspaces.ws.projects]]\nname = \"proj-a\"\n",
        )
        .unwrap();
        let mut team = crate::state::RegistryEntry::default();
        team.name = "team-1".into();
        team.status = crate::AgentStatus::Live;
        team.pid = Some(std::process::id());
        team.harness = Some("claude".into());
        team.role_scope = Some("proj-a".into());
        team.role_level = Some(1);
        team.harness_session_id = Some("5e5c-aaaa-bbbb-cccc-000000000001".into());
        team.cwd = workdir.to_string_lossy().into_owned();
        team.model = Some("glm-5.3-flash".into());
        let mut registry = crate::state::Registry::default();
        registry.entries.push(team);
        crate::registry_store::seed_raw(
            &home.registry_json(),
            serde_json::to_vec(&registry).unwrap(),
        );
        let mut calls: Vec<Vec<String>> = Vec::new();
        let mut runner_inner = |argv: &[String], _cwd: &str| -> (i32, String, String) {
            calls.push(argv.to_vec());
            let joined = argv.join(" ");
            if argv[0] == "gh" {
                let listing = if trip == "red" {
                    format!(
                        "{{\"workflow_runs\": [{}]}}",
                        ["h1", "h2", "h3"]
                            .iter()
                            .map(|h| format!(
                                "{{\"name\":\"cli-ci\",\"head_sha\":\"{h}\",\"status\":\"completed\",\"conclusion\":\"failure\"}}"
                            ))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                } else {
                    "{\"workflow_runs\": []}".to_string()
                };
                return (0, listing, String::new());
            }
            if argv[0] == "fno" && argv[2] == "mail" && joined.contains("--to-lead") {
                return if mail_ok {
                    (0, "delivered (hosted)\n".into(), String::new())
                } else {
                    (1, String::new(), String::new())
                };
            }
            if joined.contains("rev-parse --abbrev-ref HEAD") {
                return (0, "feature/x-esc\n".into(), String::new());
            }
            if joined.contains("rev-parse --abbrev-ref origin/HEAD") {
                return (0, "origin/main\n".into(), String::new());
            }
            if joined.contains("rev-list --merges") {
                return if trip == "conflict" {
                    (0, "aaaa1111\nbbbb2222\n".into(), String::new())
                } else {
                    (0, String::new(), String::new())
                };
            }
            if joined.contains("rev-list --count origin/main..HEAD") {
                return if trip == "first_edit" {
                    (0, "0\n".into(), String::new())
                } else {
                    (0, String::new(), String::new())
                };
            }
            if joined.contains("show --remerge-diff") {
                return (0, "src/lib.rs\n".into(), String::new());
            }
            if joined.contains("diff --shortstat") {
                return if trip == "diff" {
                    (
                        0,
                        " 3 files changed, 1898 insertions(+), 612 deletions(-)\n".into(),
                        String::new(),
                    )
                } else {
                    (0, String::new(), String::new())
                };
            }
            (0, String::new(), String::new())
        };
        let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "test");
        for _ in 0..passes {
            let mut runner: Runner = &mut runner_inner;
            run_pass(
                &home,
                &emitter,
                &mut runner,
                &|_| false,
                now_ms / 1000,
                7200,
                0.01,
                &proj,
                &thresholds,
            );
        }
        // The questions store routes through the state-root layout, so the
        // task read must run under the same pinned env the write ran under.
        let tasks = crate::fleet_task::open_tasks(&crate::provider_cap::questions_path(&home))
            .unwrap_or_default();
        (calls, home, td, tasks)
    }

    #[test]
    fn a_worker_with_no_edit_past_the_deadline_gets_one_note_to_its_lead() {
        let (calls, home, _td, tasks) =
            escalation_scenario("first_edit", Thresholds::default(), true, 2);
        let notes: Vec<_> = calls
            .iter()
            .filter(|a| a.join(" ").contains("--to-lead"))
            .collect();
        assert_eq!(notes.len(), 1, "one note across two passes");
        let text = notes[0].last().unwrap();
        assert!(
            text.contains(
                "Stuck: node x-esc, session 5e5c-aaaa-bbbb-cccc-000000000001 on claude has made no edit 60m"
            ),
            "{text}"
        );
        assert!(tasks.is_empty(), "{tasks:?}");
        let state = load_state(&home, "5e5c-aaaa-bbbb-cccc-000000000001").expect("state written");
        assert!(state.first_edit_noted);
        assert!(!state.escalation_noted);
        // A zero deadline turns the arm off, and a listing that cannot prove
        // the tree empty (the other scenarios print nothing) never fires it.
        for (trip, thresholds) in [
            (
                "first_edit",
                Thresholds {
                    first_edit_minutes: 0,
                    ..Thresholds::default()
                },
            ),
            ("nothing", Thresholds::default()),
        ] {
            let (calls, _home, _td, _tasks) = escalation_scenario(trip, thresholds, true, 1);
            assert!(
                !calls.iter().any(|a| a.join(" ").contains("--to-lead")),
                "{trip}"
            );
        }
    }

    #[test]
    fn escalation_trips_once_and_reaches_the_team_or_the_board() {
        // The pure trip shape keeps the exact example: a 70-minute wait
        // against the 60-minute default.
        let trips = escalation_trips(
            &Counters {
                slot_wait_min: Some(70),
                ..Counters::default()
            },
            &Thresholds::default(),
        );
        assert_eq!(
            trips,
            vec!["70m waiting at the cargo build door (threshold 60m)".to_string()]
        );
        // Each threshold trips alone; the note reaches the team. The slot
        // row trips at one minute because the marker anchors at the holder's
        // real creation (a fresh CI runner is younger than a fixed offset).
        for (trip, phrase, thresholds) in [
            (
                "red",
                "cli-ci red on 3 heads (threshold 3)",
                Thresholds::default(),
            ),
            (
                "conflict",
                "2 merges of the base needed a resolution (threshold 2)",
                Thresholds::default(),
            ),
            (
                "diff",
                "2510 changed lines against the base (threshold 2500)",
                Thresholds::default(),
            ),
            (
                "hours",
                "25h on the node (threshold 24h)",
                Thresholds::default(),
            ),
            (
                "slot",
                "waiting at the cargo build door (threshold 1m)",
                Thresholds {
                    slot_wait_minutes: 1,
                    ..Thresholds::default()
                },
            ),
        ] {
            let (calls, home, _td, tasks) = escalation_scenario(trip, thresholds, true, 1);
            let notes: Vec<_> = calls
                .iter()
                .filter(|a| a.join(" ").contains("--to-lead"))
                .collect();
            assert_eq!(notes.len(), 1, "{trip}: one note");
            let text = notes[0].last().unwrap();
            assert!(
                text.contains(
                    "node x-esc, session 5e5c-aaaa-bbbb-cccc-000000000001 on claude / glm-5.3-flash"
                ),
                "{trip}: {text}"
            );
            assert!(text.contains(phrase), "{trip}: {text}");
            // An accepted --to-lead send files no task.
            assert!(tasks.is_empty(), "{trip}: {tasks:?}");
            let state =
                load_state(&home, "5e5c-aaaa-bbbb-cccc-000000000001").expect("state written");
            assert!(state.escalation_noted, "{trip}");
        }
        // Nothing trips: no note, no task, no flag.
        {
            let (calls, home, _td, tasks) =
                escalation_scenario("nothing", Thresholds::default(), true, 1);
            assert!(!calls.iter().any(|a| a.join(" ").contains("--to-lead")));
            assert!(tasks.is_empty());
            assert!(
                !load_state(&home, "5e5c-aaaa-bbbb-cccc-000000000001")
                    .unwrap()
                    .escalation_noted
            );
        }
        // A zero threshold turns the counter off.
        {
            let (calls, _home, _td, tasks) = escalation_scenario(
                "hours",
                Thresholds {
                    hours: 0,
                    ..Thresholds::default()
                },
                true,
                1,
            );
            assert!(!calls.iter().any(|a| a.join(" ").contains("--to-lead")));
            assert!(tasks.is_empty());
        }
        // A refused send files exactly one fleet task.
        {
            let (calls, _home, _td, tasks) =
                escalation_scenario("hours", Thresholds::default(), false, 1);
            let notes: Vec<_> = calls
                .iter()
                .filter(|a| a.join(" ").contains("--to-lead"))
                .collect();
            assert_eq!(notes.len(), 1);
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].key, "capability escalation for x-esc");
            assert_eq!(
                tasks[0].run, "skills/target/scripts/handoff.sh",
                "the run line is the handoff script"
            );
            assert_eq!(tasks[0].node, "x-esc");
        }
        // A second pass sends nothing more: the flag is sticky across passes.
        {
            let (calls, _home, _td, _tasks) =
                escalation_scenario("hours", Thresholds::default(), true, 2);
            let notes: Vec<_> = calls
                .iter()
                .filter(|a| a.join(" ").contains("--to-lead"))
                .collect();
            assert_eq!(notes.len(), 1, "the note is sent once across two passes");
        }
    }
}

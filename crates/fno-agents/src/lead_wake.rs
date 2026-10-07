//! The lead-wake arm: a lead whose journaled check-in beat is one minute
//! past overdue gets woken by the daemon (the mail lane; a codex thread
//! receives it as turn/start), and the lead one rung up is told. The arm
//! writes the `lead_wake` tick rows the status table read UNOBSERVED before,
//! and a `lead_wake` journal event receipts each wake, so a lead woken
//! inside one beat is never woken twice for the same miss.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::paths::AgentsHome;
use crate::territory::Team;

/// The arm's own beat, matching its `KNOWN_ARMS` row.
pub const LEAD_WAKE_INTERVAL_S: u64 = 300;

/// One minute past the missed beat: the wake line is
/// `lead.checkin_interval` (default 55m) plus this.
pub const WAKE_AFTER_SECS: i64 = 60;

/// The mail sender name the wake and the rung-up notices carry.
const SENDER: &str = "fno-lead-wake";

/// How much of a team scope a notice shows before it elides.
const SCOPE_CAP: usize = 48;

/// One overdue lead and the rungs told beside it.
pub(crate) struct WakePlan<'a> {
    pub lead: &'a Team,
    pub idle_secs: i64,
    pub up: Vec<&'a Team>,
    pub down: Vec<&'a Team>,
    pub operator: bool,
}

fn scope_members(scope: &str) -> impl Iterator<Item = &str> {
    scope.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// The project a team roots: an L2 territory's graph project rides in
/// `projects` (scope -> project); every other rung roots at its own first
/// scope member.
pub(crate) fn team_project(team: &Team, projects: &BTreeMap<String, String>) -> Option<String> {
    if team.level == 2 {
        projects.get(&team.scope).cloned()
    } else {
        scope_members(&team.scope).next().map(str::to_string)
    }
}

/// The overdue set and who is told. A lead with no journaled beat has no
/// baseline, so it is skipped, never invented; a lead woken inside one beat
/// waits for its next miss.
pub(crate) fn plan_wakes<'a>(
    teams: &'a [Team],
    projects: &BTreeMap<String, String>,
    beats: &BTreeMap<String, Option<i64>>,
    wakes: &BTreeMap<String, i64>,
    beat_secs: i64,
    now: i64,
) -> Vec<WakePlan<'a>> {
    let wake_line = beat_secs + WAKE_AFTER_SECS;
    let mut out = Vec::new();
    for team in teams {
        let Some(last) = beats.get(&team.scope).copied().flatten() else {
            continue;
        };
        let idle_secs = now - last;
        if idle_secs < wake_line {
            continue;
        }
        if wakes.get(&team.scope).is_some_and(|w| now - w < beat_secs) {
            continue;
        }
        let level = team.level as i64;
        let up: Vec<&Team> = crate::owner_ladder::rung_up(teams, team);
        let down: Vec<&Team> = teams
            .iter()
            .filter(|t| {
                t.level as i64 == level + 1
                    && team_project(t, projects)
                        .is_some_and(|p| scope_members(&team.scope).any(|m| m == p))
            })
            .collect();
        // One rung up: a live lead session one level above; when none
        // exists the rung above is the unteamed human, so the operator
        // lane carries it at every level.
        let operator = up.is_empty();
        out.push(WakePlan {
            lead: team,
            idle_secs,
            up,
            down,
            operator,
        });
    }
    out
}

fn mins(secs: i64) -> i64 {
    secs / 60
}

fn short_scope(scope: &str) -> String {
    let mut text: String = scope.chars().take(SCOPE_CAP).collect();
    if scope.chars().count() > SCOPE_CAP {
        text.push_str("...");
    }
    text
}

pub(crate) fn wake_text(idle_secs: i64, beat_secs: i64, told: usize) -> String {
    format!(
        "Automatic lead-wake from the fno daemon: no check-in beat journaled for {}m \
         (the beat is {}m). Run your check-in now: fno agents org checkin. {}",
        mins(idle_secs),
        mins(beat_secs),
        if told > 0 {
            format!("{told} lead(s) one rung away were told.")
        } else {
            "No other lead was told.".to_string()
        },
    )
}

pub(crate) fn told_text(lead: &Team, idle_secs: i64, beat_secs: i64) -> String {
    format!(
        "Lead wake: {} (L{} {}) journaled no check-in for {}m; the beat is {}m. \
         The daemon woke it. No action needed unless it stays quiet.",
        lead.holder,
        lead.level,
        short_scope(&lead.scope),
        mins(idle_secs),
        mins(beat_secs),
    )
}

/// The arm's pass outcome, the same shape every tick arm reports.
pub(crate) struct Outcome {
    pub acted: u64,
    pub skip_reason: Option<String>,
    pub detail: String,
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_ts(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.timestamp())
}

/// The newest `lead_wake` receipt per live team scope: the dedupe memory.
/// Rows for scopes with no live team are ignored.
pub(crate) fn wake_receipts(
    journals: &[std::path::PathBuf],
    teams: &[Team],
) -> BTreeMap<String, i64> {
    let scopes: std::collections::HashSet<&str> = teams.iter().map(|t| t.scope.as_str()).collect();
    let mut out: BTreeMap<String, i64> = BTreeMap::new();
    for journal in journals {
        for line in crate::event_store::journal_text(journal, &["lead_wake"]).lines() {
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let (Some(scope), Some(ts)) = (
                row.get("data")
                    .and_then(|d| d.get("scope"))
                    .and_then(Value::as_str),
                row.get("ts").and_then(Value::as_str).and_then(parse_ts),
            ) else {
                continue;
            };
            if !scopes.contains(scope) {
                continue;
            }
            let entry = out.entry(scope.to_string()).or_insert(ts);
            *entry = (*entry).max(ts);
        }
    }
    out
}

/// The rung-2 scope -> graph project map the down-relation reads: one
/// territory resolve, degraded to an empty map when the graph is
/// unreadable (the down notices then match nothing, never invent).
fn junior_projects(config_cwd: &Path, registry: &Path) -> BTreeMap<String, String> {
    crate::territory::resolve_territories(config_cwd, registry)
        .map(|rows| {
            rows.into_iter()
                .filter(|t| t.rung == 2 && !t.project.is_empty())
                .map(|t| (t.key, t.project))
                .collect()
        })
        .unwrap_or_default()
}

/// One delivery over the shared runner: mail first, resume fallback on a
/// durable receipt (burn_watch's ladder, so a codex thread gets its
/// turn/start and a parked claude session is revived).
fn deliver(sid: &str, text: &str) -> bool {
    let runner: crate::burn_watch::Runner = &mut crate::burn_watch::run_command;
    let (ok, _lane) = crate::burn_watch::wake_with_text(sid, false, text, SENDER, runner);
    ok
}

fn notify_operator(title: &str, body: &str) -> bool {
    crate::operator_notice::notify_operator_confirmed(title, body, Some("fno agents status"))
}

/// The beat's cron act: a codex lead the wake reached gets its
/// resting goal resumed, so the beat is a work beat (check-in, then the
/// board) and not one turn that sleeps again. A claude lead, a stale
/// manifest, or an unreachable root is no signal, never an error; a provider
/// refusal rides the note, and the beat still happened.
fn resume_resting_goal(
    home: &AgentsHome,
    plan: &WakePlan,
    lead_session: &str,
    roots: &BTreeMap<String, String>,
) -> Option<String> {
    let (root, manifest) = beat_resume_target(plan, lead_session, roots)?;
    match crate::lead_goal::resume(&manifest, &root) {
        Ok(receipt) => {
            let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
            let payload = serde_json::json!({
                "session_id": lead_session,
                "scope": plan.lead.scope,
                "resumed": true,
                "successor": false,
                "reason": "beat",
                "provider_receipt": receipt,
            });
            if let Err(error) = emitter.emit("lead_goal_resumed", &payload) {
                eprintln!("lead-wake: goal resume receipt emit failed: {error}");
            }
            Some("resumed its resting goal".to_string())
        }
        Err(error) => Some(format!("goal resume refused: {error}")),
    }
}

/// The gate and the paths, no provider call: the piece a test can hold. The
/// lead manifest must name this codex harness and this exact live session;
/// anything else is a lead the beat wake already covers.
fn beat_resume_target(
    plan: &WakePlan,
    lead_session: &str,
    roots: &BTreeMap<String, String>,
) -> Option<(PathBuf, crate::lead_termination::LeadManifest)> {
    let root = roots.get(&plan.lead.holder).filter(|r| !r.is_empty())?;
    let manifest_path = crate::paths::space_dir(Path::new(root))
        .join("leads")
        .join(format!("{}.md", plan.lead.scope));
    let content = std::fs::read_to_string(&manifest_path).ok()?;
    let manifest = crate::lead_termination::parse_lead_manifest(&content)?;
    if manifest.harness.as_deref() != Some("codex")
        || manifest.harness_session_id.as_deref() != Some(lead_session)
    {
        return None;
    }
    Some((PathBuf::from(root), manifest))
}

/// One wake: deliver to the lead, tell the rungs, receipt the episode.
fn act_on_plan(
    home: &AgentsHome,
    plan: WakePlan,
    beat_secs: i64,
    roots: &BTreeMap<String, String>,
) -> (u64, String) {
    let told = plan.up.len() + plan.down.len();
    let mut acted = 0u64;
    let mut notes: Vec<String> = Vec::new();
    let lead_session = plan.lead.holder_session.clone().unwrap_or_default();
    if !lead_session.is_empty()
        && deliver(&lead_session, &wake_text(plan.idle_secs, beat_secs, told))
    {
        acted += 1;
        notes.push(format!("woke {}", plan.lead.holder));
        if let Some(note) = resume_resting_goal(home, &plan, &lead_session, roots) {
            notes.push(format!("{} {}", plan.lead.holder, note));
            acted += 1;
        }
    }
    let text = told_text(plan.lead, plan.idle_secs, beat_secs);
    for team in plan.up.iter().chain(plan.down.iter()) {
        let Some(sid) = team.holder_session.as_deref().filter(|s| !s.is_empty()) else {
            continue;
        };
        if deliver(sid, &text) {
            acted += 1;
            notes.push(format!("told {}", team.holder));
        }
    }
    if plan.operator && notify_operator("lead beat missed", &text) {
        acted += 1;
        notes.push("told operator".to_string());
    }
    emit_receipt(home, plan.lead, plan.idle_secs, beat_secs, acted);
    (acted, notes.join(", "))
}

fn emit_receipt(home: &AgentsHome, lead: &Team, idle_secs: i64, beat_secs: i64, acted: u64) {
    let emitter = crate::events::EventEmitter::new(home.events_jsonl(), "daemon");
    let payload = serde_json::json!({
        "scope": lead.scope,
        "level": lead.level,
        "holder_session": lead.holder_session,
        "idle_secs": idle_secs,
        "beat_secs": beat_secs,
        "acted": acted,
    });
    if let Err(error) = emitter.emit("lead_wake", &payload) {
        eprintln!("lead-wake: receipt emit failed: {error}");
    }
}

/// One pass: read the live teams, fold each lead's last beat and wake
/// receipt, wake what is past the line, and report the outcome.
pub(crate) fn run_pass(home: &AgentsHome, config_cwd: &Path) -> Result<Outcome, String> {
    let now = now_epoch();
    let registry = home.registry_json();
    let teams =
        crate::territory::live_teams(&registry).map_err(|e| format!("lead_wake: {}", e.0))?;
    if teams.is_empty() {
        return Ok(Outcome {
            acted: 0,
            skip_reason: Some("no_teams".to_string()),
            detail: "no live lead teams".to_string(),
        });
    }
    let beat_secs = crate::lead_verdict_inputs::checkin_interval_secs(config_cwd);
    let journals = crate::tick_ledger::journals(home);
    let mut beats: BTreeMap<String, Option<i64>> = BTreeMap::new();
    for team in &teams {
        let row = crate::lead_history::previous_beat(
            &journals,
            &team.scope,
            team.holder_session.as_deref(),
            false,
        )
        .ok()
        .flatten();
        let ts = row
            .as_ref()
            .and_then(|r| r.get("ts").and_then(Value::as_str))
            .and_then(parse_ts);
        beats.insert(team.scope.clone(), ts);
    }
    let wakes = wake_receipts(&journals, &teams);
    let projects = junior_projects(config_cwd, &registry);
    let plans = plan_wakes(&teams, &projects, &beats, &wakes, beat_secs, now);
    if plans.is_empty() {
        return Ok(Outcome {
            acted: 0,
            skip_reason: Some("none_overdue".to_string()),
            detail: format!(
                "{} lead(s) observed; none past the {}m line",
                teams.len(),
                mins(beat_secs)
            ),
        });
    }
    // holder -> repo root (project_root, else cwd): where the lead manifest
    // and the provider thread's cwd live, read once per pass, and only when
    // a wake actually plans (the quiet pass never pays the registry parse).
    let roots: BTreeMap<String, String> = crate::state::load_registry(&registry)
        .map(|loaded| {
            loaded
                .entries
                .into_iter()
                .map(|e| {
                    let root = if e.project_root.is_empty() {
                        e.cwd
                    } else {
                        e.project_root
                    };
                    (e.name, root)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut acted = 0u64;
    let mut notes: Vec<String> = Vec::new();
    for plan in plans {
        let (n, note) = act_on_plan(home, plan, beat_secs, &roots);
        acted += n;
        if !note.is_empty() {
            notes.push(note);
        }
    }
    Ok(Outcome {
        acted,
        skip_reason: None,
        detail: short(&notes.join("; ")),
    })
}

fn short(text: &str) -> String {
    text.chars().take(160).collect()
}

/// The arm as the daemon holds it: cadence stamp plus one-in-flight gate.
/// The config cwd rides at construction, so the thresholds
/// (`lead.checkin_interval`) and the territory resolve read the same root
/// the daemon declared.
pub struct Arm {
    config_cwd: PathBuf,
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

impl Arm {
    pub fn new(config_cwd: PathBuf) -> Self {
        Self {
            config_cwd,
            last_tick: Mutex::new(None),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }
}

pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    let interval = Duration::from_secs(LEAD_WAKE_INTERVAL_S);
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
    let config_cwd = arm.config_cwd.clone();
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let outcome = match run_pass(&home, &config_cwd) {
            Ok(outcome) => outcome,
            Err(error) => Outcome {
                acted: 0,
                skip_reason: Some("unreadable".to_string()),
                detail: short(&error),
            },
        };
        let journal = crate::loop_runtime::Journal::new_raw(
            home.events_jsonl(),
            crate::daemon::global_events_path(&home),
        );
        crate::tick_ledger::emit_tick(
            &journal,
            "lead_wake",
            crate::tick_ledger::SCHED_DAEMON,
            outcome.acted,
            outcome.skip_reason.as_deref(),
            Some(&outcome.detail),
            LEAD_WAKE_INTERVAL_S,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(scope: &str, level: u8, holder: &str, sid: &str) -> Team {
        Team {
            scope: scope.to_string(),
            level,
            holder: holder.to_string(),
            holder_session: Some(sid.to_string()),
        }
    }

    fn projects() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("x-aaa".to_string(), "fno".to_string()),
            ("x-bbb".to_string(), "fno".to_string()),
        ])
    }

    #[test]
    fn plan_wakes_reads_the_line_rungs_and_dedupe() {
        let teams = vec![
            team("fno", 1, "vellum", "s-head"),
            team("x-aaa", 2, "candor", "s-l2a"),
            team("x-bbb", 2, "finch", "s-l2b"),
        ];
        let now = 1_000_000_000 + 55 * 60 + 61;
        let beats = BTreeMap::from([
            ("fno".to_string(), Some(1_000_000_000)),
            ("x-aaa".to_string(), Some(now - 600)),
            ("x-bbb".to_string(), Some(now - 600)),
        ]);
        let plans = plan_wakes(&teams, &projects(), &beats, &BTreeMap::new(), 55 * 60, now);
        assert_eq!(plans.len(), 1, "only the L1 head is past the line");
        let plan = &plans[0];
        assert_eq!(plan.lead.holder, "vellum");
        assert!(plan.operator, "no L0 team: the operator lane carries it");
        assert!(plan.up.is_empty());
        assert_eq!(
            plan.down
                .iter()
                .map(|t| t.holder.as_str())
                .collect::<Vec<_>>(),
            vec!["candor", "finch"],
            "both project L2s are one rung down"
        );
        plan_wakes_sections_two();
    }

    // The fold side of the same planning contract: no baseline is skipped,
    // a lead woken inside one beat waits, and a fresh lead never plans.
    fn plan_wakes_sections_two() {
        let teams = vec![
            team("fno", 1, "vellum", "s-head"),
            team("x-aaa", 2, "candor", "s-l2a"),
            team("x-bbb", 2, "finch", "s-l2b"),
        ];
        let now = 1_000_000_000;
        // No baseline: skipped, never invented.
        let plans = plan_wakes(
            &teams,
            &projects(),
            &BTreeMap::new(),
            &BTreeMap::new(),
            3300,
            now,
        );
        assert!(plans.is_empty());
        // Woken inside one beat: waits for the next miss.
        let beats = BTreeMap::from([
            ("x-aaa".to_string(), Some(now - 4000)),
            ("x-bbb".to_string(), Some(now - 4000)),
        ]);
        let wakes = BTreeMap::from([("x-aaa".to_string(), now - 600)]);
        let plans = plan_wakes(&teams, &projects(), &beats, &wakes, 3300, now);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].lead.holder, "finch");
        assert_eq!(
            plans[0]
                .up
                .iter()
                .map(|t| t.holder.as_str())
                .collect::<Vec<_>>(),
            vec!["vellum"],
            "the L1 head is one rung up"
        );
    }

    #[test]
    fn receipts_fold_newest_per_scope_and_ignore_foreign_scopes() {
        let dir = temp_dir();
        let journal = dir.join("global.jsonl");
        let rows = r#"{"type":"lead_wake","ts":"2026-10-05T10:00:00Z","data":{"scope":"x-aaa"}}
{"type":"lead_wake","ts":"2026-10-05T11:00:00Z","data":{"scope":"x-aaa"}}
{"type":"lead_wake","ts":"2026-10-05T10:30:00Z","data":{"scope":"x-other"}}
{"type":"lead_checkin","ts":"2026-10-05T09:00:00Z","data":{"scope":"x-aaa"}}
"#;
        std::fs::write(&journal, rows).unwrap();
        let teams = vec![team("x-aaa", 2, "candor", "s-l2a")];
        let got = wake_receipts(&[journal], &teams);
        let epoch = chrono::DateTime::parse_from_rfc3339("2026-10-05T11:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(got.get("x-aaa"), Some(&epoch));
        assert!(!got.contains_key("x-other"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lead-wake-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wake_plan(lead: &Team) -> WakePlan<'_> {
        WakePlan {
            lead,
            idle_secs: 3_400,
            up: Vec::new(),
            down: Vec::new(),
            operator: false,
        }
    }

    fn write_beat_manifest(root: &Path, scope: &str, harness: &str, session: &str) {
        let dir = crate::paths::space_dir(root).join("leads");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{scope}.md")),
            format!(
                "---\nscope: {scope}\nharness: {harness}\nharness_session_id: {session}\n---\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn the_beat_resume_gate_admits_only_this_codex_sessions_manifest() {
        let _lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let spaces = tempfile::tempdir().unwrap();
        let _spaces = crate::claims::EnvVarGuard::set(
            "FNO_SPACES_DIR",
            spaces.path().to_str().expect("tempdir path is utf-8"),
        );
        let repo = tempfile::tempdir().unwrap();
        let lead = team("x-aaa", 2, "candor", "s-l2a");
        let plan = wake_plan(&lead);
        let roots = BTreeMap::from([("candor".to_string(), repo.path().display().to_string())]);
        // No manifest, a claude manifest, or another session's manifest:
        // all no signal.
        assert!(beat_resume_target(&plan, "s-l2a", &roots).is_none());
        write_beat_manifest(repo.path(), "x-aaa", "claude", "s-l2a");
        assert!(beat_resume_target(&plan, "s-l2a", &roots).is_none());
        write_beat_manifest(repo.path(), "x-aaa", "codex", "s-other");
        assert!(beat_resume_target(&plan, "s-l2a", &roots).is_none());
        // The woken codex session's own manifest: admitted, rooted at
        // the holder's repo.
        write_beat_manifest(repo.path(), "x-aaa", "codex", "s-l2a");
        let (root, manifest) =
            beat_resume_target(&plan, "s-l2a", &roots).expect("admits the woken codex lead");
        assert_eq!(root, repo.path());
        assert_eq!(manifest.scope, "x-aaa");
    }

    #[test]
    fn an_unknown_holder_or_empty_root_is_no_beat_resume_signal() {
        let _lock = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let spaces = tempfile::tempdir().unwrap();
        let _spaces = crate::claims::EnvVarGuard::set(
            "FNO_SPACES_DIR",
            spaces.path().to_str().expect("tempdir path is utf-8"),
        );
        let lead = team("x-aaa", 2, "stranger", "s-l2a");
        let plan = wake_plan(&lead);
        assert!(beat_resume_target(&plan, "s-l2a", &BTreeMap::new()).is_none());
        let roots = BTreeMap::from([("stranger".to_string(), String::new())]);
        assert!(beat_resume_target(&plan, "s-l2a", &roots).is_none());
    }
}

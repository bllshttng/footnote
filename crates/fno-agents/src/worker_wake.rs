//! Wake an ended Codex turn on an open node, once per quiet episode.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::paths::AgentsHome;
use crate::state::RegistryEntry;

const INTERVAL: Duration = Duration::from_secs(60);
const TAIL_BYTES: u64 = 256 * 1024;
const EVENT: &str = "worker_wake";

#[derive(Default)]
pub struct Arm {
    last_tick: Mutex<Option<Instant>>,
    in_flight: Arc<AtomicBool>,
}

#[derive(Default)]
struct Tail {
    last: Option<i64>,
    ended: bool,
    parked: Option<String>,
}

fn epoch(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn intentional_stop(text: &str) -> Option<String> {
    crate::loopcheck::declared_recovery_hold(text)
}

fn tail(path: &Path) -> Result<Tail, String> {
    let (_, cap, unknown) = crate::provider_cap::codex_capped_tail(path);
    if cap.is_some() {
        return Ok(Tail {
            parked: Some("provider_cap".into()),
            ..Default::default()
        });
    }
    if let Some(reason) = unknown.filter(|reason| reason != "no-assistant-entry-in-transcript") {
        return Err(reason);
    }
    let raw = crate::tail_text_strict(path, TAIL_BYTES).ok_or("invalid rollout encoding")?;
    let mut out = Tail::default();
    for line in raw.lines() {
        let row: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        if let Some(raw) = row.get("timestamp").and_then(Value::as_str) {
            out.last = epoch(raw);
        }
        let payload = &row["payload"];
        match (row["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("task_started" | "user_message"))
            | (Some("response_item"), Some("function_call" | "custom_tool_call")) => {
                out.ended = false;
                out.parked = None;
            }
            (Some("event_msg"), Some("task_complete")) => {
                out.ended = true;
                if let Some(text) = payload["last_agent_message"].as_str() {
                    out.parked = intentional_stop(text);
                }
            }
            (Some("event_msg"), Some("turn_aborted")) => out.ended = false,
            (Some("response_item"), Some("message")) => {
                if payload["role"] == "user" {
                    out.ended = false;
                    out.parked = None;
                } else if payload["role"] == "assistant" && payload["phase"] == "final_answer" {
                    out.ended = true;
                    let text = payload["content"]
                        .as_array()
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter_map(|b| b["text"].as_str())
                                .collect::<String>()
                        })
                        .unwrap_or_default();
                    out.parked = intentional_stop(&text);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

struct Pass<'a> {
    rows: &'a [RegistryEntry],
    nodes: &'a [Value],
    now: i64,
    dry_run: bool,
    liveness: &'a dyn Fn(&RegistryEntry) -> Life,
}

#[derive(Clone, Copy)]
enum Life {
    Resting,
    Dead,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Drive {
    TurnStart,
    Resume,
}

impl Drive {
    fn lane(self) -> &'static str {
        match self {
            Self::TurnStart => "codex_turn_start",
            Self::Resume => "resume",
        }
    }
}

#[derive(Clone, Copy)]
enum Claim {
    Own,
    Free,
    Held,
    Unreadable,
}

fn latest_worker(node: &Value) -> Option<&str> {
    node["sessions"]
        .as_array()?
        .iter()
        .rev()
        .find(|s| crate::graph_store::is_open_do_row(s))?["session_id"]
        .as_str()
}

fn report_skip(dry_run: bool, row: &RegistryEntry, reason: &str) {
    if dry_run {
        println!("worker-wake {}: skip {reason}", row.name);
    }
}

fn run_pass_with(
    home: &AgentsHome,
    pass: Pass<'_>,
    transcript: &dyn Fn(&RegistryEntry) -> Option<std::path::PathBuf>,
    claim_for: &dyn Fn(&str, &str) -> Claim,
    pr_for: &dyn Fn(&str, u64) -> Value,
    deliver: &dyn Fn(&str, Drive, &str) -> Result<(), String>,
    notify: &dyn Fn(&str, &str) -> bool,
) -> Result<u64, String> {
    let Pass {
        rows,
        nodes,
        now,
        dry_run,
        liveness,
    } = pass;
    let evidence = crate::watch_expiry::read_evidence(home, now)?;
    let receipts = crate::event_store::journal_text_checked(
        &home.events_jsonl(),
        &crate::event_store::EventQuery::of_types(&[EVENT]),
    )?;
    let mut acted = 0;
    let mut seen = HashSet::new();
    for row in rows {
        if row.harness_name() != "codex"
            || row.origin.as_deref() != Some("spawn")
            || row.role_level.is_some()
        {
            continue;
        }
        let disabled: Vec<String> = [
            ["recovery", "enabled"].as_slice(),
            ["autonomy", "enabled"].as_slice(),
        ]
        .iter()
        .filter(|keys| {
            crate::agents_config::config_lookup(Path::new(&row.cwd), keys).and_then(|v| v.as_bool())
                == Some(false)
        })
        .map(|keys| keys.join("."))
        .collect();
        if !disabled.is_empty() && !dry_run {
            continue;
        }
        let Some(sid) = row.harness_session_id.as_deref() else {
            report_skip(dry_run, row, "identity_missing");
            continue;
        };
        let watches = crate::codex_watch::settle_watches(&evidence);
        if let Some(w) = watches.iter().find(|w| {
            (w.watch.session_id == sid || w.codex_thread_id.as_deref() == Some(sid))
                && w.watch.expires_at_ms > now
                && crate::watch_expiry::is_current_watch(&w.watch, &evidence)
        }) {
            report_skip(
                dry_run,
                row,
                &format!("{}_watch node={}", w.watch.blocker, w.watch.node),
            );
            continue;
        }
        let candidates: Vec<&Value> = nodes
            .iter()
            .filter(|n| {
                matches!(
                    crate::graph_get::entry_status(n),
                    "in_progress" | "in_review" | "ready" | "next"
                ) && row
                    .node
                    .as_deref()
                    .map_or_else(|| latest_worker(n) == Some(sid), |node| n["id"] == node)
            })
            .collect();
        let [node_row] = candidates.as_slice() else {
            report_skip(dry_run, row, "node_missing_terminal_or_ambiguous");
            continue;
        };
        let Some(node) = node_row["id"].as_str() else {
            continue;
        };
        let Some(lead) = row
            .spawned_by_session
            .as_deref()
            .or_else(|| node_row["spawned_by_session"].as_str())
            .or_else(|| node_row["source_session_id"].as_str())
        else {
            report_skip(dry_run, row, "lead_unresolved");
            continue;
        };
        match claim_for(node, sid) {
            Claim::Own => {}
            Claim::Free if latest_worker(node_row) == Some(sid) => {}
            _ => {
                report_skip(dry_run, row, "claim_held_unreadable_or_worker_superseded");
                continue;
            }
        }
        let Some(path) = transcript(row) else {
            report_skip(dry_run, row, "transcript_missing");
            continue;
        };
        let tail = match tail(&path) {
            Ok(tail) => tail,
            Err(error) => {
                eprintln!("worker-wake: {}: rollout unreadable: {error}", row.name);
                report_skip(dry_run, row, "transcript_unreadable");
                continue;
            }
        };
        if let Some(reason) = tail.parked.as_deref() {
            report_skip(dry_run, row, reason);
            continue;
        }
        let Some(last) = tail.last else {
            report_skip(dry_run, row, "timestamp_unmeasured");
            continue;
        };
        let threshold = crate::agents_config::config_lookup(
            Path::new(&row.cwd),
            &["recovery", "idle_threshold_seconds"],
        )
        .and_then(|v| v.as_integer())
        .filter(|v| *v > 0)
        .unwrap_or(900);
        if tail.parked.is_some() || now.saturating_sub(last) < threshold.saturating_mul(1000) {
            report_skip(
                dry_run,
                row,
                tail.parked.as_deref().unwrap_or("not_overdue"),
            );
            continue;
        }
        if crate::graph_get::entry_status(node_row) == "in_review" {
            if let Some(pr) = node_row["pr_number"].as_u64().filter(|pr| *pr > 0) {
                let status = pr_for(&row.cwd, pr);
                let reason = match (
                    status["pr_state"].as_str(),
                    status["verdict"].as_str(),
                    status["settled"].as_bool(),
                ) {
                    (Some("OPEN"), Some("red"), Some(true)) => None,
                    (Some("MERGED" | "CLOSED"), _, _) => Some("pr_terminal"),
                    (Some("OPEN"), Some("green"), _) => Some("green_pr_or_grant_hold"),
                    (Some("OPEN"), _, Some(false)) | (Some("OPEN"), Some("pending"), _) => {
                        Some("ci_pending")
                    }
                    _ => Some("pr_status_unmeasured"),
                };
                if let Some(reason) = reason {
                    report_skip(dry_run, row, &format!("{reason} pr={pr}"));
                    continue;
                }
            }
        }
        let drive = match liveness(row) {
            Life::Dead => Drive::Resume,
            Life::Resting
                if tail.ended
                    && matches!(
                        row.status,
                        crate::AgentStatus::Ready
                            | crate::AgentStatus::Idle
                            | crate::AgentStatus::Live
                    ) =>
            {
                Drive::TurnStart
            }
            Life::Resting => {
                report_skip(dry_run, row, "turn_active");
                continue;
            }
            Life::Unknown => {
                report_skip(dry_run, row, "session_liveness_unmeasured");
                continue;
            }
        };
        let process_epoch = if drive == Drive::Resume {
            row.pid
                .map(|pid| format!("{pid}:{}", row.pid_start_time.unwrap_or(0)))
                .unwrap_or_default()
        } else {
            String::new()
        };
        if !seen.insert((sid.to_string(), last, drive, process_epoch.clone())) {
            report_skip(dry_run, row, "duplicate_episode");
            continue;
        }
        if receipts
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|r| {
                r["data"]["session_id"] == sid
                    && r["data"]["last_activity_ms"] == last
                    && r["data"]["via"] == drive.lane()
                    && r["data"]["process_epoch"].as_str().unwrap_or_default() == process_epoch
            })
        {
            report_skip(dry_run, row, "already_woken");
            continue;
        }
        if dry_run {
            println!(
                "worker-wake {}: recover node={node} quiet={}s via={} execution={} (dry-run)",
                row.name,
                (now - last) / 1000,
                drive.lane(),
                if disabled.is_empty() {
                    "ready".to_string()
                } else {
                    format!("held:{}=false", disabled.join(","))
                }
            );
            acted += 1;
            continue;
        }
        let text = format!("Automatic worker recovery: unfinished node {node} has been quiet for {}s. Continue the target from current evidence.", (now - last) / 1000);
        if let Err(error) = deliver(sid, drive, &text) {
            eprintln!("worker-wake: {}: {error}", row.name);
            continue;
        }
        crate::events::EventEmitter::new(home.events_jsonl(), "daemon")
            .emit(
                EVENT,
                &json!({
            "session_id": sid, "node": node, "last_activity_ms": last, "via": drive.lane(), "process_epoch": process_epoch
                }),
            )
            .map_err(|e| e.to_string())?;
        acted += 1;
        {
            let notice = format!(
                "Worker recovery: {} sent to {} on {node} after {}s idle.",
                drive.lane(),
                row.name,
                (now - last) / 1000
            );
            if !notify(lead, &notice) {
                eprintln!("worker-wake: lead notice failed for {lead}");
            }
        }
    }
    Ok(acted)
}

fn run_pass(home: &AgentsHome, dry_run: bool) -> Result<u64, String> {
    let rows = crate::state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    let nodes = crate::graph_store::read_rows_where_strict(
        &crate::gc_sweep::graph_path(home),
        &crate::backlog::RowQuery {
            fields: Some(
                [
                    "id",
                    "status",
                    "sessions",
                    "pr_number",
                    "spawned_by_session",
                    "source_session_id",
                ]
                .map(str::to_string)
                .to_vec(),
            ),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let transcripts = crate::context_run::SessionTranscripts::default();
    let now = chrono::Utc::now().timestamp_millis();
    let loaded = std::cell::OnceCell::new();
    run_pass_with(
        home,
        Pass {
            rows: &rows.entries,
            nodes: &nodes,
            now,
            dry_run,
            liveness: &|row| {
                let Some(sid) = row.harness_session_id.as_deref() else {
                    return Life::Unknown;
                };
                let probe = crate::truth_probe::family1_truth_probe_with_timeout(
                    sid,
                    Duration::from_secs(10),
                );
                if let Some(probe) = probe {
                    if probe.reachability.as_deref() == Some("unreachable")
                        && matches!(probe.basis.as_deref(), Some("process-gone" | "pane-gone"))
                    {
                        return Life::Dead;
                    }
                    if probe.reachability.as_deref() == Some("reachable") {
                        return Life::Resting;
                    }
                }
                if loaded
                    .get_or_init(crate::codex_inject::loaded_thread_ids)
                    .as_ref()
                    .is_ok_and(|ids| ids.contains(sid))
                {
                    Life::Resting
                } else {
                    Life::Unknown
                }
            },
        },
        &|row| {
            row.transcript_path
                .as_deref()
                .map(std::path::PathBuf::from)
                .or_else(|| transcripts.find(row.harness_session_id.as_deref()?, "codex"))
        },
        &|node, sid| {
            let (state, record) = crate::claims::status(&format!("node:{node}"), None);
            match state {
                crate::claims::ClaimState::Live | crate::claims::ClaimState::Suspect => {
                    if record.is_some_and(|r| {
                        r.session_id.as_deref() == Some(sid)
                            || r.holder == format!("target-session:{sid}")
                    }) {
                        Claim::Own
                    } else {
                        Claim::Held
                    }
                }
                crate::claims::ClaimState::Free | crate::claims::ClaimState::Stale => Claim::Free,
                crate::claims::ClaimState::Corrupted => Claim::Unreadable,
            }
        },
        &|cwd, pr| crate::pr_status::cache::cached_status(cwd, pr, false).1,
        &|sid, drive, text| match drive {
            Drive::TurnStart => match crate::codex_inject::deliver_via_codex_daemon_sync(sid, text)
            {
                Ok(_)
                | Err(crate::codex_inject::ReviewStartError::Reason("turn-start-unacked")) => {
                    Ok(())
                }
                Err(error) => Err(format!("{error:?}")),
            },
            Drive::Resume => {
                let argv = ["fno", "agents", "resume", sid, "--message", text].map(str::to_string);
                let (code, out, err) = crate::burn_watch::run_command(&argv, "");
                if code == 0 {
                    Ok(())
                } else {
                    Err(format!("resume refused ({code}): {out} {err}"))
                }
            }
        },
        &|sid, text| {
            let argv = [
                "fno",
                "agents",
                "mail",
                "send",
                "--from-name",
                "fno-worker-wake",
                "--origin",
                "scheduler",
                sid,
                text,
            ]
            .map(str::to_string);
            let (code, stdout, _) = crate::burn_watch::run_command(&argv, "");
            crate::mail_inject::mail_send_accepted(code, &stdout)
                || crate::mail_inject::mail_send_receipt(&stdout).contains("durable")
        },
    )
}

pub fn run_dry_run(args: &[String], home: &AgentsHome) -> i32 {
    if args != ["--dry-run"] {
        eprintln!("usage: fno-agents worker-wake --dry-run");
        return 2;
    }
    match run_pass(home, true) {
        Ok(eligible) => {
            println!("worker-wake: eligible={eligible} dry-run=true");
            0
        }
        Err(error) => {
            eprintln!("worker-wake: {error}");
            1
        }
    }
}

pub fn maybe_tick(arm: &Arm, home: AgentsHome) {
    let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|tick| tick.elapsed() < INTERVAL)
        || arm.in_flight.swap(true, Ordering::SeqCst)
    {
        return;
    }
    *last = Some(Instant::now());
    let flag = Arc::clone(&arm.in_flight);
    tokio::task::spawn_blocking(move || {
        let _gate = crate::daemon::SweepGate(flag);
        let journal = crate::loop_runtime::Journal::new_raw(
            home.events_jsonl(),
            crate::daemon::global_events_path(&home),
        );
        let (acted, error) = match run_pass(&home, false) {
            Ok(acted) => (acted, None),
            Err(error) => (0, Some(error)),
        };
        crate::tick_ledger::emit_tick(
            &journal,
            "worker_wake",
            crate::tick_ledger::SCHED_DAEMON,
            acted,
            error.as_ref().map(|_| "unreadable"),
            error.as_deref(),
            60,
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct Fixture {
        _dir: tempfile::TempDir,
        home: AgentsHome,
        path: std::path::PathBuf,
        row: RegistryEntry,
        nodes: Vec<Value>,
        claims: HashMap<String, Result<Option<String>, String>>,
        now: i64,
        life: Life,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let home = AgentsHome::at(dir.path().join("agents"));
            home.ensure_root().unwrap();
            let path = dir.path().join("rollout.jsonl");
            std::fs::create_dir_all(dir.path().join(".fno")).unwrap();
            std::fs::write(dir.path().join(".fno/config.toml"), "[recovery]\nenabled = true\nidle_threshold_seconds = 900\n[autonomy]\nenabled = true\n").unwrap();
            let row = RegistryEntry {
                name: "worker".into(),
                cwd: dir.path().to_string_lossy().into(),
                harness: Some("codex".into()),
                harness_session_id: Some("thread-a".into()),
                origin: Some("spawn".into()),
                node: Some("node-a".into()),
                spawned_by_session: Some("lead-a".into()),
                status: crate::AgentStatus::Live,
                ..Default::default()
            };
            Self {
                _dir: dir,
                home,
                path,
                row,
                nodes: vec![json!({"id": "node-a", "status": "in_progress"})],
                claims: HashMap::from([("thread-a".into(), Ok(Some("node-a".into())))]),
                now: epoch("2026-10-06T10:15:00Z").unwrap(),
                life: Life::Resting,
            }
        }

        fn write(&self, payloads: &[Value]) {
            let text = payloads.iter().map(|payload| {
                json!({"timestamp": "2026-10-06T10:00:00Z", "type": "event_msg", "payload": payload}).to_string()
            }).collect::<Vec<_>>().join("\n");
            std::fs::write(&self.path, text).unwrap();
        }

        fn pass(
            &self,
            deliver: &dyn Fn(&str, Drive, &str) -> Result<(), String>,
            notify: &dyn Fn(&str, &str) -> bool,
        ) -> u64 {
            let mut alias = self.row.clone();
            alias.name.push_str("-alias");
            let rows = [self.row.clone(), alias];
            run_pass_with(
                &self.home,
                Pass {
                    rows: &rows,
                    nodes: &self.nodes,
                    now: self.now,
                    dry_run: false,
                    liveness: &|_| self.life,
                },
                &|_| Some(self.path.clone()),
                &|node, sid| match self.claims.get(sid) {
                    Some(Ok(Some(owned))) if owned == node => Claim::Own,
                    Some(Ok(Some(_))) => Claim::Held,
                    Some(Err(_)) => Claim::Unreadable,
                    _ => Claim::Free,
                },
                &|_, _| self.nodes[0]["pr_status"].clone(),
                deliver,
                notify,
            )
            .unwrap()
        }
    }

    #[test]
    fn ended_rollout_wakes_once_and_tells_its_lead_after_a_retry() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        f.claims.clear();
        f.nodes[0]["sessions"] = json!([{"phase":"execute", "harness":"codex", "session_id":"thread-a", "started_at":"2026-10-06T09:00:00Z"}]);
        f.row.node = None;
        f.row.spawned_by_session = None;
        f.nodes[0]["source_session_id"] = json!("lead-a");
        f.write(&[
            json!({"type": "task_started"}),
            json!({"type": "task_complete"}),
        ]);
        let mut raw = std::fs::read_to_string(&f.path).unwrap();
        raw = format!(
            "{}\n{raw}",
            json!({"type":"session_meta", "payload":{"text":"é".repeat(TAIL_BYTES as usize)}})
        );
        raw.push_str("\n{\"type\":\"token_usage_record\"}");
        std::fs::write(&f.path, raw).unwrap();
        let sent = RefCell::new(Vec::new());
        let told = RefCell::new(Vec::new());
        let deliver = |sid: &str, drive: Drive, text: &str| {
            sent.borrow_mut()
                .push((sid.to_string(), text.to_string(), drive));
            Ok(())
        };
        let notify = |sid: &str, text: &str| {
            told.borrow_mut().push((sid.to_string(), text.to_string()));
            true
        };
        assert_eq!(
            run_pass_with(
                &f.home,
                Pass {
                    rows: std::slice::from_ref(&f.row),
                    nodes: &f.nodes,
                    now: f.now,
                    dry_run: true,
                    liveness: &|_| Life::Resting,
                },
                &|_| Some(f.path.clone()),
                &|_, _| Claim::Free,
                &|_, _| panic!("no PR expected"),
                &|_, _, _| panic!("dry run delivered"),
                &|_, _| panic!("dry run mailed")
            )
            .unwrap(),
            1
        );
        assert_eq!(f.pass(&|_, _, _| Err("unavailable".into()), &notify), 0);
        assert_eq!(f.pass(&deliver, &notify), 1);
        assert_eq!(f.pass(&deliver, &notify), 0);
        assert_eq!(sent.borrow().len(), 1);
        assert_eq!(sent.borrow()[0].0, "thread-a");
        assert!(sent.borrow()[0].1.contains("node-a"));
        assert_eq!(told.borrow().len(), 1);
        assert_eq!(told.borrow()[0].0, "lead-a");
        let receipts = crate::event_store::journal_text(&f.home.events_jsonl(), &[EVENT]);
        let receipt: Value = serde_json::from_str(receipts.lines().next().unwrap()).unwrap();
        assert_eq!(receipt["data"]["via"], "codex_turn_start");
        assert_eq!(receipt["data"]["last_activity_ms"], f.now - 900_000);
        f.life = Life::Dead;
        f.row.status = crate::AgentStatus::Exited;
        assert_eq!(f.pass(&deliver, &notify), 1);
        assert_eq!(sent.borrow().last().unwrap().2, Drive::Resume);
        assert_eq!(f.pass(&deliver, &notify), 0);
        let mut red = Fixture::new();
        red.write(&[json!({"type":"task_complete"})]);
        red.nodes[0]["status"] = json!("in_review");
        red.nodes[0]["pr_number"] = json!(42);
        red.nodes[0]["pr_status"] = json!({"pr_state":"OPEN", "verdict":"red", "settled":true});
        assert_eq!(red.pass(&deliver, &notify), 1);
        let mut dead = Fixture::new();
        dead.life = Life::Dead;
        dead.row.status = crate::AgentStatus::Exited;
        dead.write(&[json!({"type":"task_started"})]);
        assert_eq!(dead.pass(&deliver, &notify), 1);
        assert_eq!(sent.borrow().last().unwrap().2, Drive::Resume);
        assert_eq!(dead.pass(&deliver, &notify), 0);
        dead.row.pid = Some(4242);
        dead.row.pid_start_time = Some(2);
        assert_eq!(dead.pass(&deliver, &notify), 1);
        assert_eq!(dead.pass(&deliver, &notify), 0);
    }

    #[test]
    fn active_parked_unowned_terminal_and_unreadable_workers_stay_quiet() {
        let _env = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for case in [
            "active",
            "recent",
            "parked",
            "unowned",
            "terminal",
            "role",
            "operator",
            "busy",
            "malformed",
            "disabled",
            "ci_watch",
            "held",
            "unreadable_claim",
            "reassigned",
            "green_pr",
            "pending_pr",
            "terminal_pr",
            "unmeasured_pr",
            "complete",
            "missing_lead",
            "unknown_liveness",
            "quota",
            "quota_code_only",
            "promise_alt",
            "aborted",
        ] {
            let mut f = Fixture::new();
            f.write(&[json!({"type": "task_complete"})]);
            match case {
                "active" => f.write(&[json!({"type": "task_complete"}), json!({"type": "task_started"})]),
                "recent" => f.now -= 1,
                "parked" => std::fs::write(&f.path, json!({"timestamp":"2026-10-06T10:00:00Z", "type":"response_item", "payload":{"type":"message", "role":"assistant", "phase":"final_answer", "content":[{"text":"<watching reason=\"ci\">"}]}}).to_string()).unwrap(),
                "unowned" => f.claims.clear(),
                "missing_lead" => f.row.spawned_by_session = None,
                "unknown_liveness" => f.life = Life::Unknown,
                "quota" => f.write(&[json!({"type":"task_complete", "error":{"codex_error_info":"usage_limit_exceeded", "message":"You've hit your usage limit"}})]),
                "quota_code_only" => f.write(&[json!({"type":"task_complete", "error":{"codex_error_info":"usage_limit_exceeded"}})]),
                "promise_alt" => f.write(&[json!({"type":"task_complete", "last_agent_message":"<promise>COMPLETE</promise>"})]),
                "aborted" => f.write(&[json!({"type":"task_complete", "last_agent_message":"<aborted reason=\"operator stop\">"})]),
                "held" => { f.claims.insert("thread-a".into(), Ok(Some("other-node".into()))); }
                "unreadable_claim" => { f.claims.insert("thread-a".into(), Err("corrupt claim".into())); }
                "reassigned" => { f.claims.clear(); f.nodes[0]["sessions"] = json!([{"phase":"execute", "harness":"codex", "session_id":"other-worker", "started_at":"2026-10-06T09:00:00Z"}]); }
                "green_pr" | "pending_pr" | "terminal_pr" | "unmeasured_pr" => {
                    f.nodes[0]["status"] = json!("in_review");
                    f.nodes[0]["pr_number"] = json!(42);
                    f.nodes[0]["pr_status"] = match case {
                        "green_pr" => json!({"pr_state":"OPEN", "verdict":"green", "settled":true, "merge_authority":{"grant":false}}),
                        "pending_pr" => json!({"pr_state":"OPEN", "verdict":"pending", "settled":false}),
                        "terminal_pr" => json!({"pr_state":"MERGED", "verdict":"green", "settled":true}),
                        _ => Value::Null,
                    };
                }
                "complete" => f.write(&[json!({"type":"task_complete", "last_agent_message":"<promise>MISSION COMPLETE: shipped</promise>"})]),
                "terminal" => f.nodes[0]["status"] = json!("done"),
                "role" => f.row.role_level = Some(2),
                "operator" => f.row.origin = Some("operator".into()),
                "busy" => f.row.status = crate::AgentStatus::Busy,
                "malformed" => std::fs::write(&f.path, "{broken").unwrap(),
                "disabled" => std::fs::write(f._dir.path().join(".fno/config.toml"), "[recovery]\nenabled = false\n").unwrap(),
                "ci_watch" => std::fs::write(crate::daemon::global_events_path(&f.home), json!({
                    "ts":"2026-10-06T10:14:00Z", "type":crate::watch_expiry::WATCH_IDLE, "source":"test",
                    "data":{"session_id":"manifest-a", "codex_thread_id":"thread-a", "harness":"codex", "node":"node-a", "blocker":"ci", "expires_at_ms":f.now + 60_000}
                }).to_string()).unwrap(),
                _ => unreachable!(),
            }
            assert_eq!(
                f.pass(
                    &|_, _, _| panic!("unexpected wake: {case}"),
                    &|_, _| panic!("unexpected notice: {case}")
                ),
                0,
                "{case}"
            );
        }
    }
}

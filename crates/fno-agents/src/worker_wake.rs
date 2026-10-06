//! Wake an ended Codex turn on an open claimed node, once per quiet episode.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
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
    parked: bool,
}

fn epoch(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn tail(path: &Path) -> Result<Tail, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let offset = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(file.take(len - offset));
    if offset > 0 {
        let mut partial = Vec::new();
        reader
            .read_until(b'\n', &mut partial)
            .map_err(|e| e.to_string())?;
    }
    let mut out = Tail::default();
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let row: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if let Some(raw) = row.get("timestamp").and_then(Value::as_str) {
            out.last = epoch(raw);
        }
        let payload = &row["payload"];
        match (row["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("task_started" | "user_message"))
            | (Some("response_item"), Some("function_call" | "custom_tool_call")) => {
                out.ended = false;
                out.parked = false;
            }
            (Some("event_msg"), Some("task_complete")) => {
                out.ended = true;
                if let Some(text) = payload["last_agent_message"].as_str() {
                    out.parked = text.contains("<watching") || text.contains("<help");
                }
            }
            (Some("event_msg"), Some("turn_aborted")) => out.ended = false,
            (Some("response_item"), Some("message")) => {
                if payload["role"] == "user" {
                    out.ended = false;
                    out.parked = false;
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
                    out.parked = text.contains("<watching") || text.contains("<help");
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
    claims: &'a HashMap<String, Result<Option<String>, String>>,
    now: i64,
}

fn run_pass_with(
    home: &AgentsHome,
    pass: Pass<'_>,
    transcript: &dyn Fn(&RegistryEntry) -> Option<std::path::PathBuf>,
    deliver: &dyn Fn(&str, &str) -> Result<(), String>,
    notify: &dyn Fn(&str, &str) -> bool,
) -> Result<u64, String> {
    let Pass {
        rows,
        nodes,
        claims,
        now,
    } = pass;
    let evidence = crate::watch_expiry::read_evidence(home, now)?;
    let receipts = crate::event_store::journal_text_checked(
        &home.events_jsonl(),
        &crate::event_store::EventQuery::of_types(&[EVENT]),
    )?;
    let mut acted = 0;
    for row in rows {
        if row.harness_name() != "codex"
            || row.origin.as_deref() != Some("spawn")
            || row.crown_level.is_some()
            || !row.status.is_drive_eligible()
        {
            continue;
        }
        if [
            ["recovery", "enabled"].as_slice(),
            ["autonomy", "enabled"].as_slice(),
        ]
        .iter()
        .any(|keys| {
            crate::agents_config::config_lookup(Path::new(&row.cwd), keys).and_then(|v| v.as_bool())
                == Some(false)
        }) {
            continue;
        }
        let (Some(sid), Some(node)) = (row.harness_session_id.as_deref(), row.node.as_deref())
        else {
            continue;
        };
        if !matches!(claims.get(sid), Some(Ok(Some(owned))) if owned == node)
            || !nodes.iter().any(|n| {
                n["id"] == node
                    && matches!(
                        crate::graph_get::entry_status(n),
                        "in_progress" | "ready" | "next"
                    )
            })
        {
            continue;
        }
        if crate::codex_watch::settle_watches(&evidence)
            .iter()
            .any(|w| {
                (w.watch.session_id == sid || w.codex_thread_id.as_deref() == Some(sid))
                    && w.watch.expires_at_ms > now
                    && crate::watch_expiry::is_current_watch(&w.watch, &evidence)
            })
        {
            continue;
        }
        let Some(path) = transcript(row) else {
            continue;
        };
        let tail = match tail(&path) {
            Ok(tail) => tail,
            Err(error) => {
                eprintln!("worker-wake: {}: rollout unreadable: {error}", row.name);
                continue;
            }
        };
        let Some(last) = tail.last else {
            continue;
        };
        let threshold = crate::agents_config::config_lookup(
            Path::new(&row.cwd),
            &["recovery", "idle_threshold_seconds"],
        )
        .and_then(|v| v.as_integer())
        .filter(|v| *v > 0)
        .unwrap_or(900);
        if !tail.ended || tail.parked || now.saturating_sub(last) < threshold.saturating_mul(1000) {
            continue;
        }
        if receipts
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|r| r["data"]["session_id"] == sid && r["data"]["last_activity_ms"] == last)
        {
            continue;
        }
        let text = format!("Automatic worker wake: your turn ended on open node {node} and has been quiet for {}s. Continue the target from current evidence.", (now - last) / 1000);
        if let Err(error) = deliver(sid, &text) {
            eprintln!("worker-wake: {}: {error}", row.name);
            continue;
        }
        crate::events::EventEmitter::new(home.events_jsonl(), "daemon").emit(EVENT, &json!({
            "session_id": sid, "node": node, "last_activity_ms": last, "via": "codex_turn_start"
        })).map_err(|e| e.to_string())?;
        acted += 1;
        if let Some(lead) = row.spawned_by_session.as_deref() {
            let notice = format!(
                "Worker wake: {} resumed on {node} after {}s idle through codex turn/start.",
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

fn run_pass(home: &AgentsHome) -> Result<u64, String> {
    let rows = crate::state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    let nodes = crate::graph_store::read_rows_where_strict(
        &crate::gc_sweep::graph_path(home),
        &crate::backlog::RowQuery {
            fields: Some(["id", "status"].map(str::to_string).to_vec()),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    let claims = crate::watch_expiry::current_node_claims(home)?;
    let transcripts = crate::context_run::SessionTranscripts::default();
    let now = chrono::Utc::now().timestamp_millis();
    run_pass_with(
        home,
        Pass {
            rows: &rows.entries,
            nodes: &nodes,
            claims: &claims,
            now,
        },
        &|row| {
            row.transcript_path
                .as_deref()
                .map(std::path::PathBuf::from)
                .or_else(|| transcripts.find(row.harness_session_id.as_deref()?, "codex"))
        },
        &|sid, text| match crate::codex_inject::deliver_via_codex_daemon_sync(sid, text) {
            Ok(_) | Err(crate::codex_inject::ReviewStartError::Reason("turn-start-unacked")) => {
                Ok(())
            }
            Err(error) => Err(format!("{error:?}")),
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
        let (acted, error) = match run_pass(&home) {
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

    struct Fixture {
        _dir: tempfile::TempDir,
        home: AgentsHome,
        path: std::path::PathBuf,
        row: RegistryEntry,
        nodes: Vec<Value>,
        claims: HashMap<String, Result<Option<String>, String>>,
        now: i64,
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
            deliver: &dyn Fn(&str, &str) -> Result<(), String>,
            notify: &dyn Fn(&str, &str) -> bool,
        ) -> u64 {
            run_pass_with(
                &self.home,
                Pass {
                    rows: std::slice::from_ref(&self.row),
                    nodes: &self.nodes,
                    claims: &self.claims,
                    now: self.now,
                },
                &|_| Some(self.path.clone()),
                deliver,
                notify,
            )
            .unwrap()
        }
    }

    #[test]
    fn ended_rollout_wakes_once_and_tells_its_lead_after_a_retry() {
        let f = Fixture::new();
        f.write(&[
            json!({"type": "task_started"}),
            json!({"type": "task_complete"}),
        ]);
        let mut raw = std::fs::read_to_string(&f.path).unwrap();
        raw.push_str("\n{\"type\":\"token_usage_record\"}");
        std::fs::write(&f.path, raw).unwrap();
        let sent = RefCell::new(Vec::new());
        let told = RefCell::new(Vec::new());
        let deliver = |sid: &str, text: &str| {
            sent.borrow_mut().push((sid.to_string(), text.to_string()));
            Ok(())
        };
        let notify = |sid: &str, text: &str| {
            told.borrow_mut().push((sid.to_string(), text.to_string()));
            true
        };
        assert_eq!(f.pass(&|_, _| Err("unavailable".into()), &notify), 0);
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
    }

    #[test]
    fn active_parked_unowned_terminal_and_unreadable_workers_stay_quiet() {
        for case in [
            "active",
            "recent",
            "parked",
            "unowned",
            "terminal",
            "crown",
            "operator",
            "busy",
            "malformed",
        ] {
            let mut f = Fixture::new();
            f.write(&[json!({"type": "task_complete"})]);
            match case {
                "active" => f.write(&[json!({"type": "task_complete"}), json!({"type": "task_started"})]),
                "recent" => f.now -= 1,
                "parked" => std::fs::write(&f.path, json!({"timestamp":"2026-10-06T10:00:00Z", "type":"response_item", "payload":{"type":"message", "role":"assistant", "phase":"final_answer", "content":[{"text":"<watching reason=\"ci\">"}]}}).to_string()).unwrap(),
                "unowned" => f.claims.clear(),
                "terminal" => f.nodes[0]["status"] = json!("done"),
                "crown" => f.row.crown_level = Some(2),
                "operator" => f.row.origin = Some("operator".into()),
                "busy" => f.row.status = crate::AgentStatus::Busy,
                "malformed" => std::fs::write(&f.path, "{broken").unwrap(),
                _ => unreachable!(),
            }
            assert_eq!(
                f.pass(&|_, _| panic!("unexpected wake: {case}"), &|_, _| panic!(
                    "unexpected notice: {case}"
                )),
                0,
                "{case}"
            );
        }
    }
}

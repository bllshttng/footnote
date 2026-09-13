//! `provider-cap` verbs + daemon arm (x-7e05 wave 2).
//!
//! `status` reads the daemon's persisted snapshot when fresh, else computes
//! one on demand (fresh = measured now, never a stale read served as fresh).
//! `decide` records an operator answer for the leave/return waves to consume.
//! The daemon arm measures and persists the snapshot armed or unarmed; the
//! unarmed tick row reads `provider_cap_off`, a live measurement of why the
//! actor held.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::agents_config::provider_cap_config;
use crate::paths::AgentsHome;
use crate::provider_cap::{append_questions_row, read_persisted_snapshot};
use crate::provider_cap::{
    epoch_to_rfc3339, lane_file_token, lanes_dir, now_epoch_secs, snapshot, CapSnapshot,
    PROVIDER_CAP_INTERVAL_S,
};

pub fn run_provider_cap(args: &[String]) -> i32 {
    match args.split_first() {
        Some((action, rest)) if action == "status" => cap_status(rest),
        Some((action, rest)) if action == "decide" => cap_decide(rest),
        _ => {
            eprintln!("usage: provider-cap status [--json] [--max-age-s N] | decide <lane> --answer all|some:<id,id>|wait");
            2
        }
    }
}

fn cap_status(args: &[String]) -> i32 {
    let json = args.iter().any(|a| a == "--json");
    let max_age = flag_value(args, "--max-age-s")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1800);
    let home = AgentsHome::from_env();
    let now = now_epoch_secs();
    if let Some(snap) = read_persisted_snapshot(&home) {
        if snap.fresh(max_age, now) {
            let v = json!({
                "lanes": snap.lanes,
                "measured_at": snap.measured_at,
                "fresh": true,
                "source": "daemon-tick",
            });
            emit_status(&v, json);
            return 0;
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cfg = provider_cap_config(&cwd);
    match snapshot(&home, &cwd, now, &cfg) {
        Ok(snap) => {
            let v = json!({
                "lanes": snap.lanes,
                "measured_at": snap.measured_at,
                "fresh": true,
                "source": "on-demand",
            });
            emit_status(&v, json);
            0
        }
        Err(reason) => {
            eprintln!("provider-cap status: snapshot failed: {reason}");
            1
        }
    }
}

fn emit_status(v: &Value, json: bool) {
    if json {
        println!("{}", serde_json::to_string(v).unwrap_or_default());
    } else {
        println!("{}", render_text_snapshot(v));
    }
}

fn render_text_snapshot(v: &Value) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "measured_at={} fresh={}",
        v.get("measured_at").and_then(Value::as_str).unwrap_or("?"),
        v.get("fresh").and_then(Value::as_bool).unwrap_or(false)
    ));
    out.push_str(&format!(
        " source={}",
        v.get("source").and_then(Value::as_str).unwrap_or("?")
    ));
    for lane in v.get("lanes").and_then(Value::as_array).unwrap_or(&vec![]) {
        let lane_name = lane.get("lane").and_then(Value::as_str).unwrap_or("?");
        let state = lane.get("state").and_then(Value::as_str).unwrap_or("?");
        let reset = lane
            .get("reset_epoch")
            .and_then(Value::as_i64)
            .map(epoch_to_rfc3339)
            .unwrap_or_else(|| "unknown".to_string());
        let missing = lane
            .get("missing_reset_timezone")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        out.push_str(&format!(
            "\n{} [{}] reset={} missing_tz={}",
            lane_name, state, reset, missing
        ));
        for m in lane
            .get("members")
            .and_then(Value::as_array)
            .unwrap_or(&vec![])
        {
            out.push_str(&format!(
                "\n  {} capped={} held={} tail_unknown={}",
                m.get("name").and_then(Value::as_str).unwrap_or("?"),
                m.get("capped").and_then(Value::as_bool).unwrap_or(false),
                m.get("held").and_then(Value::as_str).unwrap_or("-"),
                m.get("cap_unknown").and_then(Value::as_str).unwrap_or("-"),
            ));
        }
    }
    out
}

fn cap_decide(args: &[String]) -> i32 {
    let (lane, rest) = match args.split_first() {
        Some(pair) => pair,
        None => {
            eprintln!("provider-cap decide: a lane token is required");
            return 2;
        }
    };
    let lane = lane.trim();
    let answer = flag_value(rest, "--answer").unwrap_or_default();
    let verdict = match answer.as_str() {
        "all" | "wait" => answer,
        a if a.starts_with("some:") => a.to_string(),
        _ => {
            eprintln!("provider-cap decide: --answer all|some:<id,id>|wait is required");
            return 2;
        }
    };
    let home = AgentsHome::from_env();
    let dir = lanes_dir(&home);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("provider-cap decide: cannot create {}: {e}", dir.display());
        return 1;
    }
    let record = json!({
        "lane": lane,
        "answer": verdict,
        "decided_at": epoch_to_rfc3339(now_epoch_secs()),
    });
    let path = dir.join(format!("decision-{}.json", lane_file_token(lane)));
    if let Err(e) = std::fs::write(&path, record.to_string()) {
        eprintln!("provider-cap decide: cannot write {}: {e}", path.display());
        return 1;
    }
    append_questions_row(&json!({
        "ts": epoch_to_rfc3339(now_epoch_secs()),
        "type": "operator_question_closed",
        "source": "provider-cap",
        "data": {
            "question_id": format!("provider-cap:{lane}"),
            "answer": verdict,
            "closed_by": "provider-cap decide",
        },
    }));
    println!("recorded: {} -> {verdict}", path.display());
    0
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Daemon arm
// ---------------------------------------------------------------------------

/// The arm as the daemon holds it: cadence stamp + one-in-flight gate, the
/// machine_watch shape.
pub struct Arm {
    last_tick: Mutex<Option<std::time::Instant>>,
    in_flight: Arc<std::sync::atomic::AtomicBool>,
}

impl Default for Arm {
    fn default() -> Self {
        Self {
            last_tick: Mutex::new(None),
            in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

/// Due-check + threaded body. Every path ends in exactly one tick row:
/// `provider_cap_off` when disarmed (still measuring, still persisting the
/// snapshot), `ok` when armed and nothing owed. Waves 3/4 hang the leave and
/// return decisions off the armed branch.
pub fn maybe_tick(arm: &Arm, home: crate::paths::AgentsHome, config_cwd: PathBuf) {
    let interval = Duration::from_secs(PROVIDER_CAP_INTERVAL_S);
    {
        let mut last = arm.last_tick.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < interval)
            || arm
                .in_flight
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        *last = Some(std::time::Instant::now());
    }
    let flag = Arc::clone(&arm.in_flight);
    std::thread::spawn(move || {
        let cfg = provider_cap_config(&config_cwd);
        let now = now_epoch_secs();
        let (skip, lanes_open) = match snapshot(&home, &config_cwd, now, &cfg) {
            Ok(snap) => {
                crate::provider_cap::persist_snapshot(&home, &snap);
                if cfg.enabled {
                    ("ok", open_lane_count(&snap))
                } else {
                    ("provider_cap_off", open_lane_count(&snap))
                }
            }
            Err(_reason) => ("snapshot_failed", 0usize),
        };
        let journal = crate::loop_runtime::Journal::new_raw(
            home.events_jsonl(),
            crate::daemon::global_events_path(&home),
        );
        crate::tick_ledger::emit_tick(
            &journal,
            "provider_cap",
            crate::tick_ledger::SCHED_DAEMON,
            0,
            if skip == "ok" { None } else { Some(skip) },
            Some(&format!("open_lanes={lanes_open}")),
            PROVIDER_CAP_INTERVAL_S,
        );
        flag.store(false, Ordering::SeqCst);
    });
}

fn open_lane_count(snap: &CapSnapshot) -> usize {
    snap.open_lanes().len()
}

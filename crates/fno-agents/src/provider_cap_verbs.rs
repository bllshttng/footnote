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
use crate::provider_cap::{append_questions_row, questions_path, read_persisted_snapshot};
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
    append_questions_row(
        &questions_path(&home),
        &json!({
            "ts": epoch_to_rfc3339(now_epoch_secs()),
            "type": "operator_question_closed",
            "source": "provider-cap",
            "data": {
                "question_id": format!("provider-cap:{lane}"),
                "answer": verdict,
                "closed_by": "provider-cap decide",
            },
        }),
    );
    // The answer consumes the open question: drop the marker so a later
    // strand on the same lane can ask fresh instead of being suppressed.
    let _ = std::fs::remove_file(dir.join(format!("question-{}.json", lane_file_token(lane))));
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
    config_cwd: PathBuf,
}

impl Arm {
    pub fn new(config_cwd: PathBuf) -> Self {
        Self {
            last_tick: Mutex::new(None),
            in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            config_cwd,
        }
    }
}

/// Due-check + threaded body. Every path ends in exactly one tick row:
/// `provider_cap_off` when disarmed (still measuring, still persisting the
/// snapshot), `ok` when armed and nothing owed. Waves 3/4 hang the leave and
/// return decisions off the armed branch.
pub fn maybe_tick(arm: &Arm, home: crate::paths::AgentsHome) {
    let config_cwd = arm.config_cwd.clone();
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
        let (skip, detail) = match snapshot(&home, &config_cwd, now, &cfg) {
            Ok(snap) => {
                crate::provider_cap::persist_snapshot(&home, &snap);
                if cfg.enabled {
                    let d = run_armed(
                        &home,
                        &crate::provider_cap::default_scan(&home, &config_cwd),
                        &snap,
                        &cfg,
                        now,
                    );
                    (None, d)
                } else {
                    (
                        Some("provider_cap_off"),
                        format!("open_lanes={}", open_lane_count(&snap)),
                    )
                }
            }
            Err(reason) => (
                Some("snapshot_failed"),
                format!("snapshot failed: {reason}"),
            ),
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
            skip.as_deref(),
            Some(&detail),
            PROVIDER_CAP_INTERVAL_S,
        );
        flag.store(false, Ordering::SeqCst);
    });
}

fn open_lane_count(snap: &CapSnapshot) -> usize {
    snap.open_lanes().len()
}

// ---------------------------------------------------------------------------
// Real-world deps for the armed path (the daemon shells out to fno).
// ---------------------------------------------------------------------------

fn run_fno(args: &[&str], cwd: Option<&std::path::Path>, timeout: std::time::Duration) -> bool {
    use std::process::{Command, Stdio};
    let fno = std::env::var_os("FNO_BIN").unwrap_or_else(|| std::ffi::OsString::from("fno"));
    let mut cmd = Command::new(&fno);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match cmd.spawn() {
        Err(_) => false,
        Ok(mut child) => {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return status.success(),
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return false;
                    }
                    Err(_) => return false,
                }
            }
        }
    }
}

/// The armed actor's world: fno verbs with bounded waits. A step that cannot
/// prove its effect returns false/Err and the journal records `unknown`.
fn real_deps() -> crate::provider_cap::LeaveDeps {
    use crate::provider_cap::LeaveDeps;
    LeaveDeps {
        refresh_usage: Box::new(|| {
            run_fno(
                &["config", "accounts", "usage", "--refresh"],
                None,
                std::time::Duration::from_secs(90),
            )
        }),
        spawn: Box::new(|member, flags, handoff_path| {
            use std::process::{Command, Stdio};
            let fno =
                std::env::var_os("FNO_BIN").unwrap_or_else(|| std::ffi::OsString::from("fno"));
            let cwd = member
                .cwd
                .clone()
                .ok_or_else(|| "no cwd on the registry row".to_string())?;
            let node = member.node.clone().unwrap_or_default();
            let verb = if member.harness == "codex" {
                "$fno:target"
            } else {
                "/fno:target"
            };
            let prompt = if node.is_empty() {
                format!(
                    "Read the handoff doc at {} and continue its work.",
                    handoff_path.display()
                )
            } else {
                format!(
                    "{verb} {node}. The handoff doc at {} names the resume point; read it first.",
                    handoff_path.display()
                )
            };
            let short = member
                .session_id
                .as_deref()
                .unwrap_or("cap")
                .get(0..8)
                .unwrap_or("cap")
                .to_string();
            let name = format!("pc-cap-{short}");
            let mut cmd = Command::new(&fno);
            cmd.current_dir(std::path::Path::new(&cwd));
            cmd.args(["agents", "spawn", "--name", &name]);
            cmd.args(flags.iter().map(|s| s.as_str()));
            cmd.arg(&prompt);
            let out = cmd
                .stdin(Stdio::null())
                .output()
                .map_err(|e| format!("spawn exec failed: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "spawn refused: {}",
                    String::from_utf8_lossy(&out.stderr)
                        .chars()
                        .take(200)
                        .collect::<String>()
                ));
            }
            // The handle we pass to confirm is the NAME we chose, not the
            // receipt: `fno agents truth` resolves a registry handle, and the
            // spawn receipt is prose no lookup accepts.
            Ok(name)
        }),
        confirm: Box::new(|sid| {
            run_fno(
                &["agents", "truth", sid],
                None,
                std::time::Duration::from_secs(60),
            )
        }),
        stop: Box::new(|member| {
            let who = member
                .session_id
                .clone()
                .unwrap_or_else(|| member.name.clone());
            if run_fno(
                &["agents", "stop", &who],
                None,
                std::time::Duration::from_secs(60),
            ) {
                Ok(())
            } else {
                Err(format!("stop {} failed", member.name))
            }
        }),
    }
}

/// Extend the armed branch: run the leave ladder per open lane. Returns the
/// tick detail.
fn run_armed(
    home: &crate::paths::AgentsHome,
    scan: &crate::provider_cap::CapScan,
    snap: &CapSnapshot,
    cfg: &crate::agents_config::ProviderCapConfig,
    now: i64,
) -> String {
    let deps = real_deps();
    let mut parts: Vec<String> = Vec::new();
    for lane in snap.open_lanes() {
        let answer = crate::provider_cap::read_decision(home, &lane.lane);
        let outcome =
            crate::provider_cap::run_leave_lane(home, scan, lane, answer.as_ref(), cfg, now, &deps);
        parts.push(format!("{}: {outcome}", lane.lane));
    }
    if parts.is_empty() {
        "ok: no open lanes".to_string()
    } else {
        parts.join(" | ")
    }
}

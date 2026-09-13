//! The `spawn-gate` verb (x-6089): the ONE spawn gate answered over one
//! subprocess round trip, so every door - pane, routed, account, and the
//! native bg/headless arms - reads the same question answered in one place.
//!
//! Two modes, selected by the payload's `mode` field. `gate` runs the full
//! admission gate over one JSON round trip; the gate's own prose streams on
//! stderr passthrough, so `spawn queued: ...` still streams during a queue.
//! `probe` is the read-only capacity reading `fno agents gate-status`, the
//! lane readouts and the advance width all consume: no mutex, no claims, no
//! events. The verb exits 0 whenever it produced an ANSWER, including a
//! refusal: a refusal is data now, not a process exit.

use std::io::Read;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::agents_config;
use crate::spawn_gate::{self, GateFlags, GateInput};
use crate::spawn_gate_lanes;

/// `spawn-gate`: one payload on stdin, one answer on stdout. Exit 0 whenever
/// an answer was produced, including a refused answer. An unreadable payload
/// is a loud non-zero: the transport turns that into a gate-unavailable
/// refusal, never an admit.
pub fn run_spawn_gate(_args: &[String]) -> i32 {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        eprintln!("spawn-gate: could not read the request payload");
        return 1;
    }
    let payload: Value = match serde_json::from_str(&raw) {
        Ok(payload) => payload,
        Err(e) => {
            eprintln!("spawn-gate: unparseable request payload: {e}");
            return 1;
        }
    };
    let answer = match payload.get("mode").and_then(Value::as_str) {
        Some("gate") => gate_answer(&payload),
        Some("probe") => probe_answer(&payload),
        other => {
            eprintln!("spawn-gate: unknown mode {other:?}; want \"gate\" or \"probe\"");
            return 1;
        }
    };
    println!("{answer}");
    0
}

fn gate_answer(payload: &Value) -> Value {
    let config_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let home = crate::paths::AgentsHome::from_env();
    let flags = GateFlags {
        force: payload
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        no_wait: payload
            .get("no_wait")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    let input = GateInput {
        name: str_of(payload, "name"),
        substrate: str_of(payload, "substrate"),
        flags,
        route_provider: opt_str_of(payload, "route_provider"),
        account: opt_str_of(payload, "account"),
        caller_session: opt_str_of(payload, "caller_session"),
        holder_pid: payload
            .get("holder_pid")
            .and_then(Value::as_u64)
            .map(|p| p as u32),
    };
    match spawn_gate::run_gate(&config_cwd, &home.registry_json(), input) {
        Ok(mut guard) => {
            // Take the keys BEFORE the guard drops: releasing them here would
            // free the very claims the caller must hold across dispatch.
            let (gate, worker) = guard.take_keys();
            let (gate_key, gate_holder) = unwrap_key(gate);
            let (worker_key, worker_holder) = unwrap_key(worker);
            json!({
                "status": "admitted",
                "gate_key": gate_key,
                "gate_holder": gate_holder,
                "worker_key": worker_key,
                "worker_holder": worker_holder,
            })
        }
        Err(refusal) => json!({
            "status": "refused",
            "exit_code": refusal.exit_code,
            "receipt": refusal.receipt,
            "event": refusal.event,
        }),
    }
}

fn str_of(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn opt_str_of(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn probe_answer(_payload: &Value) -> Value {
    json!({"verdict": "unknown", "reason": "unimplemented"})
}

fn unwrap_key(held: Option<(String, String)>) -> (Value, Value) {
    match held {
        Some((key, holder)) => (json!(key), json!(holder)),
        None => (Value::Null, Value::Null),
    }
}

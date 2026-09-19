//! The `spawn-gate` verb : the ONE spawn gate answered over one
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
    // The verb is the one cross-process transport into the gate, and its own
    // pid is dead once it has answered: a reservation held by it reads Suspect
    // for the whole worker TTL and wedges the provider lane (2026-09-15).
    // The native arms build GateInput in-process, where the
    // std::process::id() default is correct, so the refusal lives here and
    // not in run_gate.
    let Some(holder_pid) = payload.get("holder_pid").and_then(Value::as_u64) else {
        eprintln!(
            "spawn-gate: refused: a gate payload needs holder_pid, the pid that holds and releases the admitted keys"
        );
        return json!({
            "status": "refused",
            "exit_code": spawn_gate::EXIT_GATE_UNAVAILABLE,
            "receipt": {
                "status": "refused",
                "reason": "holder_pid_required",
                "remedy": "send holder_pid: the pid of the process that holds the admitted keys and releases them",
            },
            "event": {},
        });
    };
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
        holder_pid: Some(holder_pid as u32),
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
    probe::answer(_payload)
}

/// The probe's pieces, namespaced so the payload parsing and the row
/// rendering stay testable beside each other.
mod probe {
    use super::*;

    pub(super) fn answer(payload: &Value) -> Value {
        let config_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home = crate::paths::AgentsHome::from_env();
        let registry_path = home.registry_json();
        let caller = opt_str_of(payload, "caller_session");
        let lanes_only = payload
            .get("only")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).any(|s| s == "lanes"))
            .unwrap_or(false);

        let cap = agents_config::max_live(&config_cwd) as usize;
        let floor_gb = agents_config::min_free_gb(&config_cwd);
        let swap_cap = agents_config::max_swap_pct(&config_cwd);

        let mut warnings: Vec<String> = Vec::new();
        let mut out: Map<String, Value> = Map::new();

        // Registry schema first, exactly as the Python probe ordered it.
        if let Err(refusal) = spawn_gate_lanes::check_registry_schema(&registry_path, &mut warnings)
        {
            let receipt = refusal.receipt.unwrap_or(Value::Null);
            return json!({
                "verdict": "refused",
                "reason": "registry_schema",
                "message": format!(
                    "registry schema {} ahead of schema {} this fno understands; run fno doctor update",
                    receipt.get("on_disk").map(|v| v.to_string()).unwrap_or_default(),
                    receipt.get("understood").map(|v| v.to_string()).unwrap_or_default()
                ),
                "on_disk": receipt.get("on_disk").cloned().unwrap_or(Value::Null),
                "understood": receipt.get("understood").cloned().unwrap_or(Value::Null),
                "rows": [],
            });
        }

        // Read provider lanes before any refusal so the answer preserves the
        // quota evidence that explains a busy fleet.
        let lanes_result = lanes_answer(&config_cwd, &registry_path, &mut warnings);
        if let Ok(lanes) = &lanes_result {
            out.insert("lanes".into(), lanes.clone());
        }

        // The route axis of the quota wall: the SAME call the gate makes, so
        // the probe and the gate cannot disagree about what refuses (the gate
        // runs it ahead of every machine axis).
        if let Some(provider) = opt_str_of(payload, "route_provider") {
            let mut lane_warnings = Vec::new();
            if let Err(refusal) =
                spawn_gate_lanes::check_lane_quota_lock(home.root(), &provider, &mut lane_warnings)
            {
                let receipt = refusal.receipt.unwrap_or(Value::Null);
                return refuse_with(
                    "provider_quota_lock",
                    format!(
                        "provider lane {} is rate-limited (resets_at {})",
                        receipt
                            .get("lane")
                            .and_then(Value::as_str)
                            .unwrap_or(&provider),
                        receipt
                            .get("resets_at")
                            .and_then(Value::as_f64)
                            .map(|r| r.to_string())
                            .unwrap_or_else(|| "unknown".into())
                    ),
                    receipt,
                    &[],
                    out,
                );
            }
        }

        // The slot count: the same counter the gate refuses on. The rows are
        // named right away, so every verdict this answer can take (refused on
        // max_live, refused later, accepted) carries them.
        let (slot_row_entries, slot_claims) =
            spawn_gate::slot_reading(&registry_path, &mut warnings);
        let slots = slot_row_entries.len() + slot_claims;
        out.insert(
            "slot_rows".into(),
            json!(slot_row_entries
                .iter()
                .map(|r| json!({"name": r.name, "node": r.node, "provider": r.provider}))
                .collect::<Vec<_>>()),
        );

        let mut ram_row: Option<Value> = None;
        let mut cpu_rows: Vec<Value> = Vec::new();
        let mut cpu: Option<spawn_gate::AdmissionPayload> = None;

        if !lanes_only {
            // The slot cap is a probe refusal exactly as the gate refuses.
            if slots >= cap {
                return refuse_with(
                    "max_live",
                    format!("{slots} live worker slots >= max_live {cap}"),
                    json!({"count": slots, "max_live": cap}),
                    &[fleet_row(slots, cap)],
                    out,
                );
            }
            // The blueprint axis shows the same verdict a spawn would get
            //: gate-status never lies about a bp spawn's odds.
            let bp_name = opt_str_of(payload, "name").unwrap_or_default();
            let bp_live = spawn_gate::live_rows(&registry_path, &mut warnings);
            if let Err(receipt) = spawn_gate::check_blueprint_cap(
                &config_cwd,
                &registry_path,
                &bp_name,
                spawn_gate::gate_node().as_deref(),
                &bp_live,
            ) {
                let parsed: Value = serde_json::from_str(&receipt).unwrap_or(Value::Null);
                let reason = parsed
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("blueprint_cap")
                    .to_string();
                let remedy = parsed
                    .get("remedy")
                    .and_then(Value::as_str)
                    .unwrap_or("plan it in a native subagent (law d-94853e86)");
                let count = parsed
                    .get("count")
                    .and_then(Value::as_u64)
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                let max_word = parsed
                    .get("max_live")
                    .or_else(|| parsed.get("max_live_per_territory"))
                    .and_then(Value::as_u64)
                    .map(|m| m.to_string())
                    .unwrap_or_default();
                return refuse_with(
                    &reason,
                    format!("{count} live blueprint row(s) at cap {max_word}; {remedy}"),
                    parsed,
                    &[],
                    out,
                );
            }
            // Memory terms: the SAME call the gate makes, so the probe and
            // the gate cannot disagree about what refuses.
            let mem = spawn_gate::read_memory(floor_gb, swap_cap);
            let mem_term =
                spawn_gate::ram_floor_term(mem.avail, floor_gb, mem.swap, mem.swapin_bps, swap_cap);
            ram_row = Some(ram_floor_row(&mem, floor_gb, swap_cap, &mem_term));
            if let Some((reason, term)) = mem_term {
                return refuse_with(
                    reason,
                    term,
                    json!({
                        "available_gb": mem.avail,
                        "min_free_gb": floor_gb,
                        "swap_used_pct": mem.swap,
                        "max_swap_pct": swap_cap,
                        "swapin_mib_per_s": mem.swapin_bps.map(|r| r / spawn_gate::MIB),
                    }),
                    &make_rows(None, slots, cap, ram_row, Vec::new()),
                    out,
                );
            }
            // CPU axis: one footprint reading feeds the verdict and the rows.
            let (prefetched, probe_err) = match spawn_gate::footprint_cause_raw() {
                Ok(raw) => (Some(raw), None),
                Err(why) => (None, Some(why)),
            };
            let admission = spawn_gate::check_cpu_axis(prefetched.as_deref(), probe_err.as_deref());
            cpu_rows = vec![cpu_share_row(&admission.payload)];
            match admission.payload.verdict.as_str() {
                "hold" => {
                    return refuse_with(
                        "fleet_cpu_share",
                        admission.payload.reason.clone(),
                        json!({
                            "axis": "fleet_cpu_share",
                            "share_low": admission.payload.share_low,
                            "ceiling": admission.payload.ceiling,
                        }),
                        &make_rows(None, slots, cap, ram_row, cpu_rows),
                        out,
                    );
                }
                "refuse" | "undecidable" => {
                    let token = if admission.payload.axis == "cpu_instrument" {
                        "cpu_instrument_unreadable"
                    } else {
                        "cpu_share_undecidable"
                    };
                    return refuse_with(
                        token,
                        admission.payload.reason.clone(),
                        json!({
                            "axis": admission.payload.axis,
                            "share_low": admission.payload.share_low,
                            "share_high": admission.payload.share_high,
                        }),
                        &make_rows(None, slots, cap, ram_row, cpu_rows),
                        out,
                    );
                }
                _ => {}
            }
            cpu = Some(admission.payload);
        }

        // King share: only a caller whose session resolved is checked.
        let reading = spawn_gate_lanes::share_reading(&registry_path, cap, caller.as_deref());
        if let (Some(caller), Some(kings), Some(share), Some(held)) = (
            caller.as_deref(),
            reading.kings,
            reading.share,
            reading.held,
        ) {
            if !caller.is_empty() && held >= share {
                let mut message = format!(
                    "this reign holds {held} of max_live {cap} across {kings} kings (share {share})"
                );
                message.push_str(&crate::spawn_gate::held_rows_suffix(
                    reading.held_rows.as_ref(),
                ));
                return refuse_with(
                    "king_share",
                    message,
                    json!({
                        "king": caller,
                        "held": held,
                        "share": share,
                        "max_live": cap,
                        "kings": kings,
                        "held_rows": reading.held_rows.clone().unwrap_or_default(),
                    }),
                    &make_rows(None, slots, cap, ram_row, cpu_rows),
                    out,
                );
            }
        }

        // The lanes: every capped provider AND every provider a live row names.
        let lanes = match lanes_result {
            Ok(lanes) => lanes,
            Err(fault) => {
                return json!({
                    "verdict": "unknown",
                    "reason": "lane_count_unavailable",
                    "provider": fault.provider,
                    "error": fault.error,
                });
            }
        };

        // Lanes refuse only when EVERY capped lane is full.
        let mut full: Vec<String> = Vec::new();
        let mut capped_lanes = 0usize;
        for (provider, lane) in lanes.as_object().map(|m| m.iter()).into_iter().flatten() {
            let Some(lane_cap) = lane.get("cap").and_then(Value::as_u64) else {
                continue;
            };
            capped_lanes += 1;
            let live = lane.get("live").and_then(Value::as_u64).unwrap_or(0);
            if live >= lane_cap {
                full.push(format!("{provider} {live}/{lane_cap}"));
            }
        }
        if capped_lanes > 0 && full.len() == capped_lanes {
            return refuse_with(
                "provider_cap",
                format!("every dispatch lane at cap: {}", full.join(", ")),
                json!({"lanes": lanes}),
                &make_rows(Some(&lanes), slots, cap, ram_row, cpu_rows),
                out,
            );
        }

        // Accepted: the readings that admitted it, a trigger is only
        // actionable beside its reading.
        out.insert("verdict".into(), json!("accepted"));
        out.insert("lanes".into(), lanes.clone());
        out.insert("live_workers".into(), json!(slots));
        out.insert("max_live".into(), json!(cap));
        out.insert("slots".into(), json!(slots));
        out.insert("share".into(), share_json(&reading));
        if let Some(payload_adm) = &cpu {
            out.insert("share_low".into(), json!(payload_adm.share_low));
            out.insert("ceiling".into(), json!(payload_adm.ceiling));
        }
        if floor_gb > 0.0 {
            out.insert("min_free_gb".into(), json!(floor_gb));
            // available_ram_gb rides only when the RAM read ran.
        }
        if swap_cap > 0.0 {
            out.insert("max_swap_pct".into(), json!(swap_cap));
            // swap_used_pct rides only when the swap read ran.
        }
        out.insert(
            "rows".into(),
            json!(make_rows(Some(&lanes), slots, cap, ram_row, cpu_rows)),
        );
        Value::Object(out)
    }
}

fn unwrap_key(held: Option<(String, String)>) -> (Value, Value) {
    match held {
        Some((key, holder)) => (json!(key), json!(holder)),
        None => (Value::Null, Value::Null),
    }
}

/// A refused probe: verdict + reason + message + the refused fields, plus the
/// measurement rows read so far. A refusal is not the end of the answer: the
/// rows carry whatever the probe managed to read before it refused.
fn refuse_with(
    reason: &str,
    message: String,
    mut extra: Value,
    rows: &[Value],
    mut out: Map<String, Value>,
) -> Value {
    out.insert("verdict".into(), json!("refused"));
    out.insert("reason".into(), json!(reason));
    out.insert("message".into(), json!(message));
    if let Some(obj) = extra.as_object_mut() {
        let moved: Vec<(String, Value)> =
            obj.iter_mut().map(|(k, v)| (k.clone(), v.take())).collect();
        for (k, v) in moved {
            out.insert(k, v);
        }
    }
    out.insert("rows".into(), json!(rows));
    Value::Object(out)
}

/// The fleet-rows Gate dict.
fn fleet_row(slots: usize, cap: usize) -> Value {
    let mut row = Map::new();
    row.insert("name".into(), json!("fleet-rows"));
    row.insert("measured".into(), json!(slots.to_string()));
    row.insert("threshold".into(), json!(cap.to_string()));
    row.insert(
        "verdict".into(),
        json!(if slots >= cap { "refuse" } else { "pass" }),
    );
    row.insert("key".into(), json!("agents.max_live"));
    row.insert(
        "note".into(),
        json!("x-aaaa: rows are not what the machine spends; see the machine gates"),
    );
    Value::Object(row)
}

/// The Gate-dict row list: provider-lane per lane, fleet-rows, then whatever
/// machine rows the mode read (RAM floor, CPU share).
fn make_rows(
    lanes: Option<&Value>,
    slots: usize,
    cap: usize,
    ram_row: Option<Value>,
    cpu_rows: Vec<Value>,
) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    if let Some(lanes) = lanes {
        let mut lane_rows: Vec<(String, Value)> = lanes
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        lane_rows.sort_by(|a, b| a.0.cmp(&b.0));
        for (provider, lane) in lane_rows {
            let Some(live) = lane.get("live").and_then(Value::as_u64) else {
                continue;
            };
            let lane_cap = lane.get("cap").and_then(Value::as_u64);
            let full = lane_cap.is_some_and(|c| live >= c);
            let mut row = Map::new();
            row.insert("name".into(), json!("provider-lane"));
            row.insert("measured".into(), json!(format!("{live} ({provider})")));
            row.insert(
                "threshold".into(),
                json!(lane_cap
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "uncapped".into())),
            );
            row.insert(
                "verdict".into(),
                json!(if full { "refuse" } else { "pass" }),
            );
            row.insert(
                "key".into(),
                json!(format!("agents.provider_limits.{provider}.lanes")),
            );
            rows.push(Value::Object(row));
        }
    }
    rows.push(fleet_row(slots, cap));
    rows.extend(ram_row);
    rows.extend(cpu_rows);
    rows
}

fn ram_floor_row(
    mem: &spawn_gate::MemoryReading,
    floor_gb: f64,
    swap_cap: f64,
    term: &Option<(&'static str, String)>,
) -> Value {
    let avail_word = if floor_gb <= 0.0 {
        "off".to_string()
    } else {
        mem.avail
            .map(|v| format!("{v:.1}GB"))
            .unwrap_or_else(|| "unreadable".into())
    };
    let swap_word = if swap_cap <= 0.0 {
        "off".to_string()
    } else {
        mem.swap
            .map(|v| format!("{v:.1}%"))
            .unwrap_or_else(|| "unreadable".into())
    };
    let swapin_word = if swap_cap <= 0.0 {
        "off".to_string()
    } else if mem.swap.is_none_or(|s| s < swap_cap) {
        "not sampled (under cap)".to_string()
    } else {
        mem.swapin_bps
            .map(|r| format!("{:.1} MiB/s", r / spawn_gate::MIB))
            .unwrap_or_else(|| "unreadable".into())
    };
    let mut row = Map::new();
    row.insert("name".into(), json!("ram-floor"));
    row.insert(
        "measured".into(),
        json!(format!(
            "{avail_word} / swap {swap_word} / swap-in {swapin_word}"
        )),
    );
    row.insert(
        "threshold".into(),
        json!(format!("{floor_gb:.1}GB / cap {swap_cap:.0}%")),
    );
    row.insert(
        "verdict".into(),
        json!(if term.is_some() {
            "refuse"
        } else if floor_gb <= 0.0 && swap_cap <= 0.0 {
            "skipped: RAM and swap unchecked"
        } else {
            "pass"
        }),
    );
    row.insert(
        "key".into(),
        json!("agents.min_free_gb / agents.max_swap_pct"),
    );
    Value::Object(row)
}

fn cpu_share_row(payload: &spawn_gate::AdmissionPayload) -> Value {
    let unreadable = payload.axis == "cpu_instrument";
    let verdict = match payload.verdict.as_str() {
        "refuse" | "undecidable" => "refuse",
        "hold" => "hold",
        _ => "pass",
    };
    let mut row = Map::new();
    row.insert("name".into(), json!("cpu-share"));
    row.insert(
        "measured".into(),
        json!(if unreadable {
            "unreadable".to_string()
        } else {
            format!(
                "{:.2}/{:.2} cores",
                payload.fleet_cores, payload.capacity_cores
            )
        }),
    );
    row.insert(
        "threshold".into(),
        json!(if unreadable {
            "-".to_string()
        } else {
            format!("{:.0}%", payload.ceiling * 100.0)
        }),
    );
    row.insert("verdict".into(), json!(verdict));
    row.insert("key".into(), json!("agents.max_fleet_cpu_share"));
    row.insert("note".into(), json!(payload.reason));
    Value::Object(row)
}

fn share_json(reading: &spawn_gate_lanes::ShareReading) -> Value {
    let mut share = Map::new();
    share.insert("kings".into(), json!(reading.kings));
    share.insert("share".into(), json!(reading.share));
    share.insert("held".into(), json!(reading.held));
    share.insert(
        "held_rows".into(),
        json!(reading.held_rows.clone().unwrap_or_default()),
    );
    share.insert(
        "unattributed".into(),
        json!(reading
            .unattributed_rows
            .clone()
            .map(|rows| {
                let mut un = Map::new();
                un.insert("count".into(), json!(rows.len()));
                un.insert("rows".into(), json!(rows));
                Value::Object(un)
            })
            .unwrap_or(Value::Null)),
    );
    Value::Object(share)
}

/// The lanes block: every capped provider (the configured table, else the
/// built-in budgets) AND every provider a live row names, capped or not.
/// `Err` = one lane count faulted, which is the probe's unknown verdict,
/// never a zero.
fn lanes_answer(
    config_cwd: &std::path::Path,
    registry_path: &std::path::Path,
    warnings: &mut Vec<String>,
) -> Result<Value, spawn_gate_lanes::LaneFault> {
    let home = crate::paths::AgentsHome::from_env();
    let now_epoch = crate::provider_cap::now_epoch_secs();
    let quota_states = crate::provider_cap::provider_quota_states(&home, now_epoch, 1_800);
    let quota_source = match crate::provider_cap::read_persisted_snapshot(&home) {
        Some(snapshot) if now_epoch.saturating_sub(snapshot.measured_at_epoch) <= 1_800 => {
            "snapshot"
        }
        Some(_) => "stale-snapshot",
        None => "no-snapshot",
    };
    let mut providers: Vec<String> = Vec::new();
    if let Some(table) = agents_config::config_lookup(config_cwd, &["agents", "provider_limits"])
        .and_then(|t| {
            t.as_table()
                .map(|t| t.keys().cloned().collect::<Vec<String>>())
        })
    {
        providers.extend(table);
    } else {
        providers.push("zai".to_string());
    }
    if let Ok(registry) = crate::state::load_registry(registry_path) {
        let mut observed: Vec<String> = registry
            .entries
            .iter()
            .filter(|e| crate::spawn_gate::status_is_liveish(&e.status))
            .filter_map(|e| e.provider.clone())
            .filter(|p| !p.is_empty())
            .collect();
        observed.sort();
        observed.dedup();
        for p in observed {
            if !providers.contains(&p) {
                providers.push(p);
            }
        }
    }
    providers.sort();
    providers.dedup();

    let mut lanes = Map::new();
    // One journal read for the whole probe: every provider's lane count
    // judges the same waiting-worker question at the same instant.
    let questions_raw = spawn_gate_lanes::read_questions_journal(registry_path, warnings);
    for provider in providers {
        let cap = spawn_gate_lanes::provider_lanes_cap(config_cwd, &provider);
        match spawn_gate_lanes::provider_live_count_with_questions(
            registry_path,
            &provider,
            &questions_raw,
            warnings,
        ) {
            Ok((live, counted, parked)) => {
                let mut lane = Map::new();
                lane.insert("cap".into(), json!(cap));
                lane.insert("live".into(), json!(live));
                lane.insert("counted".into(), json!(counted));
                lane.insert(
                    "parked".into(),
                    json!(parked
                        .iter()
                        .map(|(name, qid)| serde_json::json!({
                            "name": name,
                            "question_id": json!(qid),
                        }))
                        .collect::<Vec<_>>()),
                );
                lane.insert(
                    "quota".into(),
                    json!(quota_states
                        .as_ref()
                        .and_then(|states| states.get(&provider))
                        .cloned()
                        .unwrap_or_else(|| "unmeasured".into())),
                );
                lanes.insert(provider, Value::Object(lane));
            }
            Err(error) => {
                return Err(spawn_gate_lanes::LaneFault { provider, error });
            }
        }
    }
    lanes.insert("quota_source".into(), json!(quota_source));
    Ok(Value::Object(lanes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spawn_gate::SWAPIN_REFUSE_BYTES_PER_S;

    fn mem(
        avail: Option<f64>,
        swap: Option<f64>,
        swapin_bps: Option<f64>,
    ) -> spawn_gate::MemoryReading {
        spawn_gate::MemoryReading {
            avail,
            swap,
            swapin_bps,
        }
    }

    /// A disabled term (`<= 0`) never renders refuse, whatever the machine
    /// reads, and a disabled reading renders `off`, never `unreadable`.
    #[test]
    fn ram_floor_row_disabled_cap_never_refuses() {
        let cases = [
            (
                mem(Some(35.0), Some(40.0), Some(SWAPIN_REFUSE_BYTES_PER_S)),
                4.0,
                0.0,
            ),
            (mem(None, Some(5.0), None), 0.0, 90.0),
        ];
        for (reading, floor, cap) in cases {
            let row = ram_floor_row(&reading, floor, cap, &None);
            assert_eq!(row["verdict"].as_str().unwrap(), "pass");
        }
        // Disabled cap: swap and swap-in render off.
        let row = ram_floor_row(&mem(Some(35.0), None, None), 4.0, 0.0, &None);
        assert!(
            row["measured"].as_str().unwrap().contains("off"),
            "a disabled term renders off"
        );
    }

    /// An enabled cap still refuses at the ceiling: over-cap swap beside a
    /// live swap-in rate.
    #[test]
    fn ram_floor_row_refuses_at_the_ceiling() {
        let reading = mem(Some(35.0), Some(94.0), Some(SWAPIN_REFUSE_BYTES_PER_S));
        let term =
            spawn_gate::ram_floor_term(reading.avail, 4.0, reading.swap, reading.swapin_bps, 90.0);
        let row = ram_floor_row(&reading, 4.0, 90.0, &term);
        assert_eq!(row["verdict"].as_str().unwrap(), "refuse");
    }

    /// The probe mirrors the gate's quota refusal: a payload naming a
    /// route_provider whose lane is walled reads refused, never accepted.
    #[test]
    fn probe_refuses_a_route_provider_with_a_walled_lane() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-lanequota-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("agents-home");
        std::fs::create_dir_all(home.join("provider-cap")).unwrap();
        std::env::set_var(crate::paths::HOME_ENV, &home);
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", dir.join(".fno").join("config.toml"));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        std::fs::write(
            home.join("provider-cap").join("snapshot.json"),
            format!(
                r#"{{"lanes":[{{"lane":"zai:default","provider":"zai","account":"default","reset_epoch":{},"reset_passed_epoch":null,"missing_reset_timezone":[],"state":"open","members":[]}}],"measured_at":"probe","measured_at_epoch":{}}}"#,
                now + 600,
                now
            ),
        )
        .unwrap();

        let answer = probe::answer(&json!({
            "name": "probe-lane-lock",
            "substrate": "headless",
            "route_provider": "zai",
            "account": ""
        }));

        std::env::remove_var(crate::paths::HOME_ENV);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        assert_eq!(answer["verdict"], "refused");
        assert_eq!(answer["reason"], "provider_quota_lock");
        assert_eq!(answer["lane"], "zai:default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Over-cap allocation with no swap-ins renders pass, and the measured
    /// word carries the swap-in reading.
    #[test]
    fn ram_floor_row_admits_allocated_swap_with_no_swapins() {
        let reading = mem(Some(35.0), Some(94.7), Some(0.0));
        let term =
            spawn_gate::ram_floor_term(reading.avail, 4.0, reading.swap, reading.swapin_bps, 90.0);
        let row = ram_floor_row(&reading, 4.0, 90.0, &term);
        assert_eq!(row["verdict"].as_str().unwrap(), "pass");
        assert!(
            row["measured"].as_str().unwrap().contains("0.0 MiB/s"),
            "the measured word carries the swap-in reading"
        );
    }

    /// AC6-HP: the probe answer names the rows behind the slot count in every
    /// verdict, and `live_workers` stays the slot count (rows + reservations),
    /// so `fno agents gate-status` can show them with no second walk.
    #[test]
    fn probe_answer_names_its_slot_rows() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("agents-home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var(crate::paths::HOME_ENV, &home);
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        // Pin the whole config: FNO_CONFIG is the sole candidate when set, so
        // a parallel test's stray FNO_CONFIG (or no config at all, whose
        // max_live default is 3) cannot flip the verdict under this fixture.
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            // max_swap_pct 0 disables the swap term: the machine this runs on
            // may genuinely sit above the default 90 percent cap, which
            // would refuse the probe before the assertions.
            "[agents]\nmax_live = 28\nmin_free_gb = 0\nmax_swap_pct = 0\n",
        )
        .unwrap();
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", fnodir.join("config.toml"));
        let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
        std::env::set_var(
            "FNO_TEST_FOOTPRINT_PAYLOAD",
            r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#,
        );

        let me = std::process::id();
        let good = crate::daemon::process_start_time(me).unwrap_or(0);
        let row = |name: &str| {
            format!(
                r#"{{"name":"{name}","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","pid":{me},"pid_start_time":{good}}}"#
            )
        };
        std::fs::write(
            home.join("registry.json"),
            format!(
                r#"{{"schema_version":{},"entries":[{}, {}, {}]}}"#,
                crate::state::REGISTRY_SCHEMA_VERSION,
                row("p1"),
                row("p2"),
                row("p2") // duplicate name, still a distinct row the count sees
            ),
        )
        .unwrap();

        let answer = probe::answer(&json!({}));
        let slot_rows = answer["slot_rows"].as_array().expect("slot_rows array");
        assert_eq!(slot_rows.len(), 3, "{slot_rows:?}");
        for r in slot_rows {
            assert!(r.get("name").is_some(), "{r:?}");
            assert!(r.get("node").is_some(), "{r:?}");
            assert!(r.get("provider").is_some(), "{r:?}");
        }
        assert_eq!(answer["live_workers"], 3, "slots stay rows + reservations");
        assert_eq!(answer["verdict"], "accepted");

        std::env::remove_var(crate::paths::HOME_ENV);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        match prior_payload {
            Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
            None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The probe's king_share refusal names the rows it charged to the
    /// caller, in `message` and as a `held_rows` key, from the same reading
    /// `share_json` reports in status mode.
    #[test]
    fn probe_king_share_refusal_names_the_held_rows() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-kingshare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("agents-home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var(crate::paths::HOME_ENV, &home);
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            // max_live 2 with one king divides to share 2; the fixture rows
            // carry no pid, so the fleet slot count stays 0 and the refusal
            // the probe answers with is the king share, not max_live.
            "[agents]\nmax_live = 2\nmin_free_gb = 0\nmax_swap_pct = 0\n",
        )
        .unwrap();
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", fnodir.join("config.toml"));
        let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
        std::env::set_var(
            "FNO_TEST_FOOTPRINT_PAYLOAD",
            r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#,
        );
        let crowned = r#"{"name":"king-a","harness":"claude","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","crown_level":1,"harness_session_id":"session-aaaaaaaa"}"#;
        let worker = |name: &str, status: &str| {
            format!(
                r#"{{"name":"{name}","harness":"claude","provider":"zai","cwd":"/tmp","status":"{status}","created_at":"2026-01-01T00:00:00Z","spawned_by_session":"session-aaaaaaaa"}}"#
            )
        };
        std::fs::write(
            home.join("registry.json"),
            format!(
                r#"{{"schema_version":{},"entries":[{},{},{},{}]}}"#,
                crate::state::REGISTRY_SCHEMA_VERSION,
                crowned,
                worker("w1", "live"),
                worker("w2", "live"),
                // The stopped shape: a row the stop wrote terminal while the
                // process survived. The share does not charge it, so the
                // naming must not either - the sweep that re-marks it live
                // is the one that puts it back in the count.
                worker("w3", "orphaned")
            ),
        )
        .unwrap();

        let answer = probe::answer(&json!({
            "name": "probe-kingshare",
            "substrate": "bg",
            "caller_session": "session-aaaaaaaa"
        }));

        std::env::remove_var(crate::paths::HOME_ENV);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        match prior_payload {
            Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
            None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
        }
        assert_eq!(answer["verdict"], "refused");
        assert_eq!(answer["reason"], "king_share");
        assert_eq!(
            answer["message"],
            "this reign holds 2 of max_live 2 across 1 kings (share 2); the rows charged to you are w1, w2"
        );
        assert_eq!(answer["held_rows"], json!(["w1", "w2"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn probe_max_live_refusal_carries_provider_lane_quota_state() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-lanes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("agents-home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var(crate::paths::HOME_ENV, &home);
        let prior_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 2\nmin_free_gb = 0\nmax_swap_pct = 0\n",
        )
        .unwrap();
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", fnodir.join("config.toml"));
        let me = std::process::id();
        let good = crate::daemon::process_start_time(me).unwrap_or(0);
        let row = |name: &str| {
            format!(
                r#"{{"name":"{name}","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","pid":{me},"pid_start_time":{good}}}"#
            )
        };
        std::fs::write(
            home.join("registry.json"),
            format!(
                r#"{{"schema_version":{},"entries":[{},{},{}]}}"#,
                crate::state::REGISTRY_SCHEMA_VERSION,
                row("p1"),
                row("p2"),
                row("p3")
            ),
        )
        .unwrap();

        let answer = probe::answer(&json!({}));

        assert_eq!(answer["verdict"], "refused");
        assert_eq!(answer["reason"], "max_live");
        assert_eq!(answer["lanes"]["zai"]["live"], 3);
        assert_eq!(answer["lanes"]["zai"]["quota"], "unmeasured");
        assert_eq!(answer["lanes"]["quota_source"], "no-snapshot");

        std::fs::create_dir_all(home.join("provider-cap")).unwrap();
        std::fs::write(
            home.join("provider-cap").join("snapshot.json"),
            serde_json::json!({
                "lanes": [{
                    "lane": "zai:default",
                    "provider": "zai",
                    "account": "default",
                    "reset_epoch": null,
                    "reset_passed_epoch": null,
                    "missing_reset_timezone": [],
                    "state": "closed",
                    "members": []
                }],
                "measured_at": "probe",
                "measured_at_epoch": crate::provider_cap::now_epoch_secs()
            })
            .to_string(),
        )
        .unwrap();
        let snapshot_answer = probe::answer(&json!({}));
        assert_eq!(snapshot_answer["lanes"]["zai"]["quota"], "closed");
        assert_eq!(snapshot_answer["lanes"]["quota_source"], "snapshot");

        std::env::remove_var(crate::paths::HOME_ENV);
        match prior_claims_root {
            Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
            None => std::env::remove_var("FNO_CLAIMS_ROOT"),
        }
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC1-HP: a gate payload with no `holder_pid` is refused before the gate
    /// runs, so the verb mints no reservation whose holder pid (the verb's
    /// own, dead once it has answered) dooms the row to Suspect for the whole
    /// worker TTL - the 2026-09-15 zai lane wedge.
    #[test]
    fn gate_refuses_a_payload_without_holder_pid() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-nopid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("agents-home");
        std::fs::create_dir_all(home.join("provider-cap")).unwrap();
        std::env::set_var(crate::paths::HOME_ENV, &home);
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        std::fs::create_dir_all(dir.join("claims-root").join(".fno").join("claims")).unwrap();
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", dir.join(".fno").join("config.toml"));
        let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
        std::env::set_var(
            "FNO_TEST_FOOTPRINT_PAYLOAD",
            r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#,
        );

        let answer = gate_answer(&json!({
            "mode": "gate",
            "name": "probe-no-pid",
            "substrate": "headless",
            "route_provider": "zai",
            "account": "",
            "no_wait": true
        }));

        let claims_dir = dir.join("claims-root").join(".fno").join("claims");
        let leftovers: Vec<_> = std::fs::read_dir(&claims_dir).unwrap().flatten().collect();
        std::env::remove_var(crate::paths::HOME_ENV);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        match prior_payload {
            Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
            None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(answer["status"], "refused");
        assert_eq!(answer["receipt"]["reason"], "holder_pid_required");
        assert_eq!(
            answer["exit_code"],
            spawn_gate::EXIT_GATE_UNAVAILABLE,
            "{answer}"
        );
        assert!(
            leftovers.is_empty(),
            "the claims dir holds {:?} after a pid-less refusal",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }
}

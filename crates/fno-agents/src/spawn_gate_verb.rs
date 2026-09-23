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
use crate::claims;
use crate::spawn_gate::{self, GateFlags, GateInput};
use crate::spawn_gate_lanes;

/// `spawn-gate`: one payload on stdin, one answer on stdout. Exit 0 whenever
/// an answer was produced, including a refused answer. An unreadable payload
/// is a loud non-zero: the transport turns that into a gate-unavailable
/// refusal, never an admit.
pub fn run_spawn_gate(args: &[String]) -> i32 {
    // The reserve mode is argv-typed, so a king can type it in one line; the
    // gate and probe modes keep their stdin JSON payloads.
    if args.first().map(String::as_str) == Some("reserve") {
        let config_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        return reserve_spawn_gate(&config_cwd, &args[1..]);
    }
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

/// The reservation TTL ceiling: only TTL expiry frees a claim and pid death
/// does not (claims.rs classification), so a four-hour reservation with a
/// dead holder is the measured four-hour lane wedge. The ceiling bounds the
/// longest a reservation can hold a slot.
pub(crate) const RESERVATION_MAX_TTL_MS: i64 = 15 * 60 * 1000;
/// A reservation holds its lane for its whole TTL when unredeemed, so the
/// default is short.
const RESERVATION_DEFAULT_TTL_MS: i64 = 10 * 60 * 1000;

/// `fno-agents spawn-gate reserve <name> --provider <p> [--ttl 10m] --reason "<why>" [--node <id>]`
///
/// Mints `worker:<name>` under the global claims root with `model_provider`,
/// `reserved_by`, `reserved_reason` (and `node` when given) metadata, anchored
/// to this process's pid with an explicit TTL. Two guards refuse before any
/// write: a TTL over [`RESERVATION_MAX_TTL_MS`], and a lane whose reservations
/// would reach the lane cap (at least one slot on every capped lane stays
/// winnable first-come). Release early with `fno agents claim release --force`
/// or wait out the TTL; nothing here queues.
pub(crate) fn reserve_spawn_gate(config_cwd: &std::path::Path, args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("--help")
        || args.first().map(String::as_str) == Some("-h")
    {
        println!("usage: fno-agents spawn-gate reserve <name> --provider <p> [--ttl 10m] --reason \"<why>\" [--node <id>]");
        println!("{}", crate::spawn_gate_reservations::RESERVATION_RULE);
        return 0;
    }
    let mut name: Option<String> = None;
    let mut provider: Option<String> = None;
    let mut node: Option<String> = None;
    let mut ttl_arg: Option<String> = None;
    let mut reason: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--provider" | "--ttl" | "--reason" | "--node" => {
                let flag = arg.as_str();
                let Some(value) = it.next() else {
                    eprintln!("spawn-gate: {flag} needs a value");
                    return 2;
                };
                if value.starts_with("--") {
                    eprintln!("spawn-gate: {flag} needs a value, got the flag {value:?}");
                    return 2;
                }
                match flag {
                    "--provider" => provider = Some(value.clone()),
                    "--ttl" => ttl_arg = Some(value.clone()),
                    "--reason" => reason = Some(value.clone()),
                    _ => node = Some(value.clone()),
                }
            }
            a if a.starts_with("--") => {
                eprintln!("spawn-gate: unknown reserve flag {a:?}");
                return 2;
            }
            a => {
                if name.is_some() {
                    eprintln!("spawn-gate: reserve takes ONE name; got {a:?} too");
                    return 2;
                }
                name = Some(a.to_string());
            }
        }
    }
    let Some(name) = name else {
        eprintln!("spawn-gate: reserve needs a worker name and --provider");
        println!("{}", crate::spawn_gate_reservations::RESERVATION_RULE);
        return 2;
    };
    let Some(provider) = provider else {
        eprintln!("spawn-gate: reserve needs --provider");
        return 2;
    };
    let Some(reason) = reason else {
        eprintln!("spawn-gate: reserve needs --reason (why this lane slot is held)");
        return 2;
    };
    let ttl_ms = match ttl_arg.as_deref() {
        None => RESERVATION_DEFAULT_TTL_MS,
        Some(raw) => match claims::parse_ttl_ms(raw) {
            Some(ms) if ms <= RESERVATION_MAX_TTL_MS => ms,
            Some(ms) => {
                eprintln!(
                    "spawn-gate: refusing --ttl {raw}: {ms}ms is over the {}s reservation ceiling. Only TTL expiry frees a claim, pid death does not; a four-hour reservation with a dead holder is what wedged the zai lane once.",
                    RESERVATION_MAX_TTL_MS / 1000
                );
                return 2;
            }
            None => {
                eprintln!("spawn-gate: unparsable --ttl {raw:?}; want 10m, 90s, 600");
                return 2;
            }
        },
    };
    // The lane headroom guard: minting refuses when the slot claims live on
    // that lane would reach the lane cap, so at least one slot on every
    // capped lane is always winnable first-come. An uncapped lane skips the
    // guard: nothing to starve.
    let mut lane_warnings = Vec::new();
    if let Some(cap) = spawn_gate_lanes::provider_lanes_cap(config_cwd, &provider) {
        let held = match spawn_gate_lanes::provider_live_slot_claims(
            &provider,
            &[],
            None,
            &mut lane_warnings,
        ) {
            Ok((n, _)) => n,
            Err(e) => {
                eprintln!("spawn-gate: reserve could not read the lane: {e}");
                return 2;
            }
        };
        if held + 1 >= cap {
            eprintln!(
                "spawn-gate: refusing reserve {name} on lane {provider}: {} reservation(s) \
                 plus this one would reach the lane cap {cap}, and at least one slot on \
                 every capped lane stays winnable first-come. {}",
                held + 1,
                crate::spawn_gate_reservations::RESERVATION_RULE
            );
            return 2;
        }
    }
    // Mint the claim. holder is the resolved caller session when ambient
    // identity resolves; the pid defaults to this process, and the explicit
    // TTL (never HOLDER_PROCESS provenance) is what frees the lane.
    let (session, _harness) = claims::resolve_identity();
    let holder = session.unwrap_or_else(|| format!("reserve:{}", std::process::id()));
    let mut metadata = serde_json::Map::new();
    metadata.insert("model_provider".into(), json!(&provider));
    metadata.insert("reserved_by".into(), json!(&holder));
    metadata.insert("reserved_reason".into(), json!(&reason));
    if let Some(node) = &node {
        metadata.insert("node".into(), json!(node));
    }
    let key = format!("worker:{name}");
    let outcome = claims::acquire(
        &key,
        &holder,
        claims::AcquireOpts {
            pid: Some(std::process::id()),
            ttl_ms: Some(ttl_ms),
            reason: Some(reason),
            metadata: Some(metadata),
            root: claims::global_claims_root(),
            ..Default::default()
        },
    );
    match outcome {
        claims::AcquireOutcome::Acquired(record) => {
            println!(
                "{}",
                json!({
                    "status": "reserved",
                    "key": key,
                    "provider": provider,
                    "holder": holder,
                    "expires_at": record.expires_at,
                    "redeem": "the spawn carried --name <this name> redeems it at admission",
                })
            );
            0
        }
        claims::AcquireOutcome::HeldByOther { holder: h, .. } => {
            eprintln!(
                "spawn-gate: refusing reserve {name}: {key} is already held by {h}; \
                 a live worker's own claim is never a reservation"
            );
            1
        }
        claims::AcquireOutcome::Error(e) => {
            eprintln!("spawn-gate: reserve failed: {e}");
            2
        }
    }
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
        node: opt_str_of(payload, "node"),
        account: opt_str_of(payload, "account"),
        caller_session: opt_str_of(payload, "caller_session"),
        holder_pid: Some(holder_pid as u32),
        seed: opt_str_of(payload, "seed"),
        session_phase: opt_str_of(payload, "session_phase"),
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
            let message = format!(
                "registry schema {} ahead of schema {} this fno understands; run fno doctor update",
                receipt
                    .get("on_disk")
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                receipt
                    .get("understood")
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            );
            return refuse_with(
                "registry_schema",
                message,
                json!({
                    "on_disk": receipt.get("on_disk").cloned().unwrap_or(Value::Null),
                    "understood": receipt.get("understood").cloned().unwrap_or(Value::Null),
                }),
                &[],
                out,
            );
        }

        // Read provider lanes before any refusal so the answer preserves the
        // quota evidence that explains a busy fleet. The probe reads the
        // named spawn's own odds: a reservation minted for that name redeems
        // at the gate, so the readout skips it the same way the gate does.
        let probe_name = opt_str_of(payload, "name");
        let lanes_result = lanes_answer(
            &config_cwd,
            &registry_path,
            probe_name.as_deref().filter(|n| !n.is_empty()),
            &mut warnings,
        );
        if let Ok(lanes) = &lanes_result {
            out.insert("lanes".into(), lanes.clone());
        }

        match crate::fleet_incident::verdict() {
            crate::fleet_incident::Verdict::Clear(_) => {}
            crate::fleet_incident::Verdict::Stopped(record) => {
                return refuse_with(
                    "fleet-stop",
                    format!(
                        "fleet incident stop is active (generation {}, reason: {})",
                        record.generation, record.reason
                    ),
                    json!({"generation": record.generation}),
                    &[],
                    out,
                );
            }
            crate::fleet_incident::Verdict::Unavailable(detail) => {
                return refuse_with(
                    "fleet-stop-unavailable",
                    format!("fleet incident state is unreadable ({detail})"),
                    json!({"detail": detail}),
                    &[],
                    out,
                );
            }
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
        // max_live, refused later, accepted) carries them. Reservations are
        // named beside the registry rows, each entry tagged with its kind.
        let (slot_row_entries, slot_reservations) =
            spawn_gate::slot_reading(&registry_path, &mut warnings);
        let slots = slot_row_entries.len() + slot_reservations.len();
        let mut slot_rows_json: Vec<Value> = slot_row_entries
            .iter()
            .map(|r| {
                json!({"kind": "registry", "name": r.name, "node": r.node, "provider": r.provider})
            })
            .collect();
        slot_rows_json.extend(slot_reservations.iter().map(|r| {
            json!({
                "kind": "reservation",
                "name": r.name,
                "holder": r.holder,
                "pid": r.pid,
                "age_s": r.age_s,
                "state": r.state,
                "provider": r.provider,
            })
        }));
        out.insert("slot_rows".into(), json!(slot_rows_json));

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
    let mut refusal_rows = rows.to_vec();
    if !refusal_rows.iter().any(|row| {
        matches!(
            row.get("verdict").and_then(Value::as_str),
            Some("refuse" | "hold")
        )
    }) {
        refusal_rows.push(json!({
            "name": "gate-verdict",
            "measured": reason,
            "threshold": "accepted",
            "verdict": "refuse",
            "note": message.clone(),
        }));
    }

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
    out.insert("rows".into(), json!(refusal_rows));
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
    redeemer: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<Value, spawn_gate_lanes::LaneFault> {
    let home = crate::paths::AgentsHome::from_env();
    let now_epoch = crate::provider_cap::now_epoch_secs();
    let snapshot = crate::provider_cap::read_persisted_snapshot(&home);
    let fresh = snapshot
        .as_ref()
        .is_some_and(|s| now_epoch.saturating_sub(s.measured_at_epoch) <= 1_800);
    let quota_source = match snapshot.as_ref() {
        Some(_) if fresh => "snapshot",
        Some(_) => "stale-snapshot",
        None => "no-snapshot",
    };
    let quota_states = if fresh {
        snapshot
            .as_ref()
            .map(crate::provider_cap::quota_states_from_snapshot)
    } else {
        None
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
            redeemer,
            warnings,
        ) {
            Ok(reading) => {
                let mut lane = Map::new();
                lane.insert("cap".into(), json!(cap));
                lane.insert("live".into(), json!(reading.count));
                lane.insert("counted".into(), json!(reading.counted));
                lane.insert(
                    "reserved".into(),
                    json!(reading
                        .reserved
                        .iter()
                        .map(|(name, exp)| serde_json::json!({
                            "name": name,
                            "expires_at": json!(exp),
                        }))
                        .collect::<Vec<_>>()),
                );
                lane.insert(
                    "parked".into(),
                    json!(reading
                        .parked
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

    #[test]
    fn probe_registry_schema_refusal_names_its_reason_row() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("fno-verb-registry-schema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("agents-home");
        std::fs::create_dir_all(&home).unwrap();
        let registry_path = home.join("registry.json");
        let on_disk = crate::state::REGISTRY_SCHEMA_VERSION as u64 + 1;
        std::fs::write(
            &registry_path,
            format!(r#"{{"schema_version":{on_disk},"entries":[]}}"#),
        )
        .unwrap();

        let prior_home = std::env::var_os(crate::paths::HOME_ENV);
        let prior_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var(crate::paths::HOME_ENV, &home);
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        std::env::set_var("FNO_CONFIG", dir.join(".fno").join("config.toml"));

        let answer = probe::answer(&json!({
            "name": "probe-registry-schema",
            "substrate": "headless",
        }));

        match prior_home {
            Some(value) => std::env::set_var(crate::paths::HOME_ENV, value),
            None => std::env::remove_var(crate::paths::HOME_ENV),
        }
        match prior_claims_root {
            Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
            None => std::env::remove_var("FNO_CLAIMS_ROOT"),
        }
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }

        assert_eq!(answer["verdict"], "refused");
        assert_eq!(answer["reason"], "registry_schema");
        assert_eq!(answer["on_disk"], on_disk);
        let rows = answer["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "gate-verdict");
        assert_eq!(rows[0]["measured"], "registry_schema");
        assert_eq!(rows[0]["verdict"], "refuse");
        assert_eq!(rows[0]["note"], answer["message"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuse_with_emits_gate_verdict_when_no_measurement_row_refuses() {
        let message = "king share is active";
        let answer = refuse_with(
            "king_share",
            String::from(message),
            json!({}),
            &[],
            Map::new(),
        );

        let rows = answer["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "gate-verdict");
        assert_eq!(rows[0]["measured"], "king_share");
        assert_eq!(rows[0]["threshold"], "accepted");
        assert_eq!(rows[0]["verdict"], "refuse");
        assert_eq!(rows[0]["note"], message);
    }

    #[test]
    fn refuse_with_keeps_an_existing_refusal_without_adding_a_generic_row() {
        let measured = fleet_row(3, 3);
        let answer = refuse_with(
            "max_live",
            "fleet full".to_string(),
            json!({}),
            std::slice::from_ref(&measured),
            Map::new(),
        );

        let rows = answer["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], measured);
        assert_eq!(rows[0]["name"], "fleet-rows");
        assert_eq!(rows[0]["verdict"], "refuse");
    }

    #[test]
    fn refuse_with_keeps_a_held_measurement_without_adding_a_generic_row() {
        let held = json!({
            "name": "cpu-share",
            "measured": "2.10/12.00 cores",
            "threshold": "50%",
            "verdict": "hold",
            "note": "measurement unavailable",
        });
        let answer = refuse_with(
            "fleet_cpu_share",
            "CPU measurement unavailable".to_string(),
            json!({}),
            std::slice::from_ref(&held),
            Map::new(),
        );

        let rows = answer["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], held);
        assert_eq!(rows[0]["name"], "cpu-share");
        assert_eq!(rows[0]["verdict"], "hold");
    }

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
            assert!(r.get("kind").is_some(), "{r:?}");
            assert!(r.get("name").is_some(), "{r:?}");
            assert!(r.get("node").is_some(), "{r:?}");
            assert!(r.get("provider").is_some(), "{r:?}");
        }
        assert_eq!(answer["live_workers"], 3, "slots stay rows + reservations");
        assert_eq!(answer["verdict"], "accepted");
        assert_ne!(answer["reason"], "fleet-stop");

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

    #[test]
    fn probe_mirrors_fleet_incident_verdict_before_capacity_and_keeps_lanes() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-incident-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("agents-home");
        std::fs::create_dir_all(&home).unwrap();
        let prior_home = std::env::var_os(crate::paths::HOME_ENV);
        std::env::set_var(crate::paths::HOME_ENV, &home);
        let claims_root = dir.join("claims-root");
        let prior_claims_root = std::env::var_os("FNO_CLAIMS_ROOT");
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
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
        std::fs::write(
            home.join("registry.json"),
            serde_json::json!({
                "schema_version": crate::state::REGISTRY_SCHEMA_VERSION,
                "entries": [],
            })
            .to_string(),
        )
        .unwrap();
        let incident_path =
            crate::fleet_incident::fleet_stop_path(&crate::paths::AgentsHome::at(&home));
        let record = crate::fleet_incident::IncidentRecord {
            version: crate::fleet_incident::STATE_VERSION,
            state: "stopped".into(),
            generation: 19,
            changed_at: "2026-09-18T19:48:00Z".into(),
            changed_by: "test".into(),
            reason: "repro".into(),
            source: Some("file".into()),
        };
        std::fs::write(&incident_path, serde_json::to_string(&record).unwrap()).unwrap();

        let stopped = probe::answer(&json!({}));
        assert_eq!(stopped["verdict"], "refused");
        assert_eq!(stopped["reason"], "fleet-stop");
        assert!(stopped["message"]
            .as_str()
            .unwrap()
            .contains("generation 19"));
        assert!(stopped["lanes"].is_object(), "{stopped}");

        std::fs::write(&incident_path, b"broken").unwrap();
        let unreadable = probe::answer(&json!({}));
        assert_eq!(unreadable["verdict"], "refused");
        assert_eq!(unreadable["reason"], "fleet-stop-unavailable");
        assert!(unreadable["message"]
            .as_str()
            .unwrap()
            .contains("unreadable"));

        let _ = std::fs::remove_file(&incident_path);
        let clear = probe::answer(&json!({}));
        assert_eq!(clear["verdict"], "accepted");
        assert_ne!(clear["reason"], "fleet-stop");

        match prior_home {
            Some(value) => std::env::set_var(crate::paths::HOME_ENV, value),
            None => std::env::remove_var(crate::paths::HOME_ENV),
        }
        match prior_claims_root {
            Some(value) => std::env::set_var("FNO_CLAIMS_ROOT", value),
            None => std::env::remove_var("FNO_CLAIMS_ROOT"),
        }
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

    /// AC2-HP: the probe names a counted reservation in `slot_rows` with its
    /// kind and the fields an operator needs to free a dead one, and the row
    /// count still equals `live_workers`.
    #[test]
    fn probe_slot_rows_names_reservations() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("agents-home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var(crate::paths::HOME_ENV, &home);
        let claims_root = dir.join("claims-root");
        std::fs::create_dir_all(claims_root.join(".fno/claims")).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
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
                r#"{{"schema_version":{},"entries":[{}, {}]}}"#,
                crate::state::REGISTRY_SCHEMA_VERSION,
                row("p1"),
                row("p2")
            ),
        )
        .unwrap();

        // One counted reservation beside the two registry rows.
        let mut m = serde_json::Map::new();
        m.insert(
            "model_provider".to_string(),
            serde_json::Value::String("zai".into()),
        );
        let outcome = crate::claims::acquire(
            "worker:w-res",
            "spawn-gate:me:w-res",
            crate::claims::AcquireOpts {
                pid: Some(me),
                pid_provenance: Some(crate::claims::HOLDER_PROCESS.to_string()),
                ttl_ms: Some(3_600_000),
                metadata: Some(m),
                root: Some(claims_root.clone()),
                ..Default::default()
            },
        );
        assert!(
            matches!(outcome, crate::claims::AcquireOutcome::Acquired(_)),
            "{outcome:?}"
        );

        let answer = probe::answer(&json!({}));
        let slot_rows = answer["slot_rows"].as_array().expect("slot_rows array");
        assert_eq!(slot_rows.len(), 3, "{slot_rows:?}");
        let res = slot_rows
            .iter()
            .find(|r| r["kind"] == "reservation")
            .expect("the reservation is named");
        assert_eq!(res["name"], "w-res");
        assert_eq!(res["state"], "live");
        assert_eq!(res["pid"], me);
        assert_eq!(res["provider"], "zai");
        assert!(res.get("age_s").is_some(), "{res:?}");
        assert_eq!(res["holder"], "spawn-gate:me:w-res");
        let registry_kinds: Vec<&str> = slot_rows
            .iter()
            .filter(|r| r["kind"] == "registry")
            .filter_map(|r| r["name"].as_str())
            .collect();
        assert_eq!(registry_kinds, ["p1", "p2"]);
        assert_eq!(answer["live_workers"], 3);
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
            "refusals before the mint write nothing: {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn gate_refuses_a_review_seed_before_the_bypass() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("agents-home");
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var(crate::paths::HOME_ENV, &home);
        let claims_root = dir.join("claims-root");
        std::fs::create_dir_all(claims_root.join(".fno").join("claims")).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &claims_root);
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", dir.join(".fno").join("config.toml"));
        let prior_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::set_var("FNO_SPAWN_GATE", "0");

        let review = gate_answer(&json!({
            "mode": "gate",
            "name": "review-probe",
            "substrate": "headless",
            "holder_pid": std::process::id(),
            "force": true,
            "seed": "$fno:review high --comment",
            "session_phase": "do"
        }));
        let think = gate_answer(&json!({
            "mode": "gate",
            "name": "think-probe",
            "substrate": "headless",
            "holder_pid": std::process::id(),
            "force": true,
            "seed": "/fno:think why"
        }));

        std::env::remove_var(crate::paths::HOME_ENV);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        match prior_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(review["status"], "refused", "{review}");
        assert_eq!(review["exit_code"], 89, "{review}");
        assert_eq!(review["receipt"]["reason"], "review_session", "{review}");
        assert_eq!(think["status"], "admitted", "{think}");
    }

    /// AC3-HP: reserve mints a claim with the reservation metadata, an expiry
    /// inside the ceiling, and the next lane count one higher; gate-status
    /// (AC4-HP) names it in the lane row's `reserved` array.
    #[test]
    fn reserve_mints_a_claim_with_metadata_and_lane_count() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-resv-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // lanes_answer resolves the agents home; a test run must declare a
        // hermetic root, never the real $HOME.
        let agents_home = dir.join("agents-home");
        std::fs::create_dir_all(&agents_home).unwrap();
        std::env::set_var(crate::paths::HOME_ENV, &agents_home);
        let root = dir.join("claims-root");
        let claims_dir = root.join(".fno").join("claims");
        std::fs::create_dir_all(&claims_dir).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n",
        )
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let argv: Vec<String> = [
            "t-reserved-x-4444",
            "--provider",
            "zai",
            "--ttl",
            "10m",
            "--reason",
            "four parked PRs",
            "--node",
            "x-4444",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let code = reserve_spawn_gate(&dir, &argv);
        // AC4-HP: the gate-status lane row names the reservation beside
        // cap/live/counted/parked.
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let reg = agents.join("registry.json");
        std::fs::write(&reg, r#"{"schema_version":1,"entries":[]}"#).unwrap();
        let mut warnings = Vec::new();
        let lanes = lanes_answer(&dir, &reg, None, &mut warnings).unwrap();
        assert_eq!(
            lanes["zai"]["reserved"][0]["name"],
            json!("t-reserved-x-4444")
        );
        assert_eq!(
            lanes["zai"]["live"],
            json!(1),
            "the reservation spends a lane slot in the readout too"
        );
        let (state, rec) = crate::claims::status("worker:t-reserved-x-4444", Some(&root));
        let rec = rec.expect("the minted claim exists");
        std::env::remove_var("FNO_CLAIMS_ROOT");
        std::env::remove_var(crate::paths::HOME_ENV);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(code, 0);
        assert_eq!(state, crate::claims::ClaimState::Live);
        assert_eq!(
            rec.metadata.get("model_provider").and_then(Value::as_str),
            Some("zai")
        );
        assert!(
            rec.metadata.get("reserved_by").is_some(),
            "reserved_by names the caller"
        );
        assert_eq!(
            rec.metadata.get("reserved_reason").and_then(Value::as_str),
            Some("four parked PRs")
        );
        assert_eq!(
            rec.metadata.get("node").and_then(Value::as_str),
            Some("x-4444")
        );
        assert!(
            rec.expires_at.unwrap_or(0) > now,
            "expires_at sits inside the ceiling, past the mint instant"
        );
        assert!(
            rec.expires_at.unwrap_or(0) <= now + RESERVATION_MAX_TTL_MS,
            "expires_at within the 15m ceiling"
        );
    }

    /// AC3-EDGE: a TTL over the ceiling refuses and writes nothing.
    #[test]
    fn reserve_refuses_a_ttl_over_the_ceiling_and_writes_nothing() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-resv-ttl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("claims-root");
        let claims_dir = root.join(".fno").join("claims");
        std::fs::create_dir_all(&claims_dir).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let argv: Vec<String> = [
            "t-reserved-x-4444",
            "--provider",
            "zai",
            "--ttl",
            "4h",
            "--reason",
            "too long",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let code = reserve_spawn_gate(&dir, &argv);
        let leftovers: Vec<_> = std::fs::read_dir(&claims_dir).unwrap().flatten().collect();
        std::env::remove_var("FNO_CLAIMS_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(code, 2);
        assert!(
            leftovers.is_empty(),
            "the ceiling refusal writes no claim: {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    /// AC3-EDGE: reservations never hold a whole lane. With zai capped at 2
    /// and one reservation live, a second refuses and writes nothing.
    #[test]
    fn reserve_refuses_to_reserve_the_whole_lane() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-verb-resv-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("claims-root");
        let claims_dir = root.join(".fno").join("claims");
        std::fs::create_dir_all(&claims_dir).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n\n\
             [agents.provider_limits.zai]\nlanes = 2\n",
        )
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let host = crate::claims::hostname();
        let first = claims_dir.join(format!(
            "{}.lock",
            crate::claims::encode_key("worker:t-first-x-4444")
        ));
        std::fs::write(
            &first,
            format!(
                "schema_version: {}\nkey: worker:t-first-x-4444\nholder: king-1\nacquired_at: {now}\nexpires_at: {}\npid: {}\nhost: {host}\nmetadata:\n  model_provider: zai\n  reserved_by: king-1\n",
                crate::claims::SCHEMA_VERSION,
                now + 600_000,
                std::process::id()
            ),
        )
        .unwrap();
        let argv: Vec<String> = [
            "t-second-x-4444",
            "--provider",
            "zai",
            "--reason",
            "one slot must stay winnable",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let code = reserve_spawn_gate(&dir, &argv);
        let leftovers: Vec<_> = std::fs::read_dir(&claims_dir).unwrap().flatten().collect();
        std::env::remove_var("FNO_CLAIMS_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(code, 2);
        assert_eq!(
            leftovers.len(),
            1,
            "only the pre-existing fixture claim is on disk; the refused mint wrote nothing"
        );
    }

    /// AC4-HP: the usage text names the rule, so the rule cannot drift from
    /// the behavior it teaches.
    #[test]
    fn reserve_usage_names_the_rule() {
        let _g = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let code = reserve_spawn_gate(std::path::Path::new("."), &["--help".to_string()]);
        assert_eq!(code, 0);
        assert!(crate::spawn_gate_reservations::RESERVATION_RULE.contains("first-come"));
        assert!(
            crate::spawn_gate_reservations::RESERVATION_RULE.contains("expires within 15 minutes")
        );
    }
}

//! The `claim acquire` leaf, ported from `cli/src/fno/claims/cli.py::acquire`
//! (cli.py:116-364) plus the native-resume rebind it drives
//! (`core.compare_and_rebind`, whose only caller this leaf is).
//!
//! Output contract: the human line is the DEFAULT (the operator surface the
//! Python leaf owned); `--json` prints the claim record. The one behavioral
//! landing note: Python's rebind left an out-of-range TTL as an uncaught
//! traceback; this port lands it on the leaf's ordinary validation exit
//! (`validation error: ...`, 2).

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

use super::{
    claim_json_string, node_aware_root, parse_metadata_arg, parse_ttl_expression,
    stamp_do_on_acquire, ttl_ms_checked,
};
use crate::claims::{self, AcquireOpts, AcquireOutcome, ClaimRecord, MIN_TTL_MS};

/// Holder prefix marking a spawn-minted launch-window claim - the only prior
/// holder a `--handover-from` takeover may replace (mirrors
/// `HANDOVER_HOLDER_PREFIX` in core.py).
const HANDOVER_HOLDER_PREFIX: &str = "spawn-handover:";

struct LeafArgs {
    key: Option<String>,
    holder: String,
    lane: Option<String>,
    max_lanes: Option<i64>,
    reason: String,
    ttl: String,
    metadata_raw: String,
    pid: Option<u32>,
    pid_bad: bool,
    pid_unavailable: bool,
    json_output: bool,
    harness: Option<String>,
    handover_from: Option<String>,
    ttl_ms_flag: Option<i64>,
    root: Option<PathBuf>,
}

pub fn run(args: &[String]) -> i32 {
    let mut a = LeafArgs {
        key: None,
        holder: String::new(),
        lane: None,
        max_lanes: None,
        reason: String::new(),
        ttl: String::new(),
        metadata_raw: String::new(),
        pid: None,
        pid_bad: false,
        pid_unavailable: false,
        json_output: false,
        harness: None,
        handover_from: None,
        ttl_ms_flag: None,
        root: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--holder" => match it.next() {
                Some(v) => a.holder = v.clone(),
                None => return usage_flag("--holder"),
            },
            "--lane" => match it.next() {
                Some(v) => a.lane = Some(v.clone()),
                None => return usage_flag("--lane"),
            },
            "--max-lanes" => match it.next().and_then(|v| v.parse::<i64>().ok()) {
                Some(v) => a.max_lanes = Some(v),
                None => return usage_flag("--max-lanes"),
            },
            "--reason" | "-R" => match it.next() {
                Some(v) => a.reason = v.clone(),
                None => return usage_flag("--reason"),
            },
            "--ttl" => match it.next() {
                Some(v) => a.ttl = v.clone(),
                None => return usage_flag("--ttl"),
            },
            "--metadata" => match it.next() {
                Some(v) => a.metadata_raw = v.clone(),
                None => return usage_flag("--metadata"),
            },
            "--pid" => match it.next().and_then(|v| v.parse::<u32>().ok()) {
                Some(v) => a.pid = Some(v),
                None => {
                    a.pid_bad = true;
                    // Consume the value so a following positional is not
                    // misread as the key.
                    let _ = it.next();
                }
            },
            "--pid-unavailable" => a.pid_unavailable = true,
            "--json" | "-J" => a.json_output = true,
            "--verbose" => {}
            "--harness" => match it.next() {
                Some(v) => a.harness = Some(v.clone()),
                None => return usage_flag("--harness"),
            },
            "--handover-from" => match it.next() {
                Some(v) => a.handover_from = Some(v.clone()),
                None => return usage_flag("--handover-from"),
            },
            // Hidden engine flags the native acquire_claim forward uses;
            // accepted here so the leaf is a superset of both surfaces.
            "--ttl-ms" => match it.next().and_then(|v| v.parse::<i64>().ok()) {
                Some(v) => a.ttl_ms_flag = Some(v),
                None => return usage_flag("--ttl-ms"),
            },
            "--root" => match it.next() {
                Some(v) => a.root = Some(PathBuf::from(v)),
                None => return usage_flag("--root"),
            },
            "--holding-recovery-lock" => {}
            other => {
                if other.starts_with('-') && other != "-" {
                    eprintln!("fno-agents: claim: unknown flag {other}");
                    return 2;
                }
                if a.key.is_some() {
                    eprintln!("fno-agents: claim: unexpected extra argument {other}");
                    return 2;
                }
                a.key = Some(other.to_string());
            }
        }
    }
    run_parsed(a)
}

fn usage_flag(name: &str) -> i32 {
    eprintln!("fno-agents: claim: {name} requires a value");
    2
}

fn run_parsed(a: LeafArgs) -> i32 {
    // Lane mode first: a slot is acquired by lane id, never by claim key
    // (cli.py:187-197).
    if let Some(lane) = a.lane.clone() {
        if a.key.is_some() || a.max_lanes.is_none() || !a.holder.is_empty() {
            eprintln!(
                "validation error: --lane takes no KEY and no --holder, and \
                 requires --max-lanes (a lane slot is acquired by lane id, not \
                 by claim key)"
            );
            return 2;
        }
        return run_lane(&a, &lane);
    }
    if a.max_lanes.is_some() {
        eprintln!("validation error: --max-lanes is the lane-slot cap and requires --lane <id>");
        return 2;
    }
    let Some(key) = a.key.clone() else {
        eprintln!("validation error: KEY is required (or use --lane <id>)");
        return 2;
    };
    if a.holder.is_empty() {
        eprintln!("validation error: --holder is required");
        return 2;
    }
    if a.pid_unavailable && a.pid.is_some() {
        eprintln!("validation error: --pid and --pid-unavailable are mutually exclusive");
        return 2;
    }
    let parsed_ttl = match parse_ttl_expression(&a.ttl) {
        Ok(v) => v,
        Err(msg) => return bad_parameter("--ttl", &msg),
    };
    if a.pid_bad {
        return bad_parameter("--pid", "value is not a valid integer");
    }
    // An omitted --pid anchors to the durable session (the nearest harness
    // ancestor), degrading to pid-unavailable on a TTL claim when no session
    // is resolvable (cli.py:227-235).
    // --ttl (the operator expression) wins over the hidden --ttl-ms engine
    // flag; the empty --ttl falls through to it. The fold lands before the
    // pid checks so either spelling counts as "has a TTL".
    let ttl_ms: Option<i64> = match parsed_ttl {
        Some(v) => match ttl_ms_checked(v) {
            Ok(ms) => Some(ms),
            Err(msg) => {
                eprintln!("validation error: {msg}");
                return 2;
            }
        },
        None => a.ttl_ms_flag,
    };
    let mut pid = a.pid;
    let mut pid_unavailable = a.pid_unavailable;
    if pid.is_none() && !pid_unavailable {
        pid = claims::open_session_pid().map(|p| p as u32);
        if pid.is_none() && ttl_ms.is_some() {
            pid_unavailable = true;
        }
    }
    if pid_unavailable && ttl_ms.is_none() {
        eprintln!("validation error: --pid-unavailable requires --ttl");
        return 2;
    }
    if let Some(expected) = a.handover_from.clone() {
        if pid.is_none() && !pid_unavailable {
            eprintln!(
                "validation error: --handover-from needs a durable pid (run from \
                 a harness session) or --pid-unavailable with --ttl; the transient \
                 CLI pid would leave the claim STALE the moment it exits"
            );
            return 2;
        }
        let metadata = match parse_metadata_arg(&a.metadata_raw) {
            Ok(m) => m,
            Err(msg) => return bad_parameter("--metadata", &msg),
        };
        let rebind_root = node_aware_root(&key);
        match compare_and_rebind(
            &key,
            &expected,
            &a.holder,
            a.harness.as_deref(),
            &a.reason,
            metadata,
            pid,
            pid_unavailable,
            ttl_ms,
            rebind_root.as_deref(),
        ) {
            Ok(Some(claim)) => {
                if key.starts_with("node:") {
                    stamp_do_on_acquire(&key, &claim, &a.holder);
                }
                if a.json_output {
                    println!("{}", claim_json_string(&claim));
                } else {
                    println!("acquired {key} (handover from {expected})");
                }
                return 0;
            }
            // Idempotent refresh or dead-owner rebound under the SAME holder:
            // fall through, the ordinary acquire applies its own rules.
            Ok(None) => {}
            Err(reason) => eprintln!("handover declined: {reason}"),
        }
    }
    ordinary_acquire(&a, &key, pid, pid_unavailable, ttl_ms)
}

fn run_lane(a: &LeafArgs, lane: &str) -> i32 {
    let max = a.max_lanes.unwrap_or(1);
    let ttl_arg = if a.ttl.is_empty() {
        "1h".to_string()
    } else {
        a.ttl.clone()
    };
    let mut args: Vec<String> = vec![
        "--lane".into(),
        lane.to_string(),
        "--max-lanes".into(),
        max.to_string(),
        "--ttl".into(),
        ttl_arg,
    ];
    if a.json_output {
        args.push("--json".into());
    }
    crate::claim_lanes_cli::run_lane_acquire(&args)
}

fn ordinary_acquire(
    a: &LeafArgs,
    key: &str,
    pid: Option<u32>,
    pid_unavailable: bool,
    ttl_ms: Option<i64>,
) -> i32 {
    let metadata = match parse_metadata_arg(&a.metadata_raw) {
        Ok(m) => m,
        Err(msg) => return bad_parameter("--metadata", &msg),
    };
    // Pre-validate: the engine folds validation into Error, but the leaf's
    // contract splits it (validation -> 2, transient -> 3).
    if let Err(e) = claims::validate_inputs(key, &a.holder, ttl_ms, pid, pid_unavailable) {
        eprintln!("validation error: {e}");
        return 2;
    }
    let root = a.root.clone().or_else(|| node_aware_root(key));
    // The Python forward drops --harness on the native path (acquire_claim
    // deletes it before building flags), so the record's identity stays the
    // ambient walk; --harness reaches the rebind only.
    let opts = AcquireOpts {
        pid,
        pid_unavailable,
        ttl_ms,
        reason: if a.reason.is_empty() {
            None
        } else {
            Some(a.reason.clone())
        },
        metadata: Some(metadata),
        // The provenance stamp mirrors core._resolve_pid_provenance: ambient
        // unless the pid IS this process's own session walk's answer on a
        // TTL claim under a harness that dies with its sessions.
        pid_provenance: Some(resolve_pid_provenance(
            pid.map(|p| p as i32),
            ttl_ms,
            claims::resolve_identity().1.as_deref(),
        )),
        root,
        events_dir: None,
        identity: None,
    };
    // task: leases keep the lazy session witness the old engine arm gave
    // them: a pid-less thread claim is assessed through it on contention.
    let outcome = if key.starts_with("task:") {
        let witness = |record: &ClaimRecord| {
            let (witness, _drain) = crate::claim_verbs::default_session_witness();
            witness(record)
        };
        let witness: claims::SessionWitness<'_> = &witness;
        claims::acquire_with_session_witness(key, &a.holder, opts, Some(witness))
    } else {
        claims::acquire(key, &a.holder, opts)
    };
    match outcome {
        AcquireOutcome::Acquired(claim) => {
            if key.starts_with("node:") {
                stamp_do_on_acquire(key, &claim, &a.holder);
            }
            if a.json_output {
                println!("{}", claim_json_string(&claim));
            } else {
                let pid_s = claim
                    .pid
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "None".into());
                println!("acquired: {key} (holder={}, pid={pid_s})", a.holder);
            }
            0
        }
        AcquireOutcome::HeldByOther {
            holder: h,
            pid: p,
            host,
        } => {
            let pid_s = p.map(|v| v.to_string()).unwrap_or_else(|| "None".into());
            eprintln!("claim '{key}' held by {h} (pid={pid_s}, host={host})");
            // In JSON mode stdout carries the machine surface
            // core._native_claim reconstructs ClaimHeldByOther from; the
            // human surface stays stderr-only.
            if a.json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "outcome": "held_by_other", "holder": h,
                        "pid": p, "host": host,
                    })
                );
            }
            1
        }
        AcquireOutcome::Error(e) => {
            if e.contains("contention retries") {
                eprintln!("contention error: {e}");
                1
            } else {
                eprintln!("transient error: {e}");
                3
            }
        }
    }
}

/// Click's layout for a BadParameter raised in the command body; the frozen
/// goldens pin the exact lines.
fn bad_parameter(flag: &str, msg: &str) -> i32 {
    // The flag name is dropped on the floor like typer does.
    let _ = flag;
    super::usage_refusal(
        "fno agents claim acquire [OPTIONS] [key]",
        "fno agents claim acquire --help",
        &format!("Error: Invalid value: {msg}"),
    )
}

// -------------------------------------------------------------------------
// The native-resume rebind (core.compare_and_rebind)
// -------------------------------------------------------------------------

/// `Ok(Some)` = a handover landed (the takeover output is the leaf's);
/// `Ok(None)` = idempotent refresh or same-holder rebound - the ordinary
/// acquire falls through and applies its own rules; `Err` = declined, the
/// reason is the frozen Python text.
#[allow(clippy::too_many_arguments)]
fn compare_and_rebind(
    key: &str,
    expected_holder: &str,
    holder_flag: &str,
    harness_flag: Option<&str>,
    reason_flag: &str,
    metadata: Map<String, Value>,
    new_pid: Option<u32>,
    pid_unavailable: bool,
    ttl_ms: Option<i64>,
    root: Option<&Path>,
) -> Result<Option<ClaimRecord>, String> {
    // The leaf has already validated key/holder/ttl range; what the rebind's
    // own _validate_inputs could still add (encoded-key length) lands on the
    // ordinary acquire right behind it, so no second pass here.
    let npid: i32 = new_pid
        .map(|p| p as i32)
        .unwrap_or(std::process::id() as i32);
    let npid_unavailable = pid_unavailable || (new_pid.is_none() && ttl_ms.is_some());
    // Resolved BEFORE the recovery mutex: the ambient walk has no business on
    // a critical section other acquirers poll on.
    let (amb_session, amb_harness) = claims::resolve_identity();
    let resolved_harness = harness_flag.map(str::to_string).or(amb_harness);
    let resolved_session = amb_session;
    let resolved_provenance = resolve_pid_provenance(
        new_pid.map(|p| p as i32),
        ttl_ms,
        resolved_harness.as_deref(),
    );
    let path = claims::claim_path(key, root).map_err(|e| e.to_string())?;
    let recovery_lock = claims::recovery_lock_path(&path);
    let Some(token) =
        claims::acquire_dir_mutex(&recovery_lock, std::time::Duration::from_secs(5), true)
    else {
        return Err("claim recovery mutex busy; retry the resume bind".into());
    };
    let outcome = rebind_locked(
        path.as_path(),
        key,
        expected_holder,
        holder_flag,
        reason_flag,
        metadata,
        npid,
        npid_unavailable,
        ttl_ms,
        resolved_harness,
        resolved_session,
        resolved_provenance,
    );
    claims::release_dir_mutex(&recovery_lock, &token);
    outcome
}

/// The critical section: re-read + re-classify under the recovery mutex,
/// then the three rewrite sites in core.py's order.
#[allow(clippy::too_many_arguments)]
fn rebind_locked(
    path: &Path,
    key: &str,
    expected_holder: &str,
    holder_flag: &str,
    reason_flag: &str,
    metadata: Map<String, Value>,
    npid: i32,
    npid_unavailable: bool,
    ttl_ms: Option<i64>,
    resolved_harness: Option<String>,
    resolved_session: Option<String>,
    resolved_provenance: String,
) -> Result<Option<ClaimRecord>, String> {
    let _ = key;
    let existing = match claims::read_claim_file(path) {
        Ok(r) => r,
        Err(claims::ReadError::GoneAway) => {
            return Err(
                "claim vanished; this target no longer owns the node (use `fno do target start` to reclaim)"
                    .into(),
            )
        }
        Err(claims::ReadError::Corrupted(_)) => {
            return Err(
                "claim corrupted; cannot verify ownership (use `fno agents claim release --force`)"
                    .into(),
            )
        }
    };
    if existing.holder != expected_holder {
        let pid_s = existing
            .pid
            .map(|v| v.to_string())
            .unwrap_or_else(|| "None".into());
        return Err(format!(
            "holder mismatch: expected '{expected_holder}', claim held by '{}' (pid={pid_s})",
            existing.holder
        ));
    }
    let state = crate::claim_verbs::status_verdict(&existing).0;
    // ONLY a launch-window holder may be replaced: naming the prior holder is
    // the proof, and `spawn-handover:<worker>` reaches only that worker.
    let handover_allowed = !holder_flag.is_empty()
        && holder_flag != existing.holder
        && existing.holder.starts_with(HANDOVER_HOLDER_PREFIX);
    if !holder_flag.is_empty() && holder_flag != existing.holder && !handover_allowed {
        // REFUSE, never fall through: the same-holder rebind below would
        // republish a foreign claim as LIVE under this process.
        return Err(format!(
            "holder '{}' is not a launch-window holder; only a spawn-side handover claim can be taken over",
            existing.holder
        ));
    }
    let effective_holder = if handover_allowed {
        Some(holder_flag.to_string())
    } else {
        None
    };
    let effective_reason = if handover_allowed && !reason_flag.is_empty() {
        Some(reason_flag.to_string())
    } else {
        None
    };
    let mut effective_metadata: Option<Map<String, Value>> = None;
    if handover_allowed {
        // The takeover rewrites session_id, erasing who dispatched the node;
        // carry the dispatcher's session in metadata.
        let mut merged = existing.metadata.clone();
        for (k, v) in &metadata {
            merged.insert(k.clone(), v.clone());
        }
        if let Some(sid) = existing.session_id.as_deref().filter(|s| !s.is_empty()) {
            merged
                .entry("dispatched_by_session".to_string())
                .or_insert_with(|| Value::String(sid.to_string()));
        }
        effective_metadata = Some(merged);
    }
    // A handover with no pinned harness resolves one from ambient markers,
    // exactly as the ordinary acquire path does; preserving the spawner's tag
    // left a claude worker under a codex lead reading as codex for the life
    // of the claim.
    let effective_harness = if handover_allowed {
        resolved_harness.clone()
    } else {
        None
    };
    if state == claims::ClaimState::Live && handover_allowed {
        // A HANDOVER, and a live prior pid does not refuse it: on the blocking
        // substrates the spawner is still alive when the worker reaches init.
        let rebound = rebound_claim(
            &existing,
            npid,
            ttl_ms,
            effective_holder,
            effective_reason,
            effective_harness,
            effective_metadata,
            Some(resolved_provenance),
            npid_unavailable,
            resolved_session.clone(),
        );
        let payload = claims::serialize_claim(&rebound)?;
        claims::atomic_replace(path, &payload).map_err(|e| e.to_string())?;
        emit_rebound(&rebound, existing.pid, state.as_str(), "handover");
        return Ok(Some(rebound));
    }
    if state == claims::ClaimState::Live {
        if existing.pid == Some(npid) {
            // Idempotent: already bound to this process; refresh the lease.
            let keep_session = existing
                .session_id
                .clone()
                .filter(|s| !s.is_empty())
                .or(resolved_session.clone());
            let rebound = rebound_claim(
                &existing,
                npid,
                ttl_ms,
                effective_holder,
                effective_reason,
                effective_harness,
                effective_metadata,
                Some(resolved_provenance),
                npid_unavailable,
                keep_session,
            );
            let payload = claims::serialize_claim(&rebound)?;
            claims::atomic_replace(path, &payload).map_err(|e| e.to_string())?;
            emit_rebound(&rebound, existing.pid, state.as_str(), "idempotent");
            return Ok(None);
        }
        let pid_s = existing
            .pid
            .map(|v| v.to_string())
            .unwrap_or_else(|| "None".into());
        return Err(format!(
            "concurrent writer: claim held by live pid {pid_s}, this pid is {npid}; refusing to rebind a live owner"
        ));
    }
    // SUSPECT or STALE. Rebind only when the dead owner is on THIS machine;
    // off-host death is unproven, so rebind would be a foreign takeover.
    if !claims::is_same_machine(&existing.host, existing.machine_id.as_deref()) {
        return Err(
            "owner is off-host or machine identity is unverifiable; death unproven, will not rebind a foreign claim"
                .into(),
        );
    }
    let session_for_row = if handover_allowed {
        resolved_session.clone()
    } else {
        existing
            .session_id
            .clone()
            .filter(|s| !s.is_empty())
            .or(resolved_session.clone())
    };
    let rebound = rebound_claim(
        &existing,
        npid,
        ttl_ms,
        effective_holder,
        effective_reason,
        effective_harness,
        effective_metadata,
        Some(resolved_provenance),
        npid_unavailable,
        session_for_row,
    );
    let payload = claims::serialize_claim(&rebound)?;
    claims::atomic_replace(path, &payload).map_err(|e| e.to_string())?;
    let mode = if handover_allowed {
        "handover"
    } else {
        "rebound"
    };
    emit_rebound(&rebound, existing.pid, state.as_str(), mode);
    if handover_allowed {
        Ok(Some(rebound))
    } else {
        Ok(None)
    }
}

/// A rebound claim: identity fields preserved, process anchor + lease fresh.
/// The deadline always runs from now (never a compounding span), a TTL claim
/// refreshes to `now + ttl` and a PID-liveness claim with a prior window to
/// `now + prior_window` floored at MIN_TTL_MS. A non-handover rebind keeps
/// the prior record's harness, so the written harness is narrowed HERE and
/// an unearned provenance stamp resets to ambient (core._rebound_claim).
#[allow(clippy::too_many_arguments)]
fn rebound_claim(
    existing: &ClaimRecord,
    npid: i32,
    ttl_ms: Option<i64>,
    new_holder: Option<String>,
    new_reason: Option<String>,
    new_harness: Option<String>,
    new_metadata: Option<Map<String, Value>>,
    new_pid_provenance: Option<String>,
    new_pid_unavailable: bool,
    new_session_id: Option<String>,
) -> ClaimRecord {
    let now = claims::now_ms();
    let expires_at = match ttl_ms {
        Some(t) => Some(now + t),
        None => existing
            .expires_at
            .map(|e| now + (e - existing.acquired_at).max(MIN_TTL_MS)),
    };
    let written_harness = new_harness.clone().or_else(|| existing.harness.clone());
    let mut provenance = new_pid_provenance;
    if !claims::pid_dies_with_session(written_harness.as_deref()) {
        provenance = Some("ambient".to_string());
    }
    let metadata = match new_metadata {
        Some(m) if !m.is_empty() => m,
        _ => existing.metadata.clone(),
    };
    ClaimRecord {
        schema_version: if new_pid_unavailable {
            claims::PID_UNAVAILABLE_SCHEMA_VERSION
        } else {
            claims::SCHEMA_VERSION
        },
        key: existing.key.clone(),
        holder: new_holder
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| existing.holder.clone()),
        acquired_at: now,
        pid: if new_pid_unavailable {
            None
        } else {
            Some(npid)
        },
        host: claims::hostname(),
        pid_unavailable: new_pid_unavailable,
        machine_id: Some(claims::machine_id()).filter(|m| !m.is_empty()),
        expires_at,
        reason: new_reason.or_else(|| existing.reason.clone()),
        harness: written_harness,
        session_id: new_session_id
            .filter(|s| !s.is_empty())
            .or_else(|| existing.session_id.clone()),
        pid_provenance: provenance,
        metadata,
    }
}

/// "session-prover" only when the pid IS this process's own session walk's
/// answer and the harness dies with its sessions; everything else is ambient
/// (core._resolve_pid_provenance).
fn resolve_pid_provenance(pid: Option<i32>, ttl_ms: Option<i64>, harness: Option<&str>) -> String {
    if pid.is_none() || ttl_ms.is_none() {
        return "ambient".into();
    }
    if !claims::pid_dies_with_session(harness) {
        return "ambient".into();
    }
    let walk = crate::spawn_context::session_identity_ambient(std::process::id()).0;
    match walk.map(|p| p as i32) {
        Some(p) if Some(p) == pid => "session-prover".into(),
        _ => "ambient".into(),
    }
}

fn emit_rebound(claim: &ClaimRecord, previous_pid: Option<i32>, previous_state: &str, mode: &str) {
    let mut data = claims::common_event_data(claim);
    data.insert(
        "previous_pid".into(),
        previous_pid.map(Value::from).unwrap_or(Value::Null),
    );
    data.insert(
        "previous_state".into(),
        Value::String(previous_state.into()),
    );
    data.insert("mode".into(), Value::String(mode.into()));
    claims::emit_audit_event(None, "claim_rebound", data);
}

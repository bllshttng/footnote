//! The node-dispatch launch half of the native advance port: the spawn
//! preference resolution and `fno agents spawn` argv that
//! `fno/agents/node_dispatch.py` built, over the native owners it already
//! has (naming, route-slot, graph store, settings, territory verdict).
//!
//! Status: the launch leg (the composed worker command) is the one seam
//! still Python-owned; until its port lands the module exposes the resolved
//! pieces and refuses a launch with a named gap rather than guessing. The
//! grouped CLI door does not route `advance`/`join`/`dispatch-lanes` natively
//! until the leg lands, so the shipped surface is unchanged.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use super::advance::{
    bounded_command_env, gate_refusal_detail, slot_queue_retry_at, BoundedOutput, SpawnFailure,
    SpawnFailureKind, EVENT_SOURCE,
};

/// The discriminator `fno agents spawn` prints on a name collision (exit 2).
const SPAWN_ALREADY_EXISTS: &str = "already exists";

/// The seam's stderr receipt prefix, shared vocabulary with spawn_axes.rs.
const SPAWN_NOTE_PREFIX: &str = "fno agents spawn: ";
const SPAWN_NOTE_CAP: usize = 20;

/// The sandbox probe's exit code (byte-parity with the Python runtime).
const EXIT_SANDBOX_UNREACHABLE: i32 = 85;

/// The launch preferences one node dispatch needs (the NodeSpawnArgs
/// contract, narrowed to what the launch builds and receipts carry).
#[derive(Debug, Clone)]
pub struct NodeSpawnArgs {
    pub node_id: String,
    pub node_cwd: Option<String>,
    pub node_slug: Option<String>,
    pub harness: String,
    pub resolved_harness: String,
    pub substrate: String,
    pub command: String,
    pub model: Option<String>,
    pub route: Option<String>,
    pub resolved_route: Option<String>,
    pub account: Option<String>,
    pub dispatch_account: Option<String>,
    pub permission_mode: String,
    pub agent_name: String,
    pub vendor: Option<String>,
    pub verb: String,
    pub verb_source: String,
    pub session_phase: Option<String>,
    pub grid_reason: Option<String>,
    pub decision: Vec<String>,
    pub is_reconcile: bool,
    pub env: BTreeMapGlobal,
}

/// Placeholder alias so the struct compiles before the env composition port
/// lands; the launch leg replaces it with HashMap<String, String>.
pub type BTreeMapGlobal = std::collections::BTreeMap<String, String>;

/// The resolved spawn outcome: Ok carries the launch identity and the
/// receipt rows the event emission reads.
pub type SpawnOk = (String, Value);

/// The spawn failure taxonomy: AlreadyRunning is the benign peer-dedup skip;
/// Failed carries the SpawnFailure whose gate_refusal() classifies
/// machine-scoped refusals.
pub enum SpawnOutcome {
    AlreadyRunning(String),
    Failed(SpawnFailure),
}

impl SpawnFailure {
    /// A GateRefusal for a machine-scoped gate refusal, else None. The
    /// `spawn-gate:` marker (`sandbox-probe:` for the sandbox probe's exit)
    /// is REQUIRED provenance: a provider crash propagates a raw exit code,
    /// so the number alone cannot prove the machine refused.
    pub fn gate_refusal(&self) -> Option<super::advance::GateRefusal> {
        let code = self.exit_code?;
        use crate::spawn_gate as g;
        let reason = match code {
            g::EXIT_QUEUE_TIMEOUT
            | g::EXIT_NO_WAIT
            | g::EXIT_RAM_REFUSED
            | g::EXIT_PROVIDER_CAP
            | g::EXIT_LOAD_REFUSED
            | g::EXIT_LEAD_SHARE
            | g::EXIT_TERRITORY_CAP
            | g::EXIT_BLUEPRINT_CAP => "capacity-refused",
            g::EXIT_REGISTRY_SCHEMA
            | g::EXIT_FLEET_STOP
            | g::EXIT_FLEET_STOP_UNAVAILABLE
            | g::EXIT_GATE_UNAVAILABLE => "gate-unavailable",
            g::EXIT_STATE_ROOT_UNGRANTED => "state-root-ungranted",
            c if c == EXIT_SANDBOX_UNREACHABLE => "sandbox-unreachable",
            _ => return None,
        };
        let detail = self.detail.trim().to_string();
        let marker = if code == EXIT_SANDBOX_UNREACHABLE {
            "sandbox-probe:"
        } else {
            "spawn-gate:"
        };
        if !detail.starts_with(marker) {
            return None;
        }
        Some(super::advance::GateRefusal {
            reason: reason.to_string(),
            exit_code: code,
            detail,
            retry_at: self.retry_at,
        })
    }
}

/// Launch identity from a thread-substrate spawn receipt. A `thread` spawn
/// prints a compact JSON receipt whose launch proof is
/// `{"name", "short_id", ...}` for claude, or for a codex thread the FULL
/// `harness_session_id`. Scans past lines that merely MENTION an id field
/// but are not the receipt. Empty string = no receipt.
pub fn spawn_receipt_identity(proc_stdout: &str) -> String {
    let mut short_id = String::new();
    let mut harness_session_id = String::new();
    for line in proc_stdout.lines() {
        if !line.contains("\"short_id\"")
            && !line.contains("\"harness_session_id\"")
            && !line.contains("\"session_id\"")
        {
            continue;
        }
        let Ok(receipt) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(obj) = receipt.as_object() else {
            continue;
        };
        short_id = obj
            .get("short_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        harness_session_id = obj
            .get("harness_session_id")
            .or_else(|| obj.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !short_id.is_empty() || !harness_session_id.is_empty() {
            break;
        }
    }
    if short_id.is_empty() {
        harness_session_id
    } else {
        short_id
    }
}

/// The territory stamp: three values, never two. Reads the
/// territory-verdict door; its unknown receipt omits both fields, so a
/// missing key stamps null and any read failure degrades to nulls with one
/// warning.
pub fn territory_stamp(node_id: &str) -> (Option<Value>, Option<Value>) {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("advance: WARNING: territory verdict unreadable for {node_id}: no binary");
        return (None, None);
    };
    let argv = vec![
        "territory-verdict".to_string(),
        "--node".to_string(),
        node_id.to_string(),
    ];
    match bounded_command_env(&exe, &argv, 60, &[]) {
        Ok(out) if out.code == 0 => {
            let verdict: Value = serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null);
            (
                Some(verdict["territory"].clone()),
                Some(verdict["kingless"].clone()),
            )
        }
        Ok(out) => {
            eprintln!(
                "advance: WARNING: territory verdict unreadable for {node_id}: exit {} {}",
                out.code,
                out.stderr.trim().chars().take(120).collect::<String>()
            );
            (None, None)
        }
        Err(e) => {
            eprintln!("advance: WARNING: territory verdict unreadable for {node_id}: {e}");
            (None, None)
        }
    }
}

/// The harness `launch` names, or None when nothing can answer. An ACCOUNT
/// RECORD (ccm/ccr) is a real binary but not a harness, so it answers
/// through its registry row; an answer the resolver cannot honor is as
/// unverifiable as no answer and degrades the same way.
pub fn launch_harness_axis(launch: &str, node_cwd: Option<&str>) -> Option<String> {
    if launch.trim().is_empty() {
        return None;
    }
    if crate::provider::KNOWN_PROVIDERS.contains(&launch) {
        return Some(launch.to_string());
    }
    record_harness(launch, node_cwd)
        .filter(|rec| crate::provider::KNOWN_PROVIDERS.contains(&rec.as_str()))
}

/// The harness an account record names, or None. Reads the accounts records
/// block through the shared config lookup; a failure answers None (pins
/// nothing).
fn record_harness(launch: &str, _node_cwd: Option<&str>) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let records = crate::agents_config::config_lookup(&cwd, &["accounts", "records"])?
        .as_array()?
        .clone();
    for rec in records {
        let id = rec.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id == launch {
            return rec
                .get("cli")
                .and_then(|v| v.as_str())
                .filter(|c| !c.is_empty())
                .map(str::to_string);
        }
    }
    None
}

/// Concrete `--model` for a node at the spawn seam, or None for default.
/// Strictly non-fatal: any error degrades to the explicit override or the
/// node's raw pin. The stage-table leg rides the native route-slot door's
/// dispatch_model mode, the same answer the Python resolver reads.
pub fn node_model(
    node: &Value,
    explicit: Option<&str>,
    provider: Option<&str>,
    resolve_difficulty: bool,
) -> Option<String> {
    let task_model = node.get("model").and_then(Value::as_str);
    if let Some(e) = explicit.filter(|e| !e.trim().is_empty()) {
        return Some(e.to_string());
    }
    if let Some(t) = task_model.filter(|t| !t.trim().is_empty()) {
        return Some(t.to_string());
    }
    let task_difficulty = if resolve_difficulty {
        node.get("difficulty").and_then(Value::as_str)
    } else {
        None
    };
    let payload = json!({
        "mode": "dispatch_model",
        "explicit": explicit,
        "task_model": task_model,
        "task_difficulty": task_difficulty,
        "plan_model": Value::Null,
        "plan_difficulty": Value::Null,
        "provider": provider.unwrap_or("claude"),
    });
    let Ok(exe) = std::env::current_exe() else {
        return explicit
            .map(str::to_string)
            .or_else(|| task_model.map(str::to_string));
    };
    let argv = vec![
        "route-slot".to_string(),
        serde_json::to_string(&payload).unwrap_or_default(),
    ];
    match bounded_command_env(&exe, &argv, 30, &[]) {
        Ok(out) if out.code == 0 => {
            let answer: Value = serde_json::from_str(out.stdout.trim()).unwrap_or(Value::Null);
            answer
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .or_else(|| task_model.map(str::to_string))
        }
        _ => explicit
            .map(str::to_string)
            .or_else(|| task_model.map(str::to_string)),
    }
}

/// The `fno agents spawn` flags for resolved args: ONE builder for every
/// node-dispatching caller, so the claude-only gates cannot drift. The
/// caller prepends the binary + verb; `cwd` rides before the model axis.
pub fn node_spawn_argv(
    args: &NodeSpawnArgs,
    cwd: Option<&str>,
    extra: &[(&str, String)],
) -> Vec<String> {
    let mut cmd = vec![
        "--harness".to_string(),
        args.harness.clone(),
        "--substrate".to_string(),
        args.substrate.clone(),
    ];
    if let Some(vendor) = &args.vendor {
        cmd.extend(["--provider".to_string(), vendor.clone()]);
    } else if let Some(route) = &args.route {
        // The row's route owns vendor AND model as one fact; an explicit
        // dispatch-time vendor pin outranks it and is never replaced.
        cmd.extend(["--route".to_string(), route.clone()]);
    } else if let (Some(route), true) = (&args.resolved_route, args.resolved_harness == "claude") {
        // No grid pick: the stage table's verb lane route, or a routed claude
        // model dies on the default endpoint at first inference.
        cmd.extend(["--route".to_string(), route.clone()]);
    }
    if let Some(account) = &args.account {
        if args.resolved_harness == "claude" {
            // The capacity pick read THIS account's quota; claude-only at the CLI.
            cmd.extend(["--account".to_string(), account.clone()]);
        } else {
            eprintln!(
                "advance: grid account {account:?} skipped (claude-only, harness {:?})",
                args.resolved_harness
            );
        }
    }
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        cmd.extend(["--cwd".to_string(), cwd.to_string()]);
    } else {
        cmd.push("--fresh".to_string());
    }
    if let Some(model) = &args.model {
        cmd.extend(["--model".to_string(), model.clone()]);
    }
    // CLAUDE-ONLY: gate on the RESOLVED harness, so a claude ACCOUNT record
    // (ccm/ccr) still gets the flag - without it the account-pinned worker
    // hangs on a prompt.
    if !args.permission_mode.is_empty() && args.resolved_harness == "claude" {
        cmd.extend([
            "--permission-mode".to_string(),
            args.permission_mode.clone(),
        ]);
    }
    // A cutover's destination account rides argv as a RECORD ID, never env:
    // the front door applies the overlay where the harness is exec'd, and a
    // HOME-carrying overlay would move the state root where nothing looks.
    if let Some(account) = &args.dispatch_account {
        cmd.extend(["--dispatch-account".to_string(), account.clone()]);
    }
    for (flag, value) in extra {
        cmd.extend([(*flag).to_string(), value.clone()]);
    }
    // The worker-to-node join; without --node the registry row names no node
    // and no instrument can answer which worker is on which node.
    cmd.extend(["--node".to_string(), args.node_id.clone()]);
    if let Some(slug) = &args.node_slug {
        cmd.extend(["--slug".to_string(), slug.clone()]);
    }
    if let Some(phase) = &args.session_phase {
        // A registry verb's declared phase. The spawn door refuses an
        // unlabeled --node spawn, so an outside verb must carry its label.
        cmd.extend(["--session-phase".to_string(), phase.clone()]);
    }
    cmd.push("--name".to_string());
    cmd.push(args.agent_name.clone());
    cmd.push(args.command.clone());
    cmd
}

/// Emit the one dispatch_spawned row and return the launch identity.
/// `receipt` mirrors the launch rows into the caller's receipt dict.
pub fn finish_spawn(
    row: &Value,
    events_path: Option<&Path>,
    receipt: Option<&mut Value>,
    notes: &[String],
) -> String {
    let kind = "dispatch_spawned";
    let target = events_path
        .map(Path::to_path_buf)
        .or_else(|| super::advance::advance_events_path(None));
    if let (Some(obj), Some(target)) = (row.as_object(), target) {
        let mut fields = serde_json::Map::new();
        for (k, v) in obj {
            fields.insert(k.clone(), v.clone());
        }
        if let Some(parent) = std::path::Path::new(&target).parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let _ = crate::events::EventEmitter::new(target, EVENT_SOURCE).emit_fields(kind, fields);
    }
    let short_id = row
        .get("short_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(receipt) = receipt {
        if let Some(obj) = receipt.as_object_mut() {
            obj.insert("short_id".to_string(), Value::from(short_id.clone()));
            obj.insert("notes".to_string(), json!(notes));
            for key in ["agent_name", "verb", "verb_source", "substrate", "kingless"] {
                if let Some(v) = row.get(key) {
                    obj.insert(key.to_string(), v.clone());
                }
            }
        }
    }
    short_id
}

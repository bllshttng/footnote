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

use crate::backlog::dispatch_resolve::grid_lane_for;
use serde_json::{json, Value};
use std::path::Path;

#[allow(unused_imports)]
use super::advance::{
    bounded_command_env, gate_refusal_detail, slot_queue_retry_at, BoundedOutput, SpawnFailure,
    SpawnFailureKind, EVENT_SOURCE,
};

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
    /// The subprocess env: the resolver's answer plus the caller's extras,
    /// keyed by name. The launch leg owns merging the base env beneath it.
    pub env: std::collections::BTreeMap<String, String>,
}

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

/// The one node-dispatch launch. The composed worker command (the verb
/// command the lifecycle table and the harness resolver compose) is the one
/// seam still Python-owned: until its port lands this launch REFUSES with a
/// named gap rather than guessing a command, so a wired door can never
/// launch a wrong worker. The caller releases its dispatch reservation on
/// any error return, keeping the node re-dispatchable.
#[allow(clippy::too_many_arguments)]
pub fn spawn_worker(
    node_id: &str,
    root: &str,
    slug: Option<&str>,
    node: &Value,
    model: Option<&str>,
    provider: Option<&str>,
    harness: Option<&str>,
    verb: Option<&str>,
    brief: Option<&str>,
    _dispatch_account: Option<&str>,
    dispatch_reservation: Option<(&str, &str)>,
    _caller: &str,
    source: Option<&str>,
    events_path: Option<&Path>,
) -> Result<SpawnOk, SpawnOutcome> {
    let args = resolve_node_spawn(
        node_id,
        Some(root),
        slug,
        node,
        model,
        provider,
        harness,
        verb,
        brief,
        source,
    )
    .map_err(SpawnOutcome::Failed)?;
    // The one launch row, proof of a launch that happened; built before the
    // reuse arm so a retasked dispatch emits the same row with reuse keys.
    let mut row = json!({
        "node_id": node_id,
        "short_id": "",
        "agent_name": args.agent_name.clone(),
        "harness": args.harness.clone(),
        "vendor": "",
        "model": args.model.clone().unwrap_or_default(),
        "account": "",
        "substrate": args.substrate.clone(),
        "command": args.command.clone(),
        "verb": args.verb.clone(),
        "verb_source": args.verb_source.clone(),
        "cwd": args.node_cwd.clone().unwrap_or_default(),
        "grid": args.grid_reason.clone().unwrap_or_default(),
        "decision": args.decision.join("; "),
    });
    // A blueprint dispatch reuses the earliest finished planner on the epic
    // first. The reuse transaction (retask + reap) is still Python-owned, so
    // a retaskable candidate REFUSES instead of cold-spawning over it - a
    // second planner would double-bill the same leg. Cold-spawning stays the
    // fallthrough when no candidate exists.
    if args.verb.trim_start_matches('/') == "blueprint" {
        match finished_planner(node_id, args.node_cwd.as_deref()) {
            Ok(Some(candidate)) => {
                if let Some((key, holder)) = dispatch_reservation {
                    super::advance::safe_release(key, holder);
                }
                return Err(SpawnOutcome::Failed(SpawnFailure::already_running(
                    format!(
                        "blueprint reuse arm unported: finished planner {} holds a \
                         retaskable leg on this epic; the reuse transaction is \
                         still Python-owned",
                        candidate.name
                    ),
                )));
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("advance: WARNING: reuse read failed, cold-spawning {node_id}: {e}");
            }
        }
    }
    // The stamp lands on the cold-spawn path only: a reuse dispatch runs no
    // subprocess at all. The record, not the veto: a kingless territory drains.
    let (territory, kingless) = territory_stamp(node_id);
    if let Some(obj) = row.as_object_mut() {
        obj.insert("territory".to_string(), territory.unwrap_or(Value::Null));
        obj.insert("kingless".to_string(), kingless.unwrap_or(Value::Null));
    }
    // The node's effort pin rides the typed-flag path; the door validates it.
    let pin = node
        .get("effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let mut argv = vec!["agents".to_string(), "spawn".to_string()];
    let extra: Vec<(&str, String)> = if pin.is_empty() {
        Vec::new()
    } else {
        vec![("--effort", pin.to_string())]
    };
    argv.extend(node_spawn_argv(&args, args.node_cwd.as_deref(), &extra));
    // The caller's dispatch reservation and the spawn door's own family-2
    // guard collide - the door acquires the SAME key, sees a foreign holder
    // it must never clear, and refuses. Hand the reservation over: release
    // ours just before the exec, and the door's atomic node handover
    // re-closes the sub-second window.
    if let Some((key, holder)) = dispatch_reservation {
        super::advance::safe_release(key, holder);
    }
    let Ok(exe) = std::env::current_exe() else {
        return Err(SpawnOutcome::Failed(SpawnFailure::general(
            "fno agents spawn: no fno-agents binary to launch",
        )));
    };
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&argv);
    // The resolver's env is AUTHORITATIVE: a stale inherited TARGET_NO_MERGE
    // never survives into a successor the resolver just granted allow-merge.
    cmd.env_clear();
    for (k, v) in &args.env {
        cmd.env(k, v);
    }
    let out = match crate::bounded_cmd::output_with_timeout_result(cmd, 600) {
        Ok(out) => out,
        Err(e) => {
            return Err(SpawnOutcome::Failed(SpawnFailure::general(format!(
                "fno agents spawn: {e}"
            ))));
        }
    };
    let code = out.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if code != 0 {
        let stderr_s = stderr.trim();
        if code == 2 && stderr_s.contains(SPAWN_ALREADY_EXISTS) {
            return Err(SpawnOutcome::Failed(SpawnFailure::already_running(
                format!("agent {} already exists", args.agent_name),
            )));
        }
        if code == crate::spawn_gate::EXIT_PROVIDER_CAP {
            let gate_detail = super::advance::gate_refusal_detail(if stderr_s.is_empty() {
                &stdout
            } else {
                stderr_s
            });
            let mut failure = SpawnFailure::queue_refused(
                format!(
                    "fno agents spawn exited {} (slot queue refused): {gate_detail}",
                    crate::spawn_gate::EXIT_PROVIDER_CAP
                ),
                super::advance::slot_queue_retry_at(&stdout),
            );
            failure.detail = gate_detail;
            return Err(SpawnOutcome::Failed(failure));
        }
        // The --node door's family-2 guard dedups by refusing with
        // already-running: the benign skip, not a spawn failure.
        if code == 2
            && stderr_s.contains("node dispatch refused")
            && stderr_s.contains("verdict=already-running")
        {
            return Err(SpawnOutcome::Failed(SpawnFailure::already_running(
                format!(
                    "door refused {node_id}: {}",
                    stderr_s.chars().take(120).collect::<String>()
                ),
            )));
        }
        let gate_detail = super::advance::gate_refusal_detail(if stderr_s.is_empty() {
            &stdout
        } else {
            stderr_s
        });
        let suffix = super::advance::gate_refusal_reason(code)
            .map(|m| format!(" ({m})"))
            .unwrap_or_default();
        return Err(SpawnOutcome::Failed(
            SpawnFailure::general(format!(
                "fno agents spawn exited {code}{suffix}: {gate_detail}"
            ))
            .with_exit(code, gate_detail),
        ));
    }
    // Receipt shape is substrate-dependent: a thread spawn prints a compact
    // JSON receipt whose launch identity we require as launch proof; a
    // headless one-shot already ran to completion on exit 0 - the clean exit
    // IS the proof.
    let mut launch_identity = "headless".to_string();
    if args.substrate == "thread" {
        launch_identity = spawn_receipt_identity(&stdout);
        if launch_identity.is_empty() {
            return Err(SpawnOutcome::Failed(SpawnFailure::general(format!(
                "fno agents spawn exit 0 but no launch-identity receipt: {}",
                (if stdout.trim().is_empty() {
                    stderr.trim()
                } else {
                    stdout.trim()
                })
                .chars()
                .take(200)
                .collect::<String>()
            ))));
        }
        // A bare 8-hex id aimed at codex is a 65.5-second timestamp bucket,
        // not an address: refuse by shape rather than bind a worker to the
        // wrong session.
        let harness_for_check = Some(args.resolved_harness.trim()).filter(|s| !s.is_empty());
        if unsafe_short_address(&launch_identity, harness_for_check) {
            return Err(SpawnOutcome::Failed(SpawnFailure::general(format!(
                "fno agents spawn receipt carries a codex head-8 launch identity \
                 ({launch_identity}): {CODEX_SHORT_ADDRESS_RULE}"
            ))));
        }
    }
    // The spawn seam prints its own receipt on stderr; match the prefix,
    // never an allowlist of message shapes - the seam adds axes, and the
    // next one must ride too.
    let notes: Vec<String> = stderr
        .lines()
        .filter(|l| l.starts_with(SPAWN_NOTE_PREFIX))
        .map(str::trim)
        .take(SPAWN_NOTE_CAP)
        .map(str::to_string)
        .collect();
    if let Some(obj) = row.as_object_mut() {
        obj.insert("short_id".to_string(), Value::from(launch_identity));
    }
    // Launch mail to the covering lead, autonomous dispatches only; best-effort.
    if source.is_some() {
        let ask = json!({
            "launch_credit_mail": {
                "node": node_id,
                "worker": {
                    "name": args.agent_name,
                    "harness": args.resolved_harness,
                    "model": args.model.clone().unwrap_or_default(),
                    "effort": pin,
                },
            }
        });
        let _ = crate::dispatch_credit::launch_credit_mail(&ask);
    }
    let short = finish_spawn(&row, events_path, None, &notes);
    Ok((short, row))
}

/// The spawn-seam failure vocabulary shared with the Python owner.
const SPAWN_ALREADY_EXISTS: &str = "already exists";
const SPAWN_NOTE_PREFIX: &str = "fno agents spawn: ";
const SPAWN_NOTE_CAP: usize = 20;

/// The codex short-address rule (the Python owner's constant): the remedy
/// sentence a head-8 receipt refusal carries.
const CODEX_SHORT_ADDRESS_RULE: &str =
    "use the full session_id or pane; codex head-8 is a 65.536-second timestamp bucket";

/// Whether the token is a bare eight-hex slice aimed at codex: a timestamp
/// bucket, not an address (codex session ids are UUIDv7). Shape-refusing is
/// the only property that does not change under you.
fn unsafe_short_address(token: &str, harness: Option<&str>) -> bool {
    let t = token.trim();
    if t.is_empty() || harness != Some("codex") || t.len() != 8 {
        return false;
    }
    t.chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// The lifecycle table's answer for the node row: the effective verb plus
/// its note, with the blueprint floor read from the node's own config (the
/// lean default when unreadable).
fn verb_answer_for(node: &Value, cfg_root: &str) -> Result<(Option<String>, String), String> {
    let floor = crate::agents_config::config_lookup(
        std::path::Path::new(cfg_root),
        &["dispatch", "blueprint_floor"],
    )
    .and_then(|v| v.as_str().map(str::to_string))
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| crate::backlog_ready::DEFAULT_BLUEPRINT_FLOOR.to_string());
    crate::backlog_ready::effective_verb_with_floor(node, &floor)
}

/// The shared project id lane ids derive from: config project.id, else the
/// repo basename (the Python owner's fallback).
fn base_project_id(canonical_root: &Path) -> String {
    let pid = crate::agents_config::config_lookup(canonical_root, &["project", "id"])
        .and_then(|v| v.as_str().map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    pid.unwrap_or_else(|| {
        canonical_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
    })
}

/// A retaskable finished blueprint planner: the registry row fields the
/// reuse arm needs.
struct RetaskCandidate {
    name: String,
}

/// The earliest-finished live blueprint worker on the same epic, else None.
/// Ports the Python owner's selection: live, pane/thread substrate, a done
/// inside-leg, same project, a node on the same epic whose blueprint phase
/// has ended; earliest received_at wins.
fn finished_planner(
    node_id: &str,
    node_cwd: Option<&str>,
) -> Result<Option<RetaskCandidate>, String> {
    let graph = super::settings::graph_path();
    let entries = crate::graph_store::read_rows(&graph).map_err(|e| e.to_string())?;
    let parent = entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(node_id))
        .and_then(|e| e.get("parent").and_then(Value::as_str))
        .map(str::to_string);
    let Some(parent) = parent.filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let Some(home) = crate::paths::AgentsHome::from_env_opt() else {
        return Err("agents home undeclared".to_string());
    };
    let registry = crate::state::load_registry(&home.registry_json()).map_err(|e| e.to_string())?;
    let anchor_root = node_cwd.map(std::path::PathBuf::from).unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });
    let project_id = base_project_id(&anchor_root);
    let mut best: Option<(String, RetaskCandidate)> = None;
    for entry in &registry.entries {
        if !matches!(entry.status, crate::AgentStatus::Live) {
            continue;
        }
        let substrate = entry.substrate.as_deref().unwrap_or("");
        if substrate != "pane" && substrate != "thread" {
            continue;
        }
        let Some(inside) = &entry.inside_leg else {
            continue;
        };
        if !matches!(inside.state, crate::state::InsideLegState::Done) {
            continue;
        }
        if base_project_id(Path::new(&entry.cwd)) != project_id {
            continue;
        }
        let parsed = crate::naming::parse_dispatch_agent_name(Some(&entry.name));
        let row_node = entry
            .node
            .clone()
            .or_else(|| parsed.as_ref().and_then(|p| p.node.clone()));
        let Some(row_node) = row_node.filter(|n| !n.is_empty() && n != node_id) else {
            continue;
        };
        let Some(row_rec) = entries
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(row_node.as_str()))
        else {
            continue;
        };
        if row_rec.get("parent").and_then(Value::as_str) != Some(parent.as_str()) {
            continue;
        }
        let blueprint_ended = row_rec
            .get("sessions")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter().filter_map(|s| s.as_object()).any(|s| {
                    s.get("phase").and_then(Value::as_str) == Some("blueprint")
                        && s.get("ended_at")
                            .and_then(Value::as_str)
                            .is_some_and(|v| !v.is_empty())
                })
            })
            .unwrap_or(false);
        if !blueprint_ended {
            continue;
        }
        let received = inside.received_at.clone();
        if best.as_ref().map(|(r, _)| received < *r).unwrap_or(true) {
            best = Some((
                received,
                RetaskCandidate {
                    name: entry.name.clone(),
                },
            ));
        }
    }
    Ok(best.map(|(_, candidate)| candidate))
}

/// The spawn-door preference assembly (the NodeSpawnArgs contract): every
/// node-dispatch preference except the launch itself, over the native
/// resolver, naming, grid lane and config reads.
#[allow(clippy::too_many_arguments)]
pub fn resolve_node_spawn(
    node_id: &str,
    node_cwd: Option<&str>,
    node_slug: Option<&str>,
    node: &Value,
    model: Option<&str>,
    provider: Option<&str>,
    harness: Option<&str>,
    verb: Option<&str>,
    brief: Option<&str>,
    source: Option<&str>,
) -> Result<NodeSpawnArgs, SpawnFailure> {
    let general = |m: String| SpawnFailure::general(m);
    let node_obj = node.as_object().ok_or_else(|| {
        general(format!(
            "refusing to dispatch {node_id}: this caller passed no node dict; \
             the builtin path has no verb evidence."
        ))
    })?;
    // The node dict IS the verb evidence. A dict without the key is a lossy
    // projection: REFUSE before anything is spent.
    if !node_obj.contains_key("dispatch_verb") {
        return Err(general(format!(
            "refusing to dispatch {node_id}: the node dict this caller passed \
             carries no dispatch_verb key; the projection feeding this \
             dispatcher is lossy; fix the projection, not the node."
        )));
    }
    let text_field = |v: Option<&Value>| -> Option<String> {
        v.and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let node_verb = verb
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let declared_verb = text_field(node_obj.get("dispatch_verb"));
    let verb_source = if declared_verb.is_some() {
        "declared"
    } else {
        "none-declared"
    };
    // The config root: the DEPENDENT node's own project, so a cross-project
    // dispatch reads the dependent's config.
    let cfg_root = node
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| node_cwd.map(str::to_string))
        .unwrap_or_else(|| root_of());
    let lifecycle = verb_answer_for(node, &cfg_root).map_err(|e| general(e))?;
    let effective_verb = lifecycle.0.clone();
    let verb_code = crate::naming::verb_code_for(
        effective_verb
            .as_deref()
            .or(node_verb.as_deref())
            .or(declared_verb.as_deref()),
    )
    .map_err(|e| general(e.to_string()))?;
    let mut model_v = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| text_field(node_obj.get("model")));
    let launch = provider.map(str::trim).filter(|s| !s.is_empty());
    // Capacity-grid deferral: the lane is picked HERE, at the seam that can
    // read live capacity. An explicit harness skips the consult.
    let mut harness_v: Option<String> = None;
    let mut grid_route: Option<String> = None;
    let mut grid_account: Option<String> = None;
    let mut grid_why: Option<String> = None;
    if harness.is_none() {
        let (gh, gm, gr, ga, decline) = grid_lane_for(
            Some(node),
            model_v.as_deref(),
            provider,
            effective_verb.as_deref(),
        );
        grid_why = decline;
        if let Some(gh) = gh {
            model_v = gm;
            harness_v = Some(gh);
            grid_route = gr;
            grid_account = ga;
        }
    }
    // An unpinned spawn bills the account's default model - the silent
    // substitution a routing law can never survive. A dropped pin REFUSES.
    if model_v.as_deref().map(str::trim).unwrap_or("").is_empty() {
        let decline = grid_why
            .as_deref()
            .map(|w| format!("; {w}"))
            .unwrap_or_default();
        return Err(general(format!(
            "refusing to dispatch {node_id}: no model survives resolution \
             (unpinned = the account default model){decline}; pin the node's \
             model or repair the routing config, then retry."
        )));
    }
    // The name mints ONCE here - after the lane/model consult, before spawn -
    // so it carries the model tag, riding the receipt.
    let agent_name = crate::naming::dispatch_agent_name(
        source,
        &verb_code,
        node_id,
        node_slug,
        None,
        None,
        model_v.as_deref(),
    )
    .map_err(|e| general(e.to_string()))?;
    // Only the literal "dispatch" grant reads true.
    let auto_merge =
        crate::agents_config::config_lookup(Path::new(&cfg_root), &["auto_merge", "grant"])
            .and_then(|v| v.as_str().map(str::to_string))
            .as_deref()
            == Some("dispatch");
    // One axis: `provider` is the harness under an older spelling, so it
    // must reach the resolver too, or the command follows the stage table.
    let launch_axis = launch_harness_axis(launch.unwrap_or(""), node_cwd);
    let mut receipt_verb = effective_verb
        .clone()
        .or_else(|| node_verb.clone())
        .or_else(|| declared_verb.clone())
        .unwrap_or_else(|| "builtin".to_string());
    if crate::provider::parse_verb_token(&receipt_verb).is_some() {
        receipt_verb = crate::backlog::fields::canonical_verb_key(&receipt_verb);
    }
    let stage_verb = effective_verb
        .as_deref()
        .or(node_verb.as_deref())
        .or(declared_verb.as_deref())
        .unwrap_or("target");
    let cfg = super::dispatch_resolve::dispatch_cfg_for(node_cwd, stage_verb);
    // A registry verb's declared phase rides the argv; a config read failure
    // degrades to None - that refusal with its remedy, never a guessed label.
    let descriptor_phase = {
        let key = crate::backlog::fields::canonical_verb_key(
            effective_verb
                .as_deref()
                .or(node_verb.as_deref())
                .unwrap_or(""),
        );
        cfg.verb_registry
            .get(&key)
            .map(|d| d.session_phase.trim().to_string())
            .filter(|p| !p.is_empty())
    };
    let input = super::dispatch_resolve::DispatchInput {
        harness: harness_v
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or(launch_axis.as_deref()),
        substrate: None,
        node_id: Some(node_id),
        command: None,
        verb: node_verb.as_deref().or(declared_verb.as_deref()),
        lifecycle: Some(lifecycle),
        brief,
        merge_posture: None,
        trigger: "autonomous".to_string(),
    };
    let resolved =
        super::dispatch_resolve::resolve_dispatch(&input, &cfg).map_err(|e| general(e))?;
    let prov = launch
        .map(str::to_string)
        .unwrap_or_else(|| resolved.harness.clone());
    if let Some(axis) = &launch_axis {
        if *axis != resolved.harness {
            return Err(general(format!(
                "refusing to spawn {node_id}: --harness {prov:?} runs {axis:?} \
                 but the command is spelled for {:?} ({:?}). Pass one axis.",
                resolved.harness, resolved.command
            )));
        }
    }
    // Explicit permission_mode > the operator's spawn default > the builtin
    // unattended answer.
    let mode = crate::agents_config::config_lookup(
        Path::new(&cfg_root),
        &["agents", "defaults", "permission_mode"],
    )
    .and_then(|v| v.as_str().map(str::to_string))
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| SPAWN_PERMISSION_BUILTIN.to_string());
    // The resolver's env is AUTHORITATIVE for the merge posture, so a stale
    // inherited TARGET_NO_MERGE never survives into a successor the resolver
    // just granted allow-merge.
    let mut run_env: std::collections::BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| k != "TARGET_NO_MERGE")
        .collect();
    for (k, v) in &resolved.env {
        run_env.insert(k.clone(), v.clone());
    }
    if let Some(src) = source {
        if matches!(src, "ac" | "rd" | "ab") {
            run_env.insert("FNO_SPAWN_TRIGGER".to_string(), format!("dispatch:{src}"));
            for key in AMBIENT_IDENTITY_ENV {
                run_env.remove(key);
            }
            for key in SEED_PROVENANCE_KEYS {
                run_env.remove(key);
            }
        }
    }
    Ok(NodeSpawnArgs {
        node_id: node_id.to_string(),
        node_cwd: node_cwd.map(str::to_string),
        node_slug: node_slug.map(str::to_string),
        harness: prov,
        resolved_harness: resolved.harness,
        substrate: resolved.substrate,
        command: resolved.command,
        model: model_v,
        route: grid_route,
        resolved_route: Some(resolved.route).filter(|r| !r.is_empty()),
        account: grid_account,
        dispatch_account: None,
        permission_mode: mode,
        agent_name,
        vendor: None,
        verb: receipt_verb,
        verb_source: verb_source.to_string(),
        session_phase: descriptor_phase,
        grid_reason: grid_why,
        decision: resolved.decision,
        is_reconcile: false,
        env: run_env,
    })
}

fn root_of() -> String {
    std::env::current_dir()
        .map(|d| d.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

/// The ambient session-identity env a dispatch-sourced child must not
/// inherit (the Python owner's AMBIENT_IDENTITY_ENV).
const AMBIENT_IDENTITY_ENV: [&str; 19] = [
    "FNO_HARNESS_NAME",
    "FNO_HARNESS_SESSION_ID",
    "CODEX_THREAD_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_SESSION_ID",
    "GEMINI_SESSION_ID",
    "OPENCODE_SESSION_ID",
    "CLAUDE_SESSION_ID",
    "CLAUDECODE",
    "CLAUDECODE_SESSION_ID",
    "HERMES_SESSION_ID",
    "TARGET_SESSION_ID",
    "CODEX_CI",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "CODEX_SHELL",
    "CODEX_COMPANION_SESSION_ID",
    "CODEX_COMPANION_TRANSCRIPT_PATH",
    "FNO_AGENT_SUBSTRATE",
    "FNO_NODE_REASON",
];

/// The seed-provenance env group, set-or-cleared together.
const SEED_PROVENANCE_KEYS: [&str; 6] = [
    "FNO_SEED_PROV_SEED_B64",
    "FNO_SEED_PROV_FROM",
    "FNO_SEED_PROV_FROM_SESSION",
    "FNO_SEED_PROV_HARNESS",
    "FNO_SEED_PROV_NODE",
    "FNO_SEED_PROV_MSG_ID",
];

/// fno's builtin unattended permission answer (spawn_compose's constant).
const SPAWN_PERMISSION_BUILTIN: &str = "bypassPermissions";

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

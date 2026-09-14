//! The provider-lane axes of the spawn gate (x-6089): the per-provider live
//! count, the king share, the registry schema guard and the account quota
//! lock, ported from the Python gate (`cli/src/fno/agents/spawn_gate.py`)
//! that this gate replaces.
//!
//! Fail-closed is the contract here, unlike the global guards: the module
//! doc in `spawn_gate.rs` lets a RAM floor or a slot count fail OPEN because
//! the gate must never brick spawning, but an UNREADABLE LANE COUNT is a
//! refusal, never a zero — assuming an empty lane oversubscribes a shared
//! account, which is the harm the provider cap exists to prevent.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::agents_config;
use crate::claims::{self, ClaimState, PidProbe};
use crate::claude_roster::ClaudeRoster;
use crate::spawn_gate::status_is_liveish;
use crate::state::{load_registry, RegistryEntry};
use crate::AgentStatus;

/// How long a reactive provider-health entry may drive the quota lock
/// (mirrors runtime_state.PROVIDER_HEALTH_TTL_SECONDS and the bound
/// fallback_chain already applies to its own health reads).
const PROVIDER_HEALTH_TTL_SECONDS: f64 = 60.0 * 60.0;

/// Pane-probe wall-clock budget: the shared mux subprocess bound
/// (mux_spawn._MUX_SUBPROCESS_TIMEOUT_S).
const PANE_PROBE_BUDGET: Duration = Duration::from_secs(30);

/// The registry statuses that can never hold a crown
/// (mirrors registry.TERMINAL_STATUSES; the crowned_sessions divisor skips
/// them exactly as Python's court reader does).
fn status_is_terminal(s: &AgentStatus) -> bool {
    matches!(
        s,
        AgentStatus::Orphaned
            | AgentStatus::Failed
            | AgentStatus::Exited
            | AgentStatus::PermanentDead
    )
}

/// The `providers.provider_limits.<provider>.lanes` cap, or `None` when the
/// provider is uncapped. A config that never named a provider_limits table
/// falls back to the built-in budget table, exactly as the Python gate's
/// `gate_settings` fails safe (`config._BUILTIN_PROVIDER_BUDGETS`).
pub(crate) fn provider_lanes_cap(config_cwd: &Path, provider: &str) -> Option<usize> {
    let lanes = match agents_config::config_lookup(config_cwd, &["agents", "provider_limits"]) {
        Some(table) => table
            .get(provider)
            .and_then(|budget| budget.get("lanes"))
            .and_then(|v| v.as_integer()),
        None => built_in_lanes(provider),
    };
    usize::try_from(lanes.unwrap_or(0))
        .ok()
        .filter(|lanes| *lanes >= 1)
}

/// The built-in budget table (`config._BUILTIN_PROVIDER_BUDGETS`): zai only.
fn built_in_lanes(provider: &str) -> Option<i64> {
    (provider == "zai").then_some(5)
}

// ---------------------------------------------------------------------------
// Pane liveness: one seam crossing, through the pane owner's own verb
// ---------------------------------------------------------------------------

/// Exact pane liveness for a registry row's mux ref: `Ok(true/false)` decided,
/// `Err(())` when the mux could not answer (the caller refuses — unreadable is
/// not absent). Shells `fno mux pane wait` the way `mux_spawn._mux_pane_alive`
/// does: exit 12 = the pane exited, 0/11 = alive; anything else is ambiguous,
/// and the authoritative pane listing decides. An EMPTY listing is
/// undecidable, never "gone": only a listing naming at least one OTHER pane
/// proves the server answered with real content.
pub(crate) fn mux_pane_alive(session: &str, pane_id: u64) -> Result<bool, ()> {
    let code = match pane_probe_code(session, pane_id) {
        Some(code) => code,
        None => return Err(()),
    };
    match code {
        12 => return Ok(false),
        0 | 11 => return Ok(true),
        _ => {}
    }
    // Ambiguous exit: ask the listing. A listing that cannot be read or names
    // no pane at all is undecidable (None), never a death.
    let out = match run_with_budget(
        Command::new(crate::scrape::fno_bin())
            .args(["mux", "pane", "ls", "--server", session, "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    ) {
        Some(out) => out,
        None => return Err(()),
    };
    if !out.status.success() {
        return Err(());
    }
    let rows: Value = match serde_json::from_slice(&out.stdout) {
        Ok(rows) => rows,
        Err(_) => return Err(()),
    };
    let rows = match rows.as_array() {
        Some(rows) if !rows.is_empty() => rows,
        _ => return Err(()),
    };
    Ok(!rows
        .iter()
        .any(|r| r.get("pane_id").and_then(Value::as_u64) == Some(pane_id)))
}

/// Run `fno mux pane wait` under the probe budget. `None` = the probe never
/// answered (spawn failure or deadline miss), which is undecidable.
fn pane_probe_code(session: &str, pane_id: u64) -> Option<i32> {
    let pane_id = pane_id.to_string();
    let mut cmd = Command::new(crate::scrape::fno_bin());
    cmd.args([
        "mux",
        "pane",
        "wait",
        "--server",
        session,
        &pane_id,
        "--timeout",
        "0",
    ])
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    run_with_budget(&mut cmd).map(|out| out.status.code().unwrap_or(-1))
}

/// Spawn a child, bound its whole run to `PANE_PROBE_BUDGET`, and reap it.
fn run_with_budget(cmd: &mut Command) -> Option<std::process::Output> {
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + PANE_PROBE_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Three-state pid liveness (the provider count refuses on undecidable)
// ---------------------------------------------------------------------------

/// `Ok(true)` alive, `Ok(false)` provably gone, `Err(())` undecidable — the
/// port of `spawn_gate._pid_alive`'s None: a denied inspection presents as an
/// undecided case, never as a decided death. When `recorded` carries the
/// row's `pid_start_time`, a live pid of a DIFFERENT incarnation (the pid was
/// recycled) reads as gone: the current start token is compared in the
/// registry's own units, as `daemon::pid_is_ours` does. An unreadable current
/// token against a recorded one is undecidable, never a silent count.
fn pid_liveness(pid: u32, recorded: Option<u64>) -> Result<bool, ()> {
    if pid <= 1 {
        return Ok(false);
    }
    match claims::probe_pid(pid as i32) {
        PidProbe::Created(_) => match recorded {
            None => Ok(true),
            Some(rec) => match crate::daemon::process_start_time(pid) {
                Some(now) => Ok(now == rec),
                None => Err(()),
            },
        },
        PidProbe::Absent => Ok(false),
        PidProbe::Refused => Err(()),
    }
}

/// One lane's count could not be proved: the probe's unknown verdict, with
/// the lane and the fault named.
pub(crate) struct LaneFault {
    pub(crate) provider: String,
    pub(crate) error: String,
}

/// The requested bg short ids with a positive live roster marker. Fails closed
/// on an unreadable roster and on an undecidable incarnation
/// (`_provider_roster_live_short_ids`).
fn provider_roster_live_short_ids(wanted: &BTreeSet<String>) -> Result<BTreeSet<String>, String> {
    let roster =
        ClaudeRoster::load_default().map_err(|e| format!("claude roster unreadable: {e}"))?;
    let mut live = BTreeSet::new();
    let mut undecidable = BTreeSet::new();
    for worker in roster.workers_deduped() {
        let short = worker.short_id().to_string();
        if !wanted.contains(&short) {
            continue;
        }
        match worker.pid {
            None => {}
            Some(pid) => match pid_liveness(pid, None) {
                Ok(true) => {
                    live.insert(short);
                }
                Ok(false) => {}
                Err(()) => {
                    undecidable.insert(short);
                }
            },
        }
    }
    let unknown: Vec<String> = undecidable.difference(&live).cloned().collect();
    if !unknown.is_empty() {
        return Err(format!(
            "process incarnation unreadable for bg worker(s) {unknown:?}"
        ));
    }
    Ok(live)
}

// ---------------------------------------------------------------------------
// The provider count
// ---------------------------------------------------------------------------

/// Count rows of ONE provider only when status and positive liveness agree
/// (the port of `spawn_gate.provider_live_count`). Returns the count plus the
/// names of the rows it included, so a display that recounts cannot disagree
/// with the refusal. Every unreadable source is an `Err`, never a zero.
pub(crate) fn provider_live_count(
    registry_path: &Path,
    provider: &str,
    warnings: &mut Vec<String>,
) -> Result<(usize, Vec<String>), String> {
    let registry =
        load_registry(registry_path).map_err(|e| format!("fno registry unreadable: {e}"))?;
    let live_rows: Vec<&RegistryEntry> = registry
        .entries
        .iter()
        .filter(|e| status_is_liveish(&e.status))
        .collect();

    // Rows minted without a provider stamp get ONE warning per (harness,
    // origin) shape; a hand-started (operator-origin) row can never carry the
    // stamp, so it is not the defect the warning names.
    let mut unattributed: std::collections::BTreeMap<(String, String), usize> = Default::default();
    for row in &live_rows {
        if row.provider.as_deref().is_some_and(|p| !p.is_empty()) {
            continue;
        }
        if row.origin.as_deref() == Some("operator") {
            continue;
        }
        let shape = (
            (!row.harness_name().is_empty())
                .then(|| row.harness_name().to_string())
                .unwrap_or_else(|| "unknown".into()),
            row.origin.clone().unwrap_or_else(|| "unknown".into()),
        );
        *unattributed.entry(shape).or_default() += 1;
    }
    for ((harness, origin), count) in &unattributed {
        warnings.push(format!(
            "{count} live row(s) were minted without a provider stamp (harness={harness}, origin={origin})"
        ));
    }

    let candidates: Vec<&RegistryEntry> = live_rows
        .iter()
        .copied()
        .filter(|row| row.provider.as_deref() == Some(provider))
        .collect();

    let bg_short_ids: BTreeSet<String> = candidates
        .iter()
        .filter(|row| row.pid.is_none() && row.harness_name() == "claude")
        .filter_map(|row| row.transport_short())
        .map(str::to_string)
        .collect();
    let bg_live = if bg_short_ids.is_empty() {
        BTreeSet::new()
    } else {
        provider_roster_live_short_ids(&bg_short_ids)?
    };

    let mut count = 0usize;
    let mut counted_names: Vec<String> = Vec::new();

    for row in &candidates {
        if let Some(pid) = row.pid {
            if row.pid_start_time.is_none() {
                // A pid without its incarnation token: a decided death skips,
                // and the pane probe settles everything else. An undecidable
                // pid does NOT fault here - the pane gets the chance first,
                // exactly as the Python counter ordered it.
                if pid_liveness(pid as u32, None) == Ok(false) {
                    continue;
                }
                match pane_state(row)? {
                    Some(true) => {
                        count += 1;
                        counted_names.push(row.name.clone());
                    }
                    Some(false) => {}
                    None => {
                        return Err(format!(
                            "process incarnation token missing for {}",
                            row.name
                        ))
                    }
                }
                continue;
            }
            // With a recorded start time, pid reuse fails closed.
            if pid_liveness(pid as u32, row.pid_start_time)
                .map_err(|_| format!("process incarnation unreadable for {}", row.name))?
            {
                count += 1;
                counted_names.push(row.name.clone());
            }
            continue;
        }
        match pane_state(row)? {
            Some(true) => {
                count += 1;
                counted_names.push(row.name.clone());
                continue;
            }
            Some(false) => continue,
            None => {}
        }
        if row.mux.is_some() {
            // Unreadable is not absent: the cap refuses before the bg fallback.
            return Err(format!("pane liveness unreadable for {}", row.name));
        }
        if let Some(short) = row.transport_short() {
            if bg_live.contains(short) {
                count += 1;
                counted_names.push(row.name.clone());
            }
        }
    }
    count += provider_live_slot_claims(provider, &counted_names, warnings)?;
    Ok((count, counted_names))
}

/// Pane liveness for one row: `Some(bool)` decided, `None` when the row
/// carries no pane ref, `Err` when the probe crashed (fail closed).
fn pane_state(row: &RegistryEntry) -> Result<Option<bool>, String> {
    match &row.mux {
        Some(mux) => mux_pane_alive(&mux.session, mux.pane_id)
            .map(Some)
            .map_err(|()| format!("pane liveness unreadable for {}", row.name)),
        None => Ok(None),
    }
}

/// Provider-tagged headless reservations not represented by rows
/// (`_provider_live_slot_claims`). A claim whose liveness cannot be proved is
/// a refusal, never an uncount.
fn provider_live_slot_claims(
    provider: &str,
    counted_names: &[String],
    warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let root = match claims::global_claims_root() {
        Some(root) => root,
        None => return Ok(0),
    };
    let dir = match claims::claims_dir_for(Some(&root)) {
        Some(dir) => dir,
        None => return Ok(0),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(0),
    };
    let counted: HashSet<&str> = counted_names.iter().map(String::as_str).collect();
    let mut count = 0usize;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let fname = fname.to_string_lossy().into_owned();
        if !fname.starts_with("worker%3A") || !fname.ends_with(".lock") {
            continue;
        }
        let key = match urldecode(&fname[..fname.len() - ".lock".len()]) {
            Some(key) => key,
            None => continue,
        };
        let name = key.strip_prefix("worker:").unwrap_or(&key);
        if counted.contains(name) {
            continue;
        }
        let (state, record) = claims::status(&key, Some(&root));
        match state {
            ClaimState::Free | ClaimState::Stale => continue,
            ClaimState::Corrupted => return Err(format!("worker reservation {key} is corrupted")),
            _ => {}
        }
        let model_provider = record.as_ref().and_then(|rec| {
            rec.metadata
                .get("model_provider")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let Some(model_provider) = model_provider else {
            warnings.push(format!(
                "live worker reservation {key} was minted without model_provider; skipping"
            ));
            continue;
        };
        if model_provider.is_empty()
            || model_provider == "__uncapped__"
            || model_provider != provider
        {
            continue;
        }
        match state {
            ClaimState::Suspect => {
                return Err(format!("worker reservation {key} liveness is suspect"))
            }
            ClaimState::Live => count += 1,
            _ => {}
        }
    }
    Ok(count)
}

/// Minimal percent-decoder for claim filenames (inverse of
/// `claims::encode_key`); `None` on malformed escapes.
fn urldecode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---------------------------------------------------------------------------
// King share
// ---------------------------------------------------------------------------

/// One share reading over the registry: the crowned sessions, the share, the
/// caller's held rows and the unattributed bucket (`share_reading` +
/// `court.crowned_sessions`). Every count is `None` when the registry is
/// unreadable — there is nothing to enforce and no zero to fail open on.
pub(crate) struct ShareReading {
    pub kings: Option<usize>,
    pub share: Option<usize>,
    pub held: Option<usize>,
    pub held_rows: Option<Vec<String>>,
    pub unattributed_rows: Option<Vec<String>>,
}

pub(crate) fn share_reading(
    registry_path: &Path,
    cap: usize,
    caller: Option<&str>,
) -> ShareReading {
    let registry = match load_registry(registry_path) {
        Ok(registry) => registry,
        Err(_) => {
            return ShareReading {
                kings: None,
                share: None,
                held: None,
                held_rows: None,
                unattributed_rows: None,
            }
        }
    };
    let mut crowned: HashSet<String> = HashSet::new();
    let mut held_rows: Vec<String> = Vec::new();
    let mut unattributed: Vec<String> = Vec::new();
    for e in &registry.entries {
        if let Some(session) = e.harness_session_id.as_deref() {
            if e.crown_level.is_some() && !session.is_empty() && !status_is_terminal(&e.status) {
                crowned.insert(session.to_string());
            }
        }
        if !status_is_liveish(&e.status) || e.crown_level.is_some() {
            continue;
        }
        match e.spawned_by_session.as_deref() {
            Some(spawner) if Some(spawner) == caller => held_rows.push(e.name.clone()),
            Some(_) => {}
            None => unattributed.push(e.name.clone()),
        }
    }
    let kings = crowned.len();
    // The Python `_king_share` divisor expression (`crowned | {caller if
    // caller in crowned}`) is a set union that can never grow the set, so the
    // divisor is exactly the crown count. The caller folds in only when
    // itself crowned - i.e. never as an extra vote (x-5283 LD2).
    let divisor = kings;
    let share = if divisor == 0 {
        1
    } else {
        (cap / divisor).max(1)
    };
    ShareReading {
        kings: Some(kings),
        share: Some(share),
        held: Some(held_rows.len()),
        held_rows: Some(held_rows),
        unattributed_rows: Some(unattributed),
    }
}

// ---------------------------------------------------------------------------
// Registry schema guard
// ---------------------------------------------------------------------------

/// Refuse a spawn into a fleet whose shared registry this binary cannot write:
/// an on-disk schema_version ahead of the version this binary writes refuses
/// (exit 81); an unreadable or missing file skips, as the Python guard skips.
/// Both trees read the version from `src/registry_schema.toml` (build.rs
/// projects the same file into the wheel), so the two guards cannot disagree
/// about a number.
pub(crate) fn check_registry_schema(
    registry_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<(), crate::spawn_gate::Refusal> {
    use crate::spawn_gate::{Refusal, EXIT_REGISTRY_SCHEMA};
    let raw = match std::fs::read_to_string(registry_path) {
        Ok(raw) => raw,
        Err(_) => return Ok(()), // fresh machine / unreadable: skip
    };
    let doc: Value = match serde_json::from_str(&raw) {
        Ok(doc) => doc,
        Err(_) => return Ok(()), // a torn registry is not a spawn-time verdict
    };
    let on_disk = doc.get("schema_version").and_then(Value::as_u64);
    let Some(on_disk) = on_disk else {
        return Ok(());
    };
    let understood = crate::state::REGISTRY_SCHEMA_VERSION as u64;
    if on_disk <= understood {
        return Ok(());
    }
    warnings.push(format!(
        "spawn-gate: the shared agent registry at {} is schema_version={on_disk}, ahead of \
         the schema_version={understood} this binary understands, so this worker could neither \
         claim its node nor stamp its mail; refusing to spawn. Upgrade this fno (fno doctor \
         update), or repair the file (fno agents registry-repair --to {understood} --apply).",
        registry_path.display()
    ));
    Err(Refusal::with_receipt(
        EXIT_REGISTRY_SCHEMA,
        serde_json::json!({
            "status": "refused",
            "reason": "registry_schema",
            "registry_path": registry_path.to_string_lossy(),
            "on_disk": on_disk,
            "understood": understood,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Account quota lock
// ---------------------------------------------------------------------------

/// A vendor quota window on the caller-named account: refuse exit 78 with
/// reason `provider_quota_lock` while the account's `rate_limited_until` is in
/// the future. NOT machine busy-ness, so `--force` never buys past it. An
/// unreadable state file reads as unlocked, exactly as the Python reader's
/// missing-disk arm reads (`is_in_cooldown`).
pub(crate) fn check_account_quota_lock(
    config_cwd: &Path,
    account: &str,
    warnings: &mut Vec<String>,
) -> Result<(), crate::spawn_gate::Refusal> {
    use crate::spawn_gate::{Refusal, EXIT_PROVIDER_CAP};
    if account.is_empty() || account == "default" {
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let Some(resets_at) = account_locked_until(&runtime_state_payload(config_cwd), account, now)
    else {
        return Ok(());
    };
    let when = chrono_like_iso(resets_at);
    warnings.push(format!(
        "spawn-gate: account {account} is rate-limited until {when}; refusing; no worker launched"
    ));
    Err(Refusal::with_receipt(
        EXIT_PROVIDER_CAP,
        serde_json::json!({
            "status": "refused",
            "reason": "provider_quota_lock",
            "account": account,
            "resets_at": resets_at,
        }),
    ))
}

/// The provider runtime-state payload: `$FNO_RUNTIME_STATE_PATH`, else the
/// configured state root's `provider-runtime-state.json`. An unreadable file
/// is an empty payload (unlocked), matching the Python reader's None arm.
fn runtime_state_payload(config_cwd: &Path) -> Value {
    let path: PathBuf = match std::env::var_os("FNO_RUNTIME_STATE_PATH") {
        Some(override_) => PathBuf::from(override_),
        None => {
            let mut path = agents_config::state_dir(config_cwd).unwrap_or_else(default_state_dir);
            path.push("provider-runtime-state.json");
            path
        }
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null)
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".fno")
}

/// The account's binding rate-limit deadline, when its health entry is fresh
/// (the shared record-level TTL) and the lock is in the future. Reuses the
/// fallback-chain's health parse instead of a second one.
fn account_locked_until(raw: &Value, account: &str, now: f64) -> Option<f64> {
    let health = crate::fallback_chain::parse_provider_health(raw).remove(account)?;
    let rlu = health.get("rate_limited_until").and_then(Value::as_f64)?;
    match health.get("last_error_at").and_then(Value::as_f64) {
        Some(at) if at < now - PROVIDER_HEALTH_TTL_SECONDS => return None,
        _ => {}
    }
    (rlu > now).then_some(rlu)
}

/// Epoch seconds as a UTC ISO-8601 string for the refusal prose; the receipt
/// keeps the epoch number the Python receipt kept.
fn chrono_like_iso(epoch_s: f64) -> String {
    chrono::DateTime::from_timestamp(epoch_s as i64, 0)
        .map(|dt| dt.to_string())
        .unwrap_or_else(|| "unknown (no reset was readable)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_registry(path: &Path, entries: &[String]) {
        std::fs::write(
            path,
            format!(
                r#"{{"schema_version":{},"entries":[{}]}}"#,
                crate::state::REGISTRY_SCHEMA_VERSION,
                entries.join(",")
            ),
        )
        .unwrap();
    }

    fn live_row(name: &str, provider: &str, pid: Option<u32>) -> String {
        let fields = pid
            .map(|p| {
                let start = crate::daemon::process_start_time(p).unwrap_or(0);
                format!(r#","pid":{p},"pid_start_time":{start}"#)
            })
            .unwrap_or_default();
        format!(
            r#"{{"name":"{name}","provider":"{provider}","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z"{fields}}}"#
        )
    }

    /// AC1-HP's counting rule: live rows of the provider count, dead pids and
    /// other providers do not.
    #[test]
    fn provider_count_counts_live_rows_of_the_provider_only() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-count-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        let reg = dir.join("registry.json");
        write_registry(
            &reg,
            &[
                live_row("a", "zai", Some(std::process::id())),
                live_row("b", "zai", Some(std::process::id())),
                // dead pid: provably gone, uncounted
                live_row("c", "zai", Some(4_194_321)),
                // other provider: not this lane
                live_row("d", "codex", Some(std::process::id())),
            ],
        );
        let mut warnings = Vec::new();
        let (count, counted) = provider_live_count(&reg, "zai", &mut warnings).unwrap();
        assert_eq!(count, 2, "two live zai rows");
        assert_eq!(counted, vec!["a".to_string(), "b".to_string()]);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC7-ERR: a recycled pid — alive, but a different process incarnation
    /// than the recorded start token — is not counted; the correct incarnation
    /// still is.
    #[test]
    fn provider_count_skips_a_recycled_pid() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-recycled-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        let me = std::process::id();
        let good = crate::daemon::process_start_time(me).unwrap_or(0);
        let reg = dir.join("registry.json");
        write_registry(
            &reg,
            &[
                live_row("good", "zai", Some(me)),
                format!(
                    r#"{{"name":"recycled","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","pid":{me},"pid_start_time":{}}}"#,
                    good + 1
                ),
            ],
        );
        let mut warnings = Vec::new();
        let (count, counted) = provider_live_count(&reg, "zai", &mut warnings).unwrap();
        assert_eq!(count, 1, "the recycled incarnation must not count");
        assert_eq!(counted, vec!["good".to_string()]);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC1-ERR: an unreadable registry refuses — never a zero that reads as
    /// an empty lane.
    #[test]
    fn provider_count_refuses_on_an_unreadable_registry() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-badreg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");
        std::fs::write(&reg, "{ not json").unwrap();
        let mut warnings = Vec::new();
        let err = provider_live_count(&reg, "zai", &mut warnings).unwrap_err();
        assert!(err.contains("fno registry unreadable"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A provider-tagged suspect reservation refuses (fail closed), and a
    /// reservation minted without the provider tag only warns.
    #[test]
    fn provider_slot_claims_refuse_on_suspect_and_warn_without_tag() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-claim-{}", std::process::id()));
        let root = dir.join("claims-root");
        let claims_dir = root.join(".fno").join("claims");
        std::fs::create_dir_all(&claims_dir).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);

        // Untagged live reservation: warned, skipped.
        let host = claims::hostname();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let untagged = claims_dir.join(format!("{}.lock", claims::encode_key("worker:untagged")));
        std::fs::write(
            &untagged,
            format!("schema_version: {}\nkey: worker:untagged\nholder: h\nacquired_at: {now}\npid: {}\nhost: {host}\n", claims::SCHEMA_VERSION, std::process::id()),
        )
        .unwrap();
        let mut warnings = Vec::new();
        let n = provider_live_slot_claims("zai", &[], &mut warnings).unwrap();
        assert_eq!(n, 0);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("without model_provider")),
            "{warnings:?}"
        );

        // Tagged reservation held by THIS live process: counted for zai.
        let tagged = claims_dir.join(format!("{}.lock", claims::encode_key("worker:tagged")));
        std::fs::write(
            &tagged,
            format!("schema_version: {}\nkey: worker:tagged\nholder: h\nacquired_at: {now}\npid: {}\nhost: {host}\nmetadata:\n  model_provider: zai\n", claims::SCHEMA_VERSION, std::process::id()),
        )
        .unwrap();
        warnings.clear();
        let n = provider_live_slot_claims("zai", &[], &mut warnings).unwrap();
        assert_eq!(n, 1, "a live zai-tagged claim counts");

        // Another provider's tag never counts for zai.
        let n = provider_live_slot_claims("codex", &[], &mut warnings).unwrap();
        assert_eq!(n, 0);

        std::env::remove_var("FNO_CLAIMS_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The king share: a crowned caller's rows divide the cap; rows naming
    /// nobody sit in the unattributed bucket; an unreadable registry nulls
    /// every count.
    #[test]
    fn king_share_divides_by_crowns_and_buckets_unattributed_rows() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-share-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");
        write_registry(
            &reg,
            &[
                format!(
                    r#"{{"name":"king-row","harness":"claude","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","crown_level":1,"harness_session_id":"king-session-uuid","spawned_by_session":null}}"#
                ),
                format!(
                    r#"{{"name":"w1","harness":"claude","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","spawned_by_session":"king-session-uuid"}}"#
                ),
                format!(
                    r#"{{"name":"w2","harness":"claude","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","spawned_by_session":null}}"#
                ),
            ],
        );
        let reading = share_reading(&reg, 6, Some("king-session-uuid"));
        assert_eq!(reading.kings, Some(1));
        assert_eq!(reading.share, Some(6));
        assert_eq!(reading.held, Some(1));
        assert_eq!(reading.held_rows, Some(vec!["w1".to_string()]));
        assert_eq!(reading.unattributed_rows, Some(vec!["w2".to_string()]));

        // A MISSING registry reads as an empty fleet (readable zeros), the
        // Python load_registry's [] arm - unlike a damaged one, which nulls.
        let missing = dir.join("nope.json");
        let reading = share_reading(&missing, 6, Some("x"));
        assert_eq!(reading.kings, Some(0));
        assert_eq!(reading.share, Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A schema_version ahead of this binary refuses exit 81 with both
    /// integers; a file this binary understands passes.
    #[test]
    fn registry_schema_ahead_refuses_with_both_integers() {
        let dir = std::env::temp_dir().join(format!("fno-lanes-schema-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":{},"entries":[]}}"#,
                crate::state::REGISTRY_SCHEMA_VERSION + 1
            ),
        )
        .unwrap();
        let mut warnings = Vec::new();
        let err = check_registry_schema(&reg, &mut warnings).unwrap_err();
        assert_eq!(err.exit_code, crate::spawn_gate::EXIT_REGISTRY_SCHEMA);
        let receipt = err.receipt.unwrap();
        assert_eq!(receipt["reason"], "registry_schema");
        assert_eq!(
            receipt["on_disk"].as_u64().unwrap() as u32,
            crate::state::REGISTRY_SCHEMA_VERSION + 1
        );
        assert_eq!(receipt["understood"], crate::state::REGISTRY_SCHEMA_VERSION);

        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":{},"entries":[]}}"#,
                crate::state::REGISTRY_SCHEMA_VERSION
            ),
        )
        .unwrap();
        warnings.clear();
        assert!(check_registry_schema(&reg, &mut warnings).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The quota lock: a future rate_limited_until on the named account
    /// refuses exit 78 provider_quota_lock; another account is untouched; an
    /// unreadable state file reads as unlocked.
    #[test]
    fn quota_lock_refuses_only_the_locked_account() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-quota-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("provider-runtime-state.json");
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            + 600.0;
        std::fs::write(
            &state,
            format!(
                r#"{{"provider_health":{{"acct-a":{{"rate_limited_until":{future},"last_error_at":{}}}}}}}"#,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64()
            ),
        )
        .unwrap();
        std::env::set_var("FNO_RUNTIME_STATE_PATH", &state);

        let mut warnings = Vec::new();
        let err = check_account_quota_lock(&dir, "acct-a", &mut warnings).unwrap_err();
        assert_eq!(err.exit_code, crate::spawn_gate::EXIT_PROVIDER_CAP);
        assert_eq!(
            err.receipt.as_ref().unwrap()["reason"],
            "provider_quota_lock"
        );
        assert_eq!(err.receipt.as_ref().unwrap()["account"], "acct-a");

        warnings.clear();
        assert!(check_account_quota_lock(&dir, "acct-b", &mut warnings).is_ok());
        assert!(check_account_quota_lock(&dir, "default", &mut warnings).is_ok());

        std::env::set_var("FNO_RUNTIME_STATE_PATH", dir.join("missing.json"));
        warnings.clear();
        assert!(check_account_quota_lock(&dir, "acct-a", &mut warnings).is_ok());

        std::env::remove_var("FNO_RUNTIME_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The lanes cap reader: the configured table wins, the built-in fallback
    /// caps only zai, and a non-positive or missing lanes is uncapped.
    #[test]
    fn lanes_cap_reads_config_with_builtin_fallback() {
        let dir = std::env::temp_dir().join(format!("fno-lanes-cap-{}", std::process::id()));
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", fnodir.join("config.toml"));
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents.provider_limits.openai]\nlanes = 3\n",
        )
        .unwrap();
        assert_eq!(provider_lanes_cap(&dir, "openai"), Some(3));
        assert_eq!(
            provider_lanes_cap(&dir, "zai"),
            None,
            "a configured table replaces the builtin"
        );

        std::fs::write(fnodir.join("config.toml"), "[agents]\nmax_live = 2\n").unwrap();
        assert_eq!(provider_lanes_cap(&dir, "zai"), Some(5), "builtin fallback");
        assert_eq!(provider_lanes_cap(&dir, "openai"), None);

        match prior_config {
            Some(v) => std::env::set_var("FNO_CONFIG", v),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

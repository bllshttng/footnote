//! The provider-lane axes of the spawn gate : the per-provider live
//! count, the king share, the registry schema guard and the account quota
//! lock, ported from the Python gate (`cli/src/fno/agents/spawn_gate.py`)
//! that this gate replaces.
//!
//! Fail-closed is the contract here, unlike the global guards: the module
//! doc in `spawn_gate.rs` lets a RAM floor or a slot count fail OPEN because
//! the gate must never brick spawning, but an UNREADABLE LANE COUNT is a
//! refusal, never a zero — assuming an empty lane oversubscribes a shared
//! account, which is the harm the provider cap exists to prevent.

use std::collections::HashSet;
use std::collections::{BTreeMap, BTreeSet};
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

// ponytail: fixed 5-hour hold matches the vendor window in the measured 429;
// upgrade is parsing the window length from the excerpt.
const UNKNOWN_RESET_HOLD_S: i64 = 5 * 3600;

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

/// A transcript quieter than this, on a row with an open operator question,
/// parks the row out of its provider lane count (the reaper's default quiet
/// grace).
const OPERATOR_WAIT_QUIET_S: u64 = 900;

/// Rows waiting on the operator: an open question they asked, and a transcript
/// quiet at least OPERATOR_WAIT_QUIET_S. Row name -> oldest open question id.
/// Only claude rows with a session id qualify; the transcript lookup is
/// claude-only. A question is open when an `operator_question` row carries its
/// id and no `operator_question_closed` row does. An unparsable line never
/// qualifies a row (the count's rule: an unreadable source never produces a
/// smaller number).
fn awaiting_operator(
    rows: &[&RegistryEntry],
    questions_raw: &str,
    transcript_age_s: impl Fn(&str) -> Option<u64>,
) -> BTreeMap<String, String> {
    let mut asks: Vec<(String, Option<String>, Option<String>)> = Vec::new(); // qid, session_id, asker
    let mut closed: HashSet<String> = HashSet::new();
    for line in questions_raw.lines() {
        if !line.contains("operator_question") {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = val.get("type").and_then(Value::as_str).unwrap_or("");
        let data = val
            .get("data")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let Some(qid) = data.get("question_id").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "operator_question" => asks.push((
                qid.to_string(),
                data.get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                data.get("asker")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            )),
            "operator_question_closed" => {
                closed.insert(qid.to_string());
            }
            _ => {}
        }
    }

    let mut waiting: BTreeMap<String, String> = BTreeMap::new();
    for row in rows {
        if row.harness_name() != "claude" {
            continue;
        }
        let Some(sid) = row.harness_session_id.as_deref() else {
            continue;
        };
        if sid.is_empty() {
            continue;
        }
        let Some(age) = transcript_age_s(sid) else {
            continue;
        };
        if age < OPERATOR_WAIT_QUIET_S {
            continue;
        }
        let short = sid.get(..8).unwrap_or(sid).to_lowercase();
        // File order is journal order, so the first OPEN ask is the oldest:
        // closed asks are filtered before the find, or a closed first question
        // would hide a newer open one.
        if let Some((qid, _, _)) = asks
            .iter()
            .filter(|(qid, _, _)| !closed.contains(qid))
            .find(|(_, q_session, q_asker)| match q_session {
                Some(s) => s.eq_ignore_ascii_case(sid),
                None => q_asker
                    .as_deref()
                    .is_some_and(|a| a.to_lowercase() == short),
            })
        {
            waiting.insert(row.name.clone(), qid.clone());
        }
    }
    waiting
}

/// The raw questions journal beside the agents home (`<fno_dir>/questions.jsonl`,
/// the path `needs.rs` `default_sources` reads). Never fails: a missing journal
/// means no questions, and any other read error pushes one warning and means no
/// questions, so waiting workers stay counted.
pub(crate) fn read_questions_journal(registry_path: &Path, warnings: &mut Vec<String>) -> String {
    match registry_path.parent().and_then(Path::parent) {
        Some(fno_dir) => {
            let path = fno_dir.join("questions.jsonl");
            match crate::event_store::journal_text_checked(
                &path,
                &crate::event_store::EventQuery::of_types(crate::needs::QUESTION_TYPES),
            ) {
                Ok(text) => text,
                Err(e) => {
                    warnings.push(format!(
                        "operator questions unreadable ({e}); waiting workers counted"
                    ));
                    String::new()
                }
            }
        }
        None => String::new(),
    }
}

/// Transcript quiet in seconds for a claude session id; `None` when the
/// transcript cannot be found or its age cannot be read.
fn transcript_age_s(sid: &str) -> Option<u64> {
    let path = crate::claude_drive::find_transcript(sid)?;
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    std::time::SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

/// IO wrapper over `awaiting_operator`: reads the questions journal and
/// measures transcript quiet through the claude transcript lookup. Never
/// fails: an unreadable journal reads as no questions, so waiting workers
/// stay counted.
pub(crate) fn read_awaiting_operator(
    registry_path: &Path,
    rows: &[&RegistryEntry],
    warnings: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let raw = read_questions_journal(registry_path, warnings);
    awaiting_operator(rows, &raw, transcript_age_s)
}

/// Count rows of ONE provider only when status and positive liveness agree
/// (the port of `spawn_gate.provider_live_count`). Returns the count, the
/// names of the rows it included, and the parked pairs it left out: a claude
/// row with an open operator question and a quiet transcript holds its process
/// and its slot but spends nothing on the lane, so it stops counting against
/// the provider cap. Every unreadable source is an `Err`, never a zero.
pub(crate) fn provider_live_count(
    registry_path: &Path,
    provider: &str,
    warnings: &mut Vec<String>,
) -> Result<(usize, Vec<String>, Vec<(String, String)>), String> {
    let questions_raw = read_questions_journal(registry_path, warnings);
    provider_live_count_with_questions(registry_path, provider, &questions_raw, warnings)
}

/// The same count over a pre-read journal, so a caller counting MANY providers
/// (the lanes probe) reads the journal once instead of once per provider.
pub(crate) fn provider_live_count_with_questions(
    registry_path: &Path,
    provider: &str,
    questions_raw: &str,
    warnings: &mut Vec<String>,
) -> Result<(usize, Vec<String>, Vec<(String, String)>), String> {
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

    let waiting = awaiting_operator(&candidates, questions_raw, transcript_age_s);

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
    let mut parked: Vec<(String, String)> = Vec::new();

    for row in &candidates {
        if let Some(qid) = waiting.get(row.name.as_str()) {
            parked.push((row.name.clone(), qid.clone()));
            continue;
        }
        if let Some(pid) = row.pid {
            if row.pid_start_time.is_none() {
                // A pid without its incarnation token: a decided death skips,
                // and the pane probe settles everything else. An undecidable
                // pid does NOT fault here - the pane gets the chance first,
                // exactly as the Python counter ordered it.
                if pid_liveness(pid, None) == Ok(false) {
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
            if pid_liveness(pid, row.pid_start_time)
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
    // A parked row's own worker reservation must not count it back: the
    // claims dedup sees both lists.
    let mut claim_seen = counted_names.clone();
    claim_seen.extend(parked.iter().map(|(n, _)| n.clone()));
    count += provider_live_slot_claims(provider, &claim_seen, warnings)?;
    Ok((count, counted_names, parked))
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
/// (`_provider_live_slot_claims`). A Suspect reservation (dead pid inside its
/// TTL) counts as live, as `live_worker_slot_claims` counts it. A corrupted
/// one refuses.
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
            ClaimState::Live | ClaimState::Suspect => count += 1,
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
    // itself crowned - i.e. never as an extra vote (LD2).
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

/// The auth wall beside the quota lock: a spawn naming an account whose
/// config dir cannot log in is refused before launch, so the node is not
/// counted as dispatched while nobody works on it. Claude-only: codex and
/// opencode status verbs read local state and cannot see a server-side
/// expiry, so a probe built on them could only ever say yes. The rules and
/// their order live in [`check_account_login_with`]; this wrapper binds the
/// production reader and probe so tests can inject both.
pub(crate) fn check_account_login(
    route_provider: Option<&str>,
    account: &str,
    warnings: &mut Vec<String>,
) -> Result<(), crate::spawn_gate::Refusal> {
    check_account_login_with(
        route_provider,
        account,
        crate::reentry::shell_account_binding,
        crate::claude_login::probe_config_dir,
        warnings,
    )
}

/// The decision body, split for tests: `binding` and `probe` are injected, so
/// a test never shells out. Rules, in order:
/// 1. no account or `default`: admit, nothing is called (the quota lock's rule).
/// 2. a route to any provider other than anthropic: admit, nothing is called
///    - a routed worker authenticates with the route's key, not the slot's login.
/// 3. an unreadable binding: admit; the Python seam's own resolver owns that
///    refusal and runs before the gate.
/// 4. a binding with no config dir (an api-key lane): admit, nothing to probe.
/// 5. a readable config dir: probe it. Logged in: admit. A logged-out verdict
///    refuses with the quota lock's exit code and a receipt naming the
///    account, the dir and the login command. An INCONCLUSIVE probe (timeout,
///    unparseable output, a non-auth error) pushes a note and admits - a
///    silent fail-open would hide a probe broken by a future claude flag
///    rename, so the note is the lane's one honesty signal.
pub(crate) fn check_account_login_with(
    route_provider: Option<&str>,
    account: &str,
    binding: impl Fn(&str) -> Result<Option<String>, String>,
    probe: impl Fn(&Path) -> crate::claude_login::Login,
    warnings: &mut Vec<String>,
) -> Result<(), crate::spawn_gate::Refusal> {
    use crate::claude_login::Login;
    use crate::spawn_gate::{Refusal, EXIT_PROVIDER_CAP};
    if account.is_empty() || account == "default" {
        return Ok(());
    }
    if route_provider.is_some_and(|p| p != "anthropic") {
        return Ok(());
    }
    let Ok(Some(dir)) = binding(account) else {
        // An Err reads admit, same as an api-key lane (rule 3/4).
        return Ok(());
    };
    let dir = PathBuf::from(dir);
    match probe(&dir) {
        Login::LoggedIn => Ok(()),
        Login::Unknown(why) => {
            warnings.push(format!(
                "spawn-gate note: account {account} login probe inconclusive ({why}); not refusing on it"
            ));
            Ok(())
        }
        Login::LoggedOut(detail) => {
            let remedy = crate::claude_login::login_command(&dir);
            warnings.push(format!(
                "spawn-gate: account {account} cannot log in ({detail}); refusing; no worker launched; run: {remedy}"
            ));
            Err(Refusal::with_receipt(
                EXIT_PROVIDER_CAP,
                serde_json::json!({
                    "status": "refused",
                    "reason": "account_not_logged_in",
                    "account": account,
                    "config_dir": dir,
                    "detail": detail,
                    "remedy": remedy,
                }),
            ))
        }
    }
}

/// The same wall on the ROUTE axis: a route-keyed spawn (`--provider zai`,
/// no `--account`) never reaches the account check above, so the lane
/// snapshot the daemon persists every PROVIDER_CAP_INTERVAL_S, armed or
/// not, is the provider-keyed lock it refuses on. A missing or unreadable
/// snapshot reads unlocked, exactly as the unreadable runtime-state arm
/// does; a reset at or before now reads unlocked (the lane is returning).
pub(crate) fn check_lane_quota_lock(
    home_root: &Path,
    provider: &str,
    warnings: &mut Vec<String>,
) -> Result<(), crate::spawn_gate::Refusal> {
    use crate::spawn_gate::{Refusal, EXIT_PROVIDER_CAP};
    if provider.is_empty() {
        return Ok(());
    }
    let home = crate::paths::AgentsHome::at(home_root.to_path_buf());
    let Some(snapshot) = crate::provider_cap::read_persisted_snapshot(&home) else {
        return Ok(());
    };
    let provider_lanes: Vec<_> = snapshot
        .lanes
        .iter()
        .filter(|lane| lane.provider == provider)
        .collect();
    if provider_lanes.is_empty() {
        warnings.push(format!(
            "spawn-gate note: provider lane {provider} quota unmeasured (no lane in snapshot); not refusing on it"
        ));
    } else if provider_lanes.iter().all(|lane| lane.state == "unmeasured") {
        warnings.push(format!(
            "spawn-gate note: provider lane {provider} quota unmeasured (no member measured); not refusing on it"
        ));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let now_epoch = now as i64;
    let Some(lane) = snapshot.lanes.iter().find(|l| {
        if l.provider != provider || l.state != "open" || l.reset_passed_epoch.is_some() {
            return false;
        }
        if l.reset_epoch.map(|r| r as f64 > now).unwrap_or(false) {
            return true;
        }
        l.reset_epoch.is_none()
            && l.members
                .iter()
                .filter(|m| m.capped)
                .filter_map(|m| m.newest_assistant.as_deref())
                .filter_map(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|ts| ts.timestamp())
                .max()
                .is_some_and(|ts| now_epoch >= ts && now_epoch - ts < UNKNOWN_RESET_HOLD_S)
    }) else {
        return Ok(());
    };
    let reset_unknown = lane.reset_epoch.is_none();
    if reset_unknown {
        let target = lane
            .missing_reset_timezone
            .first()
            .map(String::as_str)
            .unwrap_or(&lane.account);
        warnings.push(format!(
            "spawn-gate: provider lane {} has an unknown reset; refusing; set reset_timezone on the [[accounts.records]] entry for {target}; no worker launched",
            lane.lane
        ));
    } else {
        let when = chrono_like_iso(lane.reset_epoch.unwrap() as f64);
        warnings.push(format!(
            "spawn-gate: provider lane {} is rate-limited until {when}; refusing; no worker launched",
            lane.lane
        ));
    }
    Err(Refusal::with_receipt(
        EXIT_PROVIDER_CAP,
        serde_json::json!({
            "status": "refused",
            "reason": "provider_quota_lock",
            "provider": provider,
            "lane": lane.lane,
            "resets_at": lane.reset_epoch.map(|r| r as f64),
            "reset_unknown": reset_unknown,
            "missing_reset_timezone": lane.missing_reset_timezone,
        }),
    ))
}

fn runtime_state_payload(config_cwd: &Path) -> Value {
    crate::route_capacity::runtime_state_payload(config_cwd)
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

    /// A pid the OS does not report, so `is_live` reads the claim as a corpse.
    fn dead_pid() -> u32 {
        let mut candidate = 999_999u32;
        while std::path::Path::new(&format!("/proc/{candidate}")).exists()
            || unsafe { libc::kill(candidate as i32, 0) } == 0
        {
            candidate += 1;
        }
        candidate
    }

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

    /// A claude worker row with a session id and no pid (the bg shape).
    fn claude_row_json(name: &str, sid: &str) -> String {
        format!(
            r#"{{"name":"{name}","harness":"claude","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","harness_session_id":"{sid}"}}"#
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
        let (count, counted, parked) = provider_live_count(&reg, "zai", &mut warnings).unwrap();
        assert_eq!(count, 2, "two live zai rows");
        assert_eq!(counted, vec!["a".to_string(), "b".to_string()]);
        assert!(parked.is_empty(), "no parked rows in the plain fixture");
        std::env::remove_var("FNO_CLAIMS_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC7-ERR: a recycled pid, alive but a different process incarnation
    /// than the recorded start token, is not counted; the correct incarnation
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
        let (count, counted, parked) = provider_live_count(&reg, "zai", &mut warnings).unwrap();
        assert_eq!(count, 1, "the recycled incarnation must not count");
        assert_eq!(counted, vec!["good".to_string()]);
        assert!(parked.is_empty(), "no parked rows in the plain fixture");
        std::env::remove_var("FNO_CLAIMS_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unparsable journal line never qualifies a row; the parseable one
    /// after it still does.
    #[test]
    fn awaiting_operator_ignores_an_unparsable_line() {
        let a = "aaaaaaaa-0000-0000-0000-00000000000a";
        let rows = vec![serde_json::from_str::<RegistryEntry>(&claude_row_json("a", a)).unwrap()];
        let row_refs: Vec<&RegistryEntry> = rows.iter().collect();
        let questions = format!(
            "{{not json\n{{\"type\":\"operator_question\",\"data\":{{\"question_id\":\"q-1\",\"session_id\":\"{a}\"}}}}\n"
        );
        let waiting = awaiting_operator(&row_refs, &questions, |sid| (sid == a).then_some(3600));
        assert_eq!(waiting.get("a"), Some(&"q-1".to_string()));
    }

    /// AC1-HP at the helper: an open question plus a quiet transcript maps the
    /// row to its question id; a row with no question stays out even when its
    /// transcript is just as quiet.
    #[test]
    fn awaiting_operator_maps_an_open_quiet_question_to_its_row() {
        let a = "aaaaaaaa-0000-0000-0000-00000000000a";
        let b = "bbbbbbbb-0000-0000-0000-00000000000b";
        let rows: Vec<RegistryEntry> = [claude_row_json("a", a), claude_row_json("b", b)]
            .iter()
            .map(|s| serde_json::from_str::<RegistryEntry>(s).unwrap())
            .collect();
        let row_refs: Vec<&RegistryEntry> = rows.iter().collect();
        let questions = format!(
            "{{\"type\":\"operator_question\",\"data\":{{\"question_id\":\"q-1\",\"session_id\":\"{a}\"}}}}\n"
        );
        // Both transcripts quiet past the grace; only row a holds an open ask.
        let waiting = awaiting_operator(&row_refs, &questions, |_| Some(3600));
        assert_eq!(waiting.get("a"), Some(&"q-1".to_string()));
        assert_eq!(waiting.get("b"), None, "no open question, no park");
    }

    /// AC1-ACTIVE: a transcript quiet only 60 s never parks a row.
    #[test]
    fn awaiting_operator_needs_the_quiet_grace() {
        let a = "aaaaaaaa-0000-0000-0000-00000000000a";
        let rows = vec![serde_json::from_str::<RegistryEntry>(&claude_row_json("a", a)).unwrap()];
        let row_refs: Vec<&RegistryEntry> = rows.iter().collect();
        let questions = format!(
            "{{\"type\":\"operator_question\",\"data\":{{\"question_id\":\"q-1\",\"session_id\":\"{a}\"}}}}\n"
        );
        let waiting = awaiting_operator(&row_refs, &questions, |_| Some(60));
        assert!(waiting.is_empty());
    }

    /// AC1-CLOSED: a closed question does not park its row.
    #[test]
    fn awaiting_operator_skips_a_closed_question() {
        let a = "aaaaaaaa-0000-0000-0000-00000000000a";
        let rows = vec![serde_json::from_str::<RegistryEntry>(&claude_row_json("a", a)).unwrap()];
        let row_refs: Vec<&RegistryEntry> = rows.iter().collect();
        let questions = format!(
            "{{\"type\":\"operator_question\",\"data\":{{\"question_id\":\"q-1\",\"session_id\":\"{a}\"}}}}\n{{\"type\":\"operator_question_closed\",\"data\":{{\"question_id\":\"q-1\"}}}}\n"
        );
        let waiting = awaiting_operator(&row_refs, &questions, |_| Some(3600));
        assert!(waiting.is_empty());
    }

    /// The asker arm: a question carrying only the 8-character asker short id
    /// still binds to its row.
    #[test]
    fn awaiting_operator_matches_a_bare_asker_short_id() {
        let a = "aaaaaaaa-0000-0000-0000-00000000000a";
        let rows = vec![serde_json::from_str::<RegistryEntry>(&claude_row_json("a", a)).unwrap()];
        let row_refs: Vec<&RegistryEntry> = rows.iter().collect();
        let questions =
            "{\"type\":\"operator_question\",\"data\":{\"question_id\":\"q-1\",\"asker\":\"aaaaaaaa\"}}\n";
        let waiting = awaiting_operator(&row_refs, questions, |_| Some(3600));
        assert_eq!(waiting.get("a"), Some(&"q-1".to_string()));
    }

    /// AC1-ERR: an unreadable questions path (a directory) pushes exactly one
    /// warning and reads as no questions; waiting workers stay counted.
    #[test]
    fn read_awaiting_operator_warns_once_on_an_unreadable_journal() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-qdir-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("questions.jsonl")).unwrap();
        let reg = dir.join("agents").join("registry.json");
        let mut warnings = Vec::new();
        let waiting = read_awaiting_operator(&reg, &[], &mut warnings);
        assert!(waiting.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("operator questions unreadable"),
            "{warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC1-HP through the real counter: the parked row drops out of the count
    /// and its liveness is never probed (its pid token is deliberately wrong),
    /// while its question id travels in the parked list.
    #[test]
    fn provider_count_parks_a_worker_waiting_on_the_operator() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-parked-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        let projects = dir.join("projects");
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, &projects);
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let reg = agents.join("registry.json");
        let a = "aaaaaaaa-0000-0000-0000-00000000000a";
        let b = "bbbbbbbb-0000-0000-0000-00000000000b";
        std::fs::write(
            dir.join("questions.jsonl"),
            format!(
                "{{\"type\":\"operator_question\",\"data\":{{\"question_id\":\"q-1\",\"session_id\":\"{a}\"}}}}\n"
            ),
        )
        .unwrap();
        let proj = projects.join("-tmp-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let t = proj.join(format!("{a}.jsonl"));
        std::fs::write(&t, b"{}\n").unwrap();
        let old = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600;
        std::fs::File::options()
            .write(true)
            .open(&t)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(old)),
            )
            .unwrap();
        let me = std::process::id();
        let good = crate::daemon::process_start_time(me).unwrap_or(0);
        write_registry(
            &reg,
            &[
                format!(
                    r#"{{"name":"a","harness":"claude","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","harness_session_id":"{a}","pid":{me},"pid_start_time":{}}}"#,
                    good + 1
                ),
                format!(
                    r#"{{"name":"b","harness":"claude","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","harness_session_id":"{b}","pid":{me},"pid_start_time":{good}}}"#
                ),
            ],
        );
        let mut warnings = Vec::new();
        let (count, counted, parked) = provider_live_count(&reg, "zai", &mut warnings).unwrap();
        assert_eq!(count, 1, "the waiting worker stops holding the lane");
        assert_eq!(counted, vec!["b".to_string()]);
        assert_eq!(parked, vec![("a".to_string(), "q-1".to_string())]);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC2-HP: a parked row's own provider-tagged reservation does not count
    /// the row back into its lane.
    #[test]
    fn provider_count_skips_a_parked_rows_own_slot_claim() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-pclaim-{}", std::process::id()));
        let root = dir.join("claims-root");
        let claims_dir = root.join(".fno").join("claims");
        std::fs::create_dir_all(&claims_dir).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let reg = agents.join("registry.json");
        let a = "aaaaaaaa-0000-0000-0000-00000000000a";
        std::fs::write(
            dir.join("questions.jsonl"),
            format!(
                "{{\"type\":\"operator_question\",\"data\":{{\"question_id\":\"q-1\",\"session_id\":\"{a}\"}}}}\n"
            ),
        )
        .unwrap();
        let proj = dir.join("projects").join("-tmp-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let t = proj.join(format!("{a}.jsonl"));
        std::fs::write(&t, b"{}\n").unwrap();
        let old = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600;
        std::fs::File::options()
            .write(true)
            .open(&t)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(old)),
            )
            .unwrap();
        // A live worker:a reservation tagged zai, held by this live process.
        let host = claims::hostname();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let lock = claims_dir.join(format!("{}.lock", claims::encode_key("worker:a")));
        std::fs::write(
            &lock,
            format!("schema_version: {}\nkey: worker:a\nholder: h\nacquired_at: {now}\npid: {}\nhost: {host}\nmetadata:\n  model_provider: zai\n", claims::SCHEMA_VERSION, std::process::id()),
        )
        .unwrap();
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, dir.join("projects"));
        let me = std::process::id();
        write_registry(
            &reg,
            &[claude_row_json("a", a), live_row("good", "zai", Some(me))],
        );
        let mut warnings = Vec::new();
        let (count, counted, parked) = provider_live_count(&reg, "zai", &mut warnings).unwrap();
        assert_eq!(
            count, 1,
            "the reservation must not count the parked row back"
        );
        assert_eq!(counted, vec!["good".to_string()]);
        assert_eq!(parked, vec![("a".to_string(), "q-1".to_string())]);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
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

    /// A Suspect reservation counts as live, like `live_worker_slot_claims`
    /// counts it, and a reservation minted without the provider tag only warns.
    #[test]
    fn provider_slot_claims_count_suspect_as_live_and_warn_without_tag() {
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

        // A Suspect reservation (dead pid inside its TTL) counts too, so one
        // orphaned probe row cannot wedge the whole lane count behind an Err.
        let suspect = claims_dir.join(format!("{}.lock", claims::encode_key("worker:suspect")));
        std::fs::write(
            &suspect,
            format!("schema_version: {}\nkey: worker:suspect\nholder: h\nacquired_at: {now}\nexpires_at: {}\npid: {}\nhost: {host}\nmetadata:\n  model_provider: zai\n", claims::SCHEMA_VERSION, now + 600_000, dead_pid()),
        )
        .unwrap();
        assert!(matches!(
            claims::status("worker:suspect", Some(&root)).0,
            claims::ClaimState::Suspect
        ));
        warnings.clear();
        let n = provider_live_slot_claims("zai", &[], &mut warnings).unwrap();
        assert_eq!(n, 2, "a suspect zai-tagged claim counts as one slot");

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

    /// A terminal row (exited / orphaned) is already uncounted by the share:
    /// `status_is_liveish` skips it, so a stop that lands a terminal status
    /// frees the slot without any reconcile.
    #[test]
    fn share_reading_skips_terminal_rows() {
        let _guard = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-lanes-terminal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");
        write_registry(
            &reg,
            &[
                format!(
                    r#"{{"name":"live-one","harness":"claude","cwd":"/tmp","status":"idle","created_at":"2026-01-01T00:00:00Z","spawned_by_session":"caller-uuid"}}"#
                ),
                format!(
                    r#"{{"name":"exited-one","harness":"claude","cwd":"/tmp","status":"exited","created_at":"2026-01-01T00:00:00Z","spawned_by_session":"caller-uuid"}}"#
                ),
                format!(
                    r#"{{"name":"orphaned-one","harness":"claude","cwd":"/tmp","status":"orphaned","created_at":"2026-01-01T00:00:00Z","spawned_by_session":"caller-uuid"}}"#
                ),
            ],
        );
        let reading = share_reading(&reg, 6, Some("caller-uuid"));
        assert_eq!(reading.held, Some(1));
        assert_eq!(reading.held_rows, Some(vec!["live-one".to_string()]));
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

    /// The route lane lock: a fresh open lane with a future reset refuses
    /// exit 78 provider_quota_lock naming the lane; another provider, a
    /// passed reset, and a missing snapshot all read unlocked.
    #[test]
    fn lane_quota_lock_refuses_the_walled_provider_route() {
        let dir = std::env::temp_dir().join(format!("fno-lanes-lanequota-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("provider-cap")).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let lane_json = |reset: i64| {
            format!(
                r#"{{"lanes":[{{"lane":"zai:default","provider":"zai","account":"default","reset_epoch":{reset},"reset_passed_epoch":null,"missing_reset_timezone":[],"state":"open","members":[]}}],"measured_at":"probe","measured_at_epoch":{now}}}"#
            )
        };
        std::fs::write(
            dir.join("provider-cap").join("snapshot.json"),
            lane_json(now + 600),
        )
        .unwrap();

        let mut warnings = Vec::new();
        let err = check_lane_quota_lock(&dir, "zai", &mut warnings).unwrap_err();
        assert_eq!(err.exit_code, crate::spawn_gate::EXIT_PROVIDER_CAP);
        assert_eq!(
            err.receipt.as_ref().unwrap()["reason"],
            "provider_quota_lock"
        );
        assert_eq!(err.receipt.as_ref().unwrap()["lane"], "zai:default");
        assert_eq!(
            err.receipt.as_ref().unwrap()["resets_at"].as_f64(),
            Some((now + 600) as f64)
        );

        warnings.clear();
        assert!(check_lane_quota_lock(&dir, "anthropic", &mut warnings).is_ok());

        std::fs::write(
            dir.join("provider-cap").join("snapshot.json"),
            lane_json(now - 60),
        )
        .unwrap();
        warnings.clear();
        assert!(check_lane_quota_lock(&dir, "zai", &mut warnings).is_ok());

        warnings.clear();
        assert!(check_lane_quota_lock(&dir.join("absent"), "zai", &mut warnings).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lane_quota_lock_warns_when_provider_lane_is_unmeasured() {
        let dir = std::env::temp_dir().join(format!("fno-lanes-unmeasured-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("provider-cap")).unwrap();
        std::fs::write(
            dir.join("provider-cap").join("snapshot.json"),
            serde_json::json!({
                "lanes": [{
                    "lane": "openai:default",
                    "provider": "openai",
                    "account": "default",
                    "reset_epoch": null,
                    "reset_passed_epoch": null,
                    "missing_reset_timezone": [],
                    "state": "unmeasured",
                    "members": []
                }],
                "measured_at": "probe",
                "measured_at_epoch": 1_000_000_000
            })
            .to_string(),
        )
        .unwrap();

        let mut warnings = Vec::new();
        assert!(check_lane_quota_lock(&dir, "openai", &mut warnings).is_ok());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("spawn-gate note:"));
        assert!(warnings[0].contains("provider lane openai quota unmeasured"));
        assert!(warnings[0].contains("no member measured"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lane_quota_lock_holds_recent_unknown_reset_but_not_old_or_returning_lane() {
        let dir = std::env::temp_dir().join(format!("fno-lanes-unknown-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("provider-cap")).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let snapshot = |newest: i64, reset_passed_epoch: Option<i64>| {
            serde_json::json!({
                "lanes": [{
                    "lane": "zai:default",
                    "provider": "zai",
                    "account": "default",
                    "reset_epoch": null,
                    "reset_passed_epoch": reset_passed_epoch,
                    "missing_reset_timezone": ["default"],
                    "state": "open",
                    "members": [{
                        "name": "w-1",
                        "session_id": null,
                        "harness": "claude",
                        "provider": "zai",
                        "account": "default",
                        "node": null,
                        "cwd": null,
                        "capped": true,
                        "cap_unknown": null,
                        "newest_assistant": chrono::DateTime::from_timestamp(newest, 0).unwrap().to_rfc3339(),
                        "held": null,
                        "excerpt": "API Error: 429"
                    }]
                }],
                "measured_at": "probe",
                "measured_at_epoch": now
            })
        };
        std::fs::write(
            dir.join("provider-cap").join("snapshot.json"),
            snapshot(now - 600, None).to_string(),
        )
        .unwrap();

        let mut warnings = Vec::new();
        let err = check_lane_quota_lock(&dir, "zai", &mut warnings).unwrap_err();
        assert_eq!(err.exit_code, crate::spawn_gate::EXIT_PROVIDER_CAP);
        assert_eq!(err.receipt.as_ref().unwrap()["reset_unknown"], true);
        assert!(err.receipt.as_ref().unwrap()["resets_at"].is_null());
        assert_eq!(
            err.receipt.as_ref().unwrap()["missing_reset_timezone"],
            serde_json::json!(["default"])
        );
        assert!(warnings[0].contains("set reset_timezone"));

        std::fs::write(
            dir.join("provider-cap").join("snapshot.json"),
            snapshot(now - 6 * 3600, None).to_string(),
        )
        .unwrap();
        warnings.clear();
        assert!(check_lane_quota_lock(&dir, "zai", &mut warnings).is_ok());

        std::fs::write(
            dir.join("provider-cap").join("snapshot.json"),
            snapshot(now - 600, Some(now - 60)).to_string(),
        )
        .unwrap();
        warnings.clear();
        assert!(check_lane_quota_lock(&dir, "zai", &mut warnings).is_ok());
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

    // ---- check_account_login_with: one test per rule ----

    use crate::claude_login::Login;

    /// A binding/probe pair that counts every call, so a rule that must
    /// short-circuit proves it called nothing.
    fn login_lane_fakes(
        binding_result: Result<Option<String>, String>,
        probe_verdict: Login,
    ) -> (
        impl Fn(&str) -> Result<Option<String>, String>,
        impl Fn(&Path) -> Login,
        std::rc::Rc<std::cell::Cell<usize>>,
    ) {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let binding_calls = calls.clone();
        let probe_calls = calls.clone();
        let binding = move |_id: &str| {
            binding_calls.set(binding_calls.get() + 1);
            binding_result.clone()
        };
        let probe = move |_dir: &Path| {
            probe_calls.set(probe_calls.get() + 1);
            probe_verdict.clone()
        };
        (binding, probe, calls)
    }

    /// Rule 1: no account or `default` admits and calls nothing.
    #[test]
    fn login_lane_skips_empty_and_default_accounts() {
        for account in ["", "default"] {
            let (binding, probe, calls) = login_lane_fakes(Ok(None), Login::LoggedIn);
            let mut warnings = Vec::new();
            assert!(check_account_login_with(None, account, binding, probe, &mut warnings).is_ok());
            assert_eq!(calls.get(), 0, "account {account:?} must call nothing");
            assert!(warnings.is_empty());
        }
    }

    /// Rule 2: a route to any non-anthropic provider admits and calls
    /// nothing - a routed worker authenticates with the route's key.
    #[test]
    fn login_lane_skips_a_routed_non_anthropic_spawn() {
        let (binding, probe, calls) = login_lane_fakes(Ok(None), Login::LoggedIn);
        let mut warnings = Vec::new();
        assert!(
            check_account_login_with(Some("zai"), "makers", binding, probe, &mut warnings).is_ok()
        );
        assert_eq!(calls.get(), 0);
        assert!(warnings.is_empty());
    }

    /// Rule 3: an unreadable binding admits; the Python resolver owns that
    /// refusal, and the probe must not run.
    #[test]
    fn login_lane_admits_an_unreadable_binding_without_probing() {
        let (binding, probe, calls) =
            login_lane_fakes(Err("no such account".to_string()), Login::LoggedIn);
        let mut warnings = Vec::new();
        assert!(check_account_login_with(None, "makers", binding, probe, &mut warnings).is_ok());
        assert_eq!(calls.get(), 1, "binding once, probe never");
        assert!(warnings.is_empty());
    }

    /// Rule 4: a binding with no config dir (an api-key lane) admits without
    /// probing.
    #[test]
    fn login_lane_admits_an_api_key_lane_without_probing() {
        let (binding, probe, calls) = login_lane_fakes(Ok(None), Login::LoggedIn);
        let mut warnings = Vec::new();
        assert!(check_account_login_with(None, "makers", binding, probe, &mut warnings).is_ok());
        assert_eq!(calls.get(), 1, "binding once, probe never");
        assert!(warnings.is_empty());
    }

    /// Rule 5, logged-out: exit 78, the full receipt, and the warning that
    /// names the remedy and says no worker launched.
    #[test]
    fn login_lane_refuses_a_logged_out_account_with_receipt() {
        let (binding, probe, _calls) = login_lane_fakes(
            Ok(Some("/tmp/acct".to_string())),
            Login::LoggedOut("Login expired".to_string()),
        );
        let mut warnings = Vec::new();
        let err = check_account_login_with(None, "makers", binding, probe, &mut warnings)
            .expect_err("a logged-out account must refuse");
        assert_eq!(err.exit_code, crate::spawn_gate::EXIT_PROVIDER_CAP);
        let receipt = err.receipt.expect("refusal carries the receipt");
        assert_eq!(receipt["reason"], "account_not_logged_in");
        assert_eq!(receipt["account"], "makers");
        assert_eq!(receipt["config_dir"], "/tmp/acct");
        assert_eq!(receipt["detail"], "Login expired");
        assert_eq!(
            receipt["remedy"],
            "CLAUDE_CONFIG_DIR=/tmp/acct claude /login"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("no worker launched") && w.contains("claude /login")),
            "{warnings:?}"
        );
    }

    /// Rule 5, inconclusive: admit, but push the one honesty note.
    #[test]
    fn login_lane_admits_an_inconclusive_probe_with_a_note() {
        let (binding, probe, _calls) = login_lane_fakes(
            Ok(Some("/tmp/acct".to_string())),
            Login::Unknown("probe timed out after 20s".to_string()),
        );
        let mut warnings = Vec::new();
        assert!(check_account_login_with(None, "makers", binding, probe, &mut warnings).is_ok());
        assert_eq!(
            warnings.len(),
            1,
            "exactly the inconclusive note: {warnings:?}"
        );
        assert!(warnings[0].contains("inconclusive"), "{warnings:?}");
    }

    /// Rule 5, logged-in: admit with no note.
    #[test]
    fn login_lane_admits_a_logged_in_account_silently() {
        let (binding, probe, _calls) =
            login_lane_fakes(Ok(Some("/tmp/acct".to_string())), Login::LoggedIn);
        let mut warnings = Vec::new();
        assert!(check_account_login_with(None, "makers", binding, probe, &mut warnings).is_ok());
        assert!(warnings.is_empty());
    }

    #[test]
    fn questions_journal_read_reaches_the_store() {
        // AC12-GATE: a store-only open question feeds the waiting read.
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("agents").join("registry.json");
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        let questions = dir.path().join("questions.jsonl");
        let row = serde_json::json!({
            "ts": "2026-09-17T12:00:00Z", "type": "operator_question", "source": "agent",
            "data": {"question_id": "q-gate-1", "question": "proceed?", "blocks": ["x-1"]}
        });
        crate::event_store::append_envelope(&questions, &row.to_string(), None).unwrap();
        let mut warnings = Vec::new();
        let raw = read_questions_journal(&registry, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(raw.contains("q-gate-1"), "{raw}");
    }
}

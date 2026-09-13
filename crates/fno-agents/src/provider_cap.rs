//! The provider cap actor (x-7e05).
//!
//! A provider usage cap strands every session on the lane: nothing notices,
//! decides, moves them, or brings them back. The trigger half already exists
//! (measure_provider_outages + the runtime-state quota lock); this module is
//! the ACTOR. It is split from the watchdog so the thing authorized to stop a
//! session is not the supervisor the operator does not trust.
//!
//! Five traps govern every read here (measured 2026-09-11):
//! 1. Sweep ALL registry rows; the outage destroys the liveness field a
//!    liveness-filtered sweep keys on (t-df09 lost its row and its work).
//! 2. Membership keys on `observed_model`, never the declared fields
//!    (`model`/`route_provider_id` are NULL fleet-wide).
//! 3. Destination headroom is refreshed before a fleet move.
//! 4. Liveness is the newest assistant timestamp, never transcript mtime.
//! 5. An unmeasured state prints `unknown`, never a false "fine".

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agents_config::{provider_cap_config, ProviderCapConfig};
use crate::paths::AgentsHome;

/// Snapshot cadence. The snapshot persists armed or unarmed, so the readout's
/// `provider_cap_off` row is a live measurement, not a wall plaque.
pub const PROVIDER_CAP_INTERVAL_S: u64 = 120;

/// ponytail: 1MB tail window is a designed ceiling. A stranded session's
/// newest assistant entry is its final message, so the quota tail sits at the
/// very end of the file. An oversized file whose tail carries no assistant
/// line reads `cap_unknown` (trap 5), never not-capped. Widen the window if a
/// lane ever reads stranded from farther back than 1MB.
const TAIL_BYTES: u64 = 1024 * 1024;

const LANES_DIR: &str = "provider-cap";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One registry row's cap reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapMember {
    /// Registry label (the mail address).
    pub name: String,
    /// The full session id, when the row carries one.
    pub session_id: Option<String>,
    pub harness: String,
    /// Trap 2: from `observed_model`, never the declared fields.
    pub provider: String,
    pub account: String,
    pub node: Option<String>,
    pub capped: bool,
    /// None = measured, no quota tail. Some = why the tail is unmeasured.
    pub cap_unknown: Option<String>,
    pub newest_assistant: Option<String>,
    /// Compacting (or a stamp whose ceiling has not expired): listed, never
    /// acted on (AC2-COMPACT).
    pub held: Option<String>,
    /// The API-error text head, for the operator question.
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapLane {
    /// `<provider>:<account>`; the token every verb and decision names.
    pub lane: String,
    pub provider: String,
    pub account: String,
    pub reset_epoch: Option<i64>,
    pub missing_reset_timezone: Vec<String>,
    pub state: String,
    pub members: Vec<CapMember>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapSnapshot {
    pub lanes: Vec<CapLane>,
    pub measured_at: String,
    pub measured_at_epoch: i64,
}

impl CapSnapshot {
    pub fn fresh(&self, max_age_s: i64, now_epoch: i64) -> bool {
        now_epoch - self.measured_at_epoch <= max_age_s
    }

    pub fn open_lanes(&self) -> Vec<&CapLane> {
        self.lanes.iter().filter(|l| l.state == "open").collect()
    }
}

// ---------------------------------------------------------------------------
// Registry reads
// ---------------------------------------------------------------------------

/// All rows, raw. Trap 1: no liveness filter. Raw Values so Python-authored
/// fields (`observed_model`, `launch_account`, `account_record_id`) survive a
/// typed read that would silently drop them.
fn registry_rows(registry_path: &std::path::Path) -> Result<Vec<Value>, String> {
    let raw =
        std::fs::read_to_string(registry_path).map_err(|e| format!("registry unreadable: {e}"))?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("registry unparseable: {e}"))?;
    Ok(v.get("agents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn s_field<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The provider lane a session actually sits on, from the model the
/// transcript named (trap 2). `unknown` never reads as membership; those
/// rows form their own lane instead of blending into a real one.
fn provider_from_model(model: &str) -> String {
    let m = model.trim().to_lowercase();
    if m.starts_with("glm") {
        "zai".to_string()
    } else if m.starts_with("claude") {
        "anthropic".to_string()
    } else if m.starts_with("gpt") {
        "openai".to_string()
    } else if m.starts_with("deepseek") {
        "deepseek".to_string()
    } else if m.is_empty() {
        "unknown".to_string()
    } else {
        m.split(['-', '_', '.'])
            .next()
            .unwrap_or("unknown")
            .to_string()
    }
}

fn provider_of(row: &Value) -> String {
    let model = row
        .get("observed_model")
        .filter(|om| om.get("kind").and_then(Value::as_str) == Some("observed"))
        .and_then(|om| om.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("");
    provider_from_model(model)
}

fn account_of(row: &Value) -> String {
    s_field(row, "account_record_id")
        .or_else(|| s_field(row, "launch_account"))
        .unwrap_or("unknown")
        .to_string()
}

// ---------------------------------------------------------------------------
// Capped-tail reading
// ---------------------------------------------------------------------------

/// The quota signals, the same ones error_taxonomy.py classifies as
/// PROVIDER_4XX_QUOTA: the 429 status plus the body markers, case-insensitive.
fn is_quota_text(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("429")
        || t.contains("rate limit")
        || t.contains("quota exceeded")
        || t.contains("usage limit")
}

/// One transcript tail reading: the newest assistant timestamp, the quota
/// excerpt when the newest assistant entry is a capped API error, and the
/// named reason when the tail could not be measured at all.
type TailReading = (Option<String>, Option<String>, Option<String>);

/// Read the transcript tail and answer: is the NEWEST assistant entry an API
/// error carrying a quota signal? Fixture shape measured in the wild:
/// `{"type":"assistant","isApiErrorMessage":true,"message":{"content":
/// [{"type":"text","text":"API Error: Request rejected (429) · [1308][Usage
/// limit reached for 5 hour. ...]"}]}}`.
pub fn capped_tail(transcript: &Path) -> TailReading {
    let meta = match std::fs::metadata(transcript) {
        Ok(m) => m,
        Err(e) => return (None, None, Some(format!("transcript-unreadable: {e}"))),
    };
    let size = meta.len();
    let start = size.saturating_sub(TAIL_BYTES);
    use std::io::{Read, Seek, SeekFrom};
    let bytes = match std::fs::File::open(transcript).and_then(|mut f| {
        f.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::with_capacity((size - start) as usize);
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }) {
        Ok(bytes) => bytes,
        Err(e) => return (None, None, Some(format!("transcript-unreadable: {e}"))),
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    // The seek may land mid-JSONL line; the fragment cannot parse, so drop it.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    for line in lines.iter().rev() {
        if !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ts = row
            .get("timestamp")
            .and_then(Value::as_str)
            .map(String::from);
        let is_api_error = row
            .get("isApiErrorMessage")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let joined = row
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        if is_api_error && is_quota_text(&joined) {
            return (ts, Some(joined.chars().take(200).collect()), None);
        }
        return (ts, None, None);
    }
    if size > TAIL_BYTES {
        return (
            None,
            None,
            Some("tail-window-missed-newest-assistant".to_string()),
        );
    }
    (
        None,
        None,
        Some("no-assistant-entry-in-transcript".to_string()),
    )
}

// ---------------------------------------------------------------------------
// Reset + timezone audit
// ---------------------------------------------------------------------------

fn runtime_state_path() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_RUNTIME_STATE_PATH") {
        return PathBuf::from(v);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".fno").join("runtime-state.json")
}

/// The quota lock the Python recovery sweep writes through record_quota_lock
/// (the same file fallback_chain parses for rate_limited_until).
fn health_reset_at(state_path: &std::path::Path, account: &str, now_f: f64) -> Option<i64> {
    let raw = std::fs::read_to_string(state_path).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let rlu = v
        .get("provider_health")?
        .get(account)?
        .get("rate_limited_until")?
        .as_f64()?;
    (rlu > now_f).then_some(rlu as i64)
}

fn account_reset_timezones(candidates: &[PathBuf]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for path in candidates {
        let Ok(raw) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(v) = serde_yaml_ng::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(accounts) = v.get("accounts").and_then(Value::as_object) else {
            continue;
        };
        for (id, rec) in accounts {
            if let Some(tz) = rec.get("reset_timezone").and_then(Value::as_str) {
                out.entry(id.to_string()).or_insert_with(|| tz.to_string());
            }
        }
    }
    out
}

/// The settings candidates in Python loader order: FNO_CONFIG, the project's
/// `.fno/settings.yaml`, the global `~/.fno/settings.yaml`.
fn settings_candidates(cwd: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(explicit) = std::env::var_os("FNO_CONFIG") {
        candidates.push(PathBuf::from(explicit));
    }
    candidates.push(cwd.join(".fno/settings.yaml"));
    if let Some(h) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(h).join(".fno/settings.yaml"));
    }
    candidates
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// One scan's resolved inputs. The env-resolving wrapper builds this; tests
/// and the verbs pass paths directly, so nothing under test reads env vars.
pub struct CapScan {
    pub registry: std::path::PathBuf,
    pub projects_dir: std::path::PathBuf,
    pub runtime_state: std::path::PathBuf,
    pub settings_candidates: Vec<std::path::PathBuf>,
    /// AgentsHome root: compaction stamps live under `<root>/compacting/`.
    pub compaction_home: std::path::PathBuf,
}

pub fn snapshot(
    home: &AgentsHome,
    cwd: &Path,
    now_epoch: i64,
    cfg: &ProviderCapConfig,
) -> Result<CapSnapshot, String> {
    let scan = CapScan {
        registry: home.registry_json(),
        projects_dir: crate::claude_drive::claude_projects_dir(),
        runtime_state: runtime_state_path(),
        settings_candidates: settings_candidates(cwd),
        compaction_home: home.root().to_path_buf(),
    };
    snapshot_with(&scan, now_epoch, cfg)
}

pub fn snapshot_with(
    scan: &CapScan,
    now_epoch: i64,
    cfg: &ProviderCapConfig,
) -> Result<CapSnapshot, String> {
    let rows = registry_rows(&scan.registry)?;
    let timezones = account_reset_timezones(&scan.settings_candidates);
    let now_f = now_epoch as f64;
    let mut lanes: BTreeMap<(String, String), Vec<CapMember>> = BTreeMap::new();
    for row in &rows {
        let Some(name) = s_field(row, "name") else {
            continue;
        };
        let name = name.to_string();
        let session_id = s_field(row, "session_id").map(String::from);
        let harness = s_field(row, "harness").unwrap_or("unknown").to_string();
        let provider = provider_of(row);
        let account = account_of(row);
        let node = s_field(row, "node")
            .or_else(|| s_field(row, "fno_node"))
            .map(String::from);
        let mut member = CapMember {
            name,
            session_id: session_id.clone(),
            harness: harness.clone(),
            provider: provider.clone(),
            account: account.clone(),
            node,
            capped: false,
            cap_unknown: None,
            newest_assistant: None,
            held: None,
            excerpt: None,
        };
        let transcript = session_id
            .as_deref()
            .and_then(|sid| crate::claude_drive::find_transcript_in(&scan.projects_dir, sid));
        if let Some(t) = &transcript {
            let (ts, excerpt, unknown) = capped_tail(t);
            member.newest_assistant = ts;
            member.cap_unknown = unknown;
            if let Some(excerpt) = excerpt {
                member.capped = true;
                member.excerpt = Some(excerpt);
            }
            let cs = crate::compaction::compaction_state(
                &crate::paths::AgentsHome::at(scan.compaction_home.clone()),
                &harness,
                session_id.as_deref().unwrap_or(""),
                Some(t),
                now_epoch,
            );
            if cs.possibly_compacting() {
                member.held = Some("compacting".to_string());
            }
        } else {
            member.cap_unknown = Some("transcript-not-found".to_string());
        }
        lanes
            .entry((provider.clone(), account.clone()))
            .or_default()
            .push(member);
    }
    let mut out: Vec<CapLane> = Vec::new();
    for ((provider, account), members) in lanes {
        let reset = health_reset_at(&scan.runtime_state, &account, now_f);
        let capped_n = members.iter().filter(|m| m.capped).count();
        let state = if capped_n >= cfg.quorum as usize || (capped_n >= 1 && reset.is_some()) {
            "open"
        } else {
            "closed"
        };
        let missing = if timezones.contains_key(&account) {
            Vec::new()
        } else {
            vec![account.clone()]
        };
        out.push(CapLane {
            lane: format!("{provider}:{account}"),
            provider,
            account,
            reset_epoch: reset,
            missing_reset_timezone: missing,
            state: state.to_string(),
            members,
        });
    }
    Ok(CapSnapshot {
        lanes: out,
        measured_at: epoch_to_rfc3339(now_epoch),
        measured_at_epoch: now_epoch,
    })
}

pub fn epoch_to_rfc3339(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn now_epoch_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// Storage: snapshot persistence, decision records
// ---------------------------------------------------------------------------

pub fn lanes_dir(home: &AgentsHome) -> PathBuf {
    home.root().join(LANES_DIR)
}

pub(crate) fn snapshot_path(home: &AgentsHome) -> PathBuf {
    lanes_dir(home).join("snapshot.json")
}

pub fn lane_file_token(lane: &str) -> String {
    lane.replace([':', '/'], "_")
}

pub fn persist_snapshot(home: &AgentsHome, snap: &CapSnapshot) {
    let dir = lanes_dir(home);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let body = json!({
        "lanes": snap.lanes,
        "measured_at": snap.measured_at,
        "measured_at_epoch": snap.measured_at_epoch,
    });
    let tmp = dir.join(format!(".snapshot.tmp-{}", std::process::id()));
    if std::fs::write(&tmp, body.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, snapshot_path(home));
    }
}

pub fn read_persisted_snapshot(home: &AgentsHome) -> Option<CapSnapshot> {
    let raw = std::fs::read_to_string(snapshot_path(home)).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    Some(CapSnapshot {
        lanes: serde_json::from_value(v.get("lanes")?.clone()).ok()?,
        measured_at: v.get("measured_at")?.as_str()?.to_string(),
        measured_at_epoch: v.get("measured_at_epoch")?.as_i64()?,
    })
}

/// Append one line to `~/.fno/questions.jsonl` (the feed's question store).
pub fn append_questions_row(row: &Value) {
    let path = home_root_parent().join("questions.jsonl");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{}", row);
    }
}

pub(crate) fn home_root_parent() -> PathBuf {
    AgentsHome::from_env()
        .root()
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".fno"))
}

// ---------------------------------------------------------------------------
// Tests. Fixtures quote the REAL 429 assistant tail measured on this machine
// (x-a13e worktree transcript, 2026-08-17).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const FOUR29_LINE: &str = r#"{"parentUuid":"p1","isSidechain":false,"type":"assistant","timestamp":"2026-09-11T06:22:32.000Z","isApiErrorMessage":true,"message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"API Error: Request rejected (429) · [1308][Usage limit reached for 5 hour. Your limit will reset at 2026-09-11 14:37:39][20260911143739fc56663065714c5e]"}]}}"#;
    const OK_LINE: &str = r#"{"type":"assistant","timestamp":"2026-09-13T10:00:00.000Z","message":{"role":"assistant","model":"glm-5.3-flash","content":[{"type":"text","text":"Running the tests now."}]}}"#;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    fn four29_row(model: &str, session: &str, account: Option<&str>) -> String {
        let mut row = format!(
            r#"{{"name":"w-{session}","session_id":"{session}","harness":"claude","observed_model":{{"kind":"observed","model":"{model}"}},"state":"working""#
        );
        if let Some(a) = account {
            row.push_str(&format!(r#","account_record_id":"{a}""#));
        }
        row.push('}');
        row
    }

    fn cfg(quorum: u32) -> ProviderCapConfig {
        ProviderCapConfig {
            quorum,
            ..ProviderCapConfig::default()
        }
    }

    fn scan(registry: PathBuf, projects: PathBuf, state: PathBuf, home: PathBuf) -> CapScan {
        CapScan {
            registry,
            projects_dir: projects,
            runtime_state: state,
            settings_candidates: vec![],
            compaction_home: home,
        }
    }

    #[test]
    fn ac2_hp_three_capped_rows_open_one_lane_with_the_runtime_reset() {
        let root = std::env::temp_dir().join(format!("pc-hp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home_dir = root.join("home");
        let projects = root.join("projects").join("-repo");
        let state_path = root.join("runtime-state.json");
        // Three rows on the GLM lane; one is provably NOT alive (trap 1) and
        // still must be swept.
        let registry = format!(
            "[{}]",
            [
                four29_row(
                    "glm-5.3-flash[1m]",
                    "11111111-1111-1111-1111-111111111111",
                    Some("zai-main")
                ),
                four29_row(
                    "glm-5.3-flash[1m]",
                    "22222222-2222-2222-2222-222222222222",
                    Some("zai-main")
                ),
                four29_row(
                    "glm-5.3-flash[1m]",
                    "33333333-3333-3333-3333-333333333333",
                    Some("zai-main")
                ),
            ]
            .join(",")
        );
        write(
            &home_dir.join("registry.json"),
            format!(r#"{{"schema_version":25,"agents":{registry}}}"#).as_str(),
        );
        for sid in [
            "11111111-1111-1111-1111-111111111111",
            "22222222-2222-2222-2222-222222222222",
            "33333333-3333-3333-3333-333333333333",
        ] {
            write(
                &projects.join(format!("{sid}.jsonl")),
                &format!("{OK_LINE}
{FOUR29_LINE}\n"),
            );
        }
        write(
            &state_path,
            r#"{"provider_health":{"zai-main":{"rate_limited_until":9999999999.0}}}"#,
        );

        let snap = snapshot_with(
            &scan(home_dir.join("registry.json"), projects.parent().unwrap().to_path_buf(), state_path, home_dir),
            1_000_000_000,
            &cfg(2),
        )
        .unwrap();
        let glm: Vec<_> = snap.lanes.iter().filter(|l| l.provider == "zai").collect();
        assert_eq!(glm.len(), 1, "one lane, not one per row");
        assert_eq!(glm[0].members.len(), 3);
        assert_eq!(glm[0].state, "open");
        assert_eq!(glm[0].reset_epoch, Some(9_999_999_999));
    }

    #[test]
    fn ac2_err_missing_reset_and_missing_timezone_are_named_not_guessed() {
        let root = std::env::temp_dir().join(format!("pc-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home_dir = root.join("home");
        let projects = root.join("projects").join("-repo");
        let state_path = root.join("runtime-state.json");
        let registry = format!(
            "[{}]",
            four29_row("glm-5.3-flash[1m]", "44444444-4444-4444-4444-444444444444", Some("zai-main"))
        );
        write(&home_dir.join("registry.json"), format!(r#"{{"schema_version":25,"agents":{registry}}}"#).as_str());
        write(
            &projects.join("44444444-4444-4444-4444-444444444444.jsonl"),
            &format!("{OK_LINE}\n{FOUR29_LINE}\n"),
        );
        // No provider_health entry at all: reset unmeasurable.
        write(&state_path, r#"{"provider_health":{}}"#);

        let snap = snapshot_with(
            &scan(home_dir.join("registry.json"), projects.parent().unwrap().to_path_buf(), state_path, home_dir),
            1_000_000_000,
            &cfg(2),
        )
        .unwrap();
        // Quorum 2 with one capped member and NO lock: closed, nothing moves.
        let lane = snap.lanes.iter().find(|l| l.provider == "zai").unwrap();
        assert_eq!(lane.state, "closed");
        assert_eq!(lane.reset_epoch, None);
        assert_eq!(lane.missing_reset_timezone, vec!["zai-main".to_string()]);
    }
    #[test]
    fn ac2_compact_a_capped_member_that_is_compacting_is_held() {
        let root = std::env::temp_dir().join(format!("pc-cm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home_dir = root.join("home");
        let projects = root.join("projects").join("-repo");
        let state_path = root.join("runtime-state.json");
        let registry = format!(
            "[{}]",
            four29_row("glm-5.3-flash[1m]", "55555555-5555-5555-5555-555555555555", Some("zai-main"))
        );
        write(&home_dir.join("registry.json"), format!(r#"{{"schema_version":25,"agents":{registry}}}"#).as_str());
        write(
            &projects.join("55555555-5555-5555-5555-555555555555.jsonl"),
            &format!("{OK_LINE}\n{FOUR29_LINE}\n"),
        );
        let now = now_epoch_secs();
        crate::compaction::mark(
            &AgentsHome::at(home_dir.clone()),
            "55555555-5555-5555-5555-555555555555",
            now,
        )
        .unwrap();

        let snap = snapshot_with(
            &scan(home_dir.join("registry.json"), projects.parent().unwrap().to_path_buf(), state_path, home_dir),
            now,
            &cfg(99),
        )
        .unwrap();
        let lane = snap.lanes.iter().find(|l| l.provider == "zai").unwrap();
        assert_eq!(lane.members[0].held.as_deref(), Some("compacting"));
    }
    #[test]
    fn ac2_arm_config_defaults_off_and_reads_overrides() {
        let root = std::env::temp_dir().join(format!("pc-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".fno")).unwrap();
        let defaults = provider_cap_config(&root);
        assert!(!defaults.enabled);
        assert_eq!(defaults.mode, "ask");
        assert_eq!(defaults.quorum, 2);
        std::fs::write(
            root.join(".fno/config.toml"),
            "[provider_cap]\nenabled = true\nmode = \"auto\"\nmin_wait_minutes = 45\nquorum = 1\n",
        )
        .unwrap();
        let armed = provider_cap_config(&root);
        assert!(armed.enabled);
        assert_eq!(armed.mode, "auto");
        assert_eq!(armed.min_wait_minutes, 45);
        assert_eq!(armed.quorum, 1);
    }
}

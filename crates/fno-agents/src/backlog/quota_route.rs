//! The quota-aware route policy for one autonomous launch: stay, defer, or
//! cut over. Ports `fno/agents/autonomous_route.py` and the
//! `evaluate_quota_signal` / headroom fold from
//! `fno/adapters/providers/runtime_state.py` over the SHARED persisted
//! runtime-state file (`provider_cap::runtime_state_path`, machine-wide by
//! design: quota is a property of an account at a vendor).
//!
//! Fail-open and opt-in, byte for byte with the Python posture: no provider,
//! quota observation off (the default), or a p0 priority short-circuits to
//! UNKNOWN with both verdicts false, so a fresh install never defers and
//! never reroutes. The one deliberate delta: the HTTP probe-on-stale leg is
//! still Python-owned, so a STALE or missing snapshot answers UNKNOWN
//! (proceed) here instead of refreshing - the safe direction, never a false
//! defer and never a reroute off an unread observation.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// `config.accounts.quota` (the quota-aware dispatch block). Both flags
/// default off; `defer_dispatch` implies `observe` at the read sites.
#[derive(Debug, Clone)]
pub struct QuotaConfig {
    pub observe: bool,
    pub defer_dispatch: bool,
    pub defer_threshold_pct: f64,
    pub probe_ttl_seconds: f64,
    pub defer_horizon_minutes: f64,
    pub pick_on_launch: bool,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        QuotaConfig {
            observe: false,
            defer_dispatch: false,
            defer_threshold_pct: 90.0,
            probe_ttl_seconds: 300.0,
            defer_horizon_minutes: 60.0,
            pick_on_launch: false,
        }
    }
}

impl QuotaConfig {
    /// One invalid leaf degrades only that leaf; valid leaves survive, so a
    /// bad global key never discards a healthy local setting.
    pub fn from_block(block: &toml::Value) -> QuotaConfig {
        let mut cfg = QuotaConfig::default();
        if let Some(v) = block.get("observe").and_then(|v| v.as_bool()) {
            cfg.observe = v;
        }
        if let Some(v) = block.get("defer_dispatch").and_then(|v| v.as_bool()) {
            cfg.defer_dispatch = v;
        }
        if let Some(v) = block
            .get("defer_threshold_pct")
            .and_then(|v| v.as_float())
            .filter(|v| (0.0..=100.0).contains(v))
        {
            cfg.defer_threshold_pct = v;
        }
        if let Some(v) = block
            .get("probe_ttl_seconds")
            .and_then(|v| v.as_integer())
            .filter(|v| *v >= 1)
        {
            cfg.probe_ttl_seconds = v as f64;
        }
        if let Some(v) = block
            .get("defer_horizon_minutes")
            .and_then(|v| v.as_integer())
            .filter(|v| *v >= 0)
        {
            cfg.defer_horizon_minutes = v as f64;
        }
        if let Some(v) = block.get("pick_on_launch").and_then(|v| v.as_bool()) {
            cfg.pick_on_launch = v;
        }
        cfg
    }
}

/// The quota config: `accounts.quota` merged across the config layers.
pub fn load_quota_config(node_cwd: Option<&str>) -> QuotaConfig {
    let cwd = node_cwd
        .map(Path::new)
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    match crate::agents_config::config_lookup(&cwd, &["accounts", "quota"]) {
        Some(block) if block.is_table() => QuotaConfig::from_block(&block),
        _ => QuotaConfig::default(),
    }
}

/// The headroom verdict: how much room the provider's account has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadroomState {
    Ok,
    Low,
    Exhausted,
    Unknown,
}

impl HeadroomState {
    pub fn as_str(&self) -> &'static str {
        match self {
            HeadroomState::Ok => "ok",
            HeadroomState::Low => "low",
            HeadroomState::Exhausted => "exhausted",
            HeadroomState::Unknown => "unknown",
        }
    }
}

/// What one quota read says about a provider for an autonomous launch.
/// `defer` and `cutover` point in OPPOSITE directions on the same LOW window:
/// a near reset is a reason to wait, a far reset is a reason to leave.
#[derive(Debug, Clone)]
pub struct QuotaSignal {
    pub provider_id: String,
    pub state: HeadroomState,
    pub resets_at: Option<f64>,
    pub defer: bool,
    pub cutover: bool,
    pub reason: String,
}

/// The route one autonomous launch attempt resolved to.
#[derive(Debug, Clone)]
pub struct AutonomousRoute {
    /// stay | defer | cutover | unknown-proceed
    pub action: String,
    pub reason: String,
    pub source_record: String,
    pub record_id: Option<String>,
    pub harness: Option<String>,
    pub retry_at: Option<f64>,
    pub window: Option<String>,
    pub defer_fallback: bool,
}

/// One usage window read off the persisted snapshot.
struct UsageWindow {
    used_pct: f64,
    resets_at: Option<f64>,
}

struct UsageSnapshot {
    windows: Vec<UsageWindow>,
    probed_at: f64,
    partial: bool,
}

fn clamp_pct(v: f64) -> f64 {
    v.clamp(0.0, 100.0)
}

/// The persisted runtime state: usage snapshots plus the provider locks,
/// machine-wide (the file both languages answer).
fn read_state_payload(node_cwd: Option<&str>) -> Option<Value> {
    let cwd = node_cwd.map(Path::new).unwrap_or(Path::new("."));
    let path = crate::provider_cap::runtime_state_path(cwd);
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// The cached usage snapshot for `provider_id`, or None when absent or older
/// than the TTL (treated as absent: the snapshot only makes dispatch smarter,
/// never gates it).
fn read_usage(
    provider_id: &str,
    ttl_seconds: f64,
    now: f64,
    node_cwd: Option<&str>,
) -> Option<UsageSnapshot> {
    let raw = read_state_payload(node_cwd)?;
    let entry = raw.get("usage")?.get(provider_id)?.as_object()?;
    let raw_windows = entry.get("windows")?.as_array()?;
    let mut windows = Vec::new();
    for w in raw_windows {
        let obj = w.as_object()?;
        let used_pct = clamp_pct(obj.get("used_pct")?.as_f64()?);
        let resets_at = match obj.get("resets_at") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_f64()?),
        };
        windows.push(UsageWindow {
            used_pct,
            resets_at,
        });
    }
    let probed_at = entry.get("probed_at")?.as_f64()?;
    if probed_at < now - ttl_seconds {
        return None;
    }
    let partial = entry
        .get("partial")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(UsageSnapshot {
        windows,
        probed_at,
        partial,
    })
}

/// The provider-level lock: `health.<id>.rate_limited_until`, plus the
/// `last_error_at` that dates it.
fn read_lock(provider_id: &str, node_cwd: Option<&str>) -> (Option<f64>, Option<f64>) {
    let Some(raw) = read_state_payload(node_cwd) else {
        return (None, None);
    };
    let Some(health) = raw
        .get("health")
        .and_then(|h| h.get(provider_id))
        .and_then(Value::as_object)
    else {
        return (None, None);
    };
    let rlu = match health.get("rate_limited_until") {
        None | Some(Value::Null) => None,
        Some(v) => v.as_f64(),
    };
    let last_error_at = match health.get("last_error_at") {
        None | Some(Value::Null) => None,
        Some(v) => v.as_f64(),
    };
    (rlu, last_error_at)
}

struct Headroom {
    state: HeadroomState,
    resets_at: Option<f64>,
}

/// The headroom verdict from a snapshot and a lock. Fresh usage is the
/// strongest account evidence; the lock is the fallback when no fresh usable
/// usage window exists. A window whose reset is already past never binds.
fn headroom_from(
    snap: Option<&UsageSnapshot>,
    rlu: Option<f64>,
    lock_at: Option<f64>,
    now: f64,
    threshold_pct: f64,
) -> Headroom {
    if rlu.is_some() && lock_at.is_some() {
        if let Some(snap) = snap {
            if snap.probed_at <= lock_at.unwrap_or(f64::INFINITY) {
                // A death recorded after the probe is the newer fact; the
                // window cannot speak for it. The lock decides until a newer
                // probe replaces it.
                return Headroom {
                    state: HeadroomState::Exhausted,
                    resets_at: rlu,
                };
            }
        }
    }
    let Some(snap) = snap else {
        if rlu.is_some() {
            return Headroom {
                state: HeadroomState::Exhausted,
                resets_at: rlu,
            };
        }
        return Headroom {
            state: HeadroomState::Unknown,
            resets_at: None,
        };
    };
    let binding: Vec<&UsageWindow> = snap
        .windows
        .iter()
        .filter(|w| w.resets_at.map_or(true, |r| r > now))
        .collect();
    let exhausted: Vec<&&UsageWindow> = binding.iter().filter(|w| w.used_pct >= 100.0).collect();
    if !snap.windows.is_empty() {
        if !exhausted.is_empty() {
            let resets: Vec<Option<f64>> = exhausted.iter().map(|w| w.resets_at).collect();
            return Headroom {
                state: HeadroomState::Exhausted,
                resets_at: resets
                    .into_iter()
                    .flatten()
                    .fold(None, |acc: Option<f64>, r| {
                        Some(acc.map_or(r, |a| a.min(r)))
                    }),
            };
        }
        if snap.partial {
            // A partial response has a missing window, so never answer OK
            // from it; the reset is the soonest one actually observed.
            let soonest: Vec<Option<f64>> = binding.iter().map(|w| w.resets_at).collect();
            return Headroom {
                state: HeadroomState::Low,
                resets_at: soonest
                    .into_iter()
                    .flatten()
                    .fold(None, |acc: Option<f64>, r| {
                        Some(acc.map_or(r, |a| a.min(r)))
                    }),
            };
        }
        if binding.is_empty() {
            return Headroom {
                state: HeadroomState::Ok,
                resets_at: None,
            };
        }
        if let Some(worst) = binding.iter().max_by(|a, b| {
            a.used_pct
                .partial_cmp(&b.used_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        }) {
            if worst.used_pct >= threshold_pct {
                return Headroom {
                    state: HeadroomState::Low,
                    resets_at: worst.resets_at,
                };
            }
        }
        return Headroom {
            state: HeadroomState::Ok,
            resets_at: None,
        };
    }
    if rlu.is_some() {
        return Headroom {
            state: HeadroomState::Exhausted,
            resets_at: rlu,
        };
    }
    // A snapshot that reported no windows is not evidence of headroom.
    Headroom {
        state: HeadroomState::Unknown,
        resets_at: None,
    }
}

/// The quota read for one provider: config gate, p0 exemption, snapshot
/// verdict, and the two dispatch verdicts read off the SAME probe.
pub fn evaluate_quota_signal(
    provider_id: &str,
    priority: Option<&str>,
    cutover_low_after_minutes: f64,
    node_cwd: Option<&str>,
    now: f64,
) -> QuotaSignal {
    let signal = |state: HeadroomState, resets_at, defer, cutover, reason: &str| QuotaSignal {
        provider_id: provider_id.to_string(),
        state,
        resets_at,
        defer,
        cutover,
        reason: reason.to_string(),
    };
    if provider_id.is_empty() {
        return signal(HeadroomState::Unknown, None, false, false, "no-provider");
    }
    let quota = load_quota_config(node_cwd);
    if !(quota.observe || quota.defer_dispatch) {
        return signal(
            HeadroomState::Unknown,
            None,
            false,
            false,
            "quota-observation-off",
        );
    }
    if priority
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
        == Some("p0")
    {
        return signal(HeadroomState::Unknown, None, false, false, "p0-exempt");
    }
    // Probe-on-stale is the Python leg: a stale snapshot reads as absent
    // here, and absent is UNKNOWN - which proceeds.
    let snap = read_usage(provider_id, quota.probe_ttl_seconds, now, node_cwd);
    let Some(snap) = snap else {
        return signal(
            HeadroomState::Unknown,
            None,
            false,
            false,
            "probe-unavailable-native",
        );
    };
    let (rlu, _lock_at) = read_lock(provider_id, node_cwd);
    let mut h = headroom_from(Some(&snap), rlu, _lock_at, now, quota.defer_threshold_pct);
    if h.state == HeadroomState::Unknown && !snap.windows.is_empty() {
        // The probe SAW the provider but the read-back did not bind: trust
        // the in-hand observation over the disk that failed to keep it.
        h = headroom_from(Some(&snap), None, None, now, quota.defer_threshold_pct);
    }
    if !quota.defer_dispatch {
        // Observation-only: neither verdict fires.
        return signal(h.state, h.resets_at, false, false, "observed");
    }
    let defer;
    let cutover;
    if h.state == HeadroomState::Exhausted {
        defer = true;
        cutover = true;
    } else if h.state == HeadroomState::Low {
        if let Some(resets_at) = h.resets_at {
            let horizon = quota.defer_horizon_minutes * 60.0;
            defer = horizon > 0.0 && resets_at <= now + horizon;
            let after = cutover_low_after_minutes * 60.0;
            cutover = after > 0.0 && !defer && resets_at > now + after;
        } else {
            defer = false;
            cutover = false;
        }
    } else {
        defer = false;
        cutover = false;
    }
    signal(h.state, h.resets_at, defer, cutover, "probed")
}

/// Whether any explicit launch intent pins the route: a provider or model
/// named on the invocation or the node, or a configured dispatch harness.
/// A pin forbids automatic replacement, never the existing defer.
pub fn launch_is_pinned(
    node: &Value,
    provider: Option<&str>,
    model: Option<&str>,
    node_cwd: Option<&str>,
) -> bool {
    if ["provider", "model", "harness"].iter().any(|k| {
        node.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
    }) {
        return true;
    }
    if provider.map(str::trim).is_some_and(|s| !s.is_empty())
        || model.map(str::trim).is_some_and(|s| !s.is_empty())
    {
        return true;
    }
    // The stage table with the deprecated dispatch.harness folded beneath it.
    let cfg = super::dispatch_resolve::dispatch_cfg_for(node_cwd, "target");
    !cfg.harness.is_empty()
}

/// The cutover destination: the first combo member whose headroom is not
/// EXHAUSTED, in the operator's configured priority order, excluding the
/// exhausted provider. OK / LOW / UNKNOWN all count as healthy targets.
/// Returns None (the defer floor) when not configured for failover, when
/// there is no combo to walk, or when every member is excluded or exhausted.
pub fn select_destination(
    node_cwd: Option<&str>,
    exhausted_provider: &str,
    now: f64,
) -> Option<(String, String)> {
    let cwd = node_cwd.map(Path::new).unwrap_or(Path::new("."));
    let on_exhaustion = crate::agents_config::config_lookup(cwd, &["dispatch", "on_exhaustion"])
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "defer".to_string());
    if on_exhaustion != "failover" {
        return None;
    }
    // The active combo by name; a bare active PROVIDER is not a combo (the
    // walk has nothing to do, and defer is the floor).
    let combo_name = crate::agents_config::config_lookup(cwd, &["providers", "active"])
        .and_then(|v| v.as_str().map(str::to_string))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let combo = crate::agents_config::config_lookup(cwd, &["providers", "combos"])?
        .get(combo_name.as_str())?
        .as_table()?;
    let members = combo.get("providers")?.as_array()?;
    let records = crate::agents_config::config_lookup(cwd, &["providers", "records"])?;
    for member in members {
        let Some(pid) = member.as_str() else {
            continue;
        };
        if pid == exhausted_provider {
            continue;
        }
        // UNKNOWN never means exhausted: a member with no signal is a valid
        // failover destination.
        let sig = evaluate_quota_signal(pid, None, 0.0, node_cwd, now);
        if sig.state == HeadroomState::Exhausted {
            continue;
        }
        let record = record_by_id(&records, pid)?;
        let harness = record
            .get("cli")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        // A record with no harness cannot pick a --provider.
        return Some((pid.to_string(), harness.to_string()));
    }
    None
}

/// One provider record by id, from a records list (array of rows) or a map.
fn record_by_id<'a>(records: &'a toml::Value, pid: &str) -> Option<&'a toml::Table> {
    if let Some(rows) = records.as_array() {
        return rows
            .iter()
            .find(|r| r.get("id").and_then(|v| v.as_str()) == Some(pid))
            .and_then(toml::Value::as_table);
    }
    records.get(pid).and_then(toml::Value::as_table)
}

/// Whether launch-time account picking is armed AND has a live account.
/// Best-effort and conservative: anything unreadable or disarmed returns
/// False so the defer stands; only a positive answer suppresses it.
fn healthy_alternate_exists(node_cwd: Option<&str>) -> bool {
    if let Some(cwd) = node_cwd {
        let same = std::env::current_dir()
            .map(|d| d == std::path::PathBuf::from(cwd))
            .unwrap_or(false);
        if !same {
            // Scoped to the process cwd: a cross-project node answers False
            // rather than suppress a defer off another repo's registry.
            return false;
        }
    }
    let quota = load_quota_config(node_cwd);
    if !quota.pick_on_launch {
        return false;
    }
    let cwd = node_cwd.map(Path::new).unwrap_or(Path::new("."));
    let Some(raw) = crate::agents_config::config_lookup(cwd, &["accounts", "records"]) else {
        return false;
    };
    let Some(records) = raw.as_array() else {
        return false;
    };
    let now = now_secs();
    records.iter().any(|rec| {
        let Some(id) = rec.get("id").and_then(|v| v.as_str()) else {
            return false;
        };
        let sig = evaluate_quota_signal(id, None, 0.0, node_cwd, now);
        sig.state != HeadroomState::Exhausted
    })
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Resolve one autonomous launch's route from one quota read.
pub fn select_autonomous_route(
    provider_id: &str,
    priority: Option<&str>,
    pinned: bool,
    node_cwd: Option<&str>,
    node_id: Option<&str>,
    now: f64,
) -> AutonomousRoute {
    let cfg = load_quota_config(node_cwd);
    let cutover_low = {
        let cwd = node_cwd.map(Path::new).unwrap_or(Path::new("."));
        crate::agents_config::config_lookup(cwd, &["accounts", "cutover_low_after_minutes"])
            .and_then(|v| v.as_integer())
            .unwrap_or(0) as f64
    };
    let _ = cfg;
    let sig = evaluate_quota_signal(provider_id, priority, cutover_low, node_cwd, now);
    let window = sig.state.as_str().to_string();
    if sig.cutover && !pinned {
        if let Some((record_id, harness)) = select_destination(node_cwd, &sig.provider_id, now) {
            return AutonomousRoute {
                action: "cutover".to_string(),
                reason: format!("{}-cutover", window.to_lowercase()),
                source_record: sig.provider_id.clone(),
                record_id: Some(record_id),
                harness: Some(harness),
                retry_at: sig.resets_at,
                window: Some(window),
                defer_fallback: sig.defer,
            };
        }
    }
    if sig.defer {
        // Deferring is the floor, not the answer: launch-time account picking
        // is the other reroute, and a pin forbids reroutes, not defers.
        if !pinned && healthy_alternate_exists(node_cwd) {
            return AutonomousRoute {
                action: "stay".to_string(),
                reason: "alternate-account-available".to_string(),
                source_record: sig.provider_id.clone(),
                record_id: None,
                harness: None,
                retry_at: None,
                window: Some(window),
                defer_fallback: false,
            };
        }
        return AutonomousRoute {
            action: "defer".to_string(),
            reason: if pinned && sig.cutover {
                "pinned".to_string()
            } else {
                sig.reason.clone()
            },
            source_record: sig.provider_id.clone(),
            record_id: None,
            harness: None,
            retry_at: sig.resets_at,
            window: Some(window),
            defer_fallback: false,
        };
    }
    if sig.state == HeadroomState::Unknown {
        return AutonomousRoute {
            action: "unknown-proceed".to_string(),
            reason: sig.reason.clone(),
            source_record: sig.provider_id.clone(),
            record_id: None,
            harness: None,
            retry_at: None,
            window: Some(window),
            defer_fallback: false,
        };
    }
    AutonomousRoute {
        action: "stay".to_string(),
        reason: sig.reason.clone(),
        source_record: sig.provider_id.clone(),
        record_id: None,
        harness: None,
        retry_at: None,
        window: Some(window),
        defer_fallback: false,
    }
}

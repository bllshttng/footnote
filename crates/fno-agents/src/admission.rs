//! Shared-account capacity admission (the routing-admission owner).
//!
//! Python resolves the machine-side facts - the budget identity (the proven
//! principal, a Keychain read), the policy from config, the state path, the
//! probe TTL - and sends this verb one JSON payload; this module owns the
//! math and the disk, beside `fallback_chain.rs`, which reads the same
//! document.
//!
//! The write lock is the SAME sidecar the Python runtime-state writers hold
//! (`<state>.update.lock`, `fcntl.flock`, the proven interop point), so a
//! reserve cannot race an unrelated health write, and every mutation
//! re-persists the whole document so an unrelated block never loses a field.
//! Rows expire after the policy's reservation TTL: expiry is a read filter
//! on reads and a drop under the lock on writes.
//!
//! Receipt statuses: admitted, reserved_capacity, stale_observation,
//! unknown_identity, exhausted, inflight_cap, invalid_policy. Every receipt
//! names its units (subscription-percent) and carries the demand and reserve
//! the decision priced, so a Python display never re-derives the lookup.

use serde_json::{json, Map, Value};
use std::io::{self, Read};
use std::os::unix::io::AsRawFd;

fn f64_of(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

fn str_of(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

fn now_or_clock(payload: &Value) -> f64 {
    f64_of(payload.get("now")).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    })
}

/// The difficulty rank for the round-up lookup (the agents.profiles idiom):
/// the most specific declared row at or below the request's band wins, and
/// the `default` key (rank 0) is the floor every band falls back to.
fn difficulty_rank(name: &str) -> i64 {
    match name {
        "default" => 0,
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        _ => 99,
    }
}

/// The percent lookup over one verb -> difficulty -> pct table, ported from
/// AdmissionPolicy._lookup: the verb table first, the `default` verb row as
/// the fallback, most specific rank at or below the request wins.
fn lookup(table: Option<&Value>, verb: &str, difficulty: &str) -> f64 {
    let requested = difficulty_rank(difficulty);
    let mut best_rank: i64 = -1;
    let mut best_pct: f64 = 0.0;
    for source_name in [verb, "default"] {
        let source = match table
            .and_then(|t| t.get(source_name))
            .and_then(Value::as_object)
        {
            Some(s) => s,
            None => continue,
        };
        for (name, pct) in source {
            let rank = difficulty_rank(name);
            let Some(pct) = pct.as_f64() else { continue };
            if rank <= requested && rank > best_rank {
                best_rank = rank;
                best_pct = pct;
            }
        }
    }
    best_pct
}

struct Window {
    label: String,
    used_pct: f64,
    resets_at: Option<f64>,
}

struct Snapshot {
    provider_id: String,
    probed_at: f64,
    partial: bool,
    windows: Vec<Window>,
}

/// Parse one usage row. A malformed row reads as ABSENT (the Python parser's
/// discipline: the whole row is dropped, never half-parsed); `partial` is the
/// row's own per-response marker and floors the verdict at stale.
fn parse_snapshot(raw: Option<&Value>) -> Option<Snapshot> {
    let obj = raw?.as_object()?;
    let provider_id = match str_of(obj.get("provider_id")) {
        Some(p) => p.to_string(),
        None => return None,
    };
    let probed_at = f64_of(obj.get("probed_at"))?;
    let mut windows = Vec::new();
    if let Some(rows) = obj.get("windows").and_then(Value::as_array) {
        for w in rows {
            let Some(wobj) = w.as_object() else {
                return None;
            };
            let Some(label) = str_of(wobj.get("label")) else {
                return None;
            };
            let used_pct = f64_of(wobj.get("used_pct"))?.clamp(0.0, 100.0);
            let resets_at = match wobj.get("resets_at") {
                None | Some(Value::Null) => None,
                Some(v) => Some(f64_of(Some(v))?),
            };
            windows.push(Window {
                label: label.to_string(),
                used_pct,
                resets_at,
            });
        }
    }
    Some(Snapshot {
        provider_id,
        probed_at,
        partial: obj.get("partial").and_then(Value::as_bool).unwrap_or(false),
        windows,
    })
}

/// The decision's answer, with the applied pricing carried for display.
struct Receipt {
    status: &'static str,
    pool: Option<String>,
    reservation_id: Option<String>,
    binding_window: Option<String>,
    remaining_admission_pct: Option<f64>,
    retry_at: Option<f64>,
    reason: Option<String>,
    evidence_age_s: Option<f64>,
    demand_applied: Option<f64>,
    reserve_applied: Option<f64>,
}

impl Receipt {
    fn with_status(status: &'static str, demand: f64, reserve: f64) -> Receipt {
        Receipt {
            status,
            pool: None,
            reservation_id: None,
            binding_window: None,
            remaining_admission_pct: None,
            retry_at: None,
            reason: None,
            evidence_age_s: None,
            demand_applied: Some(demand),
            reserve_applied: Some(reserve),
        }
    }

    fn to_json(self) -> Value {
        json!({
            "status": self.status,
            "pool": self.pool,
            "reservation_id": self.reservation_id,
            "binding_window": self.binding_window,
            "remaining_admission_pct": self.remaining_admission_pct,
            "retry_at": self.retry_at,
            "reason": self.reason,
            "evidence_age_s": self.evidence_age_s,
            "units": "subscription-percent",
            "demand_applied": self.demand_applied,
            "reserve_applied": self.reserve_applied,
        })
    }
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

/// The pure verdict. Preview and reserve run the same function, so a preview
/// never disagrees with the reservation it described. `freshness` is the
/// caller's freshness word (absent | stale | fresh) for the no-snapshot case.
#[allow(clippy::too_many_arguments)]
fn decide(
    snap: Option<&Snapshot>,
    freshness: &str,
    outstanding_pct: f64,
    inflight_count: i64,
    demand: f64,
    reserve: f64,
    max_inflight: i64,
    now: f64,
    consume_reserve: bool,
) -> Receipt {
    let Some(s) = snap else {
        return Receipt {
            status: "stale_observation",
            reason: Some(format!("no whole fresh window observation ({freshness})")),
            ..Receipt::with_status("stale_observation", demand, reserve)
        };
    };
    if s.windows.is_empty() || s.partial {
        let word = if s.partial { "partial" } else { "empty" };
        return Receipt {
            status: "stale_observation",
            reason: Some(format!("no whole fresh window observation ({word})")),
            ..Receipt::with_status("stale_observation", demand, reserve)
        };
    }
    let observed_age = now - s.probed_at;
    let binding: Vec<&Window> = s
        .windows
        .iter()
        .filter(|w| w.resets_at.map_or(true, |r| r > now))
        .collect();
    let exhausted: Vec<&Window> = binding
        .iter()
        .copied()
        .filter(|w| w.used_pct >= 100.0)
        .collect();
    if let Some(worst) = exhausted.iter().copied().max_by(|a, b| {
        a.used_pct
            .total_cmp(&b.used_pct)
            .then(b.label.cmp(&a.label))
    }) {
        let retry_at = exhausted
            .iter()
            .filter_map(|w| w.resets_at)
            .reduce(f64::min);
        return Receipt {
            status: "exhausted",
            binding_window: Some(format!("{}/{}", s.provider_id, worst.label)),
            retry_at,
            evidence_age_s: Some(observed_age),
            reason: Some("a binding window is at or past 100 percent".into()),
            ..Receipt::with_status("exhausted", demand, reserve)
        };
    }
    if inflight_count >= max_inflight {
        return Receipt {
            status: "inflight_cap",
            evidence_age_s: Some(observed_age),
            reason: Some(format!(
                "pool holds {inflight_count} live reservations >= {max_inflight}"
            )),
            ..Receipt::with_status("inflight_cap", demand, reserve)
        };
    }
    // Conjunctive: EVERY binding window must cover the demand. The floor is
    // the protected reserve; a priority exception consumes the reserve but
    // still cannot spend below zero.
    let floor = if consume_reserve { 0.0 } else { reserve };
    let mut worst_headroom: Option<f64> = None;
    let mut worst_window: Option<String> = None;
    for w in &binding {
        let headroom = 100.0 - w.used_pct - outstanding_pct - demand;
        if worst_headroom.map_or(true, |h| headroom < h) {
            worst_headroom = Some(headroom);
            worst_window = Some(format!("{}/{}", s.provider_id, w.label));
        }
        if headroom < floor {
            return Receipt {
                status: "reserved_capacity",
                binding_window: Some(format!("{}/{}", s.provider_id, w.label)),
                remaining_admission_pct: Some(round4(headroom - floor)),
                evidence_age_s: Some(observed_age),
                reason: Some(format!(
                    "window {} at {:.0}% leaves {headroom:.1}% after {:.0}% reserved and {demand:.0}% demanded, below the {reserve:.0}% reserve",
                    w.label, w.used_pct, outstanding_pct
                )),
                ..Receipt::with_status("reserved_capacity", demand, reserve)
            };
        }
    }
    Receipt {
        status: "admitted",
        binding_window: worst_window,
        remaining_admission_pct: worst_headroom.map(round4_of_floor(floor)),
        evidence_age_s: Some(observed_age),
        ..Receipt::with_status("admitted", demand, reserve)
    }
}

fn round4_of_floor(floor: f64) -> impl Fn(f64) -> f64 {
    move |h| round4(h - floor)
}

/// The unexpired subset. Expiry is a read filter on reads and a drop under
/// the lock on writes.
fn active_reservations(doc: &Value, now: f64) -> Map<String, Value> {
    let empty = Map::new();
    let block = doc
        .get("reservations")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    block
        .iter()
        .filter(|(_, rec)| f64_of(rec.get("expires_at")).unwrap_or(0.0) > now)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Reserved-demand percent and live-reservation count for one pool.
fn outstanding_for(reservations: &Map<String, Value>, pool: &str) -> (f64, i64) {
    let mut total = 0.0;
    let mut count = 0i64;
    for rec in reservations.values() {
        if str_of(rec.get("pool")) != Some(pool) {
            continue;
        }
        if !matches!(
            str_of(rec.get("state")),
            Some("reserved") | Some("committed")
        ) {
            continue;
        }
        count += 1;
        total += f64_of(rec.get("demand_pct")).unwrap_or(0.0);
    }
    (total, count)
}

/// The freshness word for one usage row against the caller's probe TTL.
fn freshness_of(snap: Option<&Snapshot>, ttl: f64, now: f64) -> &'static str {
    match snap {
        None => "absent",
        Some(s) if s.probed_at < now - ttl => "stale",
        Some(_) => "fresh",
    }
}

/// Rewrite the document with new reservations, every other block carried
/// verbatim: temp file in the same directory, then rename over the target.
fn persist(
    state_path: &std::path::Path,
    doc: &Value,
    reservations: &Map<String, Value>,
) -> Result<(), String> {
    let mut out = doc.clone();
    let Some(obj) = out.as_object_mut() else {
        return Err("state document is not an object".into());
    };
    obj.insert("reservations".into(), Value::Object(reservations.clone()));
    let parent = state_path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        state_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "state".into()),
        nanos
    ));
    use std::io::Write;
    let body = serde_json::to_string_pretty(&out).map_err(|e| format!("serialize: {e}"))?;
    std::fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()))
        .map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, state_path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

/// The Python writers' sidecar: `<state>.update.lock`, flock LOCK_EX, unlocked
/// on drop. The interop with fcntl.flock is the load-bearing coupling.
struct StateLock {
    file: std::fs::File,
}

impl StateLock {
    fn acquire(state_path: &std::path::Path) -> Result<StateLock, String> {
        let mut lock_name = state_path.as_os_str().to_owned();
        lock_name.push(".update.lock");
        let lock_path = std::path::PathBuf::from(lock_name);
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("cannot open {}: {e}", lock_path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err("flock failed".into());
        }
        Ok(StateLock { file })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn read_document(state_path: &std::path::Path) -> Option<Value> {
    let text = std::fs::read_to_string(state_path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(&text).ok()
}

fn short_id() -> String {
    let mut bytes = [0u8; 4];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        bytes = [0u8; 4];
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The one entry: mode + policy + identity in, receipt (or ok flag) out.
pub fn resolve(payload: &Value) -> Result<Value, String> {
    let mode = str_of(payload.get("mode")).unwrap_or("preview");
    let now = now_or_clock(payload);
    let policy = payload.get("policy").cloned().unwrap_or_else(|| json!({}));
    // The armed checks gate preview and reserve only: commit, release, and
    // refresh act on existing rows (cleanup must work after an operator
    // disarms), so they never ask whether admission is armed.
    let armed = matches!(mode, "preview" | "reserve");
    if armed
        && !policy
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Ok(Receipt {
            status: "stale_observation",
            reason: Some("admission is not enabled (config.routing.admission.enabled)".into()),
            ..Receipt::with_status("stale_observation", 0.0, 0.0)
        }
        .to_json());
    }
    let config_errors = armed
        && policy
            .get("config_errors")
            .and_then(Value::as_object)
            .map(|m| !m.is_empty())
            .unwrap_or(false);
    if config_errors {
        let reason = str_of(policy.get("config_error_text"))
            .unwrap_or("routing.admission is armed but its table failed validation")
            .to_string();
        return Ok(Receipt {
            status: "invalid_policy",
            reason: Some(reason),
            ..Receipt::with_status("invalid_policy", 0.0, 0.0)
        }
        .to_json());
    }
    let record = payload.get("record").cloned().unwrap_or_else(|| json!({}));
    let provider_id = match str_of(record.get("id")) {
        Some(p) => p.to_string(),
        None => return Err("payload needs record.id".into()),
    };
    let verb = str_of(payload.get("verb")).unwrap_or("do");
    let difficulty = str_of(payload.get("difficulty")).unwrap_or("high");
    let demand = payload
        .get("demand_pct")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| lookup(policy.get("demand_pct"), verb, difficulty));
    let reserve = lookup(policy.get("reserve_pct"), verb, difficulty);
    let consume_reserve = payload
        .get("consume_reserve")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ttl = f64_of(payload.get("ttl_seconds")).unwrap_or(300.0);
    let pool = str_of(payload.get("pool")).map(|s| s.to_string());
    let max_inflight = policy
        .get("max_inflight_per_pool")
        .and_then(Value::as_i64)
        .unwrap_or(3);
    let policy_ttl = f64_of(policy.get("reservation_ttl_seconds")).unwrap_or(900.0);

    let is_write = matches!(mode, "reserve" | "commit" | "release" | "refresh");
    let state_path = match str_of(payload.get("state_path")) {
        Some(p) => Some(std::path::PathBuf::from(p)),
        None => {
            if is_write {
                return Err("a write mode needs state_path".into());
            }
            None
        }
    };

    // Preview: no lock, no write, no idempotency. The inline `state` object
    // (tests) wins over the file, the fallback_chain.rs idiom.
    if mode == "preview" {
        let doc = payload
            .get("state")
            .filter(|v| v.is_object())
            .cloned()
            .or_else(|| state_path.as_deref().and_then(read_document))
            .unwrap_or_else(|| json!({}));
        return preview_answer(
            &doc,
            payload,
            &provider_id,
            pool,
            demand,
            reserve,
            max_inflight,
            ttl,
            now,
            consume_reserve,
        );
    }

    // Unknown identity refuses preview and reserve before any state read:
    // unknown usage cannot imply 100 percent available. The mutate modes
    // (commit/release/refresh) act on the reservation ROW, which carries its
    // own pool, so they never need the request's identity.
    let pool = if mode == "reserve" {
        match pool {
            Some(p) => Some(p),
            None => {
                let reason = str_of(payload.get("identity_error"))
                    .unwrap_or("unproven identity")
                    .to_string();
                return Ok(Receipt {
                    status: "unknown_identity",
                    reason: Some(reason),
                    ..Receipt::with_status("unknown_identity", demand, reserve)
                }
                .to_json());
            }
        }
    } else {
        None
    };

    let Some(state_path) = state_path else {
        return Err("a write mode needs state_path".into());
    };
    let _lock = StateLock::acquire(&state_path)?;
    let doc_present = read_document(&state_path);
    let Some(doc) = doc_present else {
        if mode == "reserve" {
            // An unreadable/absent file reads as empty state: the decide
            // below answers stale, never exhausted.
            let empty = json!({});
            return reserve_answer(
                &empty,
                payload,
                &provider_id,
                pool.as_deref().unwrap_or(""),
                demand,
                reserve,
                max_inflight,
                policy_ttl,
                ttl,
                now,
                consume_reserve,
            );
        }
        return Ok(json!({ "ok": false, "units": "subscription-percent" }));
    };
    if mode == "reserve" {
        reserve_answer(
            &doc,
            payload,
            &provider_id,
            pool.as_deref().unwrap_or(""),
            demand,
            reserve,
            max_inflight,
            policy_ttl,
            ttl,
            now,
            consume_reserve,
        )
    } else {
        mutate(
            &doc,
            &state_path,
            payload,
            mode,
            pool.as_deref().unwrap_or(""),
            now,
            policy_ttl,
        )
    }
}

/// The preview receipt over a read-only view.
#[allow(clippy::too_many_arguments)]
fn preview_answer(
    doc: &Value,
    payload: &Value,
    provider_id: &str,
    pool: Option<String>,
    demand: f64,
    reserve: f64,
    max_inflight: i64,
    ttl: f64,
    now: f64,
    consume_reserve: bool,
) -> Result<Value, String> {
    let Some(pool_name) = pool.clone() else {
        let reason = str_of(payload.get("identity_error"))
            .unwrap_or("unproven identity")
            .to_string();
        return Ok(Receipt {
            status: "unknown_identity",
            reason: Some(reason),
            ..Receipt::with_status("unknown_identity", demand, reserve)
        }
        .to_json());
    };
    let _ = pool_name;
    let reservations = active_reservations(doc, now);
    let snap = parse_snapshot(doc.get("usage").and_then(|u| u.get(provider_id)));
    let freshness = freshness_of(snap.as_ref(), ttl, now);
    let snap_ref = if freshness == "fresh" {
        snap.as_ref()
    } else {
        None
    };
    let (outstanding, count) = outstanding_for(&reservations, pool.as_deref().unwrap_or(""));
    let mut answer = decide(
        snap_ref,
        freshness,
        outstanding,
        count,
        demand,
        reserve,
        max_inflight,
        now,
        consume_reserve,
    )
    .to_json();
    if let Some(o) = answer.as_object_mut() {
        o.insert("pool".into(), json!(pool));
    }
    Ok(answer)
}

/// The reserve receipt: idempotency, decide, persist, all under the lock the
/// caller already holds.
#[allow(clippy::too_many_arguments)]
fn reserve_answer(
    doc: &Value,
    payload: &Value,
    provider_id: &str,
    pool: &str,
    demand: f64,
    reserve: f64,
    max_inflight: i64,
    policy_ttl: f64,
    ttl: f64,
    now: f64,
    consume_reserve: bool,
) -> Result<Value, String> {
    let dispatch_id = match str_of(payload.get("dispatch_id")) {
        Some(d) if !d.trim().is_empty() => d.to_string(),
        _ => return Err("reserve needs a non-empty dispatch_id".into()),
    };
    let mut reservations = active_reservations(doc, now);
    for existing in reservations.values() {
        if str_of(existing.get("dispatch_id")) == Some(dispatch_id.as_str())
            && str_of(existing.get("provider_id")) == Some(provider_id)
            && matches!(
                str_of(existing.get("state")),
                Some("reserved") | Some("committed")
            )
        {
            let rid = str_of(existing.get("reservation_id"))
                .unwrap_or("")
                .to_string();
            let prior_pool = str_of(existing.get("pool")).map(|s| s.to_string());
            return Ok(Receipt {
                status: "admitted",
                pool: prior_pool,
                reservation_id: Some(rid),
                reason: Some("existing reservation for this dispatch".into()),
                ..Receipt::with_status("admitted", demand, reserve)
            }
            .to_json());
        }
    }
    let snap = parse_snapshot(doc.get("usage").and_then(|u| u.get(provider_id)));
    let freshness = freshness_of(snap.as_ref(), ttl, now);
    let snap_ref = if freshness == "fresh" {
        snap.as_ref()
    } else {
        None
    };
    let (outstanding, count) = outstanding_for(&reservations, pool);
    let verdict = decide(
        snap_ref,
        freshness,
        outstanding,
        count,
        demand,
        reserve,
        max_inflight,
        now,
        consume_reserve,
    );
    if verdict.status != "admitted" {
        let mut answer = verdict.to_json();
        if let Some(o) = answer.as_object_mut() {
            o.insert("pool".into(), json!(pool));
        }
        return Ok(answer);
    }
    let rid = format!("adm-{}", short_id());
    let binding_windows: Vec<Value> = snap
        .as_ref()
        .map(|s| {
            s.windows
                .iter()
                .map(|w| {
                    json!({
                        "provider_id": s.provider_id,
                        "label": w.label,
                        "used_pct": w.used_pct,
                        "resets_at": w.resets_at,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let row = json!({
        "reservation_id": rid,
        "pool": pool,
        "provider_id": provider_id,
        "dispatch_id": dispatch_id,
        "session_id": payload.get("session_id").cloned().unwrap_or(Value::Null),
        "verb": str_of(payload.get("verb")).unwrap_or("do"),
        "difficulty": str_of(payload.get("difficulty")).unwrap_or("high"),
        "demand_pct": demand,
        "state": "reserved",
        "observed_at": snap.as_ref().map(|s| s.probed_at),
        "binding_windows": binding_windows,
        "created_at": now,
        "expires_at": now + policy_ttl,
    });
    reservations.insert(rid.clone(), row);
    persist(
        std::path::Path::new(str_of(payload.get("state_path")).unwrap_or("")),
        doc,
        &reservations,
    )?;
    let mut answer = verdict.to_json();
    if let Some(o) = answer.as_object_mut() {
        o.insert("pool".into(), json!(pool));
        o.insert("reservation_id".into(), json!(rid));
    }
    Ok(answer)
}

/// commit stamps the session and extends; refresh extends; release removes.
/// Only the dispatch that holds the token may act on it.
#[allow(clippy::too_many_arguments)]
fn mutate(
    doc: &Value,
    state_path: &std::path::Path,
    payload: &Value,
    mode: &str,
    _pool: &str,
    now: f64,
    policy_ttl: f64,
) -> Result<Value, String> {
    let rid = match str_of(payload.get("reservation_id")) {
        Some(r) => r.to_string(),
        None => return Ok(json!({ "ok": false, "units": "subscription-percent" })),
    };
    let dispatch_id = str_of(payload.get("dispatch_id")).unwrap_or("");
    let mut reservations = active_reservations(doc, now);
    let record = match reservations.get(&rid) {
        Some(r) if str_of(r.get("dispatch_id")) == Some(dispatch_id) => r.clone(),
        _ => return Ok(json!({ "ok": false, "units": "subscription-percent" })),
    };
    let ttl = f64_of(payload.get("ttl_seconds")).unwrap_or(policy_ttl);
    if mode == "release" {
        reservations.remove(&rid);
    } else {
        let mut row = record;
        if let Some(o) = row.as_object_mut() {
            if mode == "commit" {
                o.insert("state".into(), json!("committed"));
                o.insert(
                    "session_id".into(),
                    payload.get("session_id").cloned().unwrap_or(Value::Null),
                );
            }
            o.insert("expires_at".into(), json!(now + ttl));
        }
        reservations.insert(rid, row);
    }
    persist(state_path, doc, &reservations)?;
    Ok(json!({ "ok": true, "units": "subscription-percent" }))
}

/// The verb entry: one JSON payload on stdin, the parsed answer on stdout.
pub fn run_admission(args: &[String]) -> i32 {
    let _ = args;
    let mut payload = String::new();
    if io::stdin().read_to_string(&mut payload).is_err() {
        eprintln!("admission: cannot read payload");
        return 2;
    }
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("admission: bad payload: {e}");
            return 2;
        }
    };
    match resolve(&parsed) {
        Ok(answer) => {
            println!("{}", serde_json::to_string(&answer).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("admission: {e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Value {
        json!({
            "enabled": true,
            "max_inflight_per_pool": 3,
            "reservation_ttl_seconds": 900.0,
            "demand_pct": {"do": {"high": 15.0}},
            "reserve_pct": {"do": {"high": 10.0}},
            "config_errors": {},
        })
    }

    fn snap(probed_at: f64, windows: Value) -> Value {
        json!({"provider_id": "rec-a", "probed_at": probed_at, "windows": windows, "partial": false})
    }

    fn one_window(used_pct: f64) -> Value {
        json!([{"label": "5h", "used_pct": used_pct, "resets_at": 1600.0}])
    }

    fn payload(mode: &str, state: Value, extra: Value) -> Value {
        let mut p = json!({
            "mode": mode,
            "now": 1000.0,
            "policy": policy(),
            "record": {"id": "rec-a"},
            "pool": "api:claude/rec-a",
            "state": state,
            "verb": "do",
            "difficulty": "high",
            "ttl_seconds": 300.0,
        });
        if let (Some(a), Some(b)) = (p.as_object_mut(), extra.as_object()) {
            for (k, v) in b {
                a.insert(k.clone(), v.clone());
            }
        }
        p
    }

    fn status(answer: &Value) -> &str {
        answer.get("status").and_then(Value::as_str).unwrap_or("")
    }

    #[test]
    fn policy_disabled_reads_stale_with_reason() {
        let mut p = payload("preview", json!({}), json!({}));
        p["policy"]["enabled"] = json!(false);
        let answer = resolve(&p).unwrap();
        assert_eq!(status(&answer), "stale_observation");
        assert!(answer["reason"]
            .as_str()
            .unwrap()
            .starts_with("admission is not enabled"));
    }

    #[test]
    fn armed_tainted_policy_is_invalid() {
        let mut p = payload("preview", json!({}), json!({}));
        p["policy"]["config_errors"] = json!({"routing.admission.reserve_pct": "bad"});
        let answer = resolve(&p).unwrap();
        assert_eq!(status(&answer), "invalid_policy");
    }

    #[test]
    fn admitted_names_window_and_headroom_above_floor() {
        // 40 remaining, 10 reserve, 15 demand -> remaining 15.
        let state = json!({"usage": {"rec-a": snap(1000.0, one_window(60.0))}});
        let answer = resolve(&payload("preview", state, json!({}))).unwrap();
        assert_eq!(status(&answer), "admitted");
        assert_eq!(answer["binding_window"], json!("rec-a/5h"));
        let remaining = answer["remaining_admission_pct"].as_f64().unwrap();
        assert!((remaining - 15.0).abs() < 0.001, "{remaining}");
        assert_eq!(answer["units"], json!("subscription-percent"));
    }

    #[test]
    fn exhausted_window_wins_before_inflight() {
        let state = json!({"usage": {"rec-a": snap(1000.0, json!([
            {"label": "5h", "used_pct": 100.0, "resets_at": 1600.0},
        ]))}});
        let answer = resolve(&payload("preview", state, json!({"consume_reserve": true}))).unwrap();
        assert_eq!(status(&answer), "exhausted");
        assert_eq!(answer["binding_window"], json!("rec-a/5h"));
        assert_eq!(answer["retry_at"], json!(1600.0));
    }

    #[test]
    fn inflight_cap_refuses_at_the_bound() {
        let state = json!({
            "usage": {"rec-a": snap(1000.0, one_window(0.0))},
            "reservations": {
                "adm-1": {"pool": "api:claude/rec-a", "state": "committed", "demand_pct": 1.0, "expires_at": 1900.0},
                "adm-2": {"pool": "api:claude/rec-a", "state": "reserved", "demand_pct": 1.0, "expires_at": 1900.0},
                "adm-3": {"pool": "api:claude/rec-a", "state": "reserved", "demand_pct": 1.0, "expires_at": 1900.0},
            },
        });
        let answer = resolve(&payload("preview", state, json!({}))).unwrap();
        assert_eq!(status(&answer), "inflight_cap");
    }

    #[test]
    fn conjunctive_windows_the_lower_headroom_decides() {
        let state = json!({"usage": {"rec-a": snap(1000.0, json!([
            {"label": "5h", "used_pct": 85.0, "resets_at": 1600.0},
            {"label": "weekly", "used_pct": 70.0, "resets_at": 1600.0},
        ]))}});
        // 5h leaves 0 after 15 demand, weekly leaves 15: 5h decides, refused.
        let answer = resolve(&payload("preview", state, json!({}))).unwrap();
        assert_eq!(status(&answer), "reserved_capacity");
        assert_eq!(answer["binding_window"], json!("rec-a/5h"));
    }

    #[test]
    fn stale_and_absent_and_partial_read_stale() {
        let stale = json!({"usage": {"rec-a": snap(600.0, one_window(0.0))}});
        let answer = resolve(&payload("preview", stale, json!({}))).unwrap();
        assert_eq!(status(&answer), "stale_observation");
        assert!(answer["reason"].as_str().unwrap().contains("(stale)"));

        let absent = json!({});
        let answer = resolve(&payload("preview", absent, json!({}))).unwrap();
        assert_eq!(status(&answer), "stale_observation");
        assert!(answer["reason"].as_str().unwrap().contains("(absent)"));

        let partial = json!({"usage": {"rec-a": {
            "provider_id": "rec-a", "probed_at": 1000.0, "partial": true, "windows": one_window(0.0),
        }}});
        let answer = resolve(&payload("preview", partial, json!({}))).unwrap();
        assert_eq!(status(&answer), "stale_observation");
        assert!(answer["reason"].as_str().unwrap().contains("(partial)"));
    }

    #[test]
    fn unknown_identity_refuses_before_any_state_read() {
        let mut p = payload("preview", json!({}), json!({}));
        p["pool"] = Value::Null;
        p["identity_error"] = json!("account_identity_unknown: unprovable");
        let answer = resolve(&p).unwrap();
        assert_eq!(status(&answer), "unknown_identity");
        assert_eq!(
            answer["reason"],
            json!("account_identity_unknown: unprovable")
        );
    }

    #[test]
    fn reserve_persists_row_and_second_dispatch_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rs.json");
        std::fs::write(
            &path,
            json!({"usage": {"rec-a": snap(1000.0, one_window(60.0))}}).to_string(),
        )
        .unwrap();
        let base = json!({"state_path": path.to_str().unwrap()});
        let first = resolve(&payload(
            "reserve",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(),
                "dispatch_id": "spawn:w1",
            }),
        ))
        .unwrap();
        assert_eq!(status(&first), "admitted");
        let rid = first["reservation_id"].as_str().unwrap().to_string();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let row = &doc["reservations"][&rid];
        assert_eq!(row["state"], json!("reserved"));
        assert_eq!(row["expires_at"], json!(1900.0));

        let again = resolve(&payload(
            "reserve",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(),
                "dispatch_id": "spawn:w1",
            }),
        ))
        .unwrap();
        assert_eq!(status(&again), "admitted");
        assert_eq!(again["reservation_id"], json!(rid));
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["reservations"].as_object().unwrap().len(), 1);
        let _ = base;
    }

    #[test]
    fn same_dispatch_on_another_record_reserves_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rs.json");
        std::fs::write(
            &path,
            json!({
                "usage": {
                    "rec-a": snap(1000.0, one_window(60.0)),
                    "rec-b": snap(1000.0, one_window(60.0)),
                }
            })
            .to_string(),
        )
        .unwrap();
        let first = resolve(&payload(
            "reserve",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(), "dispatch_id": "spawn:w1",
            }),
        ))
        .unwrap();
        assert_eq!(status(&first), "admitted");
        let mut p = payload(
            "reserve",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(), "dispatch_id": "spawn:w1",
            }),
        );
        p["record"] = json!({"id": "rec-b"});
        p["pool"] = json!("api:claude/rec-b");
        let second = resolve(&p).unwrap();
        assert_eq!(status(&second), "admitted");
        assert_ne!(first["reservation_id"], second["reservation_id"]);
    }

    #[test]
    fn concurrent_capacity_is_enforced_by_the_outstanding_math() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rs.json");
        std::fs::write(
            &path,
            json!({"usage": {"rec-a": snap(1000.0, one_window(60.0))}}).to_string(),
        )
        .unwrap();
        let first = resolve(&payload(
            "reserve",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(), "dispatch_id": "a", "demand_pct": 25.0,
            }),
        ))
        .unwrap();
        assert_eq!(status(&first), "admitted");
        let second = resolve(&payload(
            "reserve",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(), "dispatch_id": "b", "demand_pct": 25.0,
            }),
        ))
        .unwrap();
        assert_eq!(status(&second), "reserved_capacity");
        assert_eq!(second["binding_window"], json!("rec-a/5h"));
    }

    #[test]
    fn commit_stamps_session_and_release_needs_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rs.json");
        std::fs::write(
            &path,
            json!({"usage": {"rec-a": snap(1000.0, one_window(0.0))}}).to_string(),
        )
        .unwrap();
        let held = resolve(&payload(
            "reserve",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(), "dispatch_id": "live", "demand_pct": 1.0,
            }),
        ))
        .unwrap();
        let rid = held["reservation_id"].as_str().unwrap().to_string();

        let wrong = resolve(&payload(
            "release",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(),
                "dispatch_id": "other", "reservation_id": rid,
            }),
        ))
        .unwrap();
        assert_eq!(wrong["ok"], json!(false));

        let mut commit_payload = payload(
            "commit",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(),
                "dispatch_id": "live", "reservation_id": rid, "session_id": "sess-1",
            }),
        );
        // No payload ttl: the policy ttl (900) extends the lease, not the
        // builder's default 300.
        commit_payload
            .as_object_mut()
            .unwrap()
            .remove("ttl_seconds");
        let committed = resolve(&commit_payload).unwrap();
        assert_eq!(committed["ok"], json!(true));
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let row = &doc["reservations"][&rid];
        assert_eq!(row["state"], json!("committed"));
        assert_eq!(row["session_id"], json!("sess-1"));
        assert_eq!(row["expires_at"], json!(1900.0));

        let released = resolve(&payload(
            "release",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(),
                "dispatch_id": "live", "reservation_id": rid,
            }),
        ))
        .unwrap();
        assert_eq!(released["ok"], json!(true));
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc["reservations"].as_object().unwrap().is_empty());
    }

    #[test]
    fn expired_rows_stop_counting() {
        let state = json!({
            "usage": {"rec-a": snap(1000.0, one_window(0.0))},
            "reservations": {
                "adm-old": {"pool": "api:claude/rec-a", "state": "committed",
                             "demand_pct": 50.0, "expires_at": 900.0},
            },
        });
        let answer = resolve(&payload("preview", state, json!({}))).unwrap();
        assert_eq!(status(&answer), "admitted");
    }

    #[test]
    fn priority_exception_consumes_the_reserve_not_the_window() {
        let state = json!({"usage": {"rec-a": snap(1000.0, one_window(85.0))}});
        let normal = resolve(&payload("preview", state.clone(), json!({}))).unwrap();
        assert_eq!(status(&normal), "reserved_capacity");
        let p0 = resolve(&payload("preview", state, json!({"consume_reserve": true}))).unwrap();
        assert_eq!(status(&p0), "admitted");
        let remaining = p0["remaining_admission_pct"].as_f64().unwrap();
        assert!((remaining - 0.0).abs() < 0.001, "{remaining}");
    }

    #[test]
    fn persist_round_trip_keeps_unrelated_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rs.json");
        std::fs::write(
            &path,
            json!({
                "schema_version": 4,
                "provider_health": {"rec-a": {"provider_id": "rec-a"}},
                "combo_cursors": {"combo": {"name": "combo"}},
                "usage": {"rec-a": snap(1000.0, one_window(0.0))},
                "windows_opened": {"rec-a": {"window": {"opened_at": 1.0, "warned": false}}},
            })
            .to_string(),
        )
        .unwrap();
        let answer = resolve(&payload(
            "reserve",
            json!({}),
            json!({
                "state_path": path.to_str().unwrap(), "dispatch_id": "w1", "demand_pct": 1.0,
            }),
        ))
        .unwrap();
        assert_eq!(status(&answer), "admitted");
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc["provider_health"].is_object());
        assert!(doc["combo_cursors"].is_object());
        assert!(doc["usage"].is_object());
        assert!(doc["windows_opened"].is_object());
        assert_eq!(doc["schema_version"], json!(4));
        assert_eq!(doc["reservations"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn lookup_takes_the_most_specific_row_at_or_below_the_band() {
        let table = json!({"do": {"default": 10.0, "high": 25.0}});
        assert_eq!(lookup(Some(&table), "do", "medium"), 10.0);
        assert_eq!(lookup(Some(&table), "do", "high"), 25.0);
        // No `do` row at or below the band, no `default` verb row: zero.
        assert_eq!(lookup(Some(&table), "other", "high"), 0.0);
        // The `default` VERB row is the fallback for any other verb.
        let with_default = json!({"do": {"high": 25.0}, "default": {"high": 12.0}});
        assert_eq!(lookup(Some(&with_default), "other", "high"), 12.0);
        assert_eq!(lookup(None, "do", "high"), 0.0);
    }

    #[test]
    fn used_pct_clamps_on_parse() {
        let raw = json!({"provider_id": "rec-a", "probed_at": 1000.0,
                          "windows": [{"label": "5h", "used_pct": 130.0, "resets_at": null}]});
        let s = parse_snapshot(Some(&raw)).unwrap();
        assert_eq!(s.windows[0].used_pct, 100.0);
        // A reset-less window binds on percentage alone.
        assert_eq!(freshness_of(Some(&s), 300.0, 1000.0), "fresh");
    }
}

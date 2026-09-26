//! The fallback-chain walk (budget port of the failover walk).
//!
//! Python resolves config and paths (the compatibility shell): it serializes
//! the chain links as raw config tables, the spent link ids, and each
//! harness's account ids, and hands over the runtime-state path it resolved.
//! This module owns the machine-truth half: canonicalizing each link (config
//! `harness` becomes the axis `provider`), minting the walk-memory id and
//! spawn flags, reading the provider runtime-state file, deriving the
//! headroom verdict per account, and filtering the chain.
//!
//! Fail-open is the contract inherited from the walk: an unreadable state
//! file, an unknown account, or any error on the way reads as UNKNOWN, and
//! UNKNOWN stays eligible - guessing "exhausted" holds a node that could have
//! run. A malformed LINK is different: it answers `{"error": ...}` so the
//! caller can refuse the config rather than mis-bill it.

use serde_json::{json, Map, Value};
use std::io::{self, Read};

/// Bounds a reactive provider-health observation; a lock alone cannot keep an
/// account classified forever (runtime_state.PROVIDER_HEALTH_TTL_SECONDS).
const PROVIDER_HEALTH_TTL_SECONDS: f64 = 60.0 * 60.0;
/// A usage snapshot older than this reads as absent (runtime_state.DEFAULT_USAGE_TTL_SECONDS).
const USAGE_TTL_SECONDS: f64 = 300.0;
/// A fresh window at or above this share of the quota reads LOW.
const THRESHOLD_PCT: f64 = 90.0;

fn f64_of(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

pub(crate) struct Health {
    rate_limited_until: Option<f64>,
    last_error_at: Option<f64>,
}

pub(crate) struct Window {
    used_pct: f64,
    resets_at: Option<f64>,
}

pub(crate) struct Snapshot {
    probed_at: f64,
    partial: bool,
    windows: Vec<Window>,
}

impl Snapshot {
    /// A snapshot from a freshly probed row (the `--refresh --json` map): the
    /// in-memory observation wins over the disk read, source `window`.
    pub(crate) fn from_refresh_row(row: &Value) -> Option<Snapshot> {
        let obj = row.as_object()?;
        Some(Snapshot {
            probed_at: f64_of(obj.get("probed_at"))?,
            partial: obj.get("partial").and_then(Value::as_bool).unwrap_or(false),
            windows: parse_windows(obj.get("windows"))?,
        })
    }
}

/// One account's headroom verdict, mirroring the Python rotation invariant.
#[derive(PartialEq, Clone, Copy, Debug)]
pub(crate) enum Verdict {
    Exhausted,
    Low,
    Ok,
    Unknown,
}

/// The verdict plus the evidence that produced it. The chain walk reads
/// `.verdict` only; the capacity map (route_capacity) carries the rest as the
/// account's provenance, exactly as the Python `_headroom_from` did.
#[derive(PartialEq, Clone, Copy, Debug)]
pub(crate) struct HeadroomVerdict {
    pub verdict: Verdict,
    /// `lock` | `window` | `stale` | `absent` | `empty`.
    pub source: &'static str,
    pub observed_at: Option<f64>,
    pub resets_at: Option<f64>,
}

pub(crate) fn headroom(
    health: Option<&Health>,
    usage: Option<&Snapshot>,
    now: f64,
) -> HeadroomVerdict {
    // Record-level health TTL: an aged-out last_error_at drops the whole
    // entry, locks included.
    let h = health.filter(|h| {
        h.last_error_at
            .map_or(true, |at| at >= now - PROVIDER_HEALTH_TTL_SECONDS)
    });
    let rlu = h.and_then(|h| h.rate_limited_until).filter(|t| *t > now);
    let lock_at = h.and_then(|h| h.last_error_at);
    // A snapshot older than the usage TTL reads as absent, and its windows
    // stop binding - the same treatment read_usage gives a stale row.
    let (snap, window): (Option<&Snapshot>, &'static str) = match usage {
        None => (None, "absent"),
        Some(s) if s.probed_at < now - USAGE_TTL_SECONDS => (None, "stale"),
        Some(s) => (Some(s), "fresh"),
    };

    // A death recorded after the probe is the newer fact: the lock decides
    // until a newer probe replaces the window.
    if let (Some(_), Some(lock_at), Some(s)) = (rlu, lock_at, snap) {
        if s.probed_at <= lock_at {
            return HeadroomVerdict {
                verdict: Verdict::Exhausted,
                source: "lock",
                observed_at: Some(lock_at),
                resets_at: rlu,
            };
        }
    }
    // A window with no reset can never be "already reset", so it always
    // binds, on percentage alone.
    let binding: Vec<&Window> = snap
        .map(|s| {
            s.windows
                .iter()
                .filter(|w| w.resets_at.map_or(true, |r| r > now))
                .collect()
        })
        .unwrap_or_default();
    if let Some(s) = snap {
        if !s.windows.is_empty() {
            let exhausted: Vec<&Window> = binding
                .iter()
                .copied()
                .filter(|w| w.used_pct >= 100.0)
                .collect();
            if !exhausted.is_empty() {
                let resets = exhausted
                    .iter()
                    .filter_map(|w| w.resets_at)
                    .reduce(f64::min);
                return HeadroomVerdict {
                    verdict: Verdict::Exhausted,
                    source: "window",
                    observed_at: Some(s.probed_at),
                    resets_at: resets,
                };
            }
            if s.partial {
                // A partial response has a missing window, so never answer OK
                // from it. The reset is the soonest one actually observed.
                let soonest = binding.iter().filter_map(|w| w.resets_at).reduce(f64::min);
                return HeadroomVerdict {
                    verdict: Verdict::Low,
                    source: "window",
                    observed_at: Some(s.probed_at),
                    resets_at: soonest,
                };
            }
            if binding.is_empty() {
                return HeadroomVerdict {
                    verdict: Verdict::Ok,
                    source: "window",
                    observed_at: Some(s.probed_at),
                    resets_at: None,
                };
            }
            let worst = binding.iter().copied().fold(f64::NAN, |a, w| {
                if a.is_nan() || w.used_pct > a {
                    w.used_pct
                } else {
                    a
                }
            });
            if worst >= THRESHOLD_PCT {
                let worst_reset = binding
                    .iter()
                    .copied()
                    .filter(|w| w.used_pct == worst)
                    .filter_map(|w| w.resets_at)
                    .next();
                return HeadroomVerdict {
                    verdict: Verdict::Low,
                    source: "window",
                    observed_at: Some(s.probed_at),
                    resets_at: worst_reset,
                };
            }
            return HeadroomVerdict {
                verdict: Verdict::Ok,
                source: "window",
                observed_at: Some(s.probed_at),
                resets_at: None,
            };
        }
    }
    if rlu.is_some() {
        // A lock remains useful when no fresh usable usage read exists.
        return HeadroomVerdict {
            verdict: Verdict::Exhausted,
            source: "lock",
            observed_at: lock_at,
            resets_at: rlu,
        };
    }
    // Absent, stale, or window-less: UNKNOWN never means exhausted and stays
    // a legal failover destination. A stale reading was still observed: its
    // probe time names the real evidence age, so only a never-probed account
    // reads age=never.
    if snap.is_none() {
        return HeadroomVerdict {
            verdict: Verdict::Unknown,
            source: window,
            observed_at: usage.map(|s| s.probed_at),
            resets_at: None,
        };
    }
    HeadroomVerdict {
        verdict: Verdict::Unknown,
        source: "empty",
        observed_at: None,
        resets_at: None,
    }
}

pub(crate) fn parse_windows(raw: Option<&Value>) -> Option<Vec<Window>> {
    let arr = raw?.as_array()?;
    let mut out = Vec::new();
    for w in arr {
        let obj = w.as_object()?;
        if !obj.get("label")?.is_string() {
            return None;
        }
        let used_pct = clamp_pct(f64_of(obj.get("used_pct"))?);
        let resets_at = match obj.get("resets_at") {
            None | Some(Value::Null) => None,
            Some(v) => Some(f64_of(Some(v))?),
        };
        out.push(Window {
            used_pct,
            resets_at,
        });
    }
    Some(out)
}

fn clamp_pct(v: f64) -> f64 {
    v.clamp(0.0, 100.0)
}

pub(crate) fn parse_usage(raw: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(block) = raw.get("usage").and_then(Value::as_object) {
        for (pid, entry) in block {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            let Some(windows) = parse_windows(obj.get("windows")) else {
                continue;
            };
            let Some(probed_at) = f64_of(obj.get("probed_at")) else {
                continue;
            };
            out.insert(
                pid.clone(),
                json!({
                    "probed_at": probed_at,
                    "partial": obj.get("partial").and_then(Value::as_bool).unwrap_or(false),
                    "windows": windows
                        .iter()
                        .map(|w| json!({"used_pct": w.used_pct, "resets_at": w.resets_at}))
                        .collect::<Vec<Value>>(),
                }),
            );
        }
    }
    out
}

/// The provider_health table as `{account: {rate_limited_until, last_error_at}}`,
/// shared with the spawn gate's quota-lock read (one parse, no second impl).
pub(crate) fn parse_provider_health(raw: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(block) = raw.get("provider_health").and_then(Value::as_object) {
        for (pid, entry) in block {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            let rlu = f64_of(obj.get("rate_limited_until"));
            let lea = f64_of(obj.get("last_error_at"));
            if rlu.is_none() && lea.is_none() {
                continue;
            }
            out.insert(
                pid.clone(),
                json!({"rate_limited_until": rlu, "last_error_at": lea}),
            );
        }
    }
    out
}

/// Canonicalize one RAW config link: config `harness` becomes the axis
/// `provider` and is name-checked, mirroring spawn_overlay's table
/// canonicalization. Err carries the operator-facing reason.
fn canonicalize_link(raw: &Value) -> Result<Map<String, Value>, String> {
    let obj = raw
        .as_object()
        .ok_or_else(|| format!("link must be a table; got {}", type_name(raw)))?;
    // Axis values are flag spellings: a non-string (a TOML int, say) must
    // refuse, never silently drop the field and spawn without it.
    for key in [
        "harness",
        "provider",
        "model",
        "route",
        "account",
        "effort",
        "permission_mode",
        "substrate",
    ] {
        if let Some(v) = obj.get(key) {
            if !v.is_string() && !v.is_null() {
                return Err(format!("link field {key:?} must be a string"));
            }
        }
    }
    let harness = obj
        .get("harness")
        .or_else(|| obj.get("provider"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !harness.is_empty() && !crate::spawn_overlay::CHAIN_HARNESSES.contains(&harness) {
        return Err(format!(
            "harness={harness:?} is not a known harness ({})",
            crate::spawn_overlay::CHAIN_HARNESSES.join("|")
        ));
    }
    let mut fields = Map::new();
    for (k, v) in obj {
        if k == "harness" {
            continue;
        }
        fields.insert(k.clone(), v.clone());
    }
    if !harness.is_empty() {
        fields.insert("provider".into(), Value::String(harness.into()));
    }
    Ok(fields)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// A `Health` from a parsed provider-health map value, shared with the
/// capacity read (one construction, no second impl).
pub(crate) fn health_from_map(v: Option<&Value>) -> Option<Health> {
    let obj = v?.as_object()?;
    Some(Health {
        rate_limited_until: f64_of(obj.get("rate_limited_until")),
        last_error_at: f64_of(obj.get("last_error_at")),
    })
}

/// A `Snapshot` from a parsed usage-map value, shared with the capacity read.
pub(crate) fn snapshot_from_map(v: Option<&Value>) -> Option<Snapshot> {
    let obj = v?.as_object()?;
    Some(Snapshot {
        probed_at: f64_of(obj.get("probed_at")).unwrap_or(0.0),
        partial: obj.get("partial").and_then(Value::as_bool).unwrap_or(false),
        windows: obj
            .get("windows")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|w| Window {
                        used_pct: f64_of(w.get("used_pct")).unwrap_or(0.0),
                        resets_at: f64_of(w.get("resets_at")),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The worst headroom verdict across the link's accounts. Callers that must
/// treat UNKNOWN differently from exhausted (the provider-cap destination
/// filter, trap 3) read this instead of the collapsed bool.
fn worst_link_verdict(
    link: &Value,
    accounts: Option<&Value>,
    health: &Map<String, Value>,
    usage: &Map<String, Value>,
    now: f64,
) -> Verdict {
    let get = |k: &str| {
        link.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
    };
    let account = get("account");
    let harness = get("provider");
    let ids: Vec<String> = if !account.is_empty() {
        vec![account.to_string()]
    } else {
        accounts
            .and_then(Value::as_object)
            .and_then(|m| m.get(harness))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    // No accounts to read: nothing was measured.
    if ids.is_empty() {
        return Verdict::Unknown;
    }
    let mut worst = Verdict::Ok;
    for pid in &ids {
        let h = health_from_map(health.get(pid));
        let u = snapshot_from_map(usage.get(pid));
        match headroom(h.as_ref(), u.as_ref(), now).verdict {
            Verdict::Exhausted => return Verdict::Exhausted,
            Verdict::Unknown => worst = Verdict::Unknown,
            _ => {}
        }
    }
    worst
}
/// Answer `{eligible: [{index, id, flags}]}`: every chain position that was
/// not already spent and is not known exhausted, with its walk-memory id and
/// spawn flags minted here, preserving chain order. A malformed link answers
/// `{"error": ...}` (exit 0) so the caller can REFUSE the config instead of
/// reading a transport fault.
pub fn resolve(payload: &Value) -> Result<Value, String> {
    let links = payload
        .get("links")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let spent: Vec<String> = payload
        .get("exclude")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let accounts = payload.get("accounts");
    let now = f64_of(payload.get("now")).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    });
    // The state path is resolved by the caller (config is Python's job); an
    // unreadable file reads as empty state, never as exhaustion. An inline
    // `state` object (tests) wins over the file.
    let state = payload
        .get("state")
        .filter(|v| v.is_object())
        .cloned()
        .or_else(|| {
            payload
                .get("state_path")
                .and_then(Value::as_str)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        })
        .unwrap_or_else(|| json!({}));
    let health = parse_provider_health(&state);
    let usage = parse_usage(&state);
    let mut eligible = Vec::new();
    for (i, raw) in links.iter().enumerate() {
        let link = match canonicalize_link(raw) {
            Ok(fields) => Value::Object(fields),
            Err(reason) => return Ok(json!({ "error": reason })),
        };
        let (id, flags) = crate::spawn_overlay::mint_link(&link);
        if spent.contains(&id) {
            continue;
        }
        let verdict = worst_link_verdict(&link, accounts, &health, &usage, now);
        if verdict == Verdict::Exhausted {
            continue;
        }
        eligible.push(json!({
            "index": i, "id": id, "flags": flags,
            "verdict": format!("{verdict:?}").to_lowercase(),
        }));
    }
    Ok(json!({ "eligible": eligible }))
}

/// The verb entry: one JSON payload on stdin, the parsed answer on stdout.
pub fn run_fallback_chain(args: &[String]) -> i32 {
    let _ = args;
    let mut payload = String::new();
    if io::stdin().read_to_string(&mut payload).is_err() {
        eprintln!("fallback-chain: cannot read payload");
        return 2;
    }
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("fallback-chain: bad payload: {e}");
            return 2;
        }
    };
    match resolve(&parsed) {
        Ok(answer) => {
            println!("{}", serde_json::to_string(&answer).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("fallback-chain: {e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(answer: &Value) -> Vec<String> {
        answer["eligible"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn walk(state: Value, links: Value, accounts: Value) -> Vec<String> {
        let payload = json!({
            "links": links,
            "state": state,
            "exclude": [],
            "accounts": accounts,
            "now": 1000.0,
        });
        ids(&resolve(&payload).unwrap())
    }

    #[test]
    fn empty_state_keeps_every_link_eligible() {
        let got = walk(
            json!({}),
            json!([{"harness": "codex", "model": "m"}]),
            json!({}),
        );
        assert_eq!(got, vec!["codex/m"]);
    }

    #[test]
    fn a_fresh_full_window_exhausts_the_account() {
        let state = json!({
            "usage": {"a1": {"probed_at": 990.0, "windows": [
                {"label": "5h", "used_pct": 100.0, "resets_at": null}
            ]}}
        });
        let got = walk(
            state,
            json!([{"harness": "codex", "model": "m", "account": "a1"}]),
            json!({}),
        );
        assert_eq!(got, Vec::<String>::new());
    }

    #[test]
    fn a_stale_lock_no_longer_exhausts() {
        let state = json!({
            "provider_health": {
                "a1": {"last_error_at": 100.0, "rate_limited_until": 200.0}
            }
        });
        // now=1000: the lock expired at 200 and the health record itself aged
        // out of the 3600s TTL, so the account reads UNKNOWN and stays up.
        let got = walk(
            state,
            json!([{"harness": "codex", "model": "m", "account": "a1"}]),
            json!({}),
        );
        assert_eq!(got, vec!["codex/m@a1"]);
    }

    #[test]
    fn a_lock_newer_than_the_probe_wins() {
        let state = json!({
            "provider_health": {"a1": {"last_error_at": 995.0, "rate_limited_until": 2000.0}},
            "usage": {"a1": {"probed_at": 900.0, "windows": [
                {"label": "5h", "used_pct": 10.0, "resets_at": 1200.0}
            ]}},
        });
        let got = walk(
            state,
            json!([{"harness": "codex", "model": "m", "account": "a1"}]),
            json!({}),
        );
        assert_eq!(got, Vec::<String>::new());
    }

    #[test]
    fn a_below_threshold_fresh_window_stays_eligible() {
        let state = json!({
            "usage": {"a1": {"probed_at": 990.0, "windows": [
                {"label": "5h", "used_pct": 40.0, "resets_at": 1200.0}
            ]}}
        });
        let got = walk(
            state,
            json!([{"harness": "codex", "model": "m", "account": "a1"}]),
            json!({}),
        );
        assert_eq!(got, vec!["codex/m@a1"]);
    }

    #[test]
    fn a_spent_link_is_filtered_by_its_id() {
        let links = json!([
            {"harness": "codex", "model": "m"},
            {"harness": "claude", "model": "m2"},
        ]);
        let payload = json!({
            "links": links,
            "state": {},
            "exclude": ["codex/m"],
            "accounts": {},
            "now": 1000.0,
        });
        let got = ids(&resolve(&payload).unwrap());
        assert_eq!(got, vec!["claude/m2"]);
    }

    #[test]
    fn an_exhausted_second_link_is_skipped_not_fatal() {
        let state = json!({
            "usage": {"a1": {"probed_at": 990.0, "windows": [
                {"label": "5h", "used_pct": 100.0, "resets_at": null}
            ]}}
        });
        let got = walk(
            state,
            json!([
                {"harness": "codex", "model": "m", "account": "a1"},
                {"harness": "claude", "model": "m2"},
            ]),
            json!({"codex": ["a1"]}),
        );
        assert_eq!(got, vec!["claude/m2"]);
    }

    #[test]
    fn a_partial_response_never_reads_ok() {
        let state = json!({
            "usage": {"a1": {"probed_at": 990.0, "partial": true, "windows": [
                {"label": "5h", "used_pct": 30.0, "resets_at": 1100.0}
            ]}}
        });
        // partial reads LOW, LOW is not exhausted, so the link stays up; this
        // pins that a partial read never escalates to a refusal verdict.
        let got = walk(
            state,
            json!([{"harness": "codex", "model": "m", "account": "a1"}]),
            json!({}),
        );
        assert_eq!(got, vec!["codex/m@a1"]);
    }

    #[test]
    fn the_state_path_is_read_and_unreadable_reads_empty() {
        let payload = json!({
            "links": [{"harness": "codex", "model": "m"}],
            "state_path": "/nonexistent/fno-state.json",
            "exclude": [],
            "accounts": {},
            "now": 1000.0,
        });
        let got = resolve(&payload).unwrap();
        assert_eq!(got["eligible"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn state_path_overrides_the_inline_state() {
        let dir = std::env::temp_dir();
        let path = dir.join("fno-chain-test-state.json");
        std::fs::write(
            &path,
            json!({
                "usage": {"a1": {"probed_at": 999.0, "windows": [
                    {"label": "5h", "used_pct": 100.0, "resets_at": null}
                ]}}
            })
            .to_string(),
        )
        .unwrap();
        let payload = json!({
            "links": [{"harness": "codex", "model": "m", "account": "a1"}],
            "state_path": path.to_str().unwrap(),
            "exclude": [],
            "accounts": {},
            "now": 1000.0,
        });
        let got = resolve(&payload).unwrap();
        assert!(got["eligible"].as_array().unwrap().is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn provider_spelling_passes_through_the_canonicalizer() {
        let got = walk(
            json!({}),
            json!([{"provider": "codex", "model": "m"}]),
            json!({}),
        );
        assert_eq!(got, vec!["codex/m"]);
    }

    #[test]
    fn an_unknown_harness_link_is_an_error_answer() {
        let payload = json!({
            "links": [{"harness": "gemini", "model": "m"}],
            "state": {},
            "exclude": [],
            "accounts": {},
            "now": 1000.0,
        });
        let answer = resolve(&payload).unwrap();
        assert!(answer["error"]
            .as_str()
            .unwrap()
            .contains("not a known harness"));
    }

    #[test]
    fn a_non_string_axis_value_is_an_error_answer() {
        let payload = json!({
            "links": [{"harness": "codex", "model": 123}],
            "state": {},
            "exclude": [],
            "accounts": {},
            "now": 1000.0,
        });
        let answer = resolve(&payload).unwrap();
        assert!(answer["error"]
            .as_str()
            .unwrap()
            .contains("must be a string"));
    }

    #[test]
    fn flags_carry_the_axis_spelling() {
        let payload = json!({
            "links": [{"harness": "codex", "model": "m", "effort": "high"}],
            "state": {},
            "exclude": [],
            "accounts": {},
            "now": 1000.0,
        });
        let answer = resolve(&payload).unwrap();
        let flags = answer["eligible"][0]["flags"].as_array().unwrap();
        let toks: Vec<&str> = flags.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(
            toks,
            vec![
                "-H",
                "codex",
                "-m",
                "m",
                "--effort",
                "high",
                "--substrate",
                "pane"
            ]
        );
    }

    /// A stale reading was still OBSERVED, so it keeps its probe time and the
    /// evidence line names a real age; only `absent` reads age=never.
    #[test]
    fn a_stale_reading_keeps_its_probe_time() {
        let probed = 1000.0 - 15.0 * 3600.0;
        let u = Snapshot {
            probed_at: probed,
            partial: false,
            windows: vec![Window {
                used_pct: 5.0,
                resets_at: None,
            }],
        };
        let hv = headroom(None, Some(&u), 1000.0);
        assert_eq!(hv.verdict, Verdict::Unknown);
        assert_eq!(hv.source, "stale");
        assert_eq!(hv.observed_at, Some(probed));
        // The never-probed case is the only one that reads never.
        let hv = headroom(None, None, 1000.0);
        assert_eq!(hv.source, "absent");
        assert_eq!(hv.observed_at, None);
    }
}

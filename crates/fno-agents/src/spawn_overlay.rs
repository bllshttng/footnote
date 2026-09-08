//! `spawn-overlay`: the spawn-defaults resolver Python delegates to, ported
//! from Python (x-8975). Three payload kinds, one verb, direct dispatch (no
//! daemon RPC), the route-slot transport shape: one JSON payload in on stdin,
//! one JSON answer out on stdout.
//!
//! - kind `overlay`: the harness-keyed rung resolution. Refuses an overlay
//!   table naming an unknown harness or carrying a ranking field; resolves
//!   effort/substrate/permission through the six rungs for the harness the
//!   caller already resolved; picks ONE bundle args vector (lane > profile
//!   overlay > defaults overlay, never concatenated) and names the boundary
//!   that would displace it.
//! - kind `model-vendor`: the advisory/refusing model-vs-lane vendor judgment
//!   (`_check_model_vendor_mismatch` in Python). The caller supplies the
//!   already-resolved harness; the answer names the event fields so the Python
//!   event bus stays the emitter.
//! - kind `fallback`: the failover-chain validation (`validate_fallback` in
//!   Python). Returns canonical links or the exact refusal the Python raised.

use crate::provider::{known_providers_csv, KNOWN_PROVIDERS};
use serde_json::{json, Map, Value};
use std::io::{self, Read};

/// The fields an overlay block may legally carry.
const OVERLAY_FIELDS: [&str; 4] = ["permission_mode", "effort", "substrate", "args"];
/// Ranking fields are lane business; an overlay carrying one refuses.
const LANE_FIELD_NAMES: [&str; 4] = ["provider", "model", "route", "account"];
/// The failover chain's size keys.
const CHAIN_KEYS: [&str; 4] = ["S", "M", "L", "default"];
/// The harness axis is the BINARY; opencode is legally both harness and
/// provider, so the chain roster is spelled here and never inferred.
const CHAIN_HARNESSES: [&str; 4] = ["claude", "codex", "agy", "opencode"];

/// The vendor a harness's own primary lane bills when no route/vendor was
/// named. opencode is operator-configured, so it holds no opinion here.
const HARNESS_DEFAULT_VENDOR: [(&str, &str); 4] = [
    ("claude", "anthropic"),
    ("codex", "openai"),
    ("gemini", "google"),
    ("agy", "google"),
];

/// A model string's implied vendor, by prefix or tier word. A pure string
/// opinion and never a routing input: the pairing is legal and --model is
/// deliberate passthrough.
const MODEL_VENDOR_HINTS: [(&str, &str); 5] = [
    ("glm-", "zai"),
    ("gpt-", "openai"),
    ("deepseek", "deepseek"),
    ("gemini-", "google"),
    ("claude-", "anthropic"),
];
const MODEL_WORD_VENDORS: [(&str, &str); 3] = [
    ("opus", "anthropic"),
    ("sonnet", "anthropic"),
    ("haiku", "anthropic"),
];

pub fn run_spawn_overlay(args: &[String]) -> i32 {
    let mut payload = String::new();
    let read = if let Some(path) = args.iter().find_map(|a| a.strip_prefix("--payload-file=")) {
        std::fs::read_to_string(path)
    } else {
        io::stdin()
            .read_to_string(&mut payload)
            .map(|_| payload.clone())
    };
    let payload = match read {
        Ok(text) => text,
        Err(e) => {
            eprint!("spawn-overlay: cannot read payload: {e}\n");
            return 2;
        }
    };
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprint!("spawn-overlay: bad payload: {e}\n");
            return 2;
        }
    };
    match resolve(parsed) {
        Ok(answer) => {
            println!("{answer}");
            0
        }
        Err(msg) => {
            eprint!("spawn-overlay: {msg}\n");
            2
        }
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::other(String::from(msg))
}

pub fn resolve(payload: Value) -> Result<Value, String> {
    match payload.get("kind").and_then(Value::as_str) {
        Some("overlay") => resolve_overlay(&payload),
        Some("model-vendor") => resolve_model_vendor(&payload),
        Some("fallback") => resolve_fallback(&payload),
        other => Err(format!(
            "spawn-overlay: unknown kind {other:?}; expected overlay|model-vendor|fallback"
        )),
    }
}

fn block_map<'a>(owner: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    owner.get(key).and_then(Value::as_object)
}

fn overlay_scalar(block: &Value, name: &str) -> String {
    match block.get(name) {
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

fn overlay_args(block: &Value) -> Option<Vec<String>> {
    match block.get("args") {
        Some(Value::Array(items)) if !items.is_empty() => Some(
            items
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
        ),
        _ => None,
    }
}

/// The rung chain WITHOUT the lane, resolved for `harness` when given:
/// profile harness overlay > profile scalar > defaults harness overlay >
/// defaults scalar. Returns (value, rung) or nothing.
fn effective_field(
    defaults: &Value,
    profile: Option<&Value>,
    verb: &str,
    name: &str,
    harness: Option<&str>,
) -> Option<(String, String)> {
    if let (Some(h), Some(p)) = (harness, profile) {
        if let Some(table) = block_map(p, "harness") {
            if let Some(block) = table.get(h) {
                let v = overlay_scalar(block, name);
                if !v.is_empty() {
                    return Some((v, format!("agents.profiles.{verb}.harness.{h}")));
                }
            }
        }
    }
    if let Some(p) = profile {
        let v = overlay_scalar(p, name);
        if !v.is_empty() {
            return Some((v, format!("agents.profiles.{verb}")));
        }
    }
    if let Some(h) = harness {
        if let Some(table) = block_map(defaults, "harness") {
            if let Some(block) = table.get(h) {
                let v = overlay_scalar(block, name);
                if !v.is_empty() {
                    return Some((v, format!("agents.defaults.harness.{h}")));
                }
            }
        }
    }
    let v = overlay_scalar(defaults, name);
    if !v.is_empty() {
        return Some((v, "agents.defaults".to_string()));
    }
    None
}

fn resolve_overlay(payload: &Value) -> Result<Value, String> {
    let verb = payload.get("verb").and_then(Value::as_str).unwrap_or("");
    let defaults = payload.get("defaults").cloned().unwrap_or(json!({}));
    let profile = payload.get("profile").filter(|v| v.is_object()).cloned();
    let harness = payload
        .get("harness")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let lane = payload.get("lane").filter(|v| v.is_object());
    let lane_index = payload.get("lane_index").and_then(Value::as_u64);
    let argv_tail = payload
        .get("argv_tail")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // The overlay table guards, scoped to the rungs THIS spawn reads: an
    // unknown harness name or a ranking field refuses the whole composition,
    // fail-closed like the malformed-lane refusal.
    let owners: Vec<(String, &Value)> = vec![
        ("agents.defaults".to_string(), &defaults),
        (
            format!("agents.profiles.{verb}"),
            profile.as_ref().unwrap_or(&Value::Null),
        ),
    ];
    for (owner, obj) in owners {
        let table = match block_map(obj, "harness") {
            Some(t) => t,
            None => continue,
        };
        for (h, block) in table {
            if !KNOWN_PROVIDERS.contains(&h.as_str()) {
                return Ok(json!({
                    "refusal": format!(
                        "fno agents spawn: config.{owner}.harness.{h} is not a known harness; valid: {}",
                        known_providers_csv()
                    )
                }));
            }
            if let Some(extras) = block.as_object() {
                for k in extras.keys() {
                    if LANE_FIELD_NAMES.contains(&k.as_str())
                        && !OVERLAY_FIELDS.contains(&k.as_str())
                    {
                        return Ok(json!({
                            "refusal": format!(
                                "fno agents spawn: config.{owner}.harness.{h}.{k} is a lane field; declare it on the lane"
                            )
                        }));
                    }
                }
            }
        }
    }

    // The three postures, resolved through the harness rungs. The rung is
    // BARE (no field suffix): the spawn seam appends the field name when it
    // records the injection, and the doctor reads `config.{rung}.{name}`.
    let mut effective = Map::new();
    for name in ["effort", "substrate", "permission_mode"] {
        let entry = effective_field(&defaults, profile.as_ref(), verb, name, harness);
        if let Some((v, rung)) = entry {
            effective.insert(name.to_string(), json!({"value": v, "rung": rung}));
        }
    }

    // ONE bundle: lane args > profile harness overlay args > defaults harness
    // overlay args. Never concatenated; the boundary the caller's argv already
    // carries displaces it by name.
    let mut bundle: Option<(Vec<String>, String)> = None;
    if let Some(h) = harness {
        if let (Some(l), Some(idx)) = (lane, lane_index) {
            if let Some(args) = overlay_args(l) {
                bundle = Some((args, format!("agents.profiles.{verb}.lanes[{idx}].args")));
            }
        }
        if bundle.is_none() {
            for (obj, rung) in [
                (
                    profile.as_ref(),
                    format!("agents.profiles.{verb}.harness.{h}.args"),
                ),
                (Some(&defaults), format!("agents.defaults.harness.{h}.args")),
            ] {
                if let Some(p) = obj {
                    if let Some(table) = block_map(p, "harness") {
                        if let Some(block) = table.get(h) {
                            if let Some(args) = overlay_args(block) {
                                bundle = Some((args, rung));
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    let boundary = argv_tail
        .iter()
        .filter_map(Value::as_str)
        .find(|t| *t == "--" || *t == "--argv")
        .map(String::from);
    let bundle_json = match (bundle, &boundary) {
        (Some((tokens, rung)), Some(b)) => json!({
            "displaced": {"tokens": tokens, "rung": rung, "boundary": b}
        }),
        (Some((tokens, rung)), None) => json!({"tokens": tokens, "rung": rung}),
        _ => Value::Null,
    };

    Ok(json!({
        "refusal": Value::Null,
        "effective": effective,
        "bundle": bundle_json,
    }))
}

/// One flag's value from a pre-fence head (`--flag v`, `--flag=v`); stops at
/// the `--argv` payload boundary and the bare `--` fence.
fn flag_value(toks: &[&str], names: &[&str]) -> Option<String> {
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if t == "--argv" || t == "--" {
            return None;
        }
        for name in names {
            if let Some(rest) = t.strip_prefix(&format!("{name}=")) {
                return Some(rest.to_string());
            }
            if t == *name {
                return toks.get(i + 1).map(|s| s.to_string());
            }
        }
        i += 1;
    }
    None
}

fn implied_vendor(model: Option<&str>) -> Option<String> {
    let model = model?;
    let lower = model.to_lowercase();
    for candidate in [lower.as_str(), lower.rsplit('/').next().unwrap_or("")] {
        for (prefix, vendor) in MODEL_VENDOR_HINTS {
            if let Some(stripped) = candidate.strip_prefix(prefix) {
                let _ = stripped;
                return Some(vendor.to_string());
            }
        }
        let base = candidate
            .split('[')
            .next()
            .unwrap_or("")
            .split('-')
            .next()
            .unwrap_or("");
        for (word, vendor) in MODEL_WORD_VENDORS {
            if base == word {
                return Some(vendor.to_string());
            }
        }
    }
    None
}

fn resolve_lane_vendor(
    toks: &[&str],
    harness: Option<&str>,
    argv_head: Option<&str>,
) -> Option<String> {
    if let Some(route) = flag_value(toks, &["--route"]) {
        let normalized = route.replace(',', "/");
        let vendor = normalized
            .split('/')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        return if vendor.is_empty() {
            None
        } else {
            Some(vendor)
        };
    }
    if let Some(v) = flag_value(toks, &["--provider", "-P"]) {
        let v = v.trim().to_lowercase();
        return if v.is_empty() { None } else { Some(v) };
    }
    let mut resolved = harness
        .map(str::trim)
        .map(str::to_lowercase)
        .unwrap_or_default();
    if resolved.is_empty() {
        resolved = flag_value(toks, &["--harness", "-H"])
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_default();
    }
    if resolved.is_empty() {
        if let Some(head) = argv_head {
            for (h, _) in HARNESS_DEFAULT_VENDOR {
                if head == h {
                    resolved = h.to_string();
                    break;
                }
            }
        }
    }
    for (h, vendor) in HARNESS_DEFAULT_VENDOR {
        if resolved == h {
            return Some(vendor.to_string());
        }
    }
    if resolved == "opencode" {
        return implied_vendor(flag_value(toks, &["--model", "-m"]).as_deref());
    }
    None
}

fn resolve_model_vendor(payload: &Value) -> Result<Value, String> {
    let toks: Vec<String> = payload
        .get("argv_tail")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let refs: Vec<&str> = toks.iter().map(String::as_str).collect();
    let harness = payload.get("harness").and_then(Value::as_str);
    let head = payload.get("argv_head").and_then(Value::as_str);
    let model_source = payload
        .get("model_source")
        .and_then(Value::as_str)
        .map(String::from);
    let model = flag_value(&refs, &["--model", "-m"]);
    let implied = match implied_vendor(model.as_deref()) {
        Some(v) => v,
        None => return Ok(json!({"verdict": "ok", "message": Value::Null, "event": Value::Null})),
    };
    if flag_value(&refs, &["--route"]).is_some() {
        // Explicit route: a deliberate lane choice beside a deliberate model.
        return Ok(json!({"verdict": "ok", "message": Value::Null, "event": Value::Null}));
    }
    let lane = match resolve_lane_vendor(&refs, harness, head) {
        Some(l) => l,
        None => return Ok(json!({"verdict": "ok", "message": Value::Null, "event": Value::Null})),
    };
    if lane == implied {
        return Ok(json!({"verdict": "ok", "message": Value::Null, "event": Value::Null}));
    }
    let refusing = model_source.is_some() && flag_value(&refs, &["--account"]).is_none();
    let event = json!({
        "model": model,
        "implied_vendor": implied,
        "resolved_vendor": lane,
        "model_source": model_source.clone().unwrap_or_else(|| "explicit".into()),
        "outcome": if refusing { "refused" } else { "warned" },
    });
    if refusing {
        let src = model_source.unwrap_or_default();
        return Ok(json!({
            "verdict": "refuse",
            "event": event,
            "message": format!(
                "fno agents spawn: refusing to spawn. {src} supplies --model {model:?}, \
        which implies vendor {implied}, but this spawn resolves the {lane} lane. Nothing typed this \
        pairing, and the worker would start, report live, and fail on its first inference. Set a model \
        for the {lane} lane at {src}, or name the vendor on this spawn with -P {implied}."
            ),
        }));
    }
    Ok(json!({
        "verdict": "warn",
        "event": event,
        "message": format!(
            "fno agents spawn: --model {model:?} implies vendor {implied}, but the resolved \
    lane is {lane}; the model rides that lane's CLI as-is. Name the vendor with -P {implied} to route it."
        ),
    }))
}

fn resolve_fallback(payload: &Value) -> Result<Value, String> {
    let table = match payload.get("table").and_then(Value::as_object) {
        Some(t) => t,
        None => {
            return Ok(json!({"error": format!(
                "config.agents.fallback must be a table keyed by size (S|M|L|default); got {}",
                type_name(payload.get("table"))
            )}))
        }
    };
    let mut out = Map::new();
    for (size, chain) in table {
        if !CHAIN_KEYS.contains(&size.as_str()) {
            return Ok(json!({"error": format!(
                "config.agents.fallback.{size}: unknown size key; expected one of {}",
                ["S", "L", "M", "default"].join("|")
            )}));
        }
        let links = match chain.as_array() {
            Some(l) => l,
            None => {
                return Ok(json!({"error": format!(
                    "config.agents.fallback.{size} must be a list of links; got {}",
                    type_name(Some(chain))
                )}))
            }
        };
        let mut canonical: Vec<Value> = Vec::new();
        for (i, link) in links.iter().enumerate() {
            let obj = match link.as_object() {
                Some(o) => o,
                None => {
                    return Ok(json!({"error": format!(
                        "config.agents.fallback.{size}[{i}] must be a table of axis fields; got {}",
                        type_name(Some(link))
                    )}))
                }
            };
            // A link is spelled with the CORRECT axis word, `harness`; the
            // legacy `provider` spelling reads too and `harness` wins. Either
            // way the value is checked: a typo'd harness must refuse, never
            // spawn on the ambient binary.
            let harness = obj
                .get("harness")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .or_else(|| {
                    obj.get("provider")
                        .and_then(Value::as_str)
                        .map(|s| s.trim().to_string())
                })
                .unwrap_or_default();
            if !harness.is_empty() && !CHAIN_HARNESSES.contains(&harness.as_str()) {
                return Ok(json!({"error": format!(
                    "config.agents.fallback.{size}[{i}].harness={harness:?} is not a known harness ({})",
                    CHAIN_HARNESSES.join("|")
                )}));
            }
            let mut fields = Map::new();
            for (k, v) in obj {
                if k == "harness" {
                    continue;
                }
                fields.insert(k.clone(), v.clone());
            }
            if !harness.is_empty() {
                fields.insert("provider".into(), Value::String(harness));
            }
            canonical.push(Value::Object(fields));
        }
        out.insert(size.clone(), Value::Array(canonical));
    }
    Ok(json!({"error": Value::Null, "links": out}))
}

fn type_name(v: Option<&Value>) -> &'static str {
    match v {
        None | Some(Value::Null) => "NoneType",
        Some(Value::Bool(_)) => "bool",
        Some(Value::Number(_)) => "int",
        Some(Value::String(_)) => "str",
        Some(Value::Array(_)) => "list",
        Some(Value::Object(_)) => "dict",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn overlay(payload: Value) -> Value {
        resolve(json!({"kind": "overlay", "verb": "target", "argv_tail": []}).merge(&payload))
            .unwrap()
    }

    #[test]
    fn profile_harness_overlay_answers_claude_scalar_answers_codex() {
        let payload = json!({
            "kind": "overlay", "verb": "target",
            "defaults": {},
            "profile": {
                "permission_mode": "yolo",
                "harness": {"claude": {"permission_mode": "bypassPermissions"}}
            },
            "harness": "claude",
        });
        let out = resolve(payload).unwrap();
        assert!(out["refusal"].is_null());
        assert_eq!(
            out["effective"]["permission_mode"]["value"],
            "bypassPermissions"
        );
        assert_eq!(
            out["effective"]["permission_mode"]["rung"],
            "agents.profiles.target.harness.claude"
        );
    }

    #[test]
    fn unknown_overlay_harness_refuses_by_name() {
        let out = overlay(json!({
            "profile": {"harness": {"codx": {"effort": "high"}}},
        }));
        assert!(out["refusal"]
            .as_str()
            .unwrap()
            .contains("profiles.target.harness.codx is not a known harness"));
    }

    #[test]
    fn lane_field_in_overlay_refuses_by_name() {
        let out = overlay(json!({
            "defaults": {"harness": {"codex": {"model": "opus"}}},
        }));
        assert!(out["refusal"]
            .as_str()
            .unwrap()
            .contains("harness.codex.model is a lane field"));
    }

    #[test]
    fn lane_args_win_over_overlay_bundle() {
        let out = overlay(json!({
            "defaults": {"harness": {"codex": {"args": ["--profile", "overlay"]}}},
            "lane": {"provider": "codex", "args": ["--profile", "lane"]},
            "lane_index": 0,
            "harness": "codex",
        }));
        assert_eq!(out["bundle"]["tokens"][0], "--profile");
        assert_eq!(out["bundle"]["tokens"][1], "lane");
        assert!(out["bundle"]["rung"]
            .as_str()
            .unwrap()
            .ends_with("lanes[0].args"));
    }

    #[test]
    fn argv_boundary_displaces_the_bundle_by_name() {
        let out = overlay(json!({
            "defaults": {"harness": {"codex": {"args": ["--profile", "fno"]}}},
            "harness": "codex",
            "argv_tail": ["--name", "w", "hi", "--", "--profile", "other"],
        }));
        assert_eq!(out["bundle"]["displaced"]["boundary"], "--");
        assert_eq!(
            out["bundle"]["displaced"]["rung"],
            "agents.defaults.harness.codex.args"
        );
    }

    #[test]
    fn model_vendor_warns_on_typed_mismatch() {
        let out = resolve(json!({
            "kind": "model-vendor",
            "argv_tail": ["spawn", "--model", "glm-5.3", "-H", "claude"],
            "harness": "claude",
        }))
        .unwrap();
        assert_eq!(out["verdict"], "warn");
        assert!(out["message"]
            .as_str()
            .unwrap()
            .contains("implies vendor zai"));
        assert_eq!(out["event"]["outcome"], "warned");
    }

    #[test]
    fn model_vendor_refuses_injected_mismatch() {
        let out = resolve(json!({
            "kind": "model-vendor",
            "argv_tail": ["spawn", "--model", "gpt-5.6", "-H", "claude"],
            "harness": "claude",
            "model_source": "agents.profiles.target.model",
        }))
        .unwrap();
        assert_eq!(out["verdict"], "refuse");
        assert_eq!(out["event"]["outcome"], "refused");
    }

    #[test]
    fn fallback_rejects_an_unknown_harness_link() {
        let out = resolve(json!({
            "kind": "fallback",
            "table": {"S": [{"harness": "cluade", "model": "m"}]},
        }))
        .unwrap();
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("not a known harness"));
    }

    #[test]
    fn fallback_canonicalizes_provider_spelling() {
        let out = resolve(json!({
            "kind": "fallback",
            "table": {"default": [{"provider": "codex", "model": "m"}]},
        }))
        .unwrap();
        assert!(out["error"].is_null());
        assert_eq!(out["links"]["default"][0]["provider"], "codex");
    }
}

trait MergeValue {
    fn merge(self, other: &Value) -> Value;
}

impl MergeValue for Value {
    fn merge(mut self, other: &Value) -> Value {
        if let (Some(base), Some(extra)) = (self.as_object_mut(), other.as_object()) {
            for (k, v) in extra {
                base.insert(k.clone(), v.clone());
            }
        }
        self
    }
}

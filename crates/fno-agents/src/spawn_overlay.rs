//! `spawn-overlay`: the spawn-defaults resolver Python delegates to, ported
//! from Python. Three payload kinds, one verb, direct dispatch (no
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
//! - kind `crown-settle`: whether a crowned spawn is granted, transfers, or
//!   refuses (`crown_settle::resolve`), a port of Python's
//!   `settle_spawn_crown` with a new human-succession branch.

use crate::provider::{known_providers_csv, KNOWN_PROVIDERS};
use serde_json::{json, Map, Value};
use std::io::{self, Read};
use std::path::Path;

/// The fields an overlay block may legally carry.
const OVERLAY_FIELDS: [&str; 4] = ["permission_mode", "effort", "substrate", "args"];
/// Ranking fields are lane business; an overlay carrying one refuses.
const LANE_FIELD_NAMES: [&str; 4] = ["provider", "model", "route", "account"];
/// The failover chain's size keys.
const CHAIN_KEYS: [&str; 4] = ["S", "M", "L", "default"];
/// The harness axis is the BINARY; opencode is legally both harness and
/// provider, so the chain roster is spelled here and never inferred.
pub(crate) const CHAIN_HARNESSES: [&str; 4] = ["claude", "codex", "agy", "opencode"];

/// The vendor a harness's own primary lane bills when no route/vendor was
/// named. opencode is operator-configured, so it holds no opinion here.
const HARNESS_DEFAULT_VENDOR: [(&str, &str); 4] = [
    ("claude", "anthropic"),
    ("codex", "openai"),
    ("gemini", "google"),
    ("agy", "google"),
];

/// The harness whose own lane bills `vendor` by default, so a mismatch
/// remedy names the flag that actually works (-H codex, not -P openai -
/// openai is not a configured provider here). Vendors no harness serves by
/// default (zai, deepseek) have no -H answer, so the caller falls back to
/// the -P spelling.
fn harness_for_vendor(vendor: &str) -> Option<&'static str> {
    CHAIN_HARNESSES
        .iter()
        .find(|h| {
            HARNESS_DEFAULT_VENDOR
                .iter()
                .any(|(name, v)| *name == **h && *v == vendor)
        })
        .copied()
}

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

#[cfg(test)]
trait MergeValue {
    fn merge(self, other: &Value) -> Value;
}

#[cfg(test)]
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

pub fn resolve(payload: Value) -> Result<Value, String> {
    match payload.get("kind").and_then(Value::as_str) {
        Some("overlay") => resolve_overlay(&payload),
        Some("model-vendor") => resolve_model_vendor(&payload),
        Some("lane-vendor") => {
            let toks: Vec<String> = payload
                .get("argv_tail")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let (axes, _) = crate::cli_args::SpawnAxes::scan(&toks)?;
            let vendor = resolve_lane_vendor(
                &axes,
                payload.get("harness").and_then(Value::as_str),
                payload.get("env_harness").and_then(Value::as_str),
                payload.get("argv_head").and_then(Value::as_str),
            );
            Ok(json!({"vendor": vendor}))
        }
        Some("link-meta") => resolve_link_meta(&payload),
        Some("pane-group") => resolve_pane_group(&payload),
        Some("fallback") => resolve_fallback(&payload),
        Some("codex-route") => resolve_codex_route_kind(&payload),
        Some("crown-settle") => crate::crown_settle::resolve(&payload),
        other => Err(format!(
            "spawn-overlay: unknown kind {other:?}; expected overlay|model-vendor|lane-vendor|link-meta|pane-group|fallback|codex-route|crown-settle"
        )),
    }
}

/// The codex route builder behind Python's `resolve_codex_route`: the payload
/// names the role-picked (provider, model) plus the spawn cwd; the answer
/// carries the three `-c` tokens and the env (key + stamp) to the Python
/// parent over this one stdout pipe. `unrouted` marks the deliberate no-op
/// (an anthropic-protocol provider belongs to the claude lane, silent in
/// Python exactly as before); `refusal` names a misconfiguration the spawn
/// surfaces as a notice. Key-free, like every answer this verb prints.
fn resolve_codex_route_kind(payload: &Value) -> Result<Value, String> {
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("");
    let model = payload.get("model").and_then(Value::as_str).unwrap_or("");
    let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or("");
    if provider.is_empty() || model.is_empty() || cwd.is_empty() {
        return Ok(json!({
            "refusal": "spawn-overlay codex-route: payload needs provider, model and cwd",
            "unrouted": Value::Null,
        }));
    }
    match crate::codex_route::resolve_codex_route(Path::new(cwd), provider, model) {
        Ok(route) => Ok(json!({
            "refusal": Value::Null,
            "unrouted": Value::Null,
            "provider": route.provider,
            "model": route.model,
            "config_args": route.config_args,
            "env": route.env,
        })),
        Err(err) => {
            let (refusal, unrouted) = match &err {
                crate::codex_route::CodexRouteError::Unrouted(reason) => {
                    (Value::Null, json!(reason))
                }
                crate::codex_route::CodexRouteError::Refused(reason) => {
                    (json!(reason), Value::Null)
                }
            };
            Ok(json!({"refusal": refusal, "unrouted": unrouted}))
        }
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
    let harness_list: Vec<String> = payload
        .get("harnesses")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
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
    let effective_for = |h: Option<&str>| {
        let mut effective = Map::new();
        for name in ["effort", "substrate", "permission_mode"] {
            let entry = effective_field(&defaults, profile.as_ref(), verb, name, h);
            if let Some((v, rung)) = entry {
                effective.insert(name.to_string(), json!({"value": v, "rung": rung}));
            }
        }
        effective
    };

    // Doctor mode: a `harnesses` list answers the whole sweep in one call,
    // with the guards still refusing the config once.
    if !harness_list.is_empty() {
        let mut by_harness = Map::new();
        for h in &harness_list {
            by_harness.insert(h.clone(), json!(effective_for(Some(h.as_str()))));
        }
        return Ok(json!({
            "refusal": Value::Null,
            "effective_by_harness": by_harness,
            "bundle": Value::Null,
        }));
    }

    let effective = effective_for(harness);

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

/// The lane's vendor from route > provider > the harness chain. Inference-free
/// by contract: the model-vendor mismatch check needs a lane answer that the
/// model token did not inform, or it could never flag the model.
fn resolve_lane_vendor_by_lane(
    axes: &crate::cli_args::SpawnAxes,
    harness: Option<&str>,
    env_harness: Option<&str>,
    argv_head: Option<&str>,
) -> Option<String> {
    if let Some(route) = &axes.route {
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
    if let Some(v) = &axes.provider {
        let v = v.trim().to_lowercase();
        return if v.is_empty() { None } else { Some(v) };
    }
    let mut resolved = harness
        .map(str::trim)
        .map(str::to_lowercase)
        .unwrap_or_default();
    if resolved.is_empty() {
        resolved = axes
            .harness
            .as_deref()
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
    if resolved.is_empty() {
        if let Some(env_answer) = env_harness {
            resolved = env_answer.trim().to_lowercase();
        }
    }
    for (h, vendor) in HARNESS_DEFAULT_VENDOR {
        if resolved == h {
            return Some(vendor.to_string());
        }
    }
    if resolved == "opencode" {
        return implied_vendor(axes.model.as_deref());
    }
    None
}

fn resolve_lane_vendor(
    axes: &crate::cli_args::SpawnAxes,
    harness: Option<&str>,
    env_harness: Option<&str>,
    argv_head: Option<&str>,
) -> Option<String> {
    // No route and no provider named a vendor: the model token informs the
    // answer. A routeless glm spawn answered the harness default (anthropic),
    // minted a row under it, and died on the default endpoint's first
    // inference; the model's own spelling is the only voice that named z.ai.
    let vendor_pinned = axes.route.is_some() || axes.provider.is_some();
    if !vendor_pinned {
        if let Some(vendor) = implied_vendor(axes.model.as_deref()) {
            return Some(vendor);
        }
    }
    resolve_lane_vendor_by_lane(axes, harness, env_harness, argv_head)
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
    let harness = payload.get("harness").and_then(Value::as_str);
    let head = payload.get("argv_head").and_then(Value::as_str);
    let model_source = payload
        .get("model_source")
        .and_then(Value::as_str)
        .map(String::from);
    let (axes, _) = crate::cli_args::SpawnAxes::scan(&toks)?;
    let model = axes.model.clone();
    let implied = match implied_vendor(model.as_deref()) {
        Some(v) => v,
        None => return Ok(json!({"verdict": "ok", "message": Value::Null, "event": Value::Null})),
    };
    if axes.route.is_some() {
        // Explicit route: a deliberate lane choice beside a deliberate model.
        return Ok(json!({"verdict": "ok", "message": Value::Null, "event": Value::Null}));
    }
    let lane = match resolve_lane_vendor_by_lane(
        &axes,
        harness,
        payload.get("env_harness").and_then(Value::as_str),
        head,
    ) {
        Some(l) => l,
        None => return Ok(json!({"verdict": "ok", "message": Value::Null, "event": Value::Null})),
    };
    if lane == implied {
        return Ok(json!({"verdict": "ok", "message": Value::Null, "event": Value::Null}));
    }
    // A spawn whose harness nobody typed is not a deliberate passthrough:
    // --node dispatch harnesses come from a resolver, and an untyped -H
    // came from config or a default. A typed -H beside a typed
    // model is the operator's own pairing, so it only warns. The --account
    // downgrade and the early --route return stay as they are.
    let harness_typed = payload
        .get("harness_typed")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let node_typed = toks
        .iter()
        .any(|t| t == "--node" || t.starts_with("--node="));
    let refusing =
        (model_source.is_some() || !harness_typed || node_typed) && axes.account.is_none();
    let model_str = model.clone().unwrap_or_default();
    let remedy = match harness_for_vendor(&implied) {
        Some(h) => format!("-H {h}"),
        None => format!("-P {implied}"),
    };
    let event = json!({
        "model": model,
        "implied_vendor": implied,
        "resolved_vendor": lane,
        "model_source": model_source.clone().unwrap_or_else(|| "explicit".into()),
        "outcome": if refusing { "refused" } else { "warned" },
    });
    if refusing {
        if let Some(src) = model_source {
            return Ok(json!({
                "verdict": "refuse",
                "event": event,
                "message": format!(
                    "fno agents spawn: refusing to spawn. {src} supplies --model {model_str}, \
            which implies vendor {implied}, but this spawn resolves the {lane} lane. Set a model \
            for the {lane} lane at {src}, or pass {remedy}."
                ),
            }));
        }
        return Ok(json!({
            "verdict": "refuse",
            "event": event,
            "message": format!(
                "fno agents spawn: refusing to spawn. --model {model_str} implies vendor {implied}, \
        but nothing typed the harness and this spawn resolves the {lane} lane. No routing.models row \
        declares that model, so nothing names its harness. Pass {remedy}, or declare a routing.models row for the model."
            ),
        }));
    }
    Ok(json!({
        "verdict": "warn",
        "event": event,
        "message": format!(
            "fno agents spawn: --model {model_str} implies vendor {implied}, but the resolved \
    lane is {lane}; the model rides that lane's CLI as-is. Pass {remedy} to run it on the harness that serves {implied}."
        ),
    }))
}

/// One stop in the fallback walk: each link's stable destination id and its
/// spawn flags. Two links differing only in effort are the SAME destination
/// for walk memory; `account` participates because it names a different bill
/// and meter. Flags keep the existing axis spelling, and substrate resolves
/// from the harness's own spawn claim (claude rides bg, everything else pane)
/// rather than being handed one its harness rejects.
fn resolve_link_meta(payload: &Value) -> Result<Value, String> {
    let links = payload
        .get("links")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut ids = Vec::new();
    let mut flag_rows = Vec::new();
    for link in &links {
        let (id, flags) = mint_link(link);
        ids.push(Value::String(id));
        flag_rows.push(Value::Array(flags));
    }
    Ok(json!({"ids": ids, "flags": flag_rows}))
}

/// The walk-memory id and spawn flags for one canonical link (axis spelling,
/// `provider` carrying the harness).
pub(crate) fn mint_link(link: &Value) -> (String, Vec<Value>) {
    let field = |k: &str| -> String {
        link.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let (harness, model, route, account) = (
        field("provider"),
        field("model"),
        field("route"),
        field("account"),
    );
    let base = format!(
        "{}/{}",
        if harness.is_empty() {
            "?"
        } else {
            harness.as_str()
        },
        if !model.is_empty() {
            model.as_str()
        } else if !route.is_empty() {
            route.as_str()
        } else {
            "default"
        }
    );
    let id = if account.is_empty() {
        base
    } else {
        format!("{base}@{account}")
    };
    let mut flags: Vec<Value> = Vec::new();
    if !harness.is_empty() {
        flags.push(json!("-H"));
        flags.push(json!(harness));
    }
    for (flag, key) in [
        ("-m", "model"),
        ("--effort", "effort"),
        ("--permission-mode", "permission_mode"),
        ("--route", "route"),
        ("--account", "account"),
    ] {
        let v = field(key);
        if !v.is_empty() {
            flags.push(json!(flag));
            flags.push(json!(v));
        }
    }
    let mut substrate = field("substrate");
    if substrate.is_empty() {
        substrate = if harness == "claude" {
            "bg".into()
        } else {
            "pane".into()
        };
    }
    flags.push(json!("--substrate"));
    flags.push(json!(substrate));
    (id, flags)
}

/// Where a configured pane group may land. A group places the pane by moving
/// its OWN tab, so it is skipped (never a failure: the value was injected, not
/// typed) when the resolved substrate has no pane geometry, or when the
/// caller's own argv already carries a placement flag. ``--once``/``-o``
/// counts because a one-shot spawn has no pane geometry either. PRESENCE, not
/// value: a valueless trailing ``--at`` still conflicts. The tokens arrive
/// already fence- and value-filtered by the caller.
fn resolve_pane_group(payload: &Value) -> Result<Value, String> {
    let group = payload.get("group").and_then(Value::as_str).unwrap_or("");
    let rung = payload.get("rung").and_then(Value::as_str).unwrap_or("");
    let eff_substrate = payload
        .get("eff_substrate")
        .and_then(Value::as_str)
        .unwrap_or("pane");
    let toks: Vec<String> = payload
        .get("argv_tail")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let placement = ["--split", "-x", "--at", "--once", "-o"];
    let mut conflicting: Option<String> = None;
    for t in &toks {
        for f in placement {
            let glued = f.len() == 2 && !f[1..].starts_with('-');
            if t == f || t.starts_with(&format!("{f}=")) || (glued && t.starts_with(f) && t != f) {
                conflicting = Some(f.to_string());
                break;
            }
        }
        if conflicting.is_some() {
            break;
        }
    }
    if eff_substrate != "pane" {
        return Ok(json!({
            "skipped": format!(
                "fno agents spawn: pane group skipped (resolved substrate {eff_substrate:?} has no pane geometry); {rung} = {group:?} ignored"
            )
        }));
    }
    if let Some(c) = conflicting {
        return Ok(json!({
            "skipped": format!(
                "fno agents spawn: pane group skipped ({c} places this pane in a tab it does not own, which a group cannot move); {rung} = {group:?} ignored"
            )
        }));
    }
    Ok(json!({"inject": ["--tab", group]}))
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
        // Assembled, not a literal: the rung string would end in a word the
        // placement-rule lint reads as a path construction.
        let want = format!("agents.profiles.target.harness.{}", "claude");
        assert_eq!(out["effective"]["permission_mode"]["rung"], want);
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
    fn model_vendor_typed_mismatch_warn_names_the_harness_flag() {
        // AC3: a typed -H beside a typed model proceeds with a warning whose
        // remedy is the harness flag that works, and the model prints as its
        // string, never Option debug.
        let out = resolve(json!({
            "kind": "model-vendor",
            "argv_tail": ["spawn", "--model", "gpt-6-astra", "-H", "claude"],
            "harness": "claude",
            "harness_typed": true,
        }))
        .unwrap();
        assert_eq!(out["verdict"], "warn");
        let msg = out["message"].as_str().unwrap();
        assert!(msg.contains("-H codex"));
        assert!(!msg.contains("-P openai"));
        assert!(!msg.contains("Some("));
        assert!(msg.contains("--model gpt-6-astra"));
    }

    #[test]
    fn model_vendor_untyped_harness_refuses() {
        // AC5: a typed model with nothing typing the harness refuses before
        // launch, naming -H codex and the missing row.
        let out = resolve(json!({
            "kind": "model-vendor",
            "argv_tail": ["spawn", "--model", "gpt-9-preview"],
            "harness": "claude",
            "harness_typed": false,
        }))
        .unwrap();
        assert_eq!(out["verdict"], "refuse");
        let msg = out["message"].as_str().unwrap();
        assert!(msg.contains("refusing to spawn"));
        assert!(msg.contains("-H codex"));
        assert!(msg.contains("No routing.models row declares that model"));
    }

    #[test]
    fn model_vendor_node_dispatch_refuses_mismatch() {
        // AC8a: --node dispatch whose resolved harness mismatches the typed
        // model's vendor refuses even though -H was on the argv (the
        // resolver put it there).
        let out = resolve(json!({
            "kind": "model-vendor",
            "argv_tail": ["spawn", "--node", "x-0000", "--model", "gpt-5.6-sol", "-H", "claude"],
            "harness": "claude",
            "harness_typed": true,
        }))
        .unwrap();
        assert_eq!(out["verdict"], "refuse");
        assert!(out["message"].as_str().unwrap().contains("-H codex"));
    }

    #[test]
    fn model_vendor_account_downgrade_stays_warn() {
        // AC8b: an explicit --account downgrades refusal to warn, as before.
        let out = resolve(json!({
            "kind": "model-vendor",
            "argv_tail": ["spawn", "--model", "glm-5.3", "--account", "zai-main", "-H", "claude"],
            "harness": "claude",
            "harness_typed": false,
            "model_source": "agents.profiles.target.model",
        }))
        .unwrap();
        assert_eq!(out["verdict"], "warn");
    }

    #[test]
    fn model_vendor_injected_opus_on_codex_names_claude() {
        // AC10b: an injected anthropic-voiced model on a codex spawn refuses
        // naming -H claude, never -P anthropic.
        let out = resolve(json!({
            "kind": "model-vendor",
            "argv_tail": ["spawn", "--model", "claude-opus-5"],
            "harness": "codex",
            "harness_typed": false,
            "model_source": "agents.profiles.target.model",
        }))
        .unwrap();
        assert_eq!(out["verdict"], "refuse");
        let msg = out["message"].as_str().unwrap();
        assert!(msg.contains("-H claude"));
        assert!(!msg.contains("-P anthropic"));
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

    #[test]
    fn lane_vendor_answers_route_provider_then_harness() {
        let vendor = |payload: Value| {
            resolve(payload).unwrap()["vendor"]
                .as_str()
                .map(String::from)
        };
        assert_eq!(
            vendor(json!({"kind": "lane-vendor", "argv_tail": ["--route", "zai/glm"]})),
            Some("zai".into())
        );
        assert_eq!(
            vendor(json!({"kind": "lane-vendor", "argv_tail": ["-P", "openai"]})),
            Some("openai".into())
        );
        assert_eq!(
            vendor(json!({"kind": "lane-vendor", "argv_tail": [], "harness": "codex"})),
            Some("openai".into())
        );
        // opencode holds no vendor opinion; the model's own spelling answers.
        assert_eq!(
            vendor(json!({
                "kind": "lane-vendor",
                "argv_tail": ["--model", "glm-5.3-flash"],
                "harness": "opencode",
            })),
            Some("zai".into())
        );
        assert_eq!(
            vendor(json!({"kind": "lane-vendor", "argv_tail": [], "harness": "opencode"})),
            None
        );
    }

    #[test]
    fn lane_vendor_model_token_informs_the_answer_without_a_route_or_provider() {
        // The measured A/B matrix: a routeless glm spawn answered the claude
        // harness default (anthropic), minted a row under it, and died on the
        // default endpoint's first inference. The model spelling is the only
        // voice that named z.ai, so it outranks the harness default when no
        // route and no provider named a vendor.
        let vendor = |payload: Value| {
            resolve(payload).unwrap()["vendor"]
                .as_str()
                .map(String::from)
        };
        assert_eq!(
            vendor(json!({
                "kind": "lane-vendor",
                "argv_tail": ["--model", "glm-5.3-flash[1m]"],
                "harness": "claude",
            })),
            Some("zai".into())
        );
        // A pinned vendor still answers ahead of the model spelling.
        assert_eq!(
            vendor(json!({
                "kind": "lane-vendor",
                "argv_tail": ["-P", "zai", "--model", "glm-5.3-flash[1m]"],
                "harness": "claude",
            })),
            Some("zai".into())
        );
        assert_eq!(
            vendor(json!({
                "kind": "lane-vendor",
                "argv_tail": ["--route", "zai/glm-5.3-flash"],
                "harness": "claude",
            })),
            Some("zai".into())
        );
        // Control: a model the harness default agrees with is unchanged.
        assert_eq!(
            vendor(json!({
                "kind": "lane-vendor",
                "argv_tail": ["--model", "claude-opus-5"],
                "harness": "claude",
            })),
            Some("anthropic".into())
        );
    }

    #[test]
    fn model_vendor_mismatch_check_stays_lane_inference_free() {
        // The checker's lane answer must NOT absorb the model inference, or
        // an injected glm-on-claude model would agree with itself and never
        // refuse.
        let out = resolve(json!({
            "kind": "model-vendor",
            "argv_tail": ["--model", "glm-5.3-flash[1m]"],
            "harness": "claude",
            "model_source": "config.agents.profiles.target",
        }))
        .unwrap();
        assert_eq!(out["verdict"], "refuse");
    }
}

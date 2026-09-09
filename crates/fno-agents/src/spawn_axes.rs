//! The billing axes of the spawn seam: config-sourced route, account and
//! model, decided in one place. This is the port of the seam's largest
//! decision block (`inject_spawn_defaults`, the route/account/model region):
//! the Python front door projects the caller's facts and the config axes'
//! values plus rungs, this module returns the injections, receipts and skip
//! reasons verbatim, and the seam only applies them. The message strings are
//! the seam's own vocabulary - one spelling, read by tests and operators
//! alike. Pure over its input: no config, filesystem or network reads.

use serde_json::{json, Map, Value};

/// One config axis as the payload carries it: `{"value": ..., "rung": ...}`.
struct Axis {
    value: String,
    rung: String,
}

fn axis(payload: &Value, key: &str) -> Axis {
    let a = payload.get(key).cloned().unwrap_or(json!({}));
    Axis {
        value: a
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        rung: a
            .get("rung")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    }
}

fn flag(payload: &Value, key: &str) -> bool {
    payload.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn opt_str(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .map(|v| v.as_str().map(|s| s.to_string()).unwrap_or_default())
}

/// Python `f"{x!r}"` spells a plain string in single quotes; every value this
/// module interpolates is an operator-typed word, so plain quoting matches.
fn repr(s: &str) -> String {
    format!("'{s}'")
}

/// The decision over the three billing axes. Inputs are facts the seam
/// already resolved (explicit flags, the slot walk's candidate, harness
/// answers); outputs are argv tokens and receipt rows the seam applies
/// mechanically, in the seam's own field order.
pub fn decide(payload: &Value) -> Value {
    let route = axis(payload, "route");
    let account = axis(payload, "account");
    let model = axis(payload, "model");
    let explicit_route = flag(payload, "explicit_route");
    let explicit_vendor_present = flag(payload, "explicit_vendor_present");
    let explicit_vendor = payload
        .get("explicit_vendor")
        .and_then(Value::as_str)
        .unwrap_or("");
    let explicit_model_present = flag(payload, "explicit_model_present");
    let has_model = flag(payload, "has_model");
    let grid_candidate_present = flag(payload, "grid_candidate_present");
    let slot_chain: Vec<String> = payload
        .get("slot_chain")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();
    let prov = payload.get("prov").and_then(Value::as_str).unwrap_or("");
    let cfg_harness = payload
        .get("cfg_harness")
        .and_then(Value::as_str)
        .unwrap_or("");

    let mut inject: Vec<Value> = Vec::new();
    let mut applied: Vec<Value> = Vec::new();
    let mut suppressed: Vec<Value> = Vec::new();
    let mut messages: Vec<String> = Vec::new();
    let mut route_injected = false;
    let mut out_tail_append_empty = false;
    let mut bundle_inject: Vec<Value> = Vec::new();

    // --- route ---------------------------------------------------------- //
    if !route.value.is_empty()
        && !explicit_route
        && !explicit_model_present
        && !explicit_vendor_present
        && !grid_candidate_present
    {
        inject.push(json!(["--route", route.value]));
        applied.push(json!([
            "route",
            route.value,
            format!("{}.route", route.rung),
        ]));
        route_injected = true;
    } else if !route.value.is_empty() {
        // AC9-UI: config-sourced routing is never invisible. The route axis
        // is the one that bills, so a dropped route names which condition
        // fired. The grid case is named before any --model read: a grid
        // candidate excludes an explicit -m anyway.
        let why = if explicit_route {
            "the caller passed --route".to_string()
        } else if explicit_vendor_present {
            format!("the caller passed --provider {}", repr(explicit_vendor))
        } else if grid_candidate_present {
            let chain = if slot_chain.is_empty() {
                "no reason recorded".to_string()
            } else {
                slot_chain.join("; ")
            };
            format!("the capacity grid chose a lane ({chain})")
        } else if explicit_model_present {
            "the caller passed --model".to_string()
        } else {
            "no suppression reason recorded".to_string()
        };
        messages.push(format!(
            "fno agents spawn: route skipped ({why}); {}.route {} NOT applied - \
             this worker bills at the caller default",
            route.rung,
            repr(&route.value),
        ));
        suppressed.push(json!(["route", route.value, route.rung, why,]));
    }
    // route_present covers BOTH ways --route ends up in the final argv:
    // injected from config, or already explicit on the caller's argv. An
    // operator-typed `--route zai/...` with no `-m` must still suppress the
    // config model below - the route+model collision (five-opus-workers
    // defect) this exists to prevent.
    let route_present = route_injected || explicit_route;

    // --- account -------------------------------------------------------- //
    let grid_account_injected = flag(payload, "grid_account_injected");
    let account_flag_present = flag(payload, "account_flag_present");
    if !account.value.is_empty() && !grid_account_injected && !account_flag_present {
        // Accounts are Claude-only (cmd_spawn rejects --account on any other
        // harness), so a configured account must not follow an explicit
        // non-Claude harness.
        if prov == "claude" {
            inject.push(json!(["--account", account.value]));
            applied.push(json!([
                "account",
                account.value,
                format!("{}.account", account.rung),
            ]));
        } else {
            // AC9-UI: never silently drop the pin on a Claude-to-Codex
            // cutover - name the condition the way substrate/permission do.
            messages.push(format!(
                "fno agents spawn: account skipped (accounts are claude-only, resolved \
                 provider {}); {}.account {} ignored",
                repr(prov),
                account.rung,
                repr(&account.value),
            ));
            suppressed.push(json!([
                "account",
                account.value,
                account.rung,
                format!("accounts are claude-only; resolved provider {}", repr(prov)),
            ]));
        }
    }

    // --- model ---------------------------------------------------------- //
    if !model.value.is_empty() && !has_model {
        // The config model is suppressed when something else already owns
        // the model: an injected route (route carries vendor/model), a bare
        // explicit vendor, or a --role that resolves to a real route. An
        // explicit -m already won via has_model.
        if route_present {
            messages.push(format!(
                "fno agents spawn: --route owns the model; not injecting {}.model {}",
                model.rung,
                repr(&model.value),
            ));
            suppressed.push(json!([
                "model",
                model.value,
                model.rung,
                "--route owns the model",
            ]));
        } else if explicit_vendor_present {
            // A bare explicit -P/--provider names the vendor half of a route;
            // injecting the config model would pair a DIFFERENT vendor's model
            // behind the explicit vendor.
            messages.push(format!(
                "fno agents spawn: --provider {} names a vendor; not injecting \
                 {}.model {} (add --model yourself to complete the route)",
                repr(explicit_vendor),
                model.rung,
                repr(&model.value),
            ));
            suppressed.push(json!([
                "model",
                model.value,
                model.rung,
                format!("--provider {} names a vendor", repr(explicit_vendor)),
            ]));
        } else if flag(payload, "role_resolves") {
            // resolve_route is fail-SAFE: only a REAL route owns the model.
            let role = opt_str(payload, "role").unwrap_or_default();
            messages.push(format!(
                "fno agents spawn: --role {} resolves to a route; leaving model to \
                 the route (not injecting {}.model {})",
                repr(&role),
                model.rung,
                repr(&model.value),
            ));
            suppressed.push(json!([
                "model",
                model.value,
                model.rung,
                format!("--role {} resolves to a route", repr(&role)),
            ]));
        } else {
            // A provider-less config model is scoped to its HOME harness (the
            // config provider, else claude), never the ambient one. Inject
            // only when the spawn's resolved TARGET equals that home: a codex
            // spawn must not inherit a claude model (it 400s after the
            // round-trip). The seam precomputed the target; a resolution
            // failure degrades open (AC5-FR).
            let home = if cfg_harness.is_empty() {
                "claude".to_string()
            } else {
                cfg_harness.to_string()
            };
            let failed = flag(payload, "harness_target_failed");
            let target = opt_str(payload, "harness_target").filter(|s| !s.is_empty());
            if failed {
                messages.push(
                    "fno agents spawn: harness resolution failed; leaving model \
                     to the harness"
                        .to_string(),
                );
                suppressed.push(json!([
                    "model",
                    model.value,
                    model.rung,
                    "harness resolution failed",
                ]));
            } else if let Some(t) = target {
                if t == home {
                    inject.push(json!(["--model", model.value]));
                    applied.push(json!([
                        "model",
                        model.value,
                        format!("{}.model", model.rung),
                    ]));
                } else {
                    messages.push(format!(
                        "fno agents spawn: config model {} is scoped to {}; spawn \
                         resolves {}, leaving model to the harness (bind {}.provider \
                         to apply it cross-harness)",
                        repr(&model.value),
                        home,
                        t,
                        model.rung,
                    ));
                    suppressed.push(json!([
                        "model",
                        model.value,
                        model.rung,
                        format!("scoped to {home}; spawn resolves {t}"),
                    ]));
                }
            }
        }
    }

    // --- effort --------------------------------------------------------- //
    // The seam pre-validates the effort surface (effort_tokens) and passes
    // the reason when validation failed; a config value with no reason
    // injects. Explicit --effort remains fail-closed in cmd_spawn.
    let effort = axis(payload, "effort");
    if !flag(payload, "has_effort") && !effort.value.is_empty() {
        match opt_str(payload, "effort_reason").filter(|s| !s.is_empty()) {
            None => {
                inject.push(json!(["--effort", effort.value]));
                applied.push(json!([
                    "effort",
                    effort.value,
                    format!("{}.effort", effort.rung),
                ]));
            }
            Some(reason) => {
                messages.push(format!(
                    "fno agents spawn: effort skipped ({reason}); {}.effort = {} ignored",
                    effort.rung,
                    repr(&effort.value),
                ));
                suppressed.push(json!(["effort", effort.value, effort.rung, reason]));
            }
        }
    }

    // --- substrate ------------------------------------------------------ //
    // A config-sourced substrate must be a KNOWN value AND honored by the
    // resolved provider (the seam precomputes both facts); otherwise it
    // degrades open with the named condition, never failing at the spawn
    // parser. The injected value feeds the permission axis' effective lane.
    let substrate = axis(payload, "substrate");
    let explicit_substrate = opt_str(payload, "explicit_substrate").filter(|s| !s.is_empty());
    let mut injected_substrate: Option<String> = None;
    if explicit_substrate.is_none() && !substrate.value.is_empty() {
        let substrate_ok = flag(payload, "substrate_ok");
        if !prov.is_empty() && substrate_ok {
            inject.push(json!(["--substrate", substrate.value]));
            injected_substrate = Some(substrate.value.clone());
            applied.push(json!([
                "substrate",
                substrate.value,
                format!("{}.substrate", substrate.rung),
            ]));
        } else {
            let reason = if prov.is_empty() {
                "harness resolution failed".to_string()
            } else if flag(payload, "substrate_unknown") {
                format!(
                    "unknown substrate (valid: {})",
                    payload
                        .get("substrate_valid_list")
                        .and_then(Value::as_str)
                        .unwrap_or("pane, thread, headless, bg")
                )
            } else {
                format!(
                    "{prov} does not support substrate {}; thread requires a \
                     journey-proven lane (claude and codex today; opencode remains \
                     unearned), so the spawn falls back to the pane default",
                    repr(&substrate.value)
                )
            };
            messages.push(format!(
                "fno agents spawn: substrate skipped ({reason}); {}.substrate = {} ignored",
                substrate.rung,
                repr(&substrate.value),
            ));
            suppressed.push(json!([
                "substrate",
                substrate.value,
                substrate.rung,
                reason,
            ]));
        }
    }

    // --- permission mode ------------------------------------------------ //
    // Same shape as substrate, but mappability depends on the EFFECTIVE
    // substrate (explicit pin > this-run injection > the pane default): a
    // non-claude bg/headless lane refuses a mapped --permission-mode. Claude
    // honors it everywhere; the pane token check is precomputed by the seam.
    let permission = axis(payload, "permission_mode");
    if !flag(payload, "has_permission") && !permission.value.is_empty() {
        let eff = explicit_substrate
            .or(injected_substrate)
            .unwrap_or_else(|| "pane".to_string());
        let mappable = prov == "claude" || (eff == "pane" && flag(payload, "pane_tokens_ok"));
        if !prov.is_empty() && mappable {
            inject.push(json!(["--permission-mode", permission.value]));
            applied.push(json!([
                "permission_mode",
                permission.value,
                format!("{}.permission_mode", permission.rung),
            ]));
        } else {
            let reason = if prov.is_empty() {
                "harness resolution failed".to_string()
            } else {
                format!(
                    "{prov} cannot map permission mode {} on substrate {}",
                    repr(&permission.value),
                    repr(&eff)
                )
            };
            messages.push(format!(
                "fno agents spawn: permission-mode skipped ({reason}); \
                 {}.permission_mode = {} ignored",
                permission.rung,
                repr(&permission.value),
            ));
            suppressed.push(json!([
                "permission_mode",
                permission.value,
                permission.rung,
                reason,
            ]));
        }
    }

    // --- pane group ----------------------------------------------------- //
    // The placement judgment itself is the spawn-overlay verb's; this only
    // decides what the seam does with its answer. An unavailable overlay
    // degrades open with a named line, never failing the spawn.
    let pane_group = axis(payload, "pane_group");
    let tab_flag_present = flag(payload, "tab_flag_present");
    if !pane_group.value.is_empty() && !tab_flag_present {
        let pg_rung = payload
            .get("pane_group_pg_rung")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(exc) = opt_str(payload, "pane_group_unavailable").filter(|s| !s.is_empty()) {
            messages.push(format!(
                "fno agents spawn: pane group skipped ({exc}); {pg_rung} ignored"
            ));
            suppressed.push(json!([
                "pane_group",
                pane_group.value,
                pane_group.rung,
                exc
            ]));
        } else {
            let answer = payload
                .get("pane_group_answer")
                .cloned()
                .unwrap_or(json!({}));
            // The overlay's "inject" is a boolean or its suggested token
            // list: any non-empty, non-false value means the group applies.
            let injects = match answer.get("inject") {
                Some(Value::Bool(b)) => *b,
                Some(Value::Array(a)) => !a.is_empty(),
                Some(Value::Null) | None => false,
                Some(_) => true,
            };
            if injects {
                inject.push(json!(["--tab", pane_group.value]));
                applied.push(json!(["tab", pane_group.value, pg_rung]));
            } else if let Some(skipped) = answer
                .get("skipped")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                messages.push(skipped.to_string());
                suppressed.push(json!([
                    "pane_group",
                    pane_group.value,
                    pane_group.rung,
                    skipped,
                ]));
            }
        }
    }

    // --- harness bundle ------------------------------------------------- //
    // The overlay's ONE bundle answer (lane args > profile harness overlay >
    // defaults harness overlay, never concatenated) lands behind the --
    // passthrough fence; a boundary the caller already typed displaces it.
    let bundle_json = payload.get("bundle");
    if bundle_json.map(|b| b.is_object()).unwrap_or(false)
        && bundle_json
            .as_ref()
            .and_then(|b| b.get("displaced"))
            .map(|d| !d.is_null())
            .unwrap_or(false)
    {
        let displaced = bundle_json
            .as_ref()
            .and_then(|b| b.get("displaced"))
            .unwrap();
        let boundary = displaced
            .get("boundary")
            .and_then(Value::as_str)
            .unwrap_or("");
        let rung = displaced.get("rung").and_then(Value::as_str).unwrap_or("");
        messages.push(format!(
            "fno agents spawn: harness args skipped (argv already carries a \
             {boundary} passthrough); {rung} ignored"
        ));
    } else if let Some(tokens) = bundle_json
        .as_ref()
        .and_then(|b| b.get("tokens"))
        .and_then(Value::as_array)
    {
        let token_strs: Vec<String> = tokens
            .iter()
            .map(|a| a.as_str().unwrap_or("").to_string())
            .collect();
        let rung = bundle_json
            .as_ref()
            .and_then(|b| b.get("rung"))
            .and_then(Value::as_str)
            .unwrap_or("");
        // click fills positionals in order, so a spawn with no message would
        // eat the bundle's first token as MESSAGE; an explicit empty keeps
        // the slot reserved for the prompt.
        if !flag(payload, "positional_present") {
            out_tail_append_empty = true;
        }
        let mut bundle_tokens: Vec<String> = vec!["--".to_string()];
        bundle_tokens.extend(token_strs.iter().cloned());
        bundle_inject.push(json!(bundle_tokens));
        applied.push(json!(["args", token_strs.join(" "), rung]));
        messages.push(format!(
            "fno agents spawn: bundle {rung} applied unverified (fno reads no \
             effective harness config; confirm on the worker receipt)"
        ));
    }

    let mut out = Map::new();
    out.insert("out_tail_append_empty".into(), json!(out_tail_append_empty));
    out.insert("bundle_inject".into(), json!(bundle_inject));
    out.insert("inject".into(), json!(inject));
    out.insert("applied".into(), json!(applied));
    out.insert("suppressed".into(), json!(suppressed));
    out.insert("messages".into(), json!(messages));
    out.insert("route_injected".into(), json!(route_injected));
    Value::Object(out)
}

/// The verb entry: JSON payload on stdin, decision JSON on stdout (the
/// spawn-overlay shape). Exit 0 even for a "no axes" answer; exit 2 only for
/// transport-level faults (unreadable payload), which the caller reports as
/// the owner being unavailable.
pub fn run_spawn_axes(args: &[String]) -> i32 {
    use std::io::Read;

    let mut payload = String::new();
    let read = if let Some(path) = args.iter().find_map(|a| a.strip_prefix("--payload-file=")) {
        std::fs::read_to_string(path)
    } else {
        std::io::stdin()
            .read_to_string(&mut payload)
            .map(|_| payload.clone())
    };
    let payload = match read {
        Ok(text) => text,
        Err(e) => {
            eprint!("spawn-axes: cannot read payload: {e}\n");
            return 2;
        }
    };
    let parsed: Value = match serde_json::from_str(&payload) {
        Ok(v) => v,
        Err(e) => {
            eprint!("spawn-axes: bad payload: {e}\n");
            return 2;
        }
    };
    println!("{}", decide(&parsed));
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decide_map(payload: Value) -> Map<String, Value> {
        decide(&payload)
            .as_object()
            .cloned()
            .expect("decision is an object")
    }

    #[test]
    fn config_route_injects_when_nothing_explicit() {
        let out = decide_map(json!({
            "route": {"value": "zai/glm", "rung": "agents.defaults"},
            "model": {"value": "", "rung": ""},
        }));
        assert_eq!(out["inject"], json!([["--route", "zai/glm"]]));
        assert_eq!(out["route_injected"], json!(true));
        assert_eq!(out["applied"][0][0], json!("route"));
    }

    #[test]
    fn route_skips_with_the_grid_reason_before_the_model_read() {
        let out = decide_map(json!({
            "route": {"value": "zai/glm", "rung": "agents.defaults"},
            "grid_candidate_present": true,
            "slot_chain": ["slot skip agents.profiles.target.lanes[0] capacity=low"],
        }));
        assert_eq!(out["inject"], json!([]));
        let msg = out["messages"][0].as_str().unwrap();
        assert!(
            msg.starts_with("fno agents spawn: route skipped (the capacity grid chose a lane (")
        );
        assert_eq!(out["suppressed"][0][3], "the capacity grid chose a lane (slot skip agents.profiles.target.lanes[0] capacity=low)");
    }

    #[test]
    fn explicit_vendor_skips_route_with_the_named_condition() {
        let out = decide_map(json!({
            "route": {"value": "zai/glm", "rung": "r"},
            "explicit_vendor_present": true,
            "explicit_vendor": "zai",
        }));
        assert!(out["messages"][0]
            .as_str()
            .unwrap()
            .contains("the caller passed --provider 'zai'"));
    }

    #[test]
    fn claude_account_injects_and_non_claude_skips_by_name() {
        let out = decide_map(json!({
            "account": {"value": "readyrule", "rung": "agents.defaults"},
            "prov": "claude",
        }));
        assert_eq!(out["inject"], json!([["--account", "readyrule"]]));
        let out = decide_map(json!({
            "account": {"value": "readyrule", "rung": "agents.defaults"},
            "prov": "codex",
        }));
        assert_eq!(out["inject"], json!([]));
        assert!(out["messages"][0]
            .as_str()
            .unwrap()
            .starts_with("fno agents spawn: account skipped (accounts are claude-only, resolved provider 'codex')"));
    }

    #[test]
    fn route_owns_the_model_over_a_config_model() {
        let out = decide_map(json!({
            "route": {"value": "zai/glm", "rung": "agents.defaults"},
            "model": {"value": "claude-opus-5", "rung": "agents.defaults"},
        }));
        // The route injects, and because a route is present the config model
        // is suppressed: an injected route carries vendor AND model.
        assert_eq!(out["inject"], json!([["--route", "zai/glm"]]));
        assert_eq!(out["route_injected"], json!(true));
        assert!(out["messages"][0].as_str().unwrap().contains(
            "--route owns the model; not injecting agents.defaults.model 'claude-opus-5'"
        ));
    }

    #[test]
    fn config_model_scopes_to_home_and_cross_harness_target_skips() {
        let out = decide_map(json!({
            "model": {"value": "claude-opus-5", "rung": "agents.defaults"},
            "cfg_harness": "",
            "harness_target": "claude",
        }));
        assert_eq!(out["inject"], json!([["--model", "claude-opus-5"]]));
        let out = decide_map(json!({
            "model": {"value": "claude-opus-5", "rung": "agents.defaults"},
            "cfg_harness": "",
            "harness_target": "codex",
        }));
        assert_eq!(out["inject"], json!([]));
        assert!(out["messages"][0]
            .as_str()
            .unwrap()
            .contains("is scoped to claude; spawn resolves codex"));
    }

    #[test]
    fn resolution_failure_degrades_open_with_the_named_message() {
        let out = decide_map(json!({
            "model": {"value": "claude-opus-5", "rung": "agents.defaults"},
            "harness_target_failed": true,
        }));
        assert_eq!(out["inject"], json!([]));
        assert_eq!(
            out["messages"][0],
            json!("fno agents spawn: harness resolution failed; leaving model to the harness")
        );
    }

    #[test]
    fn explicit_model_wins_and_the_config_route_names_it() {
        let out = decide_map(json!({
            "route": {"value": "zai/glm", "rung": "r"},
            "model": {"value": "claude-opus-5", "rung": "r"},
            "explicit_model_present": true,
            "has_model": true,
        }));
        // explicit --route suppresses the config route (named before any
        // --model read); explicit -m means the config model block
        // short-circuits: neither injects.
        assert_eq!(out["inject"], json!([]));
        assert!(out["messages"][0]
            .as_str()
            .unwrap()
            .contains("route skipped (the caller passed --model)"));
        assert_eq!(out["route_injected"], json!(false));
    }
}
